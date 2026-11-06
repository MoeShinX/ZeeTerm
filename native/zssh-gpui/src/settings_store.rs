//! 应用级的界面设置：全进程只有一份，所有窗口（主窗口、独立终端窗口……）共用。
//!
//! 以前每个窗口的工作区各读一份 `UiSettings`、各自整份写回同一个文件，也各跑一个"跟随系统主题"的
//! 定时器：窗口 A 手动切了深色，窗口 B 手里的旧副本还认为自己在"跟随系统"，下一次对齐就把主题切回去
//! 并把旧设置整份存盘；字体、高亮也有同样的"后写覆盖先写"。
//!
//! 现在：
//! - 只有这一个设置实体（[`AppSettings`]），修改按字段进行（[`SettingsStore::update`]），每次真正改变
//!   就让修订号加一，所有窗口订阅它，窗口手里只留一份读缓存，不再自己存盘；
//! - 主题（明暗、皮肤、终端配色）的应用和"跟随系统"的轮询都归它，只有一个轮询；
//! - 落盘归 [`SettingsWriter`]：改动立刻在内存里生效，后台线程短时合并（连续调整只写最后一版）后再写，
//!   写失败可见并自动重试，退出前把待写的写完。真正属于窗口自己的状态（窗口大小、选区……）不在这里。

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity, Global, WindowAppearance};

use crate::highlight::HighlightRule;
use crate::settings::UiSettings;

/// 最后一次修改之后最多等这么久就写盘：连续拖滑块、连按字号快捷键只会写最后一版。
pub const SAVE_DELAY: Duration = Duration::from_millis(300);
/// 写失败（磁盘满、文件被占用）之后多久再试一次。
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// 退出前等待落盘的最长时间；磁盘卡死也不能把退出拖住。
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// 跟随系统主题的轮询间隔。
const FOLLOW_SYSTEM_INTERVAL: Duration = Duration::from_secs(2);

// ---- 落盘者 ----

type WriteFn = dyn Fn(&UiSettings) -> std::io::Result<()> + Send + Sync;

struct WriterState {
    /// 等着写的最新一版（后来的覆盖先来的）。
    pending: Option<UiSettings>,
    /// 最早什么时候该写；连续修改不会把它往后推，所以持续调整也每隔 [`SAVE_DELAY`] 落一次盘。
    due: Option<Instant>,
    /// 有人在等落盘完成：不再等到期，立刻写。
    flush: bool,
    writing: bool,
    closed: bool,
    /// 最近一次写失败的原因；成功后清掉。
    error: Option<String>,
    /// 每出现一次新的失败加一：界面据此只提示一次。
    error_seq: u64,
    /// 写过几次（成功的）。
    writes: u64,
    /// 写过几次（含失败的）。
    attempts: u64,
}

struct WriterShared {
    state: Mutex<WriterState>,
    wake: Condvar,
    write: Box<WriteFn>,
    delay: Duration,
    retry: Duration,
}

impl WriterShared {
    fn lock(&self) -> MutexGuard<'_, WriterState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 设置文件的唯一写入者：后台线程，合并短时间内的多次修改，保证最后一版一定落盘。
pub struct SettingsWriter {
    shared: Arc<WriterShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl SettingsWriter {
    /// 写到 `path`（先写临时文件再改名）。
    pub fn to_file(path: PathBuf) -> Arc<Self> {
        Self::with(
            Box::new(move |settings| settings.save_to(&path)),
            SAVE_DELAY,
            RETRY_DELAY,
        )
    }

    /// 可注入写入函数和时间参数（测试用）。
    pub fn with(write: Box<WriteFn>, delay: Duration, retry: Duration) -> Arc<Self> {
        let shared = Arc::new(WriterShared {
            state: Mutex::new(WriterState {
                pending: None,
                due: None,
                flush: false,
                writing: false,
                closed: false,
                error: None,
                error_seq: 0,
                writes: 0,
                attempts: 0,
            }),
            wake: Condvar::new(),
            write,
            delay,
            retry,
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("zssh-settings-writer".to_owned())
            .spawn(move || Self::run(&worker))
            .ok();
        Arc::new(Self {
            shared,
            thread: Mutex::new(thread),
        })
    }

    fn run(shared: &WriterShared) {
        let mut state = shared.lock();
        loop {
            let Some(due) = state.due.filter(|_| state.pending.is_some()) else {
                if state.closed {
                    return;
                }
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            };
            let now = Instant::now();
            if !(state.flush || state.closed) && now < due {
                state = shared
                    .wake
                    .wait_timeout(state, due - now)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
                continue;
            }
            let Some(snapshot) = state.pending.take() else {
                continue;
            };
            state.due = None;
            state.writing = true;
            drop(state);
            let result = (shared.write)(&snapshot);
            state = shared.lock();
            state.writing = false;
            state.attempts += 1;
            match result {
                Ok(()) => {
                    state.error = None;
                    state.writes += 1;
                }
                Err(err) => {
                    tracing::warn!(%err, "保存界面设置失败，稍后重试");
                    state.error = Some(err.to_string());
                    state.error_seq += 1;
                    // 期间没有更新的版本就把这一版放回去，过一会儿再试；有更新的就写更新的。
                    if state.pending.is_none() {
                        state.pending = Some(snapshot);
                        state.due = Some(Instant::now() + shared.retry);
                    }
                    // 写不进去时不要立刻原地重试（等落盘的人已经知道失败了，会自己返回）：
                    // 退出时放弃，平时按重试间隔再来。
                    state.flush = false;
                    if state.closed {
                        shared.wake.notify_all();
                        return;
                    }
                }
            }
            shared.wake.notify_all();
        }
    }

    /// 交一版要写的设置（覆盖还没写的旧版）；不阻塞。
    pub fn submit(&self, settings: UiSettings) {
        let mut state = self.shared.lock();
        let soon = Instant::now() + self.shared.delay;
        state.pending = Some(settings);
        state.due = Some(state.due.map_or(soon, |due| due.min(soon)));
        self.shared.wake.notify_all();
    }

    /// 等所有已交的版本落盘（退出前、测试里读盘验证前）。写失败时不会一直等：
    /// 尝试过一次仍失败就返回，剩下的由后台继续重试；也有总的时间上限。
    pub fn flush(&self) {
        let deadline = Instant::now() + FLUSH_TIMEOUT;
        let mut state = self.shared.lock();
        let attempts_before = state.attempts;
        state.flush = true;
        self.shared.wake.notify_all();
        loop {
            if state.pending.is_none() && !state.writing {
                break;
            }
            if state.attempts > attempts_before && state.error.is_some() && !state.writing {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self
                .shared
                .wake
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        state.flush = false;
    }

    /// 最近一次写盘失败的原因（成功后为 `None`），以及失败的累计次数。
    pub fn last_error(&self) -> (Option<String>, u64) {
        let state = self.shared.lock();
        (state.error.clone(), state.error_seq)
    }

    /// 成功写盘的次数（测试）。
    #[cfg(test)]
    pub fn writes(&self) -> u64 {
        self.shared.lock().writes
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.closed = true;
            self.shared.wake.notify_all();
        }
        let handle = self.thread.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

// ---- 设置仓库（纯逻辑，不依赖界面） ----

/// 内存里的唯一一份设置 + 修订号 + 落盘。窗口读它、改它，不各自保存副本去回写。
pub struct SettingsStore {
    settings: UiSettings,
    revision: u64,
    writer: Arc<SettingsWriter>,
}

impl SettingsStore {
    pub fn new(settings: UiSettings, writer: Arc<SettingsWriter>) -> Self {
        Self {
            settings,
            revision: 1,
            writer,
        }
    }

    pub fn settings(&self) -> &UiSettings {
        &self.settings
    }

    /// 每次真正改变加一；窗口拿它判断自己的读缓存旧没旧。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn writer(&self) -> &Arc<SettingsWriter> {
        &self.writer
    }

    /// 字段级修改：在**当前**那一份上改，不是拿某个窗口的旧副本整份覆盖。真的变了才加修订号、
    /// 交给落盘者；返回是否变了。
    pub fn update(&mut self, edit: impl FnOnce(&mut UiSettings)) -> bool {
        let mut next = self.settings.clone();
        edit(&mut next);
        let next = next.sanitized();
        if next == self.settings {
            return false;
        }
        self.settings = next;
        self.revision += 1;
        self.writer.submit(self.settings.clone());
        true
    }

    /// "跟随系统"的一次对齐：只有开着跟随、且系统明暗和现在的全局明暗不同才改，返回新的明暗。
    /// 手动选过明暗（`follow_system` 关）时什么都不做——任何窗口都没有自己的旧副本能把它改回去。
    pub fn follow_system_tick(
        &mut self,
        system_light: Option<bool>,
        current_dark: bool,
    ) -> Option<bool> {
        if !self.settings.follow_system {
            return None;
        }
        let dark = !system_light?;
        if dark == current_dark {
            return None;
        }
        self.update(|settings| settings.dark = dark);
        Some(dark)
    }
}

// ---- 窗口跟着做的事 ----

/// 设置变了以后，一个窗口里要跟着做的事。纯判断（不碰界面），窗口按它去做：自己改的和别的窗口改的
/// 走同一条路，所以每个窗口对同一次修改的反应是一样的。
#[derive(Debug, Default, PartialEq)]
pub struct WindowEffects {
    /// 字体栈变了：重新解析实际字体，设置页的输入框也要同步。
    pub font_family: bool,
    /// 回滚行数变了：已开的窗格按新上限裁剪 / 放宽。
    pub scrollback: Option<usize>,
    /// 应用级的高亮规则和这个窗口手里的不一样：换成这一份（别的窗口改的）。
    /// 自己改的规则已经先写进了窗口，两边一致时是 `None`，不会重复处理。
    pub adopt_rules: Option<Vec<HighlightRule>>,
    /// "自动切英文输入"被关掉：把输入法还回去。
    pub release_ime: bool,
    /// 光标闪烁开关变了：相位回到「亮」。
    pub reset_blink: bool,
}

pub fn window_effects(
    before: &UiSettings,
    after: &UiSettings,
    local_rules: &[HighlightRule],
) -> WindowEffects {
    let incoming = after
        .highlight_rules
        .clone()
        .unwrap_or_else(crate::highlight::default_rules);
    WindowEffects {
        font_family: before.font_family != after.font_family,
        scrollback: (before.scrollback != after.scrollback).then_some(after.scrollback),
        adopt_rules: (incoming != local_rules).then_some(incoming),
        release_ime: before.ime_english && !after.ime_english,
        reset_blink: before.cursor_blink != after.cursor_blink,
    }
}

// ---- 界面实体 ----

/// 全进程唯一的设置实体：窗口用 `cx.observe` 订阅，变了就收到通知。
pub struct AppSettings {
    store: SettingsStore,
}

struct SettingsHandle(Entity<AppSettings>);

impl Global for SettingsHandle {}

/// 写盘者的进程级引用：退出路径和调试钩子没有 `cx` 也要能把待写的写完。
static WRITER: OnceLock<Arc<SettingsWriter>> = OnceLock::new();

impl AppSettings {
    /// 启动时调用一次（开任何窗口之前）：读设置、定下主题、起唯一的跟随系统轮询、
    /// 登记退出前落盘。重复调用返回同一个实体。
    pub fn init(cx: &mut App) -> Entity<AppSettings> {
        if let Some(handle) = cx.try_global::<SettingsHandle>() {
            return handle.0.clone();
        }
        let settings = UiSettings::load();
        let writer = SettingsWriter::to_file(crate::settings::settings_path());
        let _ = WRITER.set(Arc::clone(&writer));
        // 主题要在开任何窗口之前定下来：`Theme::change` 会把颜色重置成组件库默认值，
        // 之后统一由 `app_theme` 写入自己的色板。
        let dark =
            crate::settings::resolve_dark(settings.follow_system, settings.dark, system_light(cx));
        // 皮肤和终端配色的选择先记下，`init_mode` 写主题时才取得到。
        cx.set_global(settings.look());
        // 界面语言要在开窗口之前定下来。没选过（全新安装、老设置文件里没有这个字段）时
        // `UiSettings::default` 给的就是"跟随系统"，只有在设置里明确选过中文 / 英文才固定。
        let pref = settings.language_pref();
        crate::i18n::set(
            pref.resolve(zssh_platform_windows::locale::system_is_chinese()),
            cx,
        );
        crate::app_theme::init_mode(dark, cx);

        let entity = cx.new(|_| AppSettings {
            store: SettingsStore::new(settings, writer),
        });
        cx.set_global(SettingsHandle(entity.clone()));

        // 全进程唯一的"跟随系统主题"轮询（以前每个窗口各有一个）。
        let weak = entity.downgrade();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(FOLLOW_SYSTEM_INTERVAL).await;
                let alive = weak.update(cx, |this, cx| this.sync_system_theme(cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        // 退出前把待写的写完。订阅一被丢弃就会注销，所以 forget 让它活到进程结束。
        std::mem::forget(cx.on_app_quit(|_| async {
            flush_to_disk();
        }));
        entity
    }

    /// 取设置实体；还没初始化（测试、单独起的演示窗口）就现场初始化。
    pub fn global(cx: &mut App) -> Entity<AppSettings> {
        Self::init(cx)
    }

    /// 测试用：装一份默认设置的实体，**不读不写磁盘，也不改进程级的界面语言**。`init` 会按系统语言
    /// 重设 `i18n` 的全局值，并行跑的别的测试（默认按中文断言）会被它在非中文系统上带偏；开 GPUI
    /// 测试窗口的测试用这个。
    #[cfg(test)]
    pub(crate) fn init_for_test(cx: &mut App) -> Entity<AppSettings> {
        if let Some(handle) = cx.try_global::<SettingsHandle>() {
            return handle.0.clone();
        }
        let settings = UiSettings::default();
        let writer = SettingsWriter::with(Box::new(|_| Ok(())), SAVE_DELAY, RETRY_DELAY);
        cx.set_global(settings.look());
        crate::app_theme::init_mode(true, cx);
        let entity = cx.new(|_| AppSettings {
            store: SettingsStore::new(settings, writer),
        });
        cx.set_global(SettingsHandle(entity.clone()));
        entity
    }

    pub fn settings(&self) -> &UiSettings {
        self.store.settings()
    }

    pub fn revision(&self) -> u64 {
        self.store.revision()
    }

    /// 字段级修改。真的变了：立刻应用主题相关的变化（明暗、皮肤、终端配色，全局只做这一次），
    /// 通知所有订阅的窗口，并交给落盘者。返回是否变了。
    pub fn update_with(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut UiSettings),
    ) -> bool {
        let before = self.store.settings().clone();
        if !self.store.update(edit) {
            return false;
        }
        let after = self.store.settings();
        if before.look() != after.look() {
            crate::app_theme::set_look(after.look(), cx);
        }
        if after.dark != crate::app_theme::is_dark(cx) {
            crate::app_theme::set_mode(after.dark, cx);
        }
        cx.notify();
        true
    }

    /// 对齐一次"跟随系统主题"。定时轮询在调用，Unix 上主窗口还订阅了系统外观变化，变了立刻来一次。
    pub(crate) fn sync_system_theme(&mut self, cx: &mut Context<Self>) {
        let current = crate::app_theme::is_dark(cx);
        let system_light = system_light(cx);
        if let Some(dark) = self.store.follow_system_tick(system_light, current) {
            crate::app_theme::set_mode(dark, cx);
            cx.notify();
        }
    }

    /// 最近一次写盘失败的原因和累计失败次数。
    pub fn save_error(&self) -> (Option<String>, u64) {
        self.store.writer().last_error()
    }
}

/// GPUI 的窗口外观折成门户的明暗偏好（macOS 的 Vibrant 变体按同色算）。
fn portal_scheme(appearance: WindowAppearance) -> zssh_platform_windows::theme::PortalScheme {
    use zssh_platform_windows::theme::PortalScheme;
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => PortalScheme::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => PortalScheme::Light,
    }
}

/// 系统现在是不是浅色（`Some(true)`）；读不到是 `None`。
///
/// Windows 读注册表（和以前一样，GPUI 的外观不参与）。Linux 用 GPUI 的窗口外观：它在后台读
/// xdg-desktop-portal 的 color-scheme 并在系统切换时更新（Wayland / X11 都是），`GTK_THEME` 只作后备
/// （判定规则见 `zssh_platform_windows::theme::resolve_unix`）。
pub(crate) fn system_light(cx: &App) -> Option<bool> {
    zssh_platform_windows::theme::apps_use_light_theme_with(Some(portal_scheme(
        cx.window_appearance(),
    )))
}

/// 当前这份设置的副本；设置实体还没建（单独的演示窗口、早期代码）就读盘。
pub fn snapshot(cx: &App) -> UiSettings {
    match cx.try_global::<SettingsHandle>() {
        Some(handle) => handle.0.read(cx).settings().clone(),
        None => UiSettings::load(),
    }
}

/// 把待写的设置写完（退出前、读盘验证的调试钩子）。没有待写的就立刻返回。
pub fn flush_to_disk() {
    if let Some(writer) = WRITER.get() {
        writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn gpui_appearances_map_onto_the_portal_scheme() {
        use zssh_platform_windows::theme::PortalScheme;
        assert_eq!(portal_scheme(WindowAppearance::Dark), PortalScheme::Dark);
        assert_eq!(
            portal_scheme(WindowAppearance::VibrantDark),
            PortalScheme::Dark
        );
        assert_eq!(portal_scheme(WindowAppearance::Light), PortalScheme::Light);
        assert_eq!(
            portal_scheme(WindowAppearance::VibrantLight),
            PortalScheme::Light
        );
    }

    /// 跟随系统：系统切到深色，设置里"跟随"开着的话立刻变深色；手动选过明暗的不被系统带走。
    #[test]
    fn following_the_system_tracks_a_changing_portal_value() {
        use zssh_platform_windows::theme::{PortalScheme, resolve_unix};
        let mut store = SettingsStore::new(
            UiSettings {
                follow_system: true,
                dark: false,
                ..UiSettings::default()
            },
            SettingsWriter::with(
                Box::new(|_| Ok(())),
                Duration::from_millis(5),
                Duration::from_millis(5),
            ),
        );
        let system = |scheme| resolve_unix(Some(scheme), None);
        assert_eq!(
            store.follow_system_tick(system(PortalScheme::Dark), false),
            Some(true)
        );
        assert!(store.settings().dark);
        // 没变化：不动
        assert_eq!(
            store.follow_system_tick(system(PortalScheme::Dark), true),
            None
        );
        // 系统又切回浅色
        assert_eq!(
            store.follow_system_tick(system(PortalScheme::Light), true),
            Some(false)
        );
        assert!(!store.settings().dark);
        // 手动选了明暗（关掉跟随）：系统变化不再影响
        store.update(|s| s.follow_system = false);
        assert_eq!(
            store.follow_system_tick(system(PortalScheme::Dark), false),
            None
        );
    }

    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ui-settings.json")
    }

    fn sized(font_size: f32) -> UiSettings {
        UiSettings {
            font_size,
            ..UiSettings::default()
        }
    }

    /// 记下每次写出的内容的假落盘函数。
    fn recording_writer(delay: Duration) -> (Arc<SettingsWriter>, Arc<Mutex<Vec<UiSettings>>>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&written);
        let writer = SettingsWriter::with(
            Box::new(move |settings| {
                sink.lock().unwrap().push(settings.clone());
                Ok(())
            }),
            delay,
            Duration::from_millis(30),
        );
        (writer, written)
    }

    #[test]
    fn rapid_changes_are_coalesced_and_the_last_version_is_what_gets_written() {
        let (writer, written) = recording_writer(Duration::from_millis(80));
        for step in 0..50 {
            writer.submit(sized(10.0 + step as f32 * 0.1));
        }
        writer.flush();
        let written = written.lock().unwrap();
        assert!(
            !written.is_empty() && written.len() <= 3,
            "50 次连续修改只该写出个位数次，实际 {}",
            written.len()
        );
        let last = written.last().map(|s| s.font_size).unwrap_or_default();
        assert!((last - 14.9).abs() < 1e-4, "{last}");
    }

    #[test]
    fn a_never_ending_stream_of_changes_still_reaches_disk_periodically() {
        let (writer, written) = recording_writer(Duration::from_millis(60));
        let started = Instant::now();
        let mut size = 12.0;
        while started.elapsed() < Duration::from_millis(400) {
            size += 0.01;
            writer.submit(sized(size));
            std::thread::sleep(Duration::from_millis(5));
        }
        // 修改还在继续，但不能一直憋着不写：到期时间不会被后面的修改往后推。
        let before_flush = written.lock().unwrap().len();
        assert!(
            before_flush >= 3,
            "持续调整时应隔一小段就落一次盘：{before_flush}"
        );
        writer.flush();
        let last = written.lock().unwrap().last().unwrap().font_size;
        assert!((last - size).abs() < 1e-4);
    }

    #[test]
    fn dropping_the_writer_writes_what_is_pending() {
        let path = temp_path("drop");
        let writer = SettingsWriter::to_file(path.clone());
        writer.submit(sized(18.0));
        drop(writer);
        let saved = UiSettings::load_from(&path).expect("退出时待写的设置要写完");
        assert_eq!(saved.font_size, 18.0);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_failed_write_is_visible_and_retried_until_it_succeeds() {
        let failures_left = Arc::new(AtomicUsize::new(2));
        let written = Arc::new(Mutex::new(Vec::<UiSettings>::new()));
        let (left, sink) = (Arc::clone(&failures_left), Arc::clone(&written));
        let writer = SettingsWriter::with(
            Box::new(move |settings| {
                if left.load(Ordering::SeqCst) > 0 {
                    left.fetch_sub(1, Ordering::SeqCst);
                    return Err(std::io::Error::other("disk full"));
                }
                sink.lock().unwrap().push(settings.clone());
                Ok(())
            }),
            Duration::from_millis(10),
            Duration::from_millis(20),
        );
        writer.submit(sized(21.0));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_error = false;
        while writer.writes() == 0 {
            let (error, seq) = writer.last_error();
            if error.is_some_and(|text| text.contains("disk full")) && seq >= 1 {
                saw_error = true;
            }
            assert!(Instant::now() < deadline, "重试一直没成功");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(saw_error, "失败期间要能看到原因");
        assert_eq!(writer.last_error().0, None, "成功之后错误清掉");
        assert!(writer.last_error().1 >= 2, "失败次数累计");
        let last = written.lock().unwrap().last().map(|s| s.font_size);
        assert_eq!(last, Some(21.0));
    }

    #[test]
    fn flush_does_not_hang_when_the_disk_keeps_failing_and_a_newer_version_still_wins() {
        let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let written = Arc::new(Mutex::new(Vec::<UiSettings>::new()));
        let (switch, sink) = (Arc::clone(&failing), Arc::clone(&written));
        let writer = SettingsWriter::with(
            Box::new(move |settings| {
                if switch.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("locked"));
                }
                sink.lock().unwrap().push(settings.clone());
                Ok(())
            }),
            Duration::from_millis(10),
            Duration::from_millis(500),
        );
        writer.submit(sized(11.0));
        let started = Instant::now();
        writer.flush();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "写不进去时 flush 不能一直等"
        );
        assert!(writer.last_error().0.is_some());

        // 磁盘恢复，又来了更新的一版：写出去的是更新的，不是那份失败的旧版。
        failing.store(false, Ordering::SeqCst);
        writer.submit(sized(13.0));
        writer.flush();
        let written = written.lock().unwrap();
        assert_eq!(written.last().map(|s| s.font_size), Some(13.0));
        assert!(written.iter().all(|s| s.font_size != 11.0));
    }

    #[test]
    fn update_is_field_level_and_only_a_real_change_moves_the_revision() {
        let (writer, written) = recording_writer(Duration::from_millis(10));
        let mut store = SettingsStore::new(UiSettings::default(), writer.clone());
        let start = store.revision();

        let same = store.settings().font_size;
        assert!(!store.update(|s| s.font_size = same), "没变不算修改");
        assert_eq!(store.revision(), start);

        assert!(store.update(|s| s.font_size = 16.0));
        assert!(store.update(|s| s.cursor_blink = false));
        assert_eq!(store.revision(), start + 2);
        // 第二次修改是在第一次的结果上改的：字号没有被打回去。
        assert_eq!(store.settings().font_size, 16.0);
        assert!(!store.settings().cursor_blink);

        // 越界的值被收回范围，仍是一次修改。
        assert!(store.update(|s| s.font_size = 999.0));
        assert_eq!(store.settings().font_size, crate::settings::MAX_FONT_SIZE);
        writer.flush();
        assert_eq!(
            written.lock().unwrap().last().map(|s| s.font_size),
            Some(crate::settings::MAX_FONT_SIZE)
        );
    }

    #[test]
    fn follow_system_only_acts_in_auto_mode_and_only_on_a_real_difference() {
        let (writer, _written) = recording_writer(Duration::from_millis(10));
        let mut store = SettingsStore::new(UiSettings::default(), writer);
        // 自动模式：系统浅色而界面是深色 -> 切到浅色；已经一致 -> 不动；读不到系统值 -> 不动。
        assert_eq!(store.follow_system_tick(Some(true), true), Some(false));
        assert!(!store.settings().dark);
        assert_eq!(store.follow_system_tick(Some(true), false), None);
        assert_eq!(store.follow_system_tick(None, false), None);
        // 手动选过深色：再怎么对齐系统都不动。
        store.update(|s| {
            s.follow_system = false;
            s.dark = true;
        });
        assert_eq!(store.follow_system_tick(Some(true), true), None);
        assert_eq!(store.follow_system_tick(Some(false), false), None);
        assert!(store.settings().dark && !store.settings().follow_system);
    }

    /// 另一个窗口改了高亮规则 / 回滚行数 / 字体：这个窗口要照做；而自己改的不会被重复处理。
    #[test]
    fn a_window_follows_other_windows_changes_but_does_not_redo_its_own() {
        let before = UiSettings::default();
        let own_rules = crate::highlight::default_rules();

        // 什么都没变：没有要做的事。
        assert_eq!(
            window_effects(&before, &before, &own_rules),
            WindowEffects::default()
        );

        // 别的窗口加了一条规则：本窗口手里还是旧规则 -> 换成新的那份。
        let mut other = own_rules.clone();
        other.push(HighlightRule::new(40, "oom", false));
        let after = UiSettings {
            highlight_rules: Some(other.clone()),
            scrollback: 20_000,
            font_family: "Consolas".to_owned(),
            ime_english: false,
            cursor_blink: false,
            ..UiSettings::default()
        };
        let effects = window_effects(&before, &after, &own_rules);
        assert_eq!(effects.adopt_rules, Some(other.clone()));
        assert_eq!(effects.scrollback, Some(20_000));
        assert!(effects.font_family && effects.release_ime && effects.reset_blink);

        // 自己改的规则：窗口先改了自己的，再写进设置，两边一致 -> 不重复处理。
        assert_eq!(window_effects(&before, &after, &other).adopt_rules, None);

        // 设置里规则是"默认那一组"（None）而窗口手里也是默认 -> 一致。
        assert_eq!(
            window_effects(&before, &before, &own_rules).adopt_rules,
            None
        );

        // 关掉再打开自动英文输入：只有"关"才要还输入法。
        let off = UiSettings {
            ime_english: false,
            ..UiSettings::default()
        };
        assert!(window_effects(&before, &off, &own_rules).release_ime);
        assert!(!window_effects(&off, &before, &own_rules).release_ime);
    }

    /// 窗口：读缓存 + 通过共享仓库改设置（和工作区的写法一致：改的时候在当前那份上按字段改，
    /// 别人改了就重新读一份缓存）。
    struct Window {
        store: Rc<RefCell<SettingsStore>>,
        cache: UiSettings,
        seen: u64,
    }

    impl Window {
        fn open(store: &Rc<RefCell<SettingsStore>>) -> Self {
            let (cache, seen) = {
                let store = store.borrow();
                (store.settings().clone(), store.revision())
            };
            Self {
                store: Rc::clone(store),
                cache,
                seen,
            }
        }

        fn sync(&mut self) {
            let store = self.store.borrow();
            if store.revision() != self.seen {
                self.cache = store.settings().clone();
                self.seen = store.revision();
            }
        }

        fn edit(&mut self, edit: impl FnOnce(&mut UiSettings)) {
            self.store.borrow_mut().update(edit);
            self.sync();
        }

        fn set_theme(&mut self, dark: bool) {
            self.edit(|s| {
                s.follow_system = false;
                s.dark = dark;
            });
        }

        fn set_font(&mut self, size: f32) {
            self.edit(|s| s.font_size = size);
        }

        fn add_rule(&mut self, pattern: &str) {
            let pattern = pattern.to_owned();
            self.edit(move |s| {
                let rules = s.highlight_rules.get_or_insert_with(Vec::new);
                let id = rules.iter().map(|rule| rule.id).max().unwrap_or(0) + 1;
                rules.push(HighlightRule::new(id, pattern, false));
            });
        }

        fn toggle_highlight(&mut self) {
            self.edit(|s| s.highlight_enabled = !s.highlight_enabled);
        }
    }

    /// 主窗口加两个独立窗口，交替改主题、字体、高亮：内存里三个窗口和仓库一致，
    /// 重启（重新读盘）后磁盘上也一致；每个窗口的修改都不会被另一个窗口手里的旧副本覆盖。
    #[test]
    fn three_windows_alternating_edits_agree_in_memory_and_after_a_restart() {
        let path = temp_path("three-windows");
        let writer = SettingsWriter::to_file(path.clone());
        let store = Rc::new(RefCell::new(SettingsStore::new(
            UiSettings::default(),
            writer,
        )));
        // 三个窗口都在"跟随系统"的初始状态下打开。
        let mut main = Window::open(&store);
        let mut term_a = Window::open(&store);
        let mut term_b = Window::open(&store);

        main.set_theme(true);
        term_a.set_font(16.0);
        term_b.add_rule("panic");
        main.set_font(14.0);
        term_a.set_theme(false);
        term_b.toggle_highlight();
        main.add_rule("timeout");
        term_a.edit(|s| s.cursor_blink = false);
        term_b.set_theme(true);
        term_a.add_rule("denied");
        term_b.toggle_highlight();
        main.set_font(15.0);

        for window in [&mut main, &mut term_a, &mut term_b] {
            window.sync();
        }
        let memory = store.borrow().settings().clone();
        assert_eq!(main.cache, memory);
        assert_eq!(term_a.cache, memory);
        assert_eq!(term_b.cache, memory);
        // 每个窗口改的东西都在：没有谁的修改被另一个窗口的旧副本盖掉。
        assert_eq!(memory.font_size, 15.0);
        assert!(memory.dark && !memory.follow_system);
        assert!(!memory.cursor_blink);
        assert!(memory.highlight_enabled);
        let patterns: Vec<_> = memory
            .highlight_rules
            .as_ref()
            .unwrap()
            .iter()
            .map(|rule| rule.pattern.as_str())
            .collect();
        assert_eq!(patterns, ["panic", "timeout", "denied"]);

        // 落盘（模拟退出）后重新读：和内存一致。
        store.borrow().writer().flush();
        let reloaded = UiSettings::load_from(&path).expect("读得回来").sanitized();
        assert_eq!(reloaded, memory);
        // 重启后新开的窗口看到的也是这一份。
        let reopened = Window::open(&Rc::new(RefCell::new(SettingsStore::new(
            reloaded.clone(),
            SettingsWriter::to_file(path),
        ))));
        assert_eq!(reopened.cache, reloaded);
    }

    /// 手动主题不被"跟随系统"覆盖：整个进程只有仓库里一个对齐入口，窗口手里没有自己的旧副本能把
    /// 它改回去；即使系统明暗来回变，手动选的深色仍是深色，磁盘上也是。
    #[test]
    fn a_manual_theme_survives_system_theme_changes_and_restarts() {
        let path = temp_path("manual-theme");
        let writer = SettingsWriter::to_file(path.clone());
        let store = Rc::new(RefCell::new(SettingsStore::new(
            UiSettings::default(),
            writer,
        )));
        let mut main = Window::open(&store);
        let mut term = Window::open(&store);
        // 系统是浅色，两个窗口最初都在跟随系统。
        let first = store.borrow_mut().follow_system_tick(Some(true), true);
        assert_eq!(first, Some(false));
        main.sync();
        term.sync();
        assert!(!main.cache.dark);

        // 在主窗口手动切深色。
        main.set_theme(true);
        let mut global_dark = true;
        // 之后系统明暗来回变，若干轮对齐：不能把它切回去。
        for system_light in [true, false, true, true, false, true] {
            if let Some(dark) = store
                .borrow_mut()
                .follow_system_tick(Some(system_light), global_dark)
            {
                global_dark = dark;
            }
            term.sync();
            assert!(term.cache.dark && !term.cache.follow_system);
        }
        assert!(global_dark, "手动深色没被改");

        store.borrow().writer().flush();
        let reloaded = UiSettings::load_from(&path).unwrap().sanitized();
        assert!(reloaded.dark && !reloaded.follow_system);
    }
}
