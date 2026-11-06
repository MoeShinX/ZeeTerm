//! AI 助手聊天里「复制」按钮和右键菜单的写入口（V2-U31）。
//!
//! 所有聊天界面的复制都走 [`write`]：正常情况下就是写系统剪贴板；测试 / 调试脚本可以先
//! [`capture_start`]，之后写入的内容进内存缓冲、不碰系统剪贴板（自动化不能动用户的剪贴板）。
//! 调试构建里还能用环境变量 `ZSSH_FAKE_CLIPBOARD=1` 让整个进程一开始就走缓冲。
//!
//! 系统剪贴板的写法按平台分：Windows 走 `zssh-platform-windows` 的 Win32 实现（排除历史 / 云同步、私有格式
//! 标记、30 秒到点清除）；其他平台走 GPUI 的剪贴板（Wayland / X11 都已实现，只能在界面线程调用），
//! 敏感内容靠内容指纹认出"还是我们写的那份"再清（见下面的 `linux` 模块）。

use std::sync::Mutex;

use gpui_kit::*;

/// `Some` = 抓取模式：写入的内容攒在这里，不写系统剪贴板。
static CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// 写一段文字。
pub fn write(text: String, cx: &mut App) {
    if capture_push(&text) {
        return;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

/// 写一段敏感文字（密码）：正常情况走系统的「排除历史」写法，30 秒后自动清除；抓取模式下
/// 和 [`write`] 一样只进内存缓冲，测试里不碰系统剪贴板。
pub fn write_sensitive(text: &str, cx: &mut App) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = &cx;
        write_sensitive_with(text, zssh_platform_windows::clipboard::write_sensitive)
    }
    #[cfg(not(windows))]
    write_sensitive_with(text, |text| unix::write_sensitive(text, cx))
}

/// 写一段普通文字（不做排除、不自动清除），走平台层的写法；抓取模式下同样只进内存缓冲。
/// 「复制连接信息」不带密码时用它，这样测试里复制路径一律碰不到系统剪贴板。
pub fn write_plain(text: &str, cx: &mut App) -> Result<(), String> {
    if capture_push(text) {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let _ = &cx;
        zssh_platform_windows::clipboard::write_plain(text)
    }
    #[cfg(not(windows))]
    {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        Ok(())
    }
}

/// [`write_sensitive`] 的内核：`system` 是真正写系统剪贴板的函数，抓取模式下不会被调用（测试用它证明这一点）。
fn write_sensitive_with(
    text: &str,
    system: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    if capture_push(text) {
        return Ok(());
    }
    system(text)
}

/// 抓取模式下记下来并返回 `true`；不是抓取模式返回 `false`。
fn capture_push(text: &str) -> bool {
    #[cfg(debug_assertions)]
    if crate::dev_hooks::var("ZSSH_FAKE_CLIPBOARD").is_some_and(|v| v == "1") {
        capture_start();
    }
    let Ok(mut guard) = CAPTURE.lock() else {
        return false;
    };
    match guard.as_mut() {
        Some(buffer) => {
            buffer.push(text.to_owned());
            true
        }
        None => false,
    }
}

/// 进入抓取模式（已经在抓就保留已抓到的内容）。
#[cfg(any(test, debug_assertions))]
pub fn capture_start() {
    if let Ok(mut guard) = CAPTURE.lock()
        && guard.is_none()
    {
        *guard = Some(Vec::new());
    }
}

/// 取走抓到的内容（按写入顺序）；没进抓取模式返回空。
#[cfg(any(test, debug_assertions))]
pub fn capture_take() -> Vec<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|mut guard| guard.as_mut().map(std::mem::take))
        .unwrap_or_default()
}

/// 抓到的最后一次写入（不取走）。
#[cfg(debug_assertions)]
pub fn capture_last() -> Option<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|buffer| buffer.last().cloned()))
}

/// 非 Windows 的敏感写入：GPUI 剪贴板 + 到点按内容指纹清除。
///
/// Windows 靠剪贴板里的私有格式认"是不是我们写的"；Linux 没有这种机制，改成记下内容的指纹
/// （长度 + 带随机密钥的哈希，不留明文，见 `zssh_platform_windows::clipboard::Fingerprint`），
/// 30 秒后读一次剪贴板，现在的内容算出来和指纹一致才清。
///
/// 剪贴板历史：Windows 版把内容排除在历史 / 云同步之外，Linux 没有通用的做法——Klipper、CopyQ 等认
/// `x-kde-passwordManagerHint: secret` 这个 MIME 类型，但 GPUI 的 `ClipboardItem` 只有文字 / 图片 / 文件列表三种条目，
/// Wayland 的 `data_source.offer` 和 X11 的 `TARGETS` 都是它内部写死的，挂不上自定义类型（也不许给 gpui 打补丁），
/// 所以没法让管理器跳过。到点清除只清得了当前选区、清不掉管理器已经记下的历史；复制密码后的提示
/// 在 Linux 上因此多一句「剪贴板管理器可能会记下这条内容」（见 `vault_actions` / `info_panel`）。
///
/// Wayland 的限制：只有拥有键盘焦点的窗口才能读写剪贴板，也只有这时它的"当前选区"才是新的。
/// 所以到点时窗口不在前台就继续等，回到前台并稳定半秒（等合成器把新的选区通知过来）之后再读；
/// 读不到就当作"不知道"，不清，每秒再试，最多等 10 分钟就放弃（宁可留着也不误清用户后来复制的内容）。
/// X11 没有这个限制，到点直接读。
#[cfg(not(windows))]
mod unix {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use zssh_platform_windows::clipboard::{CLEAR_AFTER, ClearVerdict, Fingerprint, clear_verdict};

    use super::{App, ClipboardItem};

    /// 到点后每隔多久再看一次。
    const POLL: Duration = Duration::from_secs(1);
    /// 窗口回到前台后要稳定这么久才读（Wayland 的新选区通知在获得焦点之后才到）。
    const SETTLE: Duration = Duration::from_millis(500);
    /// 从写入起超过这么久还没能确认，就放弃。
    const GIVE_UP_AFTER: Duration = Duration::from_secs(10 * 60);

    /// 最近一次敏感写入的编号：新的写入让旧的等待作废（只清最新那份）。
    static LATEST: AtomicU64 = AtomicU64::new(0);

    /// 清除计划能对剪贴板做的事；`App` 是真实实现，测试里换替身。
    pub(super) trait Port {
        /// 读写剪贴板是不是要求本应用窗口在前台（Wayland）。
        fn needs_focus(&self) -> bool;
        /// 本应用现在有没有窗口在前台。
        fn is_active(&self) -> bool;
        /// `None` = 读不到；`Some(None)` = 读到了但不是文字；`Some(Some(text))` = 文字。
        fn read(&mut self) -> Option<Option<String>>;
        /// 把剪贴板内容换成空串。
        fn clear(&mut self);
    }

    struct AppPort<'a>(&'a mut App);

    impl Port for AppPort<'_> {
        fn needs_focus(&self) -> bool {
            self.0.compositor_name() == "Wayland"
        }

        fn is_active(&self) -> bool {
            self.0.active_window().is_some()
        }

        fn read(&mut self) -> Option<Option<String>> {
            self.0.read_from_clipboard().map(|item| item.text())
        }

        fn clear(&mut self) {
            self.0
                .write_to_clipboard(ClipboardItem::new_string(String::new()));
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Outcome {
        /// 剪贴板上还是我们的那份，已清空。
        Cleared,
        /// 剪贴板上已经是别的内容，原样保留。
        Foreign,
        /// 一直没法确认，放弃（没有动剪贴板）。
        GaveUp,
    }

    /// 一次敏感写入的等待状态。
    pub(super) struct Plan {
        ours: Fingerprint,
        written_at: Instant,
        active_since: Option<Instant>,
    }

    impl Plan {
        pub(super) fn new(text: &str, now: Instant) -> Self {
            Self {
                ours: Fingerprint::of(text),
                written_at: now,
                active_since: None,
            }
        }

        /// 走一步：`None` = 还要继续等，`Some` = 结束了。
        pub(super) fn step(&mut self, now: Instant, port: &mut impl Port) -> Option<Outcome> {
            let age = now.saturating_duration_since(self.written_at);
            if age < CLEAR_AFTER {
                return None;
            }
            if age > GIVE_UP_AFTER {
                return Some(Outcome::GaveUp);
            }
            if port.needs_focus() {
                if !port.is_active() {
                    self.active_since = None;
                    return None;
                }
                let since = *self.active_since.get_or_insert(now);
                if now.saturating_duration_since(since) < SETTLE {
                    return None;
                }
            }
            let read = port.read();
            match clear_verdict(read.as_ref().map(|text| text.as_deref()), &self.ours) {
                ClearVerdict::Clear => {
                    port.clear();
                    Some(Outcome::Cleared)
                }
                ClearVerdict::Foreign => Some(Outcome::Foreign),
                ClearVerdict::Unreadable => None,
            }
        }
    }

    pub(super) fn write_sensitive(text: &str, cx: &mut App) -> Result<(), String> {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        let generation = LATEST.fetch_add(1, Ordering::AcqRel) + 1;
        let mut plan = Plan::new(text, Instant::now());
        cx.spawn(async move |cx| {
            cx.background_executor().timer(CLEAR_AFTER).await;
            loop {
                if LATEST.load(Ordering::Acquire) != generation {
                    // 又有一份新的敏感内容写进来了：由它自己的等待负责
                    return;
                }
                match cx.update(|cx| plan.step(Instant::now(), &mut AppPort(cx))) {
                    None => cx.background_executor().timer(POLL).await,
                    Some(Outcome::Cleared | Outcome::Foreign) => return,
                    Some(Outcome::GaveUp) => {
                        tracing::warn!(
                            "sensitive clipboard content could not be verified, left as is"
                        );
                        return;
                    }
                }
            }
        })
        .detach();
        Ok(())
    }

    #[cfg(test)]
    pub(super) mod fake {
        use super::Port;

        /// 替身剪贴板：窗口前台状态、能不能读、里面是什么都由测试摆。
        pub struct Fake {
            pub needs_focus: bool,
            pub active: bool,
            pub content: Option<Option<String>>,
            pub cleared: bool,
            pub reads: usize,
        }

        impl Fake {
            pub fn with(text: &str) -> Self {
                Self {
                    needs_focus: false,
                    active: true,
                    content: Some(Some(text.to_owned())),
                    cleared: false,
                    reads: 0,
                }
            }
        }

        impl Port for Fake {
            fn needs_focus(&self) -> bool {
                self.needs_focus
            }
            fn is_active(&self) -> bool {
                self.active
            }
            fn read(&mut self) -> Option<Option<String>> {
                self.reads += 1;
                self.content.clone()
            }
            fn clear(&mut self) {
                self.cleared = true;
                self.content = Some(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{capture_push, capture_start, capture_take, write_sensitive_with};

    #[cfg(not(windows))]
    mod unix_clear {
        use std::time::{Duration, Instant};

        use super::super::unix::{Outcome, Plan, fake::Fake};

        fn secs(base: Instant, s: u64) -> Instant {
            base + Duration::from_secs(s)
        }

        #[test]
        fn nothing_is_touched_before_the_deadline() {
            let t0 = Instant::now();
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("secret");
            assert_eq!(plan.step(secs(t0, 29), &mut clip), None);
            assert_eq!(clip.reads, 0, "没到点不读剪贴板");
            assert!(!clip.cleared);
        }

        #[test]
        fn our_own_text_is_cleared_at_the_deadline() {
            let t0 = Instant::now();
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("secret");
            assert_eq!(plan.step(secs(t0, 30), &mut clip), Some(Outcome::Cleared));
            assert!(clip.cleared);
        }

        #[test]
        fn something_the_user_copied_later_is_never_cleared() {
            let t0 = Instant::now();
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("what the user copied afterwards");
            assert_eq!(plan.step(secs(t0, 31), &mut clip), Some(Outcome::Foreign));
            assert!(!clip.cleared);
            // 图片等非文字内容同理
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("");
            clip.content = Some(None);
            assert_eq!(plan.step(secs(t0, 31), &mut clip), Some(Outcome::Foreign));
            assert!(!clip.cleared);
        }

        #[test]
        fn an_unreadable_clipboard_is_not_cleared_but_tried_again() {
            let t0 = Instant::now();
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("secret");
            clip.content = None;
            assert_eq!(plan.step(secs(t0, 31), &mut clip), None);
            assert!(!clip.cleared);
            // 后来读得到了，还是我们的：清
            clip.content = Some(Some("secret".to_owned()));
            assert_eq!(plan.step(secs(t0, 32), &mut clip), Some(Outcome::Cleared));
        }

        #[test]
        fn it_gives_up_without_touching_the_clipboard_if_it_never_becomes_readable() {
            let t0 = Instant::now();
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("secret");
            clip.content = None;
            assert_eq!(plan.step(secs(t0, 601), &mut clip), Some(Outcome::GaveUp));
            assert!(!clip.cleared);
        }

        #[test]
        fn on_wayland_it_waits_for_the_window_to_be_in_front_and_settled() {
            let t0 = Instant::now();
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("secret");
            clip.needs_focus = true;
            clip.active = false;
            // 窗口不在前台：不读不写
            assert_eq!(plan.step(secs(t0, 40), &mut clip), None);
            assert_eq!(clip.reads, 0);
            // 刚回到前台：先等一会儿，让新选区通知到
            clip.active = true;
            assert_eq!(plan.step(secs(t0, 60), &mut clip), None);
            assert_eq!(clip.reads, 0);
            // 稳定之后才读，并且清掉
            let later = secs(t0, 60) + Duration::from_millis(600);
            assert_eq!(plan.step(later, &mut clip), Some(Outcome::Cleared));
            // 中途又失去前台，要重新计稳定时间
            let mut plan = Plan::new("secret", t0);
            let mut clip = Fake::with("secret");
            clip.needs_focus = true;
            assert_eq!(plan.step(secs(t0, 40), &mut clip), None);
            clip.active = false;
            assert_eq!(plan.step(secs(t0, 41), &mut clip), None);
            clip.active = true;
            assert_eq!(plan.step(secs(t0, 42), &mut clip), None, "重新开始计时");
            assert_eq!(clip.reads, 0);
        }
    }

    /// 抓取缓冲是进程里唯一的一份，用它的测试不能并行。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn capture_collects_in_order_and_take_drains() {
        let _serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        capture_start();
        let _ = capture_take();
        assert!(capture_push("a"));
        assert!(capture_push("b"));
        assert_eq!(capture_take(), vec!["a".to_owned(), "b".to_owned()]);
        assert!(capture_take().is_empty());
    }

    #[test]
    fn sensitive_writes_stay_in_the_buffer_while_capturing() {
        let _serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        capture_start();
        let _ = capture_take();
        let result =
            write_sensitive_with(
                "demo-pass",
                |_| Err("抓取模式下不该写系统剪贴板".to_owned()),
            );
        assert_eq!(result, Ok(()));
        assert_eq!(capture_take(), vec!["demo-pass".to_owned()]);
    }
}
