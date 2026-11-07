//! 锁屏（V2-U26）在界面这一层的东西：全局的“锁着没有”、输错后的退避、后台校验任务、锁屏页视图。
//! 密码规则、哈希和设置的读写都在 `zssh_core::app_lock`，这里只做转接和画界面。
//!
//! 锁定的做法：**不画**——窗口根视图在锁着的时候只画锁屏页（不透明、盖满整个窗口），终端、主机树、
//! AI 面板这些视图这一帧根本没有进元素树，既看不到也点不到，键盘事件到不了终端。会话、传输、AI 回合
//! 是各自的后台任务，跟画不画无关，照常跑；但 AI 要在服务器上做的事（执行命令、读写文件、看终端）
//! 锁着时一律变成“要确认”，等解锁后由用户决定（见 `agent_host::set_ai_locked`）。所有窗口读同一份
//! 状态，任何一个窗口里解锁，全部解锁。
//!
//! 状态放在线程局部里：界面只在主线程上动它，测试各自一个线程，互不干扰（全局原子量会让并行的
//! 测试互相锁住对方）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::gpui::{
    AnyElement, App, Context, Entity, Image, ImageFormat, IntoElement, Render, SharedString,
    Window, img, px,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::app_lock::{
    Backoff, KdfCost, LockConfig, LockStatus, OsCheck, OsVerifier, PasswordRecord, RevealGate,
};
use zssh_core::store::Database;

use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::main_window::titlebar;
use crate::main_window::widgets::{ButtonLook, text_button};
use crate::typography::{Role, Typo as _, UiFontStyled as _};

/// 锁屏页自己的界面文案。
///
/// 只放应用自己的固定文案；输错次数、产品名这类值由 `format!` 带进来。锁屏密码本身、
/// 后台任务的调试输出都不进这张表。
#[derive(Clone, Copy, Debug)]
pub(crate) enum LockMsg {
    LockedSuffix,
    Subtitle,
    PasswordPlaceholder,
    EmptyPassword,
    WrongPassword,
    WrongPasswordPrefix,
    Unlock,
    Verifying,
    ForgotQuestion,
    ForgotReset,
    ForgotText,
    ForgotTextInline,
    ForgotTextNoOs,
    WaitPrefix,
    WaitSuffix,
    VerifyMessage,
    LockNotEnabled,
    LockAlreadyEnabled,
    SavePasswordFailedPrefix,
    CreatePasswordFailedPrefix,
    DisableFailedPrefix,
    SettingsDbOpenFailedPrefix,
}

impl Msg for LockMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::LockedSuffix => " 已锁定",
            Self::Subtitle => "连接和传输仍在后台运行，输入锁屏密码后继续使用",
            Self::PasswordPlaceholder => "锁屏密码",
            Self::EmptyPassword => "请输入锁屏密码",
            Self::WrongPassword => "密码错误",
            Self::WrongPasswordPrefix => "密码错误。",
            Self::Unlock => "解锁",
            Self::Verifying => "验证中…",
            Self::ForgotQuestion => "忘记密码？",
            Self::ForgotReset => "验证 Windows 账户并重置…",
            Self::ForgotText => {
                "锁屏密码只保存在这台电脑上，无法找回。可以退出 ZeeTerm（会断开所有会话），重新打开后在 设置 → 安全与锁定 里用 Windows 账户验证重置。"
            }
            Self::ForgotTextInline => {
                "锁屏密码只保存在这台电脑上，无法找回。可以用 Windows Hello / PIN（没有就用 Windows 登录密码）验证你是这台电脑的用户，然后设一个新密码，会话不受影响。"
            }
            Self::ForgotTextNoOs => {
                "锁屏密码只保存在这台电脑上，无法找回；这个系统上没有可用来重置的系统身份验证。确实忘记时，退出 ZeeTerm，把数据目录里的 zssh.db（连同 zssh.db-wal、zssh.db-shm）改名或删除后重新打开（本机保存的主机、凭据等会一起清空，有同步或备份的可以再恢复）。"
            }
            Self::WaitPrefix => "输错次数过多，请 ",
            Self::WaitSuffix => " 秒后再试",
            Self::VerifyMessage => "验证你的身份，以重置 ZeeTerm 的锁屏密码",
            Self::LockNotEnabled => "锁屏没有启用",
            Self::LockAlreadyEnabled => "锁屏已经启用",
            Self::SavePasswordFailedPrefix => "没能保存锁屏密码：",
            Self::CreatePasswordFailedPrefix => "没能生成锁屏密码：",
            Self::DisableFailedPrefix => "没能关闭锁屏：",
            Self::SettingsDbOpenFailedPrefix => "打不开设置库：",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::LockedSuffix => " is locked",
            Self::Subtitle => {
                "Connections and transfers keep running in the background. Enter the lock password to continue."
            }
            Self::PasswordPlaceholder => "Lock password",
            Self::EmptyPassword => "Enter the lock password",
            Self::WrongPassword => "Incorrect password",
            Self::WrongPasswordPrefix => "Incorrect password. ",
            Self::Unlock => "Unlock",
            Self::Verifying => "Verifying…",
            Self::ForgotQuestion => "Forgot password?",
            Self::ForgotReset => "Verify Windows account and reset…",
            Self::ForgotText => {
                "The lock password is stored only on this computer and cannot be recovered. You can quit ZeeTerm (this disconnects all sessions), reopen it, and reset the password with Windows account verification under Settings → Security & Lock."
            }
            Self::ForgotTextInline => {
                "The lock password is stored only on this computer and cannot be recovered. You can verify that you are a user of this computer with Windows Hello / PIN (or your Windows sign-in password), then set a new password; sessions are not affected."
            }
            Self::ForgotTextNoOs => {
                "The lock password is stored only on this computer and cannot be recovered, and this system has no OS identity verification that could reset it. If you really forgot it, quit ZeeTerm, rename or delete zssh.db (with zssh.db-wal and zssh.db-shm) in the data folder, and reopen (saved hosts, credentials and other local data are cleared with it; restore them from sync or a backup)."
            }
            Self::WaitPrefix => "Too many failed attempts. Try again in ",
            Self::WaitSuffix => " s",
            Self::VerifyMessage => "Verify your identity to reset the ZeeTerm lock password",
            Self::LockNotEnabled => "App lock is not enabled",
            Self::LockAlreadyEnabled => "App lock is already enabled",
            Self::SavePasswordFailedPrefix => "Could not save the lock password: ",
            Self::CreatePasswordFailedPrefix => "Could not create the lock password: ",
            Self::DisableFailedPrefix => "Could not turn off app lock: ",
            Self::SettingsDbOpenFailedPrefix => "Could not open the settings database: ",
        }
    }
}

/// 锁屏页上“忘记密码”里那个按钮的回调（由主窗口给，其他窗口没有）。
pub type ForgotHook = Rc<dyn Fn(&mut Window, &mut App)>;

thread_local! {
    static LOCKED: Cell<bool> = const { Cell::new(false) };
    /// 每次锁定加一：锁屏页据此知道“又锁了一次”，清空输入框和上次的提示。
    static EPOCH: Cell<u64> = const { Cell::new(0) };
    static BACKOFF: RefCell<Backoff> = RefCell::new(Backoff::new());
    /// 查看 / 复制已保存密码前的验证状态（V2-U35）。
    static REVEAL: RefCell<RevealGate> = RefCell::new(RevealGate::new());
}

/// 现在锁着吗。任何要把用户输入送给会话的地方都可以问一句（见 `Pane::write`）。
pub fn is_locked() -> bool {
    LOCKED.with(Cell::get)
}

pub fn lock_epoch() -> u64 {
    EPOCH.with(Cell::get)
}

/// 改状态（不刷新窗口）。测试和 [`lock`] / [`unlock`] 用。
pub(crate) fn set_locked(locked: bool) {
    let was = LOCKED.with(|cell| cell.replace(locked));
    crate::agent_host::set_ai_locked(locked);
    if locked && !was {
        EPOCH.with(|cell| cell.set(cell.get() + 1));
    }
    // 锁上、解开都让“查看 / 复制密码”的验证作废：之后第一次又要输锁屏密码。
    if locked != was {
        REVEAL.with(|gate| gate.borrow_mut().reset());
    }
}

// ---- 查看 / 复制已保存密码前的验证（V2-U35） ----

/// 现在查看 / 复制已保存的密码要不要先验证（锁屏启用着，且 5 分钟窗口期已过）。
pub fn reveal_needs_verification(lock_enabled: bool, now: Instant) -> bool {
    REVEAL.with(|gate| gate.borrow().needs_verification(lock_enabled, now))
}

/// 验证通过：开始 5 分钟窗口期。
pub fn reveal_mark_verified(now: Instant) {
    REVEAL.with(|gate| gate.borrow_mut().mark_verified(now));
}

/// 窗口期作废（测试和调试驱动用；正式路径靠 [`set_locked`] 的状态变化）。
#[cfg(any(test, debug_assertions))]
pub fn dev_reset_reveal() {
    REVEAL.with(|gate| gate.borrow_mut().reset());
}

/// 锁定：之后每个窗口的下一帧都只画锁屏页。
pub fn lock(cx: &mut App) {
    set_locked(true);
    cx.refresh_windows();
}

/// 解锁：恢复正常画面。
pub fn unlock(cx: &mut App) {
    set_locked(false);
    cx.refresh_windows();
}

// ---- 输错后的退避（所有校验入口共用一份） ----

/// 还要等多久才能再试；`None` = 现在可以。
pub fn remaining_wait(now: Instant) -> Option<Duration> {
    BACKOFF.with(|b| b.borrow().remaining(now))
}

/// 记一次校验的结果；返回接下来要等多久（输对了恒为零）。
pub fn record_attempt(correct: bool, now: Instant) -> Duration {
    BACKOFF.with(|b| {
        let mut b = b.borrow_mut();
        if correct {
            b.record_success();
            Duration::ZERO
        } else {
            b.record_failure(now)
        }
    })
}

#[cfg(test)]
pub fn wrong_count() -> u32 {
    BACKOFF.with(|b| b.borrow().failures())
}

/// 调试驱动和测试把输错计数清零（脚本不用真等 60 秒）。
#[cfg(any(test, debug_assertions))]
pub fn dev_reset_backoff() {
    BACKOFF.with(|b| *b.borrow_mut() = Backoff::new());
}

/// “请 N 秒后再试”的文字（向上取整，别出现“0 秒”）。
pub fn wait_text(left: Duration) -> String {
    let secs = left.as_millis().div_ceil(1000).max(1);
    format!("{}{secs}{}", t(LockMsg::WaitPrefix), t(LockMsg::WaitSuffix))
}

/// 这条提示是不是「输错次数过多…」的倒计时（两种语言的写法都认）。
fn is_wait_notice(text: &str) -> bool {
    text.contains(LockMsg::WaitSuffix.zh()) || text.contains(LockMsg::WaitPrefix.en())
}

// ---- 后台任务：哈希要上百毫秒，不能在界面线程上做 ----

/// 一次要在后台做的事。
#[derive(Clone, PartialEq, Eq)]
pub enum Job {
    /// 锁屏页上的解锁。
    Unlock { password: String },
    /// 启用锁屏（设置第一个密码）。
    Enable { password: String },
    /// 修改密码：原密码要对。
    Change { old: String, new: String },
    /// 关闭锁屏：原密码要对。
    Disable { old: String },
    /// 忘记密码，Windows 验证通过之后设新密码。
    Reset { new: String },
    /// 查看 / 复制已保存密码前的验证（V2-U35）：只核对，不改任何东西。
    Verify { password: String },
}

impl std::fmt::Debug for Job {
    // 日志里可能会打到：只留种类，不留密码。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unlock { .. } => "Job::Unlock",
            Self::Enable { .. } => "Job::Enable",
            Self::Change { .. } => "Job::Change",
            Self::Disable { .. } => "Job::Disable",
            Self::Reset { .. } => "Job::Reset",
            Self::Verify { .. } => "Job::Verify",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Done,
    /// （原）密码不对：要计入输错次数。
    WrongPassword,
    /// 其他原因没做成（库写失败、状态不对），文字直接给界面。
    Failed(String),
}

fn check_old(config: &LockConfig, old: &str) -> Result<(), JobOutcome> {
    match &config.status {
        LockStatus::Enabled(record) if record.verify(old) => Ok(()),
        // 记录读不懂时没法校验原密码：当作不对，用户走“忘记密码”重置。
        LockStatus::Enabled(_) | LockStatus::Unreadable => Err(JobOutcome::WrongPassword),
        LockStatus::Disabled => Err(JobOutcome::Failed(t(LockMsg::LockNotEnabled).to_owned())),
    }
}

/// 在后台线程里执行（会读写库、算 Argon2）。`cost` 正式用 `KdfCost::STANDARD`，测试用小参数。
pub fn run_job(db: &Database, job: Job, cost: KdfCost) -> JobOutcome {
    let config = LockConfig::load(db);
    let save = |password: &str| match PasswordRecord::create_with(password, cost) {
        Ok(record) => match zssh_core::app_lock::save_password(db, &record) {
            Ok(()) => JobOutcome::Done,
            Err(err) => JobOutcome::Failed(format!(
                "{}{}",
                t(LockMsg::SavePasswordFailedPrefix),
                crate::error_text::error_text(&err)
            )),
        },
        Err(err) => JobOutcome::Failed(format!(
            "{}{}",
            t(LockMsg::CreatePasswordFailedPrefix),
            crate::error_text::error_text(&err)
        )),
    };
    match job {
        Job::Unlock { password } | Job::Verify { password } => match &config.status {
            // 别处（比如设置被改了）已经关掉了锁：放行，不让人卡在锁屏页上。
            LockStatus::Disabled => JobOutcome::Done,
            _ => {
                check_old(&config, &password).map_or_else(|outcome| outcome, |()| JobOutcome::Done)
            }
        },
        Job::Enable { password } => {
            if config.status.is_enabled() {
                return JobOutcome::Failed(t(LockMsg::LockAlreadyEnabled).to_owned());
            }
            save(&password)
        }
        Job::Change { old, new } => match check_old(&config, &old) {
            Ok(()) => save(&new),
            Err(outcome) => outcome,
        },
        Job::Disable { old } => match check_old(&config, &old) {
            Ok(()) => match zssh_core::app_lock::clear_password(db) {
                Ok(()) => JobOutcome::Done,
                Err(err) => JobOutcome::Failed(format!(
                    "{}{}",
                    t(LockMsg::DisableFailedPrefix),
                    crate::error_text::error_text(&err)
                )),
            },
            Err(outcome) => outcome,
        },
        Job::Reset { new } => {
            if !config.status.is_enabled() {
                return JobOutcome::Failed(t(LockMsg::LockNotEnabled).to_owned());
            }
            save(&new)
        }
    }
}

fn db_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("zssh.db")
}

/// 把 [`Job`] 放到后台线程跑，跑完回到 `T` 的上下文里调 `done`。
pub fn spawn_job<T: 'static>(
    window: &mut Window,
    cx: &mut Context<T>,
    job: Job,
    done: impl FnOnce(&mut T, JobOutcome, &mut Window, &mut Context<T>) + 'static,
) {
    let path = db_path();
    cx.spawn_in(window, async move |this, cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move {
                match Database::open(&path) {
                    Ok(db) => run_job(&db, job, KdfCost::STANDARD),
                    Err(err) => JobOutcome::Failed(format!(
                        "{}{err}",
                        t(LockMsg::SettingsDbOpenFailedPrefix)
                    )),
                }
            })
            .await;
        let _ = this.update_in(cx, |this, window, cx| done(this, outcome, window, cx));
    })
    .detach();
}

// ---- 忘记密码：Windows 验证 ----

/// 调试构建里 `ZSSH_LOCK_OS_STUB` 可以代替真实的 Windows 验证（自动化不能真的弹 Windows Hello）：
/// `pass` / `fail` / `cancel` = Hello 直接给这个结果；`cred_pass` / `cred_fail` = 没有 Hello、凭据框给这个结果；
/// `unavailable` = 两条路都走不了。release 里这条路径不存在。
fn stub_from_env() -> Option<StubVerifier> {
    let (hello, credentials) = match crate::dev_hooks::var("ZSSH_LOCK_OS_STUB")?.as_str() {
        "pass" => (OsCheck::Verified, OsCheck::Unavailable),
        "fail" => (OsCheck::Failed, OsCheck::Unavailable),
        "cancel" => (OsCheck::Cancelled, OsCheck::Unavailable),
        "cred_pass" => (OsCheck::Unavailable, OsCheck::Verified),
        "cred_fail" => (OsCheck::Unavailable, OsCheck::Failed),
        "unavailable" => (OsCheck::Unavailable, OsCheck::Unavailable),
        _ => return None,
    };
    Some(StubVerifier { hello, credentials })
}

/// 固定结果的验证器（调试钩子和测试用）。
#[derive(Clone, Copy)]
pub struct StubVerifier {
    pub hello: OsCheck,
    pub credentials: OsCheck,
}

impl OsVerifier for StubVerifier {
    fn windows_hello(&self, _message: &str) -> OsCheck {
        self.hello
    }

    fn windows_credentials(&self, _message: &str) -> OsCheck {
        self.credentials
    }
}

/// 这个平台有没有"系统身份验证"可用来重置锁屏密码。Windows 有（Hello / 凭据框）；Linux、macOS 没有等价物，
/// 界面上就不能出现"验证 Windows 账户并重置"这类入口，否则是个点了只会报错的按钮。
/// 调试构建里的 `ZSSH_LOCK_OS_STUB` 算有（自动化要驱动这条流程）。
pub fn os_verification_available() -> bool {
    cfg!(windows) || stub_from_env().is_some()
}

/// 阻塞地做一次“证明你是这台电脑的 Windows 用户”（在后台线程里调）。
pub fn verify_windows_user(window: Option<isize>) -> OsCheck {
    let message = t(LockMsg::VerifyMessage);
    if let Some(stub) = stub_from_env() {
        return zssh_core::app_lock::verify_os_user(&stub, message);
    }
    #[cfg(windows)]
    {
        zssh_core::app_lock::verify_os_user(
            &zssh_core::app_lock::WindowsVerifier::new(window),
            message,
        )
    }
    // Unix 还没有系统身份验证后端：明确返回不可用，由调用方走密码校验，
    // 不能假装验证成功。
    #[cfg(not(windows))]
    {
        let _ = (window, message);
        OsCheck::Unavailable
    }
}

/// 这个窗口的系统句柄（Windows Hello 对话框挂靠用；其他平台没有）。
pub fn window_hwnd(window: &Window) -> Option<isize> {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // `Window` 自己也有一个返回 gpui 窗口句柄的 `window_handle()`：这里要的是系统的那个，写明白。
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

// ---- 锁屏页 ----

const LOGO_PNG: &[u8] = include_bytes!("../assets/brand/zeeterm-logo-64.png");

/// 抖一下的位移序列（逻辑像素），每 35 毫秒走一格。
const SHAKE: [f32; 8] = [-12.0, 12.0, -9.0, 9.0, -5.0, 5.0, -2.0, 0.0];

/// 锁屏页：一个窗口一份。所有窗口的锁屏页读的是同一份全局状态。
pub struct LockScreen {
    input: Entity<InputState>,
    logo: Arc<Image>,
    /// 自己画标题栏（拖动 + 最小化 / 最大化 / 关闭）；系统画标题栏的窗口不用。
    custom_titlebar: bool,
    /// 有就在“忘记密码？”展开的说明下面多一个“验证 Windows 账户并重置…”按钮。
    forgot_hook: Option<ForgotHook>,
    error: Option<String>,
    checking: bool,
    show_forgot: bool,
    shake_step: Option<usize>,
    /// 上一次看到的锁定编号：变了说明又锁了一次。
    epoch: u64,
    focus_pending: bool,
    ticking: bool,
}

impl LockScreen {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        custom_titlebar: bool,
        forgot_hook: Option<ForgotHook>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(LockMsg::PasswordPlaceholder))
        });
        cx.observe(&input, |_this, _input, cx| cx.notify()).detach();
        cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit(window, cx);
                }
            },
        )
        .detach();
        Self {
            input,
            logo: Arc::new(Image::from_bytes(ImageFormat::Png, LOGO_PNG.to_vec())),
            custom_titlebar,
            forgot_hook,
            error: None,
            checking: false,
            show_forgot: false,
            shake_step: None,
            epoch: 0,
            focus_pending: true,
            ticking: false,
        }
    }

    /// 又锁了一次：清掉上次留下的输入和提示，把键盘给密码框。
    fn sync_epoch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = lock_epoch();
        if now != self.epoch {
            self.epoch = now;
            self.error = None;
            self.checking = false;
            self.show_forgot = false;
            self.shake_step = None;
            self.focus_pending = true;
            self.input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.checking || !is_locked() {
            return;
        }
        if let Some(left) = remaining_wait(Instant::now()) {
            self.error = Some(wait_text(left));
            cx.notify();
            return;
        }
        let password = self.input.read(cx).value().to_string();
        if password.is_empty() {
            self.error = Some(t(LockMsg::EmptyPassword).to_owned());
            cx.notify();
            return;
        }
        self.checking = true;
        self.error = None;
        cx.notify();
        spawn_job(
            window,
            cx,
            Job::Unlock { password },
            |this, outcome, window, cx| this.finished(outcome, window, cx),
        );
    }

    fn finished(&mut self, outcome: JobOutcome, window: &mut Window, cx: &mut Context<Self>) {
        self.checking = false;
        match outcome {
            JobOutcome::Done => {
                record_attempt(true, Instant::now());
                self.input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.error = None;
                unlock(cx);
            }
            JobOutcome::WrongPassword => {
                let wait = record_attempt(false, Instant::now());
                self.error = Some(if wait.is_zero() {
                    t(LockMsg::WrongPassword).to_owned()
                } else {
                    format!("{}{}", t(LockMsg::WrongPasswordPrefix), wait_text(wait))
                });
                self.input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.start_shake(window, cx);
            }
            JobOutcome::Failed(reason) => self.error = Some(reason),
        }
        cx.notify();
    }

    /// 密码错：输入框左右抖几下。
    fn start_shake(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shake_step = Some(0);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(35))
                    .await;
                let more = this
                    .update(cx, |this, cx| {
                        let next = this.shake_step.map_or(SHAKE.len(), |step| step + 1);
                        this.shake_step = (next < SHAKE.len()).then_some(next);
                        cx.notify();
                        this.shake_step.is_some()
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- 调试驱动（只在 debug 构建里编进去，见 `main_window::dev_acts_u26`） ----

    #[cfg(debug_assertions)]
    pub fn dev_type(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
    }

    #[cfg(debug_assertions)]
    pub fn dev_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    #[cfg(debug_assertions)]
    pub fn dev_toggle_forgot(&mut self, cx: &mut Context<Self>) {
        self.show_forgot = !self.show_forgot;
        cx.notify();
    }

    /// 锁屏页现在的提示文字；`None` = 没有提示，或校验还在后台跑。
    #[cfg(debug_assertions)]
    pub fn dev_error(&self) -> Option<String> {
        self.error.clone()
    }

    /// 在锁屏页上显示一条提示（Windows 账户验证没通过之类，主窗口在没有别的地方可显示时用）。
    pub fn show_notice(&mut self, text: &str, cx: &mut Context<Self>) {
        self.error = Some(text.to_owned());
        cx.notify();
    }

    /// 别的东西（后台的面板、停靠视图）抢走了键盘焦点就拿回来：锁着的时候只有密码框该有焦点。
    pub fn keep_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.input.read(cx).focus_handle(cx);
        if !handle.is_focused(window) {
            window.focus(&handle, cx);
        }
    }

    /// 等待倒计时期间每秒刷新一次文字。
    fn start_countdown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let more = this
                    .update(cx, |this, cx| {
                        let left = remaining_wait(Instant::now());
                        if left.is_none() {
                            // 等完了：把“请 N 秒后再试”撤掉。
                            if this.error.as_deref().is_some_and(is_wait_notice) {
                                this.error = None;
                            }
                            this.ticking = false;
                        } else if this.error.as_deref().is_some_and(is_wait_notice) {
                            // 保留“密码错误。”前缀的写法也统一成纯倒计时。
                            this.error = left.map(wait_text);
                        }
                        cx.notify();
                        this.ticking
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        })
        .detach();
    }

    fn title_bar(&self, tokens: &Tokens, window: &Window) -> AnyElement {
        h_flex()
            .id("lock-title-bar")
            .h(px(titlebar::BAR_HEIGHT))
            .w_full()
            .flex_none()
            .child(
                div()
                    .id("lock-title-drag")
                    .flex_1()
                    .h_full()
                    .map(crate::window_frame::drag_region),
            )
            .child(titlebar::window_controls(tokens, window))
            .into_any_element()
    }
}

impl Render for LockScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_epoch(window, cx);
        if self.focus_pending {
            self.focus_pending = false;
            let handle = self.input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        let tokens = Tokens::current(cx);
        let waiting = remaining_wait(Instant::now());
        if waiting.is_some() {
            self.start_countdown(window, cx);
        }
        let shake = self
            .shake_step
            .and_then(|step| SHAKE.get(step).copied())
            .unwrap_or(0.0);
        let entity = cx.entity();
        let forgot = entity.clone();
        let unlock_button = entity.clone();

        let card = v_flex()
            .w(px(340.0))
            .max_w_full()
            .gap_4()
            .items_center()
            .child(img(self.logo.clone()).size(px(64.0)))
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(div().typo(Role::PageTitle, &tokens).child(format!(
                        "{}{}",
                        crate::product::NAME,
                        t(LockMsg::LockedSuffix)
                    )))
                    .child(
                        div()
                            .typo(Role::Caption, &tokens)
                            .child(t(LockMsg::Subtitle)),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .relative()
                    .left(px(shake))
                    .child(Input::new(&self.input)),
            )
            .child(
                div()
                    .w_full()
                    .min_h(px(18.0))
                    .text_center()
                    .text_xs()
                    .text_color(tokens.danger)
                    .children(
                        self.error
                            .clone()
                            .or_else(|| waiting.map(wait_text))
                            .map(SharedString::from),
                    ),
            )
            .child(
                div().w_full().child(
                    text_button(
                        "lock-unlock",
                        if self.checking {
                            t(LockMsg::Verifying)
                        } else {
                            t(LockMsg::Unlock)
                        },
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            unlock_button.update(cx, |this, cx| this.submit(window, cx));
                        },
                    )
                    .w_full(),
                ),
            )
            .child(text_button(
                "lock-forgot",
                t(LockMsg::ForgotQuestion),
                ButtonLook::Ghost,
                move |_, _window, cx| {
                    forgot.update(cx, |this, cx| {
                        this.show_forgot = !this.show_forgot;
                        cx.notify();
                    });
                },
            ))
            .when(self.show_forgot, |d| {
                let hook = self.forgot_hook.clone();
                let has_os = os_verification_available();
                d.child(
                    v_flex()
                        .id("lock-forgot-text")
                        .w_full()
                        .p_3()
                        .gap_2()
                        .rounded_md()
                        .border_1()
                        .border_color(tokens.border)
                        .bg(tokens.subtle)
                        .child(div().typo(Role::Caption, &tokens).child(if !has_os {
                            t(LockMsg::ForgotTextNoOs)
                        } else if hook.is_some() {
                            t(LockMsg::ForgotTextInline)
                        } else {
                            t(LockMsg::ForgotText)
                        }))
                        .children(hook.filter(|_| has_os).map(|hook| {
                            text_button(
                                "lock-forgot-reset",
                                t(LockMsg::ForgotReset),
                                ButtonLook::Outline,
                                move |_, window, cx| hook(window, cx),
                            )
                            .w_full()
                        })),
                )
            });

        v_flex()
            .id("lock-screen")
            .size_full()
            // 不透明的底：下面是什么都不能透出来；`occlude` 让鼠标事件到不了别的东西。
            .bg(tokens.background)
            .text_color(tokens.text)
            .occlude()
            .ui_font_fallbacks(cx)
            .when(self.custom_titlebar, |d| {
                d.child(self.title_bar(&tokens, window))
            })
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_4()
                    .child(card),
            )
    }
}

/// 窗口根视图用：锁着就返回整窗的锁屏页，没锁返回 `None`。`slot` 是这个窗口自己的那一份锁屏页，
/// 第一次需要时才建。
pub fn cover<T: 'static>(
    slot: &mut Option<Entity<LockScreen>>,
    custom_titlebar: bool,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Option<AnyElement> {
    cover_with(slot, custom_titlebar, None, window, cx)
}

/// 同 [`cover`]，锁屏页第一次建出来时可以带上“忘记密码”按钮的回调（只有主窗口给）。
pub fn cover_with<T: 'static>(
    slot: &mut Option<Entity<LockScreen>>,
    custom_titlebar: bool,
    forgot_hook: Option<ForgotHook>,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Option<AnyElement> {
    if !is_locked() {
        return None;
    }
    let screen = match slot {
        Some(screen) => screen.clone(),
        None => {
            let screen = cx.new(|cx| LockScreen::new(window, cx, custom_titlebar, forgot_hook));
            *slot = Some(screen.clone());
            screen
        }
    };
    Some(screen.into_any_element())
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        Job, JobOutcome, LockMsg, StubVerifier, dev_reset_backoff, dev_reset_reveal, is_locked,
        lock_epoch, record_attempt, remaining_wait, reveal_mark_verified,
        reveal_needs_verification, run_job, set_locked, wait_text, wrong_count,
    };
    use std::time::{Duration, Instant};
    use zssh_core::app_lock::{
        HASH_KEY, KdfCost, LOCK_ON_TRAY_KEY, LockConfig, LockStatus, OsCheck,
    };
    use zssh_core::store::Database;

    const CHEAP: KdfCost = KdfCost {
        memory_kib: 8 * 1024,
        iterations: 1,
        parallelism: 1,
    };

    /// 测试用的临时库：目录随守卫一起删。
    struct TempDb {
        dir: std::path::PathBuf,
        db: Database,
    }

    impl std::ops::Deref for TempDb {
        type Target = Database;
        fn deref(&self) -> &Database {
            &self.db
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn open_db() -> TempDb {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zssh-lock-ui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        TempDb { dir, db }
    }

    fn pw(text: &str) -> String {
        text.to_owned()
    }

    fn enabled_db(password: &str) -> TempDb {
        let db = open_db();
        assert_eq!(
            run_job(
                &db,
                Job::Enable {
                    password: pw(password)
                },
                CHEAP
            ),
            JobOutcome::Done
        );
        db
    }

    // ---- 状态 ----

    #[test]
    fn locking_and_unlocking_flip_the_shared_flag() {
        set_locked(false);
        assert!(!is_locked());
        let before = lock_epoch();
        set_locked(true);
        assert!(is_locked());
        assert_eq!(lock_epoch(), before + 1, "每次锁定编号加一");
        set_locked(true);
        assert_eq!(lock_epoch(), before + 1, "已经锁着再锁不算新的一次");
        set_locked(false);
        assert!(!is_locked());
        set_locked(true);
        assert_eq!(lock_epoch(), before + 2);
        set_locked(false);
    }

    #[test]
    fn the_flag_is_per_thread_so_tests_cannot_lock_each_other() {
        set_locked(true);
        let other = std::thread::spawn(is_locked).join().unwrap();
        assert!(!other, "别的线程（别的测试）看不到这里的锁");
        set_locked(false);
    }

    // ---- 退避 ----

    #[test]
    fn the_shared_backoff_starts_waiting_after_five_wrong_tries() {
        dev_reset_backoff();
        let now = Instant::now();
        for _ in 0..4 {
            assert_eq!(record_attempt(false, now), Duration::ZERO);
        }
        assert_eq!(remaining_wait(now), None);
        assert_eq!(record_attempt(false, now), Duration::from_secs(5));
        assert_eq!(remaining_wait(now), Some(Duration::from_secs(5)));
        assert_eq!(wrong_count(), 5);
        assert_eq!(record_attempt(true, now), Duration::ZERO);
        assert_eq!(remaining_wait(now), None);
        assert_eq!(wrong_count(), 0);
        dev_reset_backoff();
    }

    #[test]
    fn the_wait_text_rounds_up_and_never_says_zero() {
        assert_eq!(
            wait_text(Duration::from_secs(5)),
            "输错次数过多，请 5 秒后再试"
        );
        assert_eq!(
            wait_text(Duration::from_millis(4100)),
            "输错次数过多，请 5 秒后再试"
        );
        assert_eq!(
            wait_text(Duration::from_millis(10)),
            "输错次数过多，请 1 秒后再试"
        );
    }

    // ---- 密码的启用 / 修改 / 关闭 / 重置 / 解锁 ----

    #[test]
    fn enabling_stores_a_hash_and_unlocking_needs_the_same_password() {
        let db = enabled_db("open-sesame");
        assert!(LockConfig::load(&db).status.is_enabled());
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("open-sesame")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("open-sesamE")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword
        );
        // 库里只有哈希：明文密码不在其中。
        let stored = db.setting_get(HASH_KEY).unwrap().unwrap();
        assert!(!stored.contains("open-sesame"));
    }

    #[test]
    fn enabling_twice_is_refused_instead_of_overwriting_the_password() {
        let db = enabled_db("first-pass");
        let outcome = run_job(
            &db,
            Job::Enable {
                password: pw("second-pass"),
            },
            CHEAP,
        );
        assert!(matches!(outcome, JobOutcome::Failed(_)), "{outcome:?}");
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("first-pass")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
    }

    #[test]
    fn changing_the_password_needs_the_original_one() {
        let db = enabled_db("old-pass");
        let wrong = run_job(
            &db,
            Job::Change {
                old: pw("not-it"),
                new: pw("new-pass"),
            },
            CHEAP,
        );
        assert_eq!(wrong, JobOutcome::WrongPassword);
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("old-pass")
                },
                CHEAP
            ),
            JobOutcome::Done,
            "原密码输错时什么都没改"
        );

        let ok = run_job(
            &db,
            Job::Change {
                old: pw("old-pass"),
                new: pw("new-pass"),
            },
            CHEAP,
        );
        assert_eq!(ok, JobOutcome::Done);
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("old-pass")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword
        );
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("new-pass")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
    }

    #[test]
    fn disabling_needs_the_current_password_and_clears_the_record() {
        let db = enabled_db("keep-it");
        assert_eq!(
            run_job(&db, Job::Disable { old: pw("guess") }, CHEAP),
            JobOutcome::WrongPassword
        );
        assert!(LockConfig::load(&db).status.is_enabled(), "输错时锁还在");
        assert_eq!(
            run_job(&db, Job::Disable { old: pw("keep-it") }, CHEAP),
            JobOutcome::Done
        );
        assert_eq!(LockConfig::load(&db).status, LockStatus::Disabled);
        // 没启用时再关、再改都是“没启用”，不是“密码错”。
        for job in [
            Job::Disable { old: pw("x") },
            Job::Change {
                old: pw("x"),
                new: pw("yyyy"),
            },
            Job::Reset { new: pw("yyyy") },
        ] {
            assert!(matches!(run_job(&db, job, CHEAP), JobOutcome::Failed(_)));
        }
    }

    #[test]
    fn a_reset_replaces_the_password_without_knowing_the_old_one() {
        let db = enabled_db("forgotten");
        assert_eq!(
            run_job(
                &db,
                Job::Reset {
                    new: pw("brand-new")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("forgotten")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword
        );
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("brand-new")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
    }

    #[test]
    fn an_unreadable_record_cannot_be_unlocked_but_can_be_reset() {
        let db = open_db();
        db.setting_set(HASH_KEY, "{\"v\":1,\"broken\":1}").unwrap();
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("anything")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword,
            "读不懂的记录不能当成没锁"
        );
        assert_eq!(
            run_job(
                &db,
                Job::Disable {
                    old: pw("anything")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword
        );
        assert_eq!(
            run_job(
                &db,
                Job::Reset {
                    new: pw("fresh-pass")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("fresh-pass")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
    }

    #[test]
    fn unlocking_while_the_lock_was_switched_off_elsewhere_just_lets_you_in() {
        let db = open_db();
        assert_eq!(
            run_job(
                &db,
                Job::Unlock {
                    password: pw("whatever")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
    }

    #[test]
    fn a_job_never_prints_the_password() {
        let job = Job::Change {
            old: pw("hunter22"),
            new: pw("hunter33"),
        };
        let text = format!("{job:?}");
        assert!(!text.contains("hunter"), "{text}");
    }

    #[test]
    fn the_lock_options_live_in_their_own_local_keys() {
        let db = open_db();
        zssh_core::app_lock::save_lock_on_tray(&db, true).unwrap();
        assert_eq!(
            db.setting_get(LOCK_ON_TRAY_KEY).unwrap().as_deref(),
            Some("true")
        );
    }

    // ---- 忘记密码的三种结果（桩） ----

    #[test]
    fn the_forgot_password_stub_covers_pass_fail_and_unavailable() {
        let verify = |hello, credentials| {
            zssh_core::app_lock::verify_os_user(&StubVerifier { hello, credentials }, "msg")
        };
        assert_eq!(
            verify(OsCheck::Verified, OsCheck::Failed),
            OsCheck::Verified
        );
        assert_eq!(verify(OsCheck::Failed, OsCheck::Verified), OsCheck::Failed);
        assert_eq!(
            verify(OsCheck::Unavailable, OsCheck::Verified),
            OsCheck::Verified
        );
        assert_eq!(
            verify(OsCheck::Unavailable, OsCheck::Unavailable),
            OsCheck::Unavailable
        );
    }

    // ---- 查看 / 复制已保存密码前的验证（V2-U35） ----

    #[test]
    fn verifying_checks_the_lock_password_and_changes_nothing() {
        let db = enabled_db("lock-pass");
        let before = db.setting_get(HASH_KEY).unwrap();
        assert_eq!(
            run_job(
                &db,
                Job::Verify {
                    password: pw("lock-pass")
                },
                CHEAP
            ),
            JobOutcome::Done
        );
        assert_eq!(
            run_job(
                &db,
                Job::Verify {
                    password: pw("lock-pasS")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword
        );
        assert_eq!(db.setting_get(HASH_KEY).unwrap(), before, "只核对，不写库");
        // 读不懂的记录不能当成“验证通过”。
        db.setting_set(HASH_KEY, "{\"v\":1,\"broken\":1}").unwrap();
        assert_eq!(
            run_job(
                &db,
                Job::Verify {
                    password: pw("anything")
                },
                CHEAP
            ),
            JobOutcome::WrongPassword
        );
    }

    #[test]
    fn the_reveal_window_asks_once_and_is_not_asked_without_a_lock() {
        dev_reset_reveal();
        let now = Instant::now();
        assert!(!reveal_needs_verification(false, now), "没启用锁屏不问");
        assert!(reveal_needs_verification(true, now), "启用了第一次要问");
        reveal_mark_verified(now);
        assert!(!reveal_needs_verification(true, now));
        assert!(!reveal_needs_verification(
            true,
            now + Duration::from_secs(299)
        ));
        assert!(reveal_needs_verification(
            true,
            now + Duration::from_secs(300)
        ));
        dev_reset_reveal();
    }

    #[test]
    fn locking_and_unlocking_both_restart_the_reveal_window() {
        set_locked(false);
        dev_reset_reveal();
        let now = Instant::now();
        reveal_mark_verified(now);
        assert!(!reveal_needs_verification(true, now));

        set_locked(true);
        assert!(reveal_needs_verification(true, now), "锁屏后要重新验证");

        reveal_mark_verified(now);
        set_locked(true);
        assert!(
            !reveal_needs_verification(true, now),
            "已经锁着再锁不算一次变化"
        );

        set_locked(false);
        assert!(reveal_needs_verification(true, now), "解锁后也要重新验证");
        dev_reset_reveal();
    }

    #[test]
    fn wrong_reveal_passwords_use_the_same_counter_as_the_lock_screen() {
        // 对话框和锁屏页走同一对函数（`record_attempt` / `remaining_wait`）：在这里输错的次数，
        // 锁屏页上的下一次尝试也要等。
        dev_reset_backoff();
        let now = Instant::now();
        for _ in 0..5 {
            record_attempt(false, now);
        }
        assert_eq!(wrong_count(), 5);
        assert_eq!(remaining_wait(now), Some(Duration::from_secs(5)));
        // 退避期间再提交，界面一律先问 `remaining_wait`，不会去后台校验——密码对也不行。
        assert!(remaining_wait(now + Duration::from_secs(1)).is_some());
        assert_eq!(remaining_wait(now + Duration::from_secs(5)), None);
        // 输对一次，两边的计数一起清零。
        record_attempt(true, now + Duration::from_secs(5));
        assert_eq!(wrong_count(), 0);
        dev_reset_backoff();
    }

    #[test]
    fn the_inline_forgot_text_points_at_windows_verification_not_at_restarting() {
        assert!(crate::i18n::t(LockMsg::ForgotTextInline).contains("Windows"));
        assert!(crate::i18n::t(LockMsg::ForgotTextInline).contains("无法找回"));
        assert!(
            !crate::i18n::t(LockMsg::ForgotTextInline).contains("重新打开"),
            "开了启动时锁定，重新打开仍是锁屏页，不能再让人去重启"
        );
    }

    #[test]
    fn the_forgot_text_says_what_the_card_asks_for() {
        for part in [
            "只保存在这台电脑上",
            "无法找回",
            "退出 ZeeTerm",
            "断开所有会话",
            "Windows 账户",
        ] {
            assert!(crate::i18n::t(LockMsg::ForgotText).contains(part), "{part}");
        }
    }

    /// Linux / macOS 没有 Windows Hello / 凭据框：不能有"验证 Windows 账户并重置"这类入口，
    /// 真的去验证也只会得到"不可用"，不会冒充通过（macOS 的 Touch ID 还没做）。
    #[cfg(unix)]
    #[test]
    fn unix_has_no_os_verification() {
        if super::stub_from_env().is_some() {
            return; // 自动化设了调试桩：此时按设计"有"
        }
        assert!(!super::os_verification_available());
        assert_eq!(super::verify_windows_user(None), OsCheck::Unavailable);
    }
}
