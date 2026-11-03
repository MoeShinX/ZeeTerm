//! SSH 连接中途的提问（主机指纹确认、密码 / 口令 / keyboard-interactive）的中转站。
//!
//! 连接跑在 Tokio 线程上，对话框要画在界面线程的窗口里，两边用这里的 [`Hub`] 交接：
//! 连接侧 [`Hub::submit`] 放进一个请求并 `await` 回答；每个窗口里的提问层（`ssh_prompt_ui`）
//! 定时 [`Hub::poll`] 看有没有轮到自己显示的请求，用户点完按钮再 [`Hub::answer`] 送回去。
//!
//! 几条规矩都落在这里（纯逻辑，不依赖窗口，单元测试直接测）：
//! - **一次只显示一个**：请求按到达顺序排队，队首答完才轮到下一个；
//! - **弹在哪个窗口**：请求到达时哪个窗口是活动窗口就弹在哪个（用户正是在那里点的连接），
//!   那个窗口已经没了就弹在主窗口；
//! - **锁屏时不弹**：[`Hub::poll`] 锁着时什么都不给，请求一直排着，解锁后下一次轮询就显示，不超时；
//! - **连接取消 = 请求撤回**：等回答的 future 被丢掉（标签关了、连接被取消）时 [`Pending`] 的 `Drop`
//!   把请求从队里摘掉，正在显示的对话框下一次轮询就收起；
//! - **窗口关闭**：正在这个窗口显示的请求按拒绝 / 取消回答。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use zssh_core::ssh::{HostKeyDecision, HostKeyInfo, HostKeyStatus, PromptAnswers, PromptRequest};

/// 窗口的编号（每个窗口的提问层各取一个，进程内唯一）。
pub type WindowKey = u64;

/// 对话框刚弹出多久之内忽略 Enter：用户可能正好在键盘上打字，一个回车不能就把指纹确认了。
pub const ENTER_GUARD: Duration = Duration::from_millis(300);

/// 一个待回答的提问。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    HostKey(HostKeyInfo),
    Prompt(PromptRequest),
}

/// 用户的回答。里面可能有密码，所以不实现 `Debug`，免得被日志顺手打出来。
pub enum Answer {
    HostKey(HostKeyDecision),
    Prompt(Option<PromptAnswers>),
}

impl Ask {
    /// 没有回答时的兜底：指纹按拒绝、提问按取消。
    fn declined(&self) -> Answer {
        match self {
            Self::HostKey(_) => Answer::HostKey(HostKeyDecision::Reject),
            Self::Prompt(_) => Answer::Prompt(None),
        }
    }
}

/// 轮到某个窗口显示的请求。
#[derive(Clone, Debug)]
pub struct Shown {
    pub id: u64,
    pub ask: Ask,
    /// 这个窗口开始显示它的时刻（Enter 防误触从这里算）。
    pub shown_at: Instant,
}

struct Entry {
    id: u64,
    /// 请求到达时的活动窗口。
    origin: Option<WindowKey>,
    ask: Ask,
    reply: Option<oneshot::Sender<Answer>>,
    shown_in: Option<WindowKey>,
    shown_at: Option<Instant>,
}

struct Registered {
    key: WindowKey,
    main: bool,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    queue: VecDeque<Entry>,
    windows: Vec<Registered>,
    last_active: Option<WindowKey>,
}

impl Inner {
    /// 这个请求该弹在哪个窗口：它的来源窗口还在就是它；否则主窗口；再否则随便一个还开着的窗口。
    fn target_of(&self, entry: &Entry) -> Option<WindowKey> {
        let alive = |key: WindowKey| self.windows.iter().any(|w| w.key == key);
        entry
            .origin
            .filter(|key| alive(*key))
            .or_else(|| self.windows.iter().find(|w| w.main).map(|w| w.key))
            .or_else(|| self.windows.first().map(|w| w.key))
    }
}

/// 中转站。进程里只有一个（[`hub`]）；测试自己 `Hub::default()` 造。
#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
}

/// 连接侧手里的“等回答”凭据。丢掉它就是撤回请求。
pub struct Pending {
    hub: Arc<Hub>,
    id: u64,
    rx: oneshot::Receiver<Answer>,
    /// 调试驱动不能 `await`，先收下回答再让它查看。
    #[cfg(debug_assertions)]
    got: Option<Answer>,
}

impl Pending {
    /// 等用户回答。`None` = 请求被撤销（窗口关了等），调用方按拒绝 / 取消处理。
    pub async fn wait(&mut self) -> Option<Answer> {
        (&mut self.rx).await.ok()
    }

    /// 调试驱动：回答到了就给它看（不阻塞）。
    #[cfg(debug_assertions)]
    pub fn peek(&mut self) -> Option<&Answer> {
        if self.got.is_none() {
            self.got = self.rx.try_recv().ok();
        }
        self.got.as_ref()
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.hub.withdraw(self.id);
    }
}

/// 进程级的那一个中转站。
pub fn hub() -> &'static Arc<Hub> {
    static HUB: OnceLock<Arc<Hub>> = OnceLock::new();
    HUB.get_or_init(|| Arc::new(Hub::default()))
}

impl Hub {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // 持锁的代码里没有会 panic 的东西；万一中毒也接着用，提问不能因此永远弹不出来。
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ---- 连接侧 ----

    /// 放进一个请求（排在队尾）。
    pub fn submit(self: &Arc<Self>, ask: Ask) -> Pending {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        let origin = inner.last_active;
        inner.queue.push_back(Entry {
            id,
            origin,
            ask,
            reply: Some(tx),
            shown_in: None,
            shown_at: None,
        });
        Pending {
            hub: Arc::clone(self),
            id,
            rx,
            #[cfg(debug_assertions)]
            got: None,
        }
    }

    /// 撤回一个请求（不回答）。
    fn withdraw(&self, id: u64) {
        let mut inner = self.lock();
        inner.queue.retain(|entry| entry.id != id);
    }

    // ---- 窗口侧 ----

    /// 窗口登记自己（`main` = 主窗口，找不到来源窗口时的落点）。
    pub fn register(&self, key: WindowKey, main: bool) {
        let mut inner = self.lock();
        inner.windows.retain(|w| w.key != key);
        inner.windows.push(Registered { key, main });
    }

    /// 窗口关了：正在它那里显示的请求按拒绝 / 取消回答；还在排队的转去别的窗口。
    pub fn unregister(&self, key: WindowKey) {
        let mut replies = Vec::new();
        {
            let mut inner = self.lock();
            inner.windows.retain(|w| w.key != key);
            if inner.last_active == Some(key) {
                inner.last_active = None;
            }
            let mut kept = VecDeque::with_capacity(inner.queue.len());
            for mut entry in std::mem::take(&mut inner.queue) {
                if entry.shown_in == Some(key) {
                    if let Some(tx) = entry.reply.take() {
                        replies.push((tx, entry.ask.declined()));
                    }
                } else {
                    kept.push_back(entry);
                }
            }
            inner.queue = kept;
        }
        for (tx, answer) in replies {
            let _ = tx.send(answer);
        }
    }

    /// 记一下这个窗口现在是活动窗口（之后到达的请求弹在它那里）。
    pub fn note_active(&self, key: WindowKey) {
        self.lock().last_active = Some(key);
    }

    /// 窗口问一句：现在有没有该我显示的请求。`locked` 为真（锁屏）时一律没有。
    pub fn poll(&self, key: WindowKey, locked: bool, now: Instant) -> Option<Shown> {
        let mut inner = self.lock();
        let target = inner.queue.front().and_then(|entry| inner.target_of(entry));
        let entry = inner.queue.front_mut()?;
        if locked || target != Some(key) {
            if entry.shown_in == Some(key) {
                entry.shown_in = None;
                entry.shown_at = None;
            }
            return None;
        }
        if entry.shown_in != Some(key) {
            entry.shown_in = Some(key);
            entry.shown_at = Some(now);
        }
        Some(Shown {
            id: entry.id,
            ask: entry.ask.clone(),
            shown_at: entry.shown_at.unwrap_or(now),
        })
    }

    /// 用户的回答。指纹的回答在这里再过一遍 [`sanitize_decision`]；种类对不上按拒绝 / 取消。
    pub fn answer(&self, id: u64, answer: Answer) {
        let taken = {
            let mut inner = self.lock();
            let index = inner.queue.iter().position(|entry| entry.id == id);
            index.and_then(|index| inner.queue.remove(index))
        };
        let Some(mut entry) = taken else {
            return;
        };
        let answer = match (&entry.ask, answer) {
            (Ask::HostKey(info), Answer::HostKey(decision)) => {
                Answer::HostKey(sanitize_decision(info.status, decision))
            }
            (Ask::Prompt(_), Answer::Prompt(answers)) => Answer::Prompt(answers),
            (ask, _) => ask.declined(),
        };
        if let Some(tx) = entry.reply.take() {
            let _ = tx.send(answer);
        }
    }

    /// 排着的请求数（测试用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().queue.len()
    }
}

// ---- 指纹确认对话框的按钮与回答 ----

/// 指纹确认对话框上可能出现的按钮。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyButton {
    TrustAndSave,
    TrustOnce,
    Cancel,
    /// 指纹变了、用户明确选择换成新的。
    UpdateAndConnect,
    /// 吊销的密钥：只能关掉。
    Close,
}

impl HostKeyButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::TrustAndSave => "信任并保存",
            Self::TrustOnce => "仅本次",
            Self::Cancel => "取消",
            Self::UpdateAndConnect => "更新指纹并连接",
            Self::Close => "关闭",
        }
    }
}

/// 这个状态下对话框有哪些按钮，从左到右（最右边是主按钮）。
pub fn host_key_buttons(status: HostKeyStatus) -> &'static [HostKeyButton] {
    match status {
        HostKeyStatus::Unknown | HostKeyStatus::OtherKeyTypesSaved => &[
            HostKeyButton::Cancel,
            HostKeyButton::TrustOnce,
            HostKeyButton::TrustAndSave,
        ],
        // 指纹变了没有「仅本次」：core 规定 `Changed` 只认 `TrustAndSave`。
        HostKeyStatus::Changed => &[HostKeyButton::Cancel, HostKeyButton::UpdateAndConnect],
        HostKeyStatus::Revoked => &[HostKeyButton::Close],
    }
}

/// Enter 触发哪个按钮：必须是安全的那个。指纹变了是取消；未知主机是「信任并保存」（有 300ms 防误触）。
pub fn enter_button(status: HostKeyStatus) -> HostKeyButton {
    match status {
        HostKeyStatus::Unknown | HostKeyStatus::OtherKeyTypesSaved => HostKeyButton::TrustAndSave,
        HostKeyStatus::Changed => HostKeyButton::Cancel,
        HostKeyStatus::Revoked => HostKeyButton::Close,
    }
}

/// 点某个按钮得到的回答。
pub fn button_decision(status: HostKeyStatus, button: HostKeyButton) -> HostKeyDecision {
    let wanted = match button {
        HostKeyButton::TrustAndSave | HostKeyButton::UpdateAndConnect => {
            HostKeyDecision::TrustAndSave
        }
        HostKeyButton::TrustOnce => HostKeyDecision::TrustOnce,
        HostKeyButton::Cancel | HostKeyButton::Close => HostKeyDecision::Reject,
    };
    sanitize_decision(status, wanted)
}

/// 兜底规则：吊销的密钥无论如何拒绝；指纹变了只认「更新指纹」（`TrustAndSave`）。
pub fn sanitize_decision(status: HostKeyStatus, decision: HostKeyDecision) -> HostKeyDecision {
    match (status, decision) {
        (HostKeyStatus::Revoked, _) => HostKeyDecision::Reject,
        (HostKeyStatus::Changed, HostKeyDecision::TrustAndSave) => HostKeyDecision::TrustAndSave,
        (HostKeyStatus::Changed, _) => HostKeyDecision::Reject,
        (_, decision) => decision,
    }
}

/// 刚弹出的 [`ENTER_GUARD`] 之内的 Enter 不算数。
pub fn enter_allowed(shown_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(shown_at) >= ENTER_GUARD
}

// ---- 对话框上的文字 ----

/// 指纹确认对话框的文字（纯数据，界面只管摆）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyText {
    pub title: &'static str,
    pub intro: &'static str,
    /// 提示是不是危险级（指纹变了、已吊销）：界面用警示色。
    pub danger: bool,
    /// 要不要列出主机 / 密钥类型 / 指纹。吊销的密钥只说明一句并给「关闭」，不列这些。
    pub show_details: bool,
    pub host: String,
    pub key_type: String,
    pub fingerprint: String,
    /// 指纹变了时已保存的那条旧指纹。
    pub stored: Option<String>,
}

pub fn host_key_text(info: &HostKeyInfo) -> HostKeyText {
    let (title, intro, danger) = match info.status {
        HostKeyStatus::Unknown => ("确认主机指纹", "第一次连接这台主机，请核对指纹。", false),
        HostKeyStatus::OtherKeyTypesSaved => (
            "确认主机指纹",
            "这台主机以前保存的是别的类型的密钥，请核对指纹。",
            false,
        ),
        HostKeyStatus::Changed => (
            "主机指纹已变化",
            "可能是服务器重装，也可能有人在中间拦截。确认是你自己改过的再更新。",
            true,
        ),
        HostKeyStatus::Revoked => ("确认主机指纹", "这个密钥已被标记为吊销，不能连接。", true),
    };
    HostKeyText {
        title,
        intro,
        danger,
        show_details: info.status != HostKeyStatus::Revoked,
        host: host_port(&info.host, info.port),
        key_type: clean_text(&info.key_type),
        fingerprint: clean_text(&info.fingerprint),
        stored: match info.status {
            HostKeyStatus::Changed => info.stored_fingerprint.as_deref().map(clean_text),
            _ => None,
        },
    }
}

/// `host:port`，IPv6 地址加方括号。
fn host_port(host: &str, port: u16) -> String {
    let host = clean_text(host);
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// 输入对话框的文字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptText {
    pub title: String,
    /// 服务器给的说明（可能多行），没有就是空串。
    pub instructions: String,
    /// 每道题的文字。
    pub questions: Vec<String>,
}

pub fn prompt_text(request: &PromptRequest) -> PromptText {
    use zssh_core::ssh::PromptKind;
    let name = clean_text(&request.name);
    let title = match request.kind {
        PromptKind::Password => "输入密码".to_owned(),
        PromptKind::Passphrase => "输入密钥口令".to_owned(),
        PromptKind::KeyboardInteractive if !name.trim().is_empty() => name,
        PromptKind::KeyboardInteractive => "服务器验证".to_owned(),
    };
    PromptText {
        title,
        instructions: clean_text(&request.instructions).trim().to_owned(),
        questions: request
            .prompts
            .iter()
            .map(|item| clean_text(&item.text))
            .collect(),
    }
}

/// 服务器 / 对端给的文字上界面之前先洗一遍：去掉控制字符（换行留着）和能让文字显示顺序颠倒的
/// 双向控制符，并限制长度，免得对端用它伪造界面内容或撑爆对话框。
pub fn clean_text(raw: &str) -> String {
    const MAX_CHARS: usize = 2000;
    raw.chars()
        .filter(|c| {
            (*c == '\n' || !c.is_control())
                && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .take(MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ssh::{PromptItem, PromptKind};

    const ALL: [HostKeyStatus; 4] = [
        HostKeyStatus::Unknown,
        HostKeyStatus::OtherKeyTypesSaved,
        HostKeyStatus::Changed,
        HostKeyStatus::Revoked,
    ];

    fn info(status: HostKeyStatus) -> HostKeyInfo {
        HostKeyInfo {
            host: "demo.example.com".to_owned(),
            port: 22,
            key_type: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:newnewnewnewnewnewnewnewnewnewnewnewnewnew".to_owned(),
            status,
            stored_fingerprint: matches!(status, HostKeyStatus::Changed | HostKeyStatus::Revoked)
                .then(|| "SHA256:oldoldoldoldoldoldoldoldoldoldoldoldoldold".to_owned()),
        }
    }

    fn password_request() -> PromptRequest {
        PromptRequest::single(PromptKind::Password, "请输入密码", true)
    }

    fn hub_with_main() -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        hub.register(1, true);
        hub
    }

    fn decision_of(answer: Option<Answer>) -> Option<HostKeyDecision> {
        match answer {
            Some(Answer::HostKey(decision)) => Some(decision),
            _ => None,
        }
    }

    // ---- 按钮集合与回答 ----

    #[test]
    fn each_status_gets_its_own_buttons() {
        use HostKeyButton::*;
        assert_eq!(
            host_key_buttons(HostKeyStatus::Unknown),
            [Cancel, TrustOnce, TrustAndSave]
        );
        assert_eq!(
            host_key_buttons(HostKeyStatus::OtherKeyTypesSaved),
            [Cancel, TrustOnce, TrustAndSave]
        );
        assert_eq!(
            host_key_buttons(HostKeyStatus::Changed),
            [Cancel, UpdateAndConnect]
        );
        assert_eq!(host_key_buttons(HostKeyStatus::Revoked), [Close]);
    }

    #[test]
    fn a_changed_key_never_offers_trust_once() {
        assert!(!host_key_buttons(HostKeyStatus::Changed).contains(&HostKeyButton::TrustOnce));
        assert_eq!(
            button_decision(HostKeyStatus::Changed, HostKeyButton::TrustOnce),
            HostKeyDecision::Reject,
            "就算硬塞一个「仅本次」进来也只会被拒绝"
        );
        assert_eq!(
            button_decision(HostKeyStatus::Changed, HostKeyButton::UpdateAndConnect),
            HostKeyDecision::TrustAndSave
        );
    }

    #[test]
    fn buttons_map_to_the_right_decisions() {
        use HostKeyButton::*;
        for status in [HostKeyStatus::Unknown, HostKeyStatus::OtherKeyTypesSaved] {
            assert_eq!(
                button_decision(status, TrustAndSave),
                HostKeyDecision::TrustAndSave
            );
            assert_eq!(
                button_decision(status, TrustOnce),
                HostKeyDecision::TrustOnce
            );
            assert_eq!(button_decision(status, Cancel), HostKeyDecision::Reject);
        }
        assert_eq!(
            button_decision(HostKeyStatus::Changed, Cancel),
            HostKeyDecision::Reject
        );
    }

    #[test]
    fn a_revoked_key_is_rejected_whatever_is_clicked() {
        use HostKeyButton::*;
        for button in [TrustAndSave, TrustOnce, Cancel, UpdateAndConnect, Close] {
            assert_eq!(
                button_decision(HostKeyStatus::Revoked, button),
                HostKeyDecision::Reject
            );
        }
        for decision in [
            HostKeyDecision::TrustAndSave,
            HostKeyDecision::TrustOnce,
            HostKeyDecision::Reject,
        ] {
            assert_eq!(
                sanitize_decision(HostKeyStatus::Revoked, decision),
                HostKeyDecision::Reject
            );
        }
    }

    #[test]
    fn enter_only_ever_triggers_a_safe_button() {
        assert_eq!(enter_button(HostKeyStatus::Changed), HostKeyButton::Cancel);
        assert_eq!(enter_button(HostKeyStatus::Revoked), HostKeyButton::Close);
        for status in ALL {
            assert_eq!(
                button_decision(status, enter_button(status)) == HostKeyDecision::TrustAndSave,
                matches!(
                    status,
                    HostKeyStatus::Unknown | HostKeyStatus::OtherKeyTypesSaved
                ),
                "只有从未见过的主机，Enter 才会信任"
            );
            assert!(
                host_key_buttons(status).contains(&enter_button(status)),
                "Enter 的目标必须真在对话框上"
            );
        }
    }

    #[test]
    fn enter_is_ignored_right_after_the_dialog_appears() {
        let shown = Instant::now();
        assert!(!enter_allowed(shown, shown));
        assert!(!enter_allowed(shown, shown + Duration::from_millis(299)));
        assert!(enter_allowed(shown, shown + ENTER_GUARD));
        assert!(enter_allowed(shown, shown + Duration::from_secs(5)));
        assert!(
            !enter_allowed(shown + Duration::from_secs(1), shown),
            "时钟倒退也不放行"
        );
    }

    // ---- 文字 ----

    #[test]
    fn the_texts_follow_the_status() {
        let unknown = host_key_text(&info(HostKeyStatus::Unknown));
        assert_eq!(unknown.title, "确认主机指纹");
        assert!(unknown.intro.contains("第一次连接"));
        assert!(!unknown.danger);
        assert_eq!(unknown.host, "demo.example.com:22");
        assert_eq!(unknown.stored, None);

        let other = host_key_text(&info(HostKeyStatus::OtherKeyTypesSaved));
        assert!(other.intro.contains("别的类型"));

        let changed = host_key_text(&info(HostKeyStatus::Changed));
        assert_eq!(changed.title, "主机指纹已变化");
        assert!(changed.danger);
        assert!(changed.intro.contains("中间拦截"));
        assert!(changed.stored.as_deref().is_some_and(|s| s.contains("old")));

        let revoked = host_key_text(&info(HostKeyStatus::Revoked));
        assert!(revoked.danger);
        assert!(revoked.intro.contains("吊销"));
        assert_eq!(revoked.stored, None, "吊销时不显示旧指纹");
        assert!(!revoked.show_details, "吊销只显示说明和「关闭」");
        assert!(unknown.show_details && changed.show_details);
    }

    #[test]
    fn ipv6_hosts_get_brackets() {
        let mut v6 = info(HostKeyStatus::Unknown);
        v6.host = "2001:db8::1".to_owned();
        v6.port = 2222;
        assert_eq!(host_key_text(&v6).host, "[2001:db8::1]:2222");
    }

    #[test]
    fn server_text_is_cleaned_before_it_reaches_the_screen() {
        assert_eq!(clean_text("a\u{0007}b\u{1b}[31mc"), "ab[31mc");
        assert_eq!(clean_text("line1\nline2"), "line1\nline2");
        assert_eq!(clean_text("x\u{202E}evil\u{2066}"), "xevil");
        assert_eq!(clean_text(&"长".repeat(5000)).chars().count(), 2000);
    }

    #[test]
    fn keyboard_interactive_shows_the_servers_name_and_questions() {
        let request = PromptRequest {
            kind: PromptKind::KeyboardInteractive,
            name: "Duo 验证".to_owned(),
            instructions: "请输入验证码\n或回复 push".to_owned(),
            prompts: vec![
                PromptItem {
                    text: "密码: ".to_owned(),
                    echo: false,
                },
                PromptItem {
                    text: "验证码: ".to_owned(),
                    echo: true,
                },
            ],
            can_remember: false,
        };
        let text = prompt_text(&request);
        assert_eq!(text.title, "Duo 验证");
        assert_eq!(text.instructions, "请输入验证码\n或回复 push");
        assert_eq!(text.questions, ["密码: ", "验证码: "]);

        let mut unnamed = request;
        unnamed.name.clear();
        assert_eq!(prompt_text(&unnamed).title, "服务器验证");
        assert_eq!(prompt_text(&password_request()).title, "输入密码");
    }

    // ---- 排队、路由、取消 ----

    #[tokio::test]
    async fn the_answer_reaches_the_waiting_connection() {
        let hub = hub_with_main();
        let mut pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let shown = hub.poll(1, false, Instant::now()).expect("轮到主窗口");
        hub.answer(shown.id, Answer::HostKey(HostKeyDecision::TrustAndSave));
        assert_eq!(
            decision_of(pending.wait().await),
            Some(HostKeyDecision::TrustAndSave)
        );
        assert_eq!(hub.len(), 0);
    }

    #[tokio::test]
    async fn a_changed_key_answer_is_checked_again_at_the_hub() {
        let hub = hub_with_main();
        let mut pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Changed)));
        let shown = hub.poll(1, false, Instant::now()).expect("显示");
        hub.answer(shown.id, Answer::HostKey(HostKeyDecision::TrustOnce));
        assert_eq!(
            decision_of(pending.wait().await),
            Some(HostKeyDecision::Reject)
        );
    }

    #[tokio::test]
    async fn a_revoked_key_is_rejected_even_if_the_window_says_trust() {
        let hub = hub_with_main();
        let mut pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Revoked)));
        let shown = hub.poll(1, false, Instant::now()).expect("显示");
        hub.answer(shown.id, Answer::HostKey(HostKeyDecision::TrustAndSave));
        assert_eq!(
            decision_of(pending.wait().await),
            Some(HostKeyDecision::Reject)
        );
    }

    #[tokio::test]
    async fn a_prompt_gets_its_answers_or_none_when_cancelled() {
        let hub = hub_with_main();
        let mut first = hub.submit(Ask::Prompt(password_request()));
        let shown = hub.poll(1, false, Instant::now()).expect("显示");
        let Ask::Prompt(request) = &shown.ask else {
            panic!("应该是提问");
        };
        assert_eq!(request.prompts.len(), 1);
        hub.answer(
            shown.id,
            Answer::Prompt(Some(PromptAnswers {
                answers: vec!["secret".to_owned()],
                remember: false,
            })),
        );
        match first.wait().await {
            Some(Answer::Prompt(Some(answers))) => assert_eq!(answers.answers, ["secret"]),
            _ => panic!("应该拿到回答"),
        }

        let mut second = hub.submit(Ask::Prompt(password_request()));
        let shown = hub.poll(1, false, Instant::now()).expect("显示");
        hub.answer(shown.id, Answer::Prompt(None));
        assert!(matches!(second.wait().await, Some(Answer::Prompt(None))));
    }

    #[tokio::test]
    async fn cancelling_a_dialog_rejects_the_host_key() {
        let hub = hub_with_main();
        let mut pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let shown = hub.poll(1, false, Instant::now()).expect("显示");
        let cancel = button_decision(HostKeyStatus::Unknown, HostKeyButton::Cancel);
        hub.answer(shown.id, Answer::HostKey(cancel));
        assert_eq!(
            decision_of(pending.wait().await),
            Some(HostKeyDecision::Reject)
        );
    }

    #[tokio::test]
    async fn a_mismatched_answer_kind_counts_as_a_refusal() {
        let hub = hub_with_main();
        let mut pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let shown = hub.poll(1, false, Instant::now()).expect("显示");
        hub.answer(shown.id, Answer::Prompt(None));
        assert_eq!(
            decision_of(pending.wait().await),
            Some(HostKeyDecision::Reject)
        );
    }

    #[test]
    fn only_one_request_is_shown_at_a_time_and_the_next_waits_its_turn() {
        let hub = hub_with_main();
        let _a = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let _b = hub.submit(Ask::Prompt(password_request()));
        let now = Instant::now();
        let first = hub.poll(1, false, now).expect("先显示第一个");
        assert!(matches!(first.ask, Ask::HostKey(_)));
        let again = hub.poll(1, false, now).expect("第一个还没答完");
        assert_eq!(again.id, first.id, "答完之前一直是同一个");
        hub.answer(first.id, Answer::HostKey(HostKeyDecision::Reject));
        let second = hub.poll(1, false, now).expect("轮到第二个");
        assert!(matches!(second.ask, Ask::Prompt(_)));
        assert_ne!(second.id, first.id);
    }

    #[test]
    fn the_dialog_stays_hidden_while_locked_and_appears_after_unlock() {
        let hub = hub_with_main();
        let _pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let t0 = Instant::now();
        assert!(hub.poll(1, true, t0).is_none(), "锁屏时不弹");
        assert_eq!(hub.len(), 1, "请求一直等着，没有超时作废");
        assert!(hub.poll(1, true, t0 + Duration::from_secs(3600)).is_none());
        let later = t0 + Duration::from_secs(3601);
        let shown = hub.poll(1, false, later).expect("解锁后显示");
        assert_eq!(shown.shown_at, later, "防误触从真正显示的时刻算");
    }

    #[test]
    fn locking_while_shown_hides_it_and_restarts_the_enter_guard() {
        let hub = hub_with_main();
        let _pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let t0 = Instant::now();
        let first = hub.poll(1, false, t0).expect("显示");
        assert!(hub.poll(1, true, t0 + Duration::from_secs(10)).is_none());
        let t1 = t0 + Duration::from_secs(20);
        let again = hub.poll(1, false, t1).expect("解锁后再显示");
        assert_eq!(again.id, first.id);
        assert_eq!(again.shown_at, t1);
        assert!(!enter_allowed(again.shown_at, t1));
    }

    #[test]
    fn the_request_is_shown_in_the_window_that_was_active_when_it_arrived() {
        let hub = hub_with_main();
        hub.register(2, false);
        hub.note_active(2);
        let _pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        // 请求到达之后活动窗口又换了，请求也不跟着换。
        hub.note_active(1);
        let now = Instant::now();
        assert!(hub.poll(1, false, now).is_none(), "主窗口不该显示");
        assert!(hub.poll(2, false, now).is_some(), "弹在发起的那个窗口");
    }

    #[test]
    fn without_an_origin_the_request_goes_to_the_main_window() {
        let hub = hub_with_main();
        hub.register(2, false);
        let _pending = hub.submit(Ask::Prompt(password_request()));
        let now = Instant::now();
        assert!(hub.poll(2, false, now).is_none());
        assert!(hub.poll(1, false, now).is_some());
    }

    #[test]
    fn if_the_origin_window_is_gone_it_falls_back_to_the_main_window() {
        let hub = hub_with_main();
        hub.register(2, false);
        hub.note_active(2);
        let _pending = hub.submit(Ask::Prompt(password_request()));
        hub.unregister(2);
        assert!(hub.poll(1, false, Instant::now()).is_some());
    }

    #[tokio::test]
    async fn closing_the_window_that_shows_a_dialog_declines_it() {
        let hub = hub_with_main();
        hub.register(2, false);
        hub.note_active(2);
        let mut host_key = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let mut prompt = hub.submit(Ask::Prompt(password_request()));
        assert!(hub.poll(2, false, Instant::now()).is_some());
        hub.unregister(2);
        assert_eq!(
            decision_of(host_key.wait().await),
            Some(HostKeyDecision::Reject),
            "正在显示的指纹确认按拒绝"
        );
        // 排在后面的那个还没显示过，转去主窗口继续等，不被误杀。
        assert_eq!(hub.len(), 1);
        let shown = hub.poll(1, false, Instant::now()).expect("转到主窗口");
        hub.answer(shown.id, Answer::Prompt(None));
        assert!(matches!(prompt.wait().await, Some(Answer::Prompt(None))));
    }

    #[test]
    fn dropping_the_waiting_future_withdraws_the_request() {
        let hub = hub_with_main();
        let pending = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let now = Instant::now();
        assert!(hub.poll(1, false, now).is_some());
        drop(pending);
        assert_eq!(hub.len(), 0);
        assert!(hub.poll(1, false, now).is_none(), "窗口下一次轮询就收起");
    }

    #[test]
    fn a_withdrawn_request_in_the_middle_does_not_disturb_the_others() {
        let hub = hub_with_main();
        let a = hub.submit(Ask::HostKey(info(HostKeyStatus::Unknown)));
        let b = hub.submit(Ask::Prompt(password_request()));
        let _c = hub.submit(Ask::HostKey(info(HostKeyStatus::Changed)));
        drop(b);
        assert_eq!(hub.len(), 2);
        let first = hub.poll(1, false, Instant::now()).expect("显示");
        hub.answer(first.id, Answer::HostKey(HostKeyDecision::Reject));
        let next = hub.poll(1, false, Instant::now()).expect("下一个");
        assert!(matches!(
            next.ask,
            Ask::HostKey(HostKeyInfo {
                status: HostKeyStatus::Changed,
                ..
            })
        ));
        drop(a);
    }

    #[test]
    fn answering_an_unknown_id_is_harmless() {
        let hub = hub_with_main();
        hub.answer(99, Answer::Prompt(None));
        assert_eq!(hub.len(), 0);
    }
}
