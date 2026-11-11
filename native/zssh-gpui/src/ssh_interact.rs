//! GPUI 入口的 SSH 交互：主机指纹确认、密码 / 口令 / keyboard-interactive 的输入。
//!
//! 这里不弹任何东西：请求交给 [`crate::ssh_prompt`] 的中转站，由某个窗口里的应用内对话框
//! （`ssh_prompt_ui`）显示，用户答完再送回来。连接 future 被丢掉（标签关了、连接取消）时请求自动撤回，
//! 对话框跟着收起。
//!
//! 自动化可用 `ZSSH_SSH_TRUST_ONCE=1` 跳过指纹确认（只信任本次，不写 known_hosts）；
//! 该钩子只在调试构建里生效，见 `dev_hooks`。

use std::sync::Arc;

use tokio::sync::watch;
use zssh_core::ssh::{
    BoxFuture, HopProgress, HostKeyDecision, HostKeyInfo, Interactor, PromptAnswers, PromptRequest,
};

use crate::ssh_prompt::{Answer, Ask, Hub, hub};

/// 把提问交给应用内对话框的交互器。
pub struct NativeInteractor {
    hub: Arc<Hub>,
}

impl NativeInteractor {
    /// 接在指定的中转站上（测试用自己的；正式路径用 [`interactor`]）。
    pub fn with_hub(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

impl Interactor for NativeInteractor {
    fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async move {
            if crate::dev_hooks::var_os("ZSSH_SSH_TRUST_ONCE").is_some() {
                return HostKeyDecision::TrustOnce;
            }
            let mut pending = self.hub.submit(Ask::HostKey(info));
            match pending.wait().await {
                Some(Answer::HostKey(decision)) => decision,
                // 请求被撤销（窗口关了）或种类对不上：按拒绝。
                _ => HostKeyDecision::Reject,
            }
        })
    }

    fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        Box::pin(async move {
            // 没有题目就没什么可问的，按取消。
            if request.prompts.is_empty() {
                return None;
            }
            let mut pending = self.hub.submit(Ask::Prompt(request));
            match pending.wait().await {
                Some(Answer::Prompt(answers)) => answers,
                _ => None,
            }
        })
    }
}

/// 让入口只拿一个 `Arc<dyn Interactor>`。
pub fn interactor() -> Arc<dyn Interactor> {
    Arc::new(NativeInteractor::with_hub(Arc::clone(hub())))
}

/// 给没有保险库记录可更新的连接用（快速连接：密码是现填的，不是存的）：把提问里的「保存」选项关掉，
/// 免得重新询问时给出一个勾了也不会有任何效果的「更新保存的密码」勾选框。
pub fn without_remember(inner: Arc<dyn Interactor>) -> Arc<dyn Interactor> {
    Arc::new(WithoutRemember { inner })
}

struct WithoutRemember {
    inner: Arc<dyn Interactor>,
}

impl Interactor for WithoutRemember {
    fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        self.inner.confirm_host_key(info)
    }

    fn prompt(&self, mut request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        request.can_remember = false;
        self.inner.prompt(request)
    }

    fn hop_started(&self, progress: HopProgress) -> BoxFuture<'_, ()> {
        self.inner.hop_started(progress)
    }

    fn authentication_started(&self) -> BoxFuture<'_, ()> {
        self.inner.authentication_started()
    }

    fn credential_rejected(&self) {
        self.inner.credential_rejected();
    }
}

/// 一次连接尝试的“取消开关”。握着它的那段代码被丢掉（拨号任务被中止：标签关了、连接取消）而没有
/// [`CancelGuard::disarm`] 时，包在 [`scoped`] 里的交互器会让正在等回答的提问立刻撤回、按拒绝 / 取消返回。
///
/// 为什么需要：主机指纹确认发生在 russh 自己起的会话任务里，连接 future 被丢掉并不会把它一起丢掉，
/// 没有这个开关，对话框就会一直开着（用户点了“信任并保存”还会写进 known_hosts）。
pub struct CancelGuard {
    tx: watch::Sender<bool>,
    armed: bool,
}

impl CancelGuard {
    /// 连接已经有结果（成功或失败）：不再取消。
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tx.send(true);
        }
    }
}

/// 给一个交互器套上取消开关。
pub fn scoped(inner: Arc<dyn Interactor>) -> (Arc<dyn Interactor>, CancelGuard) {
    let (tx, rx) = watch::channel(false);
    (
        Arc::new(Scoped {
            inner,
            cancelled: rx,
        }),
        CancelGuard { tx, armed: true },
    )
}

struct Scoped {
    inner: Arc<dyn Interactor>,
    cancelled: watch::Receiver<bool>,
}

impl Scoped {
    /// 取消开关被触发时才返回；开关被放手（`disarm` 后丢掉）则永远不返回。
    async fn cancelled(&self) {
        let mut rx = self.cancelled.clone();
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Interactor for Scoped {
    fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async move {
            tokio::select! {
                decision = self.inner.confirm_host_key(info) => decision,
                () = self.cancelled() => HostKeyDecision::Reject,
            }
        })
    }

    fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        Box::pin(async move {
            tokio::select! {
                answers = self.inner.prompt(request) => answers,
                () = self.cancelled() => None,
            }
        })
    }

    fn hop_started(&self, progress: HopProgress) -> BoxFuture<'_, ()> {
        self.inner.hop_started(progress)
    }

    fn authentication_started(&self) -> BoxFuture<'_, ()> {
        self.inner.authentication_started()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use zssh_core::ssh::{AuthConfig, ChainHop, Connection, HostKeyStatus, KnownHosts, PromptKind};

    use super::*;
    use crate::ssh_prompt::{HostKeyButton, button_decision};
    use crate::test_ssh_server;

    /// 临时目录里的 known_hosts；随机唯一的名字，用完整个目录删掉。
    struct TempDir(tempfile::TempDir);

    impl TempDir {
        fn new(tag: &str) -> Self {
            Self(
                tempfile::Builder::new()
                    .prefix(&format!("zssh-u29-{tag}-"))
                    .tempdir()
                    .expect("临时目录"),
            )
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    /// 代替“窗口 + 用户点击”：盯着中转站，看到指纹确认就点 `button`；
    /// 看到输入框就按 `password` 回答。返回值是线程句柄和“看到过几次指纹确认”的计数。
    fn spawn_clicker(
        hub: Arc<Hub>,
        button: HostKeyButton,
        password: Option<&'static str>,
    ) -> (std::thread::JoinHandle<()>, Arc<AtomicUsize>) {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        hub.register(1, true);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + std::time::Duration::from_secs(20);
            // 连接那一侧（交互器）放手之后中转站只剩这里一份引用，就收工。
            while Instant::now() < deadline && Arc::strong_count(&hub) > 1 {
                if let Some(shown) = hub.poll(1, false, Instant::now()) {
                    match &shown.ask {
                        Ask::HostKey(info) => {
                            counter.fetch_add(1, Ordering::SeqCst);
                            let decision = button_decision(info.status, button);
                            hub.answer(shown.id, Answer::HostKey(decision));
                        }
                        Ask::Prompt(request) => {
                            let answer = password.map(|pw| PromptAnswers {
                                answers: request.prompts.iter().map(|_| pw.to_owned()).collect(),
                                remember: false,
                            });
                            hub.answer(shown.id, Answer::Prompt(answer));
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        (handle, seen)
    }

    fn hops(addr: std::net::SocketAddr, dir: &Path, password: Option<&str>) -> Vec<ChainHop> {
        vec![ChainHop {
            label: "测试服务器".to_owned(),
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth: AuthConfig::password(password.map(str::to_owned)),
            known_hosts: KnownHosts::new(dir.join("known_hosts")),
            legacy_algorithms: false,
        }]
    }

    #[tokio::test]
    async fn first_connection_trust_and_save_writes_known_hosts() {
        let dir = TempDir::new("save");
        let addr = test_ssh_server::spawn().await;
        let hub = Arc::new(Hub::default());
        let (clicker, seen) = spawn_clicker(Arc::clone(&hub), HostKeyButton::TrustAndSave, None);
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        let connection =
            Connection::connect_chain(hops(addr, dir.path(), Some("pw")), interactor).await;

        let known = KnownHosts::new(dir.path().join("known_hosts"));
        assert!(connection.is_ok(), "信任并保存后应该连上");
        assert_eq!(seen.load(Ordering::SeqCst), 1, "只问了一次指纹");
        assert!(
            known
                .saved_fingerprint(&addr.ip().to_string(), addr.port())
                .is_some(),
            "known_hosts 里应该有这台主机的记录"
        );
        let _ = clicker;
    }

    #[tokio::test]
    async fn trust_once_connects_without_saving() {
        let dir = TempDir::new("once");
        let addr = test_ssh_server::spawn().await;
        let hub = Arc::new(Hub::default());
        let (_clicker, seen) = spawn_clicker(Arc::clone(&hub), HostKeyButton::TrustOnce, None);
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        let connection =
            Connection::connect_chain(hops(addr, dir.path(), Some("pw")), interactor).await;

        assert!(connection.is_ok());
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let known = KnownHosts::new(dir.path().join("known_hosts"));
        assert!(
            known
                .saved_fingerprint(&addr.ip().to_string(), addr.port())
                .is_none(),
            "仅本次不写 known_hosts"
        );
    }

    #[tokio::test]
    async fn cancelling_the_fingerprint_dialog_fails_the_connection_and_saves_nothing() {
        let dir = TempDir::new("cancel");
        let addr = test_ssh_server::spawn().await;
        let hub = Arc::new(Hub::default());
        let (_clicker, seen) = spawn_clicker(Arc::clone(&hub), HostKeyButton::Cancel, None);
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        let connection =
            Connection::connect_chain(hops(addr, dir.path(), Some("pw")), interactor).await;

        assert!(connection.is_err(), "取消后连接应该失败");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(
            !dir.path().join("known_hosts").exists()
                || KnownHosts::new(dir.path().join("known_hosts"))
                    .saved_fingerprint(&addr.ip().to_string(), addr.port())
                    .is_none(),
            "取消不留记录"
        );
    }

    #[tokio::test]
    async fn the_password_dialog_answer_is_used_to_log_in() {
        let dir = TempDir::new("password");
        let addr = test_ssh_server::spawn_requiring_password("hunter2").await;
        let hub = Arc::new(Hub::default());
        let (_clicker, _seen) = spawn_clicker(
            Arc::clone(&hub),
            HostKeyButton::TrustAndSave,
            Some("hunter2"),
        );
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        // 没带密码：认证时会弹输入框，按“用户输入”回答。
        let connection = Connection::connect_chain(hops(addr, dir.path(), None), interactor).await;

        assert!(connection.is_ok(), "输入框里填的密码应该用于登录");
    }

    #[tokio::test]
    async fn cancelling_the_password_dialog_aborts_the_connection() {
        let dir = TempDir::new("password-cancel");
        let addr = test_ssh_server::spawn_requiring_password("hunter2").await;
        let hub = Arc::new(Hub::default());
        // password = None：输入框一律按取消回答。
        let (_clicker, _seen) = spawn_clicker(Arc::clone(&hub), HostKeyButton::TrustAndSave, None);
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        let connection = Connection::connect_chain(hops(addr, dir.path(), None), interactor).await;

        assert!(connection.is_err());
    }

    #[tokio::test]
    async fn dropping_the_connection_withdraws_the_open_dialog() {
        let dir = TempDir::new("withdraw");
        let addr = test_ssh_server::spawn().await;
        let hub = Arc::new(Hub::default());
        hub.register(1, true);
        let interactor: Arc<dyn Interactor> =
            Arc::new(NativeInteractor::with_hub(Arc::clone(&hub)));
        let chain = hops(addr, dir.path(), Some("pw"));
        // 和 `ssh_backend::acquire_factory` 里一样：拨号任务握着取消开关。
        let task = tokio::spawn(async move {
            let (interactor, guard) = scoped(interactor);
            let _ = Connection::connect_chain(chain, interactor).await;
            guard.disarm();
        });
        // 等指纹对话框出现。
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let shown = loop {
            if let Some(shown) = hub.poll(1, false, Instant::now()) {
                break shown;
            }
            assert!(Instant::now() < deadline, "对话框没有出现");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(matches!(
            shown.ask,
            Ask::HostKey(HostKeyInfo {
                status: HostKeyStatus::Unknown,
                ..
            })
        ));
        // 连接被取消（标签关了）：请求撤回，对话框下一次轮询就没了。
        task.abort();
        let _ = task.await;
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while hub.poll(1, false, Instant::now()).is_some() {
            assert!(Instant::now() < deadline, "取消之后对话框还开着");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 另一把随机主机密钥的公钥：假装 known_hosts 里存着“旧指纹”。
    fn other_public_key() -> russh::keys::PublicKey {
        russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
            .expect("生成密钥")
            .public_key()
            .clone()
    }

    #[tokio::test]
    async fn a_changed_fingerprint_is_replaced_only_after_update_and_connect() {
        let dir = TempDir::new("changed-update");
        let addr = test_ssh_server::spawn().await;
        let (host, port) = (addr.ip().to_string(), addr.port());
        let known = KnownHosts::new(dir.path().join("known_hosts"));
        known
            .append(&host, port, &other_public_key())
            .expect("写入旧指纹");
        let old = known.saved_fingerprint(&host, port);
        assert!(old.is_some());
        let hub = Arc::new(Hub::default());
        let (_clicker, seen) =
            spawn_clicker(Arc::clone(&hub), HostKeyButton::UpdateAndConnect, None);
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        let connection =
            Connection::connect_chain(hops(addr, dir.path(), Some("pw")), interactor).await;

        assert!(connection.is_ok(), "更新指纹并连接后应该连上");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let now = known.saved_fingerprint(&host, port);
        assert!(now.is_some() && now != old, "旧指纹应该被换成新的");
    }

    #[tokio::test]
    async fn cancelling_a_changed_fingerprint_keeps_the_old_record_and_refuses() {
        let dir = TempDir::new("changed-cancel");
        let addr = test_ssh_server::spawn().await;
        let (host, port) = (addr.ip().to_string(), addr.port());
        let known = KnownHosts::new(dir.path().join("known_hosts"));
        known
            .append(&host, port, &other_public_key())
            .expect("写入旧指纹");
        let old = known.saved_fingerprint(&host, port);
        let hub = Arc::new(Hub::default());
        let (_clicker, seen) = spawn_clicker(Arc::clone(&hub), HostKeyButton::Cancel, None);
        let interactor: Arc<dyn Interactor> = Arc::new(NativeInteractor::with_hub(hub));

        let connection =
            Connection::connect_chain(hops(addr, dir.path(), Some("pw")), interactor).await;

        assert!(connection.is_err(), "取消后不能连上指纹变了的主机");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(known.saved_fingerprint(&host, port), old, "旧记录不动");
    }

    fn demo_key_info() -> HostKeyInfo {
        HostKeyInfo {
            host: "demo.example.com".to_owned(),
            port: 22,
            key_type: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:demo".to_owned(),
            status: HostKeyStatus::Unknown,
            stored_fingerprint: None,
            saved_keys: Vec::new(),
        }
    }

    #[tokio::test]
    async fn dropping_an_armed_guard_declines_the_waiting_question() {
        let hub = Arc::new(Hub::default());
        hub.register(1, true);
        let (scoped, guard) = scoped(Arc::new(NativeInteractor::with_hub(Arc::clone(&hub))));
        let waiting = tokio::spawn(async move { scoped.confirm_host_key(demo_key_info()).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(hub.poll(1, false, Instant::now()).is_some(), "对话框开着");
        drop(guard);
        let decision = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("取消后应该很快返回")
            .expect("任务没 panic");
        assert_eq!(decision, HostKeyDecision::Reject);
        assert!(hub.poll(1, false, Instant::now()).is_none(), "请求一并撤回");
    }

    #[tokio::test]
    async fn a_disarmed_guard_leaves_the_question_alone() {
        let hub = Arc::new(Hub::default());
        hub.register(1, true);
        let (scoped, guard) = scoped(Arc::new(NativeInteractor::with_hub(Arc::clone(&hub))));
        guard.disarm();
        let still_waiting = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            scoped.confirm_host_key(demo_key_info()),
        )
        .await;
        assert!(still_waiting.is_err(), "连接已经成功之后不该被当成取消");
    }

    /// 记下每次提问、用同一个密码回答的交互器（指纹一律「仅本次」）。
    struct Recording {
        requests: std::sync::Mutex<Vec<PromptRequest>>,
    }

    impl Interactor for Recording {
        fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::TrustOnce })
        }

        fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
            Box::pin(async move {
                let count = request.prompts.len();
                if let Ok(mut seen) = self.requests.lock() {
                    seen.push(request);
                }
                Some(PromptAnswers {
                    answers: vec!["hunter2".to_owned(); count],
                    remember: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn a_quick_connect_never_offers_to_update_a_saved_password() {
        // 快速连接的密码是现填的，保险库里没有对应的记录：「更新保存的密码」勾了也没处写，所以关掉。
        let recording = Arc::new(Recording {
            requests: std::sync::Mutex::new(Vec::new()),
        });
        let inner: Arc<dyn Interactor> = Arc::clone(&recording) as Arc<dyn Interactor>;
        let interactor = without_remember(inner);
        let mut request = PromptRequest::single(PromptKind::Password, "ops@web-1:22", "密码", true);
        request.reason = Some(zssh_core::ssh::PromptReason::SavedPasswordRejected);
        assert!(interactor.prompt(request).await.is_some());
        let seen = recording.requests.lock().expect("锁");
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].can_remember, "不能给勾选框");
        assert_eq!(
            seen[0].reason,
            Some(zssh_core::ssh::PromptReason::SavedPasswordRejected),
            "原因照旧传给界面"
        );
    }

    #[tokio::test]
    async fn the_password_prompt_names_the_hop_that_is_being_authenticated() {
        let dir = TempDir::new("target-chain");
        let jump = test_ssh_server::spawn_forwarding().await;
        let target = test_ssh_server::spawn_requiring_password("hunter2").await;
        let mut chain = hops(jump, dir.path(), None);
        chain[0].username = "jumper".to_owned();
        let mut last = hops(target, dir.path(), None);
        last[0].username = "ops".to_owned();
        chain.append(&mut last);
        let recording = Arc::new(Recording {
            requests: std::sync::Mutex::new(Vec::new()),
        });
        let interactor: Arc<dyn Interactor> = Arc::clone(&recording) as Arc<dyn Interactor>;

        let connection = Connection::connect_chain(chain, interactor).await;

        assert!(connection.is_ok(), "两跳都应该连上");
        let seen = recording.requests.lock().expect("锁");
        let targets: Vec<&str> = seen.iter().map(|request| request.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                format!("jumper@127.0.0.1:{}", jump.port()),
                format!("ops@127.0.0.1:{}", target.port()),
            ],
            "每次提问写的是当时在认证的那一跳"
        );
    }

    #[tokio::test]
    async fn a_prompt_without_questions_is_cancelled_without_asking() {
        let hub = Arc::new(Hub::default());
        hub.register(1, true);
        let interactor = NativeInteractor::with_hub(Arc::clone(&hub));
        let request = PromptRequest {
            kind: PromptKind::KeyboardInteractive,
            target: String::new(),
            name: String::new(),
            instructions: String::new(),
            prompts: Vec::new(),
            can_remember: false,
            reason: None,
        };
        assert!(interactor.prompt(request).await.is_none());
        assert!(hub.poll(1, false, Instant::now()).is_none());
    }
}
