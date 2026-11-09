//! 共用进程意外没了之后，对话在下一次发消息时自动重开（AI 对话上下文保持 B）。
//!
//! opencode / codex 的对话挂在一个共用进程里。进程崩了、被杀了、或者工具违规被我们结束了，
//! 对话句柄里的 HTTP / JSON-RPC 通道就全断了，再发消息只会报"事件流已断开，请新开一个对话"，
//! 用户在界面里的对话和模型已经看到的上下文都白白丢掉。能力密钥（`ScopeHandle`）、界面的消息、
//! 智能体侧的会话 id 其实都还在，缺的只是一个活着的进程。
//!
//! 所以 [`ReopeningConversation`] 包在驱动句柄外面：每次发消息前看一眼驱动的
//! [`AgentConversation::runtime_closed`]，进程没了就让 hub 拿一个新的共用进程，以**同一把能力
//! 密钥**和原来的会话标识重开对话，换掉里面的驱动句柄，再发这条消息。事件仍然发往原来那条
//! 事件通道，界面那一侧什么都不用知道。回合进行中进程死了不在这里处理（保持现有的报错收尾）：
//! 重开只发生在两次发送之间。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use zssh_core::ai::agent::{AgentConversation, AgentError, AgentEvent, AgentModelCatalog};
use zssh_core::ssh::BoxFuture;

use super::AgentHostMsg;
use crate::i18n::t;

/// 重开对话时交给"开会话函数"的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReopenRequest {
    /// 对话的能力密钥：和第一次开对话时是同一把，scope 不重建。
    pub capability: String,
    /// 原来对话的续接标识（`resume_id()`）；没有就开一个新会话。
    pub resume: Option<String>,
}

/// 新开出来的驱动句柄，和"是不是真的接上了原来的会话"。
pub type Reopened = (Arc<dyn AgentConversation>, bool);

/// 开会话函数：产品里是"向 hub 要一个活着的共用进程，在里面续接会话"，测试里换成替身。
pub type Reopen =
    Arc<dyn Fn(ReopenRequest) -> BoxFuture<'static, Result<Reopened, String>> + Send + Sync>;

/// 进程没了会自动重开的对话（只用在 opencode / codex 上）。
pub struct ReopeningConversation {
    current: Mutex<Arc<dyn AgentConversation>>,
    capability: String,
    /// 驱动事件的出口：重开出来的新驱动句柄往这里发（和旧的是同一条通道）。
    events: mpsc::UnboundedSender<AgentEvent>,
    reopen: Reopen,
    /// 重开和结束对话互斥：并发的两次发送只重开一次，重开的同时关闭对话不会漏关新句柄。
    gate: tokio::sync::Mutex<()>,
    /// 被换掉的旧句柄用过的动态密钥：历史文本里也要抹掉。
    retired_secrets: Mutex<Vec<String>>,
    /// 第一个驱动句柄的会话标识（`session_id()` 要返回引用，只能留这一份；续接用 `resume_id()`）。
    first_id: String,
    ended: AtomicBool,
}

impl ReopeningConversation {
    pub fn wrap(
        conversation: Arc<dyn AgentConversation>,
        capability: String,
        events: mpsc::UnboundedSender<AgentEvent>,
        reopen: Reopen,
    ) -> Arc<dyn AgentConversation> {
        Arc::new(Self {
            first_id: conversation.session_id().to_owned(),
            current: Mutex::new(conversation),
            capability,
            events,
            reopen,
            gate: tokio::sync::Mutex::new(()),
            retired_secrets: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
        })
    }

    fn current(&self) -> Arc<dyn AgentConversation> {
        Arc::clone(
            &self
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn ended_error() -> AgentError {
        AgentError::Other(t(AgentHostMsg::ConversationEnded).to_owned())
    }

    /// 发送之前：进程还活着什么也不做；没了就重开。重开失败时旧句柄原样留着，下一次发送再试。
    async fn ensure_live(&self) -> Result<(), AgentError> {
        if self.ended.load(Ordering::SeqCst) {
            return Err(Self::ended_error());
        }
        if !self.current().runtime_closed() {
            return Ok(());
        }
        let _gate = self.gate.lock().await;
        if self.ended.load(Ordering::SeqCst) {
            return Err(Self::ended_error());
        }
        let old = self.current();
        // 排在前面的那次发送已经重开过了。
        if !old.runtime_closed() {
            return Ok(());
        }
        let request = ReopenRequest {
            capability: self.capability.clone(),
            resume: old.resume_id(),
        };
        let (fresh, resumed) = (self.reopen)(request).await.map_err(|err| {
            AgentError::Other(format!("{}{err}", t(AgentHostMsg::ReopenFailedPrefix)))
        })?;
        if self.ended.load(Ordering::SeqCst) {
            // 重开期间对话被结束了：新开的句柄没人要，正经关掉。
            fresh.shutdown().await;
            return Err(Self::ended_error());
        }
        self.retired_secrets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(old.history_secrets());
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
        // 旧句柄只是从（已经死掉的）进程的对话表里摘掉；能力密钥属于 scope，不受影响。
        old.shutdown().await;
        tracing::info!(resumed, "共用进程已经退出，已按原密钥重开对话");
        if !resumed {
            // 智能体那边的会话接不上：消息还在界面上，但模型没有上文，照实提示。
            let _ = self.events.send(AgentEvent::ResumeFailed);
        }
        Ok(())
    }
}

impl AgentConversation for ReopeningConversation {
    fn session_id(&self) -> &str {
        &self.first_id
    }

    fn resume_id(&self) -> Option<String> {
        self.current().resume_id()
    }

    fn history_secrets(&self) -> Vec<String> {
        let mut secrets = self
            .retired_secrets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        secrets.extend(self.current().history_secrets());
        secrets
    }

    fn runtime_closed(&self) -> bool {
        self.current().runtime_closed()
    }

    fn models<'a>(&'a self) -> BoxFuture<'a, Result<AgentModelCatalog, AgentError>> {
        Box::pin(async move { self.current().models().await })
    }

    fn send<'a>(
        &'a self,
        text: &'a str,
        model: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            self.ensure_live().await?;
            self.current().send(text, model).await
        })
    }

    fn stop<'a>(&'a self) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move { self.current().stop().await })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.ended.store(true, Ordering::SeqCst);
            // 等正在进行的重开收尾（它看到 `ended` 会自己关掉新句柄），再关当前的。
            let _gate = self.gate.lock().await;
            self.current().shutdown().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 能被"杀掉进程"的驱动替身：记下收到的消息、关闭次数。
    struct FakeConversation {
        id: String,
        resume: Option<String>,
        closed: AtomicBool,
        sent: Mutex<Vec<String>>,
        shutdowns: AtomicUsize,
        secrets: Vec<String>,
    }

    impl FakeConversation {
        fn new(id: &str, resume: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_owned(),
                resume: resume.map(str::to_owned),
                closed: AtomicBool::new(false),
                sent: Mutex::new(Vec::new()),
                shutdowns: AtomicUsize::new(0),
                secrets: Vec::new(),
            })
        }

        fn with_secret(id: &str, resume: Option<&str>, secret: &str) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_owned(),
                resume: resume.map(str::to_owned),
                closed: AtomicBool::new(false),
                sent: Mutex::new(Vec::new()),
                shutdowns: AtomicUsize::new(0),
                secrets: vec![secret.to_owned()],
            })
        }

        fn kill(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl AgentConversation for FakeConversation {
        fn session_id(&self) -> &str {
            &self.id
        }

        fn resume_id(&self) -> Option<String> {
            self.resume.clone()
        }

        fn history_secrets(&self) -> Vec<String> {
            self.secrets.clone()
        }

        fn runtime_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }

        fn models<'a>(&'a self) -> BoxFuture<'a, Result<AgentModelCatalog, AgentError>> {
            Box::pin(async { Ok(AgentModelCatalog::default()) })
        }

        fn send<'a>(
            &'a self,
            text: &'a str,
            _model: Option<&'a str>,
        ) -> BoxFuture<'a, Result<(), AgentError>> {
            Box::pin(async move {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(AgentError::Transport(
                        "事件流已断开，请新开一个对话".to_owned(),
                    ));
                }
                self.sent.lock().unwrap().push(text.to_owned());
                Ok(())
            })
        }

        fn stop<'a>(&'a self) -> BoxFuture<'a, Result<(), AgentError>> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    type Answer = Result<(Arc<FakeConversation>, bool), String>;

    /// 开会话函数替身：记下每次收到的请求，按预设的结果依次应答。
    struct Opener {
        requests: Mutex<Vec<ReopenRequest>>,
        answers: Mutex<Vec<Answer>>,
    }

    impl Opener {
        fn new(answers: Vec<Answer>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                answers: Mutex::new(answers.into_iter().rev().collect()),
            })
        }

        fn as_reopen(self: &Arc<Self>) -> Reopen {
            let this = Arc::clone(self);
            Arc::new(move |request| {
                let this = Arc::clone(&this);
                Box::pin(async move {
                    this.requests.lock().unwrap().push(request);
                    // 让并发的两次发送真的能交错起来。
                    tokio::task::yield_now().await;
                    let answer = this
                        .answers
                        .lock()
                        .unwrap()
                        .pop()
                        .unwrap_or_else(|| Err("没有预设的应答".to_owned()));
                    answer.map(|(conversation, resumed)| {
                        (conversation as Arc<dyn AgentConversation>, resumed)
                    })
                })
            })
        }

        fn requests(&self) -> Vec<ReopenRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn rig(
        first: &Arc<FakeConversation>,
        opener: &Arc<Opener>,
    ) -> (
        Arc<dyn AgentConversation>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (events, rx) = mpsc::unbounded_channel();
        let wrapped = ReopeningConversation::wrap(
            Arc::clone(first) as Arc<dyn AgentConversation>,
            "cap-original".to_owned(),
            events,
            opener.as_reopen(),
        );
        (wrapped, rx)
    }

    #[tokio::test]
    async fn a_closed_process_is_replaced_before_sending_with_the_same_key_and_resume_id() {
        let first = FakeConversation::new("thread-1", Some("thread-1"));
        let fresh = FakeConversation::new("thread-1", Some("thread-1"));
        let opener = Opener::new(vec![Ok((Arc::clone(&fresh), true))]);
        let (conversation, mut events) = rig(&first, &opener);

        // 进程活着：直接发给原来的句柄，不重开。
        conversation.send("第一条", None).await.unwrap();
        assert_eq!(first.sent(), vec!["第一条"]);
        assert!(opener.requests().is_empty());

        first.kill();
        conversation.send("第二条", Some("m")).await.unwrap();

        assert_eq!(
            opener.requests(),
            vec![ReopenRequest {
                capability: "cap-original".to_owned(),
                resume: Some("thread-1".to_owned()),
            }],
            "要用原来的密钥和原来的会话标识重开"
        );
        assert_eq!(fresh.sent(), vec!["第二条"], "消息发给了新句柄");
        assert_eq!(first.sent(), vec!["第一条"], "旧句柄不再收消息");
        assert_eq!(
            first.shutdowns.load(Ordering::SeqCst),
            1,
            "被换掉的旧句柄要摘干净"
        );
        assert!(
            events.try_recv().is_err(),
            "接上了原来的会话，不该提示上下文丢失"
        );
        assert_eq!(conversation.resume_id(), Some("thread-1".to_owned()));
    }

    #[tokio::test]
    async fn when_the_old_session_cannot_be_resumed_the_user_is_told_and_the_message_still_goes_out()
     {
        let first = FakeConversation::new("thread-1", Some("thread-1"));
        let fresh = FakeConversation::new("thread-2", Some("thread-2"));
        let opener = Opener::new(vec![Ok((Arc::clone(&fresh), false))]);
        let (conversation, mut events) = rig(&first, &opener);

        first.kill();
        conversation.send("继续", None).await.unwrap();

        assert_eq!(events.try_recv().unwrap(), AgentEvent::ResumeFailed);
        assert!(events.try_recv().is_err(), "只提示一次");
        assert_eq!(fresh.sent(), vec!["继续"]);
        // 密钥仍然是原来那把，只是会话是新的。
        assert_eq!(opener.requests()[0].capability, "cap-original");
        assert_eq!(conversation.resume_id(), Some("thread-2".to_owned()));
    }

    #[tokio::test]
    async fn a_failed_reopen_fails_the_send_cleanly_and_the_next_send_tries_again() {
        let first = FakeConversation::new("thread-1", Some("thread-1"));
        let fresh = FakeConversation::new("thread-1", Some("thread-1"));
        let opener = Opener::new(vec![
            Err("codex 启动失败".to_owned()),
            Ok((Arc::clone(&fresh), true)),
        ]);
        let (conversation, mut events) = rig(&first, &opener);

        first.kill();
        let error = conversation.send("丢不了", None).await.unwrap_err();
        assert!(
            error.to_string().contains("codex 启动失败"),
            "原因要带给界面：{error}"
        );
        assert!(first.sent().is_empty() && fresh.sent().is_empty());
        assert!(events.try_recv().is_err());
        assert_eq!(
            first.shutdowns.load(Ordering::SeqCst),
            0,
            "没换成就别摘旧的"
        );

        // 下一次发送重新尝试，这回成功。
        conversation.send("丢不了", None).await.unwrap();
        assert_eq!(opener.requests().len(), 2);
        assert_eq!(fresh.sent(), vec!["丢不了"]);
    }

    #[tokio::test]
    async fn two_sends_racing_on_a_dead_process_reopen_only_once() {
        let first = FakeConversation::new("thread-1", Some("thread-1"));
        let fresh = FakeConversation::new("thread-1", Some("thread-1"));
        let opener = Opener::new(vec![Ok((Arc::clone(&fresh), true))]);
        let (conversation, _events) = rig(&first, &opener);

        first.kill();
        let (a, b) = tokio::join!(conversation.send("甲", None), conversation.send("乙", None));
        a.unwrap();
        b.unwrap();
        assert_eq!(opener.requests().len(), 1);
        let mut sent = fresh.sent();
        sent.sort();
        assert_eq!(sent, vec!["乙", "甲"]);
    }

    #[tokio::test]
    async fn secrets_of_replaced_handles_stay_in_the_history_scrub_list() {
        let first = FakeConversation::with_secret("thread-1", Some("thread-1"), "old-secret");
        let fresh = FakeConversation::with_secret("thread-1", Some("thread-1"), "new-secret");
        let opener = Opener::new(vec![Ok((Arc::clone(&fresh), true))]);
        let (conversation, _events) = rig(&first, &opener);

        first.kill();
        conversation.send("x", None).await.unwrap();
        let secrets = conversation.history_secrets();
        assert!(secrets.contains(&"old-secret".to_owned()), "{secrets:?}");
        assert!(secrets.contains(&"new-secret".to_owned()), "{secrets:?}");
    }

    #[tokio::test]
    async fn after_shutdown_nothing_is_reopened_or_sent() {
        let first = FakeConversation::new("thread-1", Some("thread-1"));
        let opener = Opener::new(vec![Ok((FakeConversation::new("x", None), true))]);
        let (conversation, _events) = rig(&first, &opener);

        conversation.shutdown().await;
        assert_eq!(first.shutdowns.load(Ordering::SeqCst), 1);
        first.kill();
        assert!(conversation.send("晚了", None).await.is_err());
        assert!(opener.requests().is_empty(), "结束了的对话不能再拉起进程");
    }

    #[tokio::test]
    async fn shutdown_after_a_reopen_closes_the_new_handle() {
        let first = FakeConversation::new("thread-1", Some("thread-1"));
        let fresh = FakeConversation::new("thread-1", Some("thread-1"));
        let opener = Opener::new(vec![Ok((Arc::clone(&fresh), true))]);
        let (conversation, _events) = rig(&first, &opener);

        first.kill();
        conversation.send("x", None).await.unwrap();
        conversation.shutdown().await;
        assert_eq!(fresh.shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(
            first.shutdowns.load(Ordering::SeqCst),
            1,
            "旧的不会被关第二次"
        );
    }
}
