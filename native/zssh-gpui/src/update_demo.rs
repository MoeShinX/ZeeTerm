//! 更新检查窗口：取更新清单、下载安装包、校验 minisign 签名、生成安装计划。
//!
//! 发行版：当前版本是编译进来的包版本，公钥和更新源取自 `zssh_update::embedded_config`
//! （`native/zssh-update/updater.json`），清单和安装包只允许 `https://`，
//! 下载有大小上限和总时长上限，重定向只跟到 https。联网走「软件网络」代理设置。
//! 公钥还是占位值时（见 [`SIGNING_UNCONFIGURED`]）不请求、不下载、不安装。
//! 调试版额外认 `ZSSH_UPDATE_MANIFEST`（本地清单路径或 URL）、`ZSSH_CURRENT_VERSION`、
//! `ZSSH_UPDATE_PUBKEY`、`ZSSH_UPDATE_PROXY`，并放开 `http://` / `file://`；这些变量
//! 经 `dev_hooks` 读取，release 里恒为未设置。
//!
//! 安装包先整个下到内存、在内存里验签，通过后才以独占方式写进
//! `<数据目录>/update-cache/<哈希前缀>/`；安装计划带着验签时的 SHA-256，
//! 启动安装器前（含"退出后安装"）会重新算一次哈希，不一致就拒绝。
//! 后台任务经 `std::sync::mpsc` 把进度交给界面，100ms tick 取一次。
//!
//! 「安装」分两步：第一次只调用 `zssh_update::plan_install` 生成计划并展示，
//! 点「确认安装」才用 `ShellExecuteW` 启动已校验的安装器；启动成功后界面退出，
//! 安装器继续运行。
//!
//! 「退出后安装」只把计划排进进程级队列，界面不退出；真正的 `ShellExecuteW`
//! 放在 `App::on_app_quit` 注册的回调里，等程序退出时再启动。

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use ::gpui::{App, FocusHandle, Subscription, Task, Window, WindowBounds, WindowOptions};
use gpui_kit::component::button::Button;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tokio::io::AsyncReadExt as _;
use zssh_update::{
    InstallPlan, InstallRequest, Package, UpdateError, command_line, plan_install, sha256_hex,
};

use crate::install_state;

/// 更新检查窗口的编号；IME 按窗口 key 管理。
static NEXT_UPDATE: AtomicU64 = AtomicU64::new(1);

/// 「退出后安装」排好的计划；窗口关掉后仍然有效，进程退出时统一启动。
static PENDING_INSTALL: OnceLock<Mutex<Option<InstallPlan>>> = OnceLock::new();

/// 后台事件轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 下载进度上报间隔；界面本来就 100ms 取一次，再密没有意义。
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// 单次读取的块大小。
const CHUNK_SIZE: usize = 64 * 1024;
/// 更新包缓存目录名。
const CACHE_DIR: &str = "update-cache";
/// 内嵌公钥解析不出来时的固定前缀（正常构建里有单测保证不会发生）。
const PUBKEY_UNAVAILABLE: &str = "内置的更新公钥不可用，无法校验更新包";
/// 更新签名公钥还是占位值时的提示：这时没法证明安装包来自发布者，所以什么都不请求、不下载、不安装。
pub const SIGNING_UNCONFIGURED: &str = "更新签名未配置：这个构建还没有设置更新签名公钥，不能在线更新（不会下载或安装任何更新）。请到发布页下载新的安装包。";
/// 安装包大小上限。清单里没有大小字段，用一个宽松但有限的值挡住"无限流"和撑满内存。
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
/// 清单大小上限。
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// 下载安装包的总时长上限（含读完响应体）。
const PACKAGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 取清单的总时长上限。
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 最多跟随的重定向次数。
const MAX_REDIRECTS: usize = 5;
/// 启动安装器后等界面画出结果再退出。
const EXIT_DELAY: Duration = Duration::from_millis(300);
/// 「退出后安装」排队后的提示。
const QUEUED_MESSAGE: &str = "已排队：退出程序后启动安装器";

pub struct UpdateDemo {
    current: String,
    status: String,
    /// 清单里选中的包；没有更新或清单出错时为 `None`。
    package: Option<Package>,
    phase: DownloadState,
    /// 安装后是否重启；只在生成安装计划时使用。
    restart: bool,
    install: InstallState,
    /// 安装前安全检查拦下的原因；下一次点「安装 / 确认安装」时重算。
    blocked: Option<String>,
    /// 「退出后安装」的成功提示；排队状态本身在进程级队列里，关窗不丢。
    queued: Option<String>,
    /// 当前下载任务的代号；重新检查/取消后旧事件按代号丢掉。
    generation: u64,
    job: Option<DownloadJob>,
    events: Receiver<DownloadEvent>,
    sender: Sender<DownloadEvent>,
    focus: FocusHandle,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
    _tick: Task<()>,
}

impl UpdateDemo {
    /// `preset`：后台自动检查已经取到的结果（状态文字 + 选出的包），有它就不再重复请求清单。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        ime_key: String,
        preset: Option<(String, Option<Package>)>,
    ) -> Self {
        let focus = cx.focus_handle();
        let _ime_focus = cx.on_focus(&focus, window, {
            let key = ime_key.clone();
            move |_this, _window, _cx| {
                crate::ime::apply(&key, true);
            }
        });
        let _ime_blur = cx.on_blur(&focus, window, move |_this, _window, _cx| {
            crate::ime::apply(&ime_key, false);
        });
        let (sender, events) = std::sync::mpsc::channel();
        let mut this = Self {
            current: current_version(),
            status: String::new(),
            package: None,
            phase: DownloadState::Idle,
            restart: true,
            install: InstallState::Idle,
            blocked: None,
            queued: None,
            generation: 0,
            job: None,
            events,
            sender,
            focus,
            _ime_focus,
            _ime_blur,
            _tick: Task::ready(()),
        };
        match preset {
            Some((status, package)) => {
                this.status = status;
                this.package = package;
            }
            None => this.reload(cx),
        }
        // 100ms 取一次下载/校验进度；窗口没了 `update_in` 会报错，任务自己退出。
        this._tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update_in(cx, |this, _window, cx| this.poll_events(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 重新取一次清单；界面上的「重新检查」也走这里。网络请求在后台跑，结果经事件回来。
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.cancel_job();
        self.current = current_version();
        self.package = None;
        self.phase = DownloadState::Idle;
        self.install = InstallState::Idle;
        self.blocked = None;
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.status = "正在检查更新…".to_owned();
                runtime.spawn(run_check(
                    self.generation,
                    self.current.clone(),
                    self.sender.clone(),
                ));
            }
            Err(err) => self.status = format!("检查更新失败：{err}"),
        }
        cx.notify();
    }

    /// 作废当前下载任务；后台任务看到取消标志后删除半截文件。
    fn cancel_job(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 在 Tokio runtime 上开始下载；界面只收事件，不等待网络。
    fn start_download(&mut self, cx: &mut Context<Self>) {
        let Some(package) = self.package.clone() else {
            return;
        };
        if !self.phase.can_start() {
            return;
        }
        let generation = self.generation.wrapping_add(1);
        self.generation = generation;
        self.phase.start();
        self.install = InstallState::Idle;

        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(DownloadJob {
            cancel: Arc::clone(&cancel),
        });
        let sender = self.sender.clone();
        let cache_root = update_cache_dir();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                runtime.spawn(run_download(
                    generation, package, cache_root, cancel, sender,
                ));
            }
            Err(err) => {
                self.phase.fail(format!("启动下载失败：{err}"));
                self.job = None;
            }
        }
        cx.notify();
    }

    /// 取消下载：先改界面状态，后台任务随后删除半截文件。
    fn cancel_download(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.phase.cancel();
        cx.notify();
    }

    fn toggle_restart(&mut self, cx: &mut Context<Self>) {
        self.restart = !self.restart;
        // 参数变了，旧计划不能再显示；旧的拦截提示也一起清掉。
        self.install = InstallState::Idle;
        self.blocked = None;
        cx.notify();
    }

    /// 「安装」按钮：第一次点击只生成并展示计划，第二次才启动安装器。
    ///
    /// 返回是否已经启动；调用方据此安排退出。
    fn click_install(&mut self, cx: &mut Context<Self>) -> bool {
        let (installer, version, sha256) = match (&self.phase, &self.package) {
            (DownloadState::Verified { path, sha256, .. }, Some(package)) => {
                (path.clone(), package.version.clone(), sha256.clone())
            }
            _ => return false,
        };
        // 安装器一启动本程序就要退出：SFTP 传输 / 真实 AI 回合还在跑时先拦住。
        // 第一次点「安装」和第二次点「确认安装」都走这里，计划已生成也不会启动。
        if let Some(reason) = install_state::blocking_reason() {
            self.blocked = Some(reason);
            cx.notify();
            return false;
        }
        self.blocked = None;
        let cache_root = update_cache_dir();
        let args = current_exe_args();
        let restart = self.restart;
        let (state, launched) = advance_install(
            std::mem::take(&mut self.install),
            || build_install_plan(&installer, &cache_root, &version, restart, &args, &sha256),
            launch_installer,
        );
        self.install = state;
        if launched {
            // 安装器已经立即启动了，退出回调里不能再启动一次排队的计划。
            clear_pending_install();
            self.queued = None;
        }
        cx.notify();
        launched
    }

    /// 「退出后安装」：生成计划并排队，界面不退出，等进程结束时再启动安装器。
    fn click_queue_install(&mut self, cx: &mut Context<Self>) {
        let (installer, version, sha256) = match (&self.phase, &self.package) {
            (DownloadState::Verified { path, sha256, .. }, Some(package)) => {
                (path.clone(), package.version.clone(), sha256.clone())
            }
            _ => return,
        };
        let cache_root = update_cache_dir();
        let args = current_exe_args();
        let restart = self.restart;
        match build_install_plan(&installer, &cache_root, &version, restart, &args, &sha256) {
            Ok(plan) => {
                queue_install(plan);
                self.queued = Some(QUEUED_MESSAGE.to_owned());
                self.blocked = None;
            }
            // 没排上队：沿用「安装」失败时的提示，之前排好的计划保持不动。
            Err(err) => self.install = InstallState::Failed(format!("准备安装失败：{err}")),
        }
        cx.notify();
    }

    /// 取走后台事件；过期的任务结果直接丢掉。
    fn poll_events(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut auto_download = false;
        while let Ok(event) = self.events.try_recv() {
            if event.generation() != self.generation {
                continue;
            }
            match event {
                DownloadEvent::Progress {
                    received, total, ..
                } => self.phase.progress(received, total),
                DownloadEvent::Verifying { .. } => self.phase.begin_verify(),
                DownloadEvent::Verified {
                    path, size, sha256, ..
                } => {
                    self.phase.verified(path, size, sha256);
                    self.job = None;
                }
                DownloadEvent::Checked {
                    status, package, ..
                } => {
                    self.status = status;
                    self.package = package;
                    // 调试钩子：查到新版本就直接开始下载（自动化 / 截图下载中的画面用，发行版里读不到）。
                    auto_download = self.package.is_some()
                        && crate::dev_hooks::var_os("ZSSH_UPDATE_AUTODOWNLOAD").is_some();
                }
                DownloadEvent::Failed { message, .. } => {
                    self.phase.fail(message);
                    self.job = None;
                }
                DownloadEvent::Cancelled { .. } => {
                    self.phase.cancel();
                    self.job = None;
                }
            }
            changed = true;
        }
        if auto_download {
            self.start_download(cx);
        }
        if changed {
            cx.notify();
        }
    }
}

impl Render for UpdateDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let tokens = crate::app_theme::Tokens::current(cx);
        let lines: Vec<String> = self.status.lines().map(str::to_owned).collect();
        let phase_text = self.phase.text();
        let verified = match &self.phase {
            DownloadState::Verified { path, size, .. } => Some((path.clone(), *size)),
            _ => None,
        };
        let can_download = self.package.is_some() && self.phase.can_start();
        let downloading = self.phase.is_downloading();
        let install_text = match &self.install {
            InstallState::Idle => None,
            InstallState::Confirming(plan) => Some(format!(
                "{}\n再次点击「确认安装」启动安装器",
                command_line(plan)
            )),
            InstallState::Launched => Some("安装器已启动，程序即将退出".to_owned()),
            InstallState::Failed(err) => Some(err.clone()),
        };
        let awaiting_confirm = self.install.awaiting_confirm();
        let launched = matches!(self.install, InstallState::Launched);
        let install_label = if awaiting_confirm {
            "确认安装"
        } else {
            "安装"
        };
        let restart = self.restart;
        let blocked = self.blocked.clone();
        let queued = self.queued.clone();

        let recheck = entity.clone();
        let download = entity.clone();
        let cancel = entity.clone();
        let toggle = entity.clone();
        let queue = entity.clone();
        let install = entity;

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            // 文字色取皮肤的色板（不写的话深色皮肤下是黑字）。
            .text_color(tokens.text)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().child("检查更新"))
                    .child(
                        Button::new("recheck")
                            .label("重新检查")
                            .on_click(move |_, _, cx| {
                                recheck.update(cx, |this, cx| this.reload(cx));
                            }),
                    ),
            )
            .child(div().text_sm().child(format!("当前版本：{}", self.current)))
            .child(div().text_sm().child(format!(
                "更新源：{}（下载后校验签名，确认后才启动安装器）",
                source_label()
            )))
            // 更新说明可能很长：放进可滚动的区域，下面的按钮始终在窗口里。
            .child(
                div()
                    .id("update-status")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .gap_1()
                            .children(lines.into_iter().map(|line| div().text_sm().child(line))),
                    ),
            )
            .when(can_download, |section| {
                section.child(Button::new("download").label("下载并校验").on_click(
                    move |_, _, cx| {
                        download.update(cx, |this, cx| this.start_download(cx));
                    },
                ))
            })
            .when_some(phase_text, |section, text| {
                section.child(div().text_sm().child(text))
            })
            .when(downloading, |section| {
                section.child(Button::new("cancel").label("取消").compact().on_click(
                    move |_, _, cx| {
                        cancel.update(cx, |this, cx| this.cancel_download(cx));
                    },
                ))
            })
            .when_some(verified, |section, verified| {
                let (path, size) = verified;
                section
                    .child(div().text_sm().child(file_summary(&path, size)))
                    .child(h_flex().gap_2().items_center().when(!launched, |row| {
                        row.child(
                            Button::new("restart")
                                .label("安装后重启")
                                .compact()
                                .toggled(restart)
                                .on_click(move |_, _, cx| {
                                    toggle.update(cx, |this, cx| this.toggle_restart(cx));
                                }),
                        )
                        .child(
                            Button::new("install")
                                .label(install_label)
                                .compact()
                                .on_click(move |_, _, cx| {
                                    if install.update(cx, |this, cx| this.click_install(cx)) {
                                        quit_after_paint(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("queue-install")
                                .label("退出后安装")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    queue.update(cx, |this, cx| this.click_queue_install(cx));
                                }),
                        )
                    }))
            })
            .when_some(install_text, |section, text| {
                section.children(
                    text.lines()
                        .map(|line| div().text_sm().child(line.to_owned())),
                )
            })
            .when_some(queued, |section, text| {
                section.child(div().text_sm().child(text))
            })
            .when_some(blocked, |section, text| {
                section.child(div().text_sm().text_color(tokens.danger).child(text))
            })
    }
}

/// 打开更新检查窗口；同一个进程可以开多个。手动检查（关于页、托盘）走这里：抢焦点，窗口里自己去取清单。
pub fn open_update_window(cx: &mut App) {
    open_window_with(cx, true, None);
}

/// 后台自动检查发现了新版本：把已经取到的结果交给窗口（不重复请求），并且**不抢焦点**——
/// 用户可能正在终端里打字，窗口只是出现在那里等他有空看。
pub fn open_update_window_for_found(cx: &mut App, status: String, package: Package) {
    open_window_with(cx, false, Some((status, Some(package))));
}

fn open_window_with(cx: &mut App, focus: bool, preset: Option<(String, Option<Package>)>) {
    let n = NEXT_UPDATE.fetch_add(1, Ordering::Relaxed);
    let ime_key = format!("update-window-{n}");
    let close_key = ime_key.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(640.), ::gpui::px(520.)),
            cx,
        )),
        focus,
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| UpdateDemo::new(window, cx, ime_key, preset));
        window.set_window_title("检查更新");
        window.on_window_should_close(cx, move |_window, _cx| {
            crate::ime::window_destroyed(&close_key);
            true
        });
        if focus {
            window.focus(&view.read(cx).focus_handle(), cx);
        }
        view
    });
}

/// 下载/校验阶段；单独抽出来方便单测状态迁移。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum DownloadState {
    #[default]
    Idle,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Verifying,
    Verified {
        path: PathBuf,
        size: u64,
        /// 验签通过的字节的 SHA-256，安装计划要带着它。
        sha256: String,
    },
    Failed(String),
    Cancelled,
}

impl DownloadState {
    fn start(&mut self) {
        if self.can_start() {
            *self = Self::Downloading {
                received: 0,
                total: None,
            };
        }
    }

    fn progress(&mut self, received: u64, total: Option<u64>) {
        if matches!(self, Self::Downloading { .. }) {
            *self = Self::Downloading { received, total };
        }
    }

    fn begin_verify(&mut self) {
        if matches!(self, Self::Downloading { .. }) {
            *self = Self::Verifying;
        }
    }

    fn verified(&mut self, path: PathBuf, size: u64, sha256: String) {
        if matches!(self, Self::Verifying) {
            *self = Self::Verified { path, size, sha256 };
        }
    }

    fn fail(&mut self, message: String) {
        *self = Self::Failed(message);
    }

    fn cancel(&mut self) {
        if matches!(self, Self::Downloading { .. }) {
            *self = Self::Cancelled;
        }
    }

    /// 只有空闲、失败或已取消时可以重新下载。
    fn can_start(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_) | Self::Cancelled)
    }

    fn is_downloading(&self) -> bool {
        matches!(self, Self::Downloading { .. })
    }

    fn text(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Downloading { received, total } => Some(progress_text(*received, *total)),
            Self::Verifying => Some("正在校验签名…".to_owned()),
            Self::Verified { .. } => Some("签名校验通过".to_owned()),
            Self::Failed(message) => Some(message.clone()),
            Self::Cancelled => Some("已取消下载".to_owned()),
        }
    }
}

/// 安装的两段确认状态：第一次点击只生成计划，第二次才启动安装器。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum InstallState {
    #[default]
    Idle,
    /// 计划已生成，等用户点「确认安装」；这一阶段不会启动任何进程。
    Confirming(InstallPlan),
    /// 安装器已交给系统，界面即将退出。
    Launched,
    Failed(String),
}

impl InstallState {
    /// 是否已经点过一次「安装」、正在等确认。
    fn awaiting_confirm(&self) -> bool {
        matches!(self, Self::Confirming(_))
    }
}

/// 点「安装」/「确认安装」时的纯状态迁移。
///
/// 第一次点击只生成计划并进入 [`InstallState::Confirming`]；再点一次才调用
/// `launch`。返回 `(新状态, 是否已启动安装器)`，调用方据此决定是否退出。
/// `launch` 由调用方注入，测试里换成假实现，不会真的启动进程。
fn advance_install(
    state: InstallState,
    plan: impl FnOnce() -> Result<InstallPlan, String>,
    launch: impl FnOnce(&InstallPlan) -> Result<(), String>,
) -> (InstallState, bool) {
    match state {
        // 已经启动过，界面正在退出，不再响应点击。
        InstallState::Launched => (InstallState::Launched, false),
        InstallState::Confirming(plan) => match launch(&plan) {
            Ok(()) => (InstallState::Launched, true),
            Err(err) => (
                InstallState::Failed(format!("启动安装器失败：{err}")),
                false,
            ),
        },
        _ => match plan() {
            Ok(plan) => (InstallState::Confirming(plan), false),
            Err(err) => (InstallState::Failed(format!("准备安装失败：{err}")), false),
        },
    }
}

/// 取消标志；后台任务在每个数据块之间检查。
struct DownloadJob {
    cancel: Arc<AtomicBool>,
}

/// 后台任务 → 界面的消息（100ms tick 取走）。
#[derive(Debug)]
enum DownloadEvent {
    Progress {
        generation: u64,
        received: u64,
        total: Option<u64>,
    },
    Verifying {
        generation: u64,
    },
    Verified {
        generation: u64,
        path: PathBuf,
        size: u64,
        sha256: String,
    },
    Failed {
        generation: u64,
        message: String,
    },
    Cancelled {
        generation: u64,
    },
    /// 取清单的结果。
    Checked {
        generation: u64,
        status: String,
        package: Option<Package>,
    },
}

impl DownloadEvent {
    fn generation(&self) -> u64 {
        match self {
            Self::Progress { generation, .. }
            | Self::Verifying { generation }
            | Self::Verified { generation, .. }
            | Self::Failed { generation, .. }
            | Self::Cancelled { generation }
            | Self::Checked { generation, .. } => *generation,
        }
    }
}

/// 取消 / 失败；下载和取清单共用。
enum DownloadFailure {
    Cancelled,
    Failed(String),
}

/// 更新源是不是放开了 `http://` / `file://`：只有调试构建放开。
fn allow_insecure_sources() -> bool {
    crate::dev_hooks::ENABLED
}

/// 更新清单和安装包的地址检查：发行版只认 `https://`。
///
/// `allow_insecure` 为真（调试构建）时才放行 `http://` 和本地 `file://`；
/// `file://` 带非本机主机名（Windows 上会变成 UNC 路径，打开就触发 SMB 认证）一律拒绝。
fn check_update_url(url: &str, allow_insecure: bool) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|err| format!("更新地址无效：{err}"))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if allow_insecure => Ok(parsed),
        "file" if allow_insecure => match parsed.host_str() {
            None | Some("") | Some("localhost") => Ok(parsed),
            Some(_) => Err("更新地址不能指向其他主机的文件".to_owned()),
        },
        other => Err(format!("更新地址必须是 https，实际是 {other}")),
    }
}

/// 重定向规则：次数有限，且不能跳到不允许的协议（尤其是 https → http）。
fn redirect_allowed(
    scheme: &str,
    previous: usize,
    allow_insecure: bool,
) -> Result<(), &'static str> {
    if previous > MAX_REDIRECTS {
        return Err("更新地址重定向次数过多");
    }
    match scheme {
        "https" => Ok(()),
        "http" if allow_insecure => Ok(()),
        _ => Err("更新地址重定向到了非 https 的地址，已拒绝"),
    }
}

fn redirect_policy(allow_insecure: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        match redirect_allowed(
            attempt.url().scheme(),
            attempt.previous().len(),
            allow_insecure,
        ) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(message),
        }
    })
}

/// reqwest 的错误只写"跟随重定向失败"这类外层原因，真正的缘由（比如我们自己的重定向规则）
/// 在 `source()` 链里，一起拼出来用户才看得懂。
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push('：');
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// 服务器返回非 2xx 时给用户看的原因：最常见的几种说人话，其余带上状态码。
fn http_status_message(status: reqwest::StatusCode) -> String {
    let code = status.as_u16();
    match code {
        404 => "下载失败：服务器上找不到这个文件（HTTP 404），可能这个版本还没发布或已被撤回"
            .to_owned(),
        401 | 403 => format!("下载失败：服务器拒绝访问（HTTP {code}）"),
        429 => "下载失败：请求太频繁，被服务器限流了（HTTP 429），稍后再试".to_owned(),
        500..=599 => format!("下载失败：服务器出错了（HTTP {code}），稍后再试"),
        _ => format!("下载失败：HTTP {status}"),
    }
}

fn too_large_message(max_bytes: u64) -> String {
    format!(
        "更新文件超过大小上限（{} MiB），已中止",
        max_bytes / (1024 * 1024)
    )
}

/// 追加一个数据块，累计超过上限就报错；下载和读本地文件共用，方便单测。
fn append_capped(buffer: &mut Vec<u8>, chunk: &[u8], max_bytes: u64) -> Result<(), String> {
    if (buffer.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
        return Err(too_large_message(max_bytes));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

/// 把 `url` 的内容读进内存。超过 `max_bytes`、超过 `timeout` 或被取消都会中止，
/// 不会留下任何文件。`progress(已收, 总数, 是否强制上报)` 由调用方节流后发给界面。
async fn fetch_bytes(
    client: &reqwest::Client,
    url: &reqwest::Url,
    max_bytes: u64,
    timeout: Duration,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>, bool),
) -> Result<Vec<u8>, DownloadFailure> {
    let failed = |message: String| DownloadFailure::Failed(message);
    // 预分配别太大：Content-Length 是对方说了算的。
    let prealloc = |total: Option<u64>| total.unwrap_or(0).min(16 * 1024 * 1024) as usize;

    if url.scheme() == "file" {
        let source = url
            .to_file_path()
            .map_err(|_| failed("更新地址不是本地文件路径".to_owned()))?;
        let mut input = tokio::fs::File::open(&source)
            .await
            .map_err(|err| failed(format!("读取本地更新包失败：{err}")))?;
        let total = input
            .metadata()
            .await
            .ok()
            .map(|meta| meta.len())
            .filter(|len| *len > 0);
        if total.is_some_and(|len| len > max_bytes) {
            return Err(failed(too_large_message(max_bytes)));
        }
        let mut data = Vec::with_capacity(prealloc(total));
        let mut buffer = vec![0u8; CHUNK_SIZE];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DownloadFailure::Cancelled);
            }
            let read = input
                .read(&mut buffer)
                .await
                .map_err(|err| failed(format!("读取本地更新包失败：{err}")))?;
            if read == 0 {
                break;
            }
            append_capped(&mut data, &buffer[..read], max_bytes).map_err(failed)?;
            progress(data.len() as u64, total, false);
        }
        progress(data.len() as u64, total, true);
        return Ok(data);
    }

    let mut response = client
        .get(url.clone())
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| failed(format!("下载失败：{}", error_chain(&err))))?;
    if !response.status().is_success() {
        return Err(failed(http_status_message(response.status())));
    }
    let total = response.content_length().filter(|len| *len > 0);
    if total.is_some_and(|len| len > max_bytes) {
        return Err(failed(too_large_message(max_bytes)));
    }
    let mut data = Vec::with_capacity(prealloc(total));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| failed(format!("下载失败：{}", error_chain(&err))))?
    {
        if cancel.load(Ordering::Relaxed) {
            return Err(DownloadFailure::Cancelled);
        }
        append_capped(&mut data, &chunk, max_bytes).map_err(failed)?;
        progress(data.len() as u64, total, false);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(DownloadFailure::Cancelled);
    }
    progress(data.len() as u64, total, true);
    Ok(data)
}

/// 取清单并判断有没有更新；结果经事件交给界面。
async fn run_check(generation: u64, current: String, sender: Sender<DownloadEvent>) {
    let (status, package) = check_for_update(&current).await;
    let _ = sender.send(DownloadEvent::Checked {
        generation,
        status,
        package,
    });
}

/// 清单来源，按顺序尝试，第一个读成功的算数。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManifestSource {
    /// 本地清单文件（仅调试钩子）。
    File(PathBuf),
    /// 清单地址：内嵌的更新源，或调试钩子给的 URL。
    Url(String),
}

/// `hook` 是 `ZSSH_UPDATE_MANIFEST`（调试钩子，已经过 `dev_hooks`）；没有就用内嵌更新源。
fn resolve_manifest_sources(
    hook: Option<std::ffi::OsString>,
    embedded: Result<zssh_update::UpdaterConfig, String>,
) -> Vec<ManifestSource> {
    if let Some(value) = hook.filter(|value| !value.is_empty()) {
        let text = value.to_string_lossy().trim().to_owned();
        let lower = text.to_ascii_lowercase();
        return vec![
            if lower.starts_with("http://") || lower.starts_with("https://") {
                ManifestSource::Url(text)
            } else {
                ManifestSource::File(PathBuf::from(value))
            },
        ];
    }
    embedded
        .map(|config| {
            config
                .endpoints
                .into_iter()
                .map(ManifestSource::Url)
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_sources() -> Vec<ManifestSource> {
    resolve_manifest_sources(
        crate::dev_hooks::var_os("ZSSH_UPDATE_MANIFEST"),
        zssh_update::embedded_config(),
    )
}

/// 界面上显示的更新源说明。
fn source_label() -> &'static str {
    if crate::dev_hooks::var_os("ZSSH_UPDATE_MANIFEST").is_some_and(|value| !value.is_empty()) {
        "调试指定的更新说明"
    } else {
        "官方发布页"
    }
}

async fn load_manifest_text(
    source: &ManifestSource,
    client: &reqwest::Client,
    allow_insecure: bool,
) -> Result<String, String> {
    let bytes = match source {
        ManifestSource::File(path) => {
            let meta = tokio::fs::metadata(path)
                .await
                .map_err(|err| err.to_string())?;
            if meta.len() > MAX_MANIFEST_BYTES {
                return Err(too_large_message(MAX_MANIFEST_BYTES));
            }
            tokio::fs::read(path).await.map_err(|err| err.to_string())?
        }
        ManifestSource::Url(url) => {
            let url = check_update_url(url, allow_insecure)?;
            let never = AtomicBool::new(false);
            fetch_bytes(
                client,
                &url,
                MAX_MANIFEST_BYTES,
                MANIFEST_TIMEOUT,
                &never,
                |_, _, _| {},
            )
            .await
            .map_err(|failure| match failure {
                DownloadFailure::Cancelled => "已取消".to_owned(),
                DownloadFailure::Failed(message) => message,
            })?
        }
    };
    String::from_utf8(bytes).map_err(|_| "更新说明不是有效的 UTF-8 文本".to_owned())
}

/// 读清单 + 选择包；读不到也算界面错误，不让窗口崩。
///
/// 更新签名公钥是占位值时直接返回"更新签名未配置"，一个请求都不发。
pub(crate) async fn check_for_update(current: &str) -> (String, Option<Package>) {
    let pubkey = update_pubkey();
    // 公钥不可用时先不建客户端（建客户端要读设置库）：下面 `check_with` 会原样返回原因。
    let client = if pubkey.is_ok() {
        match update_client() {
            Ok(client) => client,
            Err(err) => return (format!("检查更新失败：{err}"), None),
        }
    } else {
        reqwest::Client::new()
    };
    check_with(
        current,
        &manifest_sources(),
        &client,
        allow_insecure_sources(),
        pubkey.as_deref(),
    )
    .await
}

/// [`check_for_update`] 的可注入版本：清单来源、客户端和协议放行由调用方给（测试用本机服务）。
async fn check_with(
    current: &str,
    sources: &[ManifestSource],
    client: &reqwest::Client,
    allow_insecure: bool,
    pubkey: Result<&str, &String>,
) -> (String, Option<Package>) {
    // 没有可用的验签公钥（占位值、内置配置坏了）就不发任何请求：后面下的包也验不了。
    match pubkey {
        Err(message) => return (message.clone(), None),
        Ok(key) if zssh_update::is_placeholder_pubkey(key) => {
            return (SIGNING_UNCONFIGURED.to_owned(), None);
        }
        Ok(_) => {}
    }
    if sources.is_empty() {
        return ("未配置更新源".to_owned(), None);
    }
    let mut last_error = String::new();
    for source in sources {
        match load_manifest_text(source, client, allow_insecure).await {
            Ok(text) => return evaluate_manifest(&text, current, allow_insecure),
            Err(err) => last_error = err,
        }
    }
    (format!("读取更新说明失败：{last_error}"), None)
}

/// 下载、校验、上报结果都在 runtime 线程里跑。
async fn run_download(
    generation: u64,
    package: Package,
    cache_root: PathBuf,
    cancel: Arc<AtomicBool>,
    sender: Sender<DownloadEvent>,
) {
    match download_and_verify(generation, &package, &cache_root, &cancel, &sender).await {
        Ok((path, size, sha256)) => {
            let _ = sender.send(DownloadEvent::Verified {
                generation,
                path,
                size,
                sha256,
            });
        }
        Err(DownloadFailure::Cancelled) => {
            let _ = sender.send(DownloadEvent::Cancelled { generation });
        }
        Err(DownloadFailure::Failed(message)) => {
            let _ = sender.send(DownloadEvent::Failed {
                generation,
                message,
            });
        }
    }
}

/// 下到内存 → 验签 → 把验签通过的字节写进缓存。返回 `(路径, 大小, SHA-256)`。
async fn download_and_verify(
    generation: u64,
    package: &Package,
    cache_root: &Path,
    cancel: &AtomicBool,
    sender: &Sender<DownloadEvent>,
) -> Result<(PathBuf, u64, String), DownloadFailure> {
    // 公钥是占位值就在这里挡住，连文件名都不用看，更不会发出下载请求。
    let pubkey = update_pubkey().map_err(DownloadFailure::Failed)?;
    let client = update_client().map_err(DownloadFailure::Failed)?;
    download_with(
        &DownloadContext {
            pubkey: &pubkey,
            client: &client,
            allow_insecure: allow_insecure_sources(),
        },
        generation,
        package,
        cache_root,
        cancel,
        sender,
    )
    .await
}

/// 下载用的环境：验签公钥、HTTP 客户端、是否放行 http / file（测试注入本机服务）。
struct DownloadContext<'a> {
    pubkey: &'a str,
    client: &'a reqwest::Client,
    allow_insecure: bool,
}

/// [`download_and_verify`] 的可注入版本。
async fn download_with(
    env: &DownloadContext<'_>,
    generation: u64,
    package: &Package,
    cache_root: &Path,
    cancel: &AtomicBool,
    sender: &Sender<DownloadEvent>,
) -> Result<(PathBuf, u64, String), DownloadFailure> {
    if zssh_update::is_placeholder_pubkey(env.pubkey) {
        return Err(DownloadFailure::Failed(SIGNING_UNCONFIGURED.to_owned()));
    }
    let url =
        check_update_url(&package.url, env.allow_insecure).map_err(DownloadFailure::Failed)?;
    let name = download_file_name(&package.url).map_err(DownloadFailure::Failed)?;
    // 先把文件名查一遍，名字不合法就不必下载。
    cache_path(cache_root, &name).map_err(DownloadFailure::Failed)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(DownloadFailure::Cancelled);
    }

    let mut last_report = None;
    let bytes = fetch_bytes(
        env.client,
        &url,
        MAX_PACKAGE_BYTES,
        PACKAGE_TIMEOUT,
        cancel,
        |received, total, force| {
            report_progress(sender, generation, received, total, &mut last_report, force);
        },
    )
    .await?;

    let _ = sender.send(DownloadEvent::Verifying { generation });
    let package = package.clone();
    let cache_root = cache_root.to_path_buf();
    let pubkey = env.pubkey.to_owned();
    let stored = tokio::task::spawn_blocking(move || {
        verify_and_store(&bytes, &package, &pubkey, &cache_root, &name)
    })
    .await
    .map_err(|err| DownloadFailure::Failed(format!("校验更新包失败：{err}")))?
    .map_err(DownloadFailure::Failed)?;
    if cancel.load(Ordering::Relaxed) {
        // 用户在校验期间取消了：别把这份文件留着当"已校验"。
        let _ = std::fs::remove_file(&stored.0);
        return Err(DownloadFailure::Cancelled);
    }
    Ok(stored)
}

/// 在内存里验包字节、签名和 trusted comment 里的版本，通过后才落盘。
///
/// 落盘的就是验签的那份字节：写到 `<缓存>/<哈希前 16 位>/<文件名>`，独占创建
/// （`create_new`），已有同名文件先删再建，删完被别的进程抢先建出来就报错而不是沿用。
/// 返回 `(路径, 大小, SHA-256)`。
fn verify_and_store(
    bytes: &[u8],
    package: &Package,
    pubkey: &str,
    cache_root: &Path,
    name: &str,
) -> Result<(PathBuf, u64, String), String> {
    zssh_update::verify_signature(bytes, &package.signature, pubkey, &package.version, true)
        .map_err(|err| err.to_string())?;
    let sha256 = sha256_hex(bytes);
    // 哈希目录让不同版本 / 不同内容互不覆盖：排队等退出安装的那份不会被后来的下载截断。
    let dir = cache_root.join(&sha256[..16]);
    let path = cache_path(&dir, name)?;
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建更新缓存目录失败：{err}"))?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("清理旧的更新包失败：{err}")),
    }
    let write = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    };
    if let Err(err) = write() {
        // 半截文件不能留着被误当成完整安装包。
        let _ = std::fs::remove_file(&path);
        return Err(format!("写入更新包失败：{err}"));
    }
    Ok((path, bytes.len() as u64, sha256))
}

/// 验签用的公钥：调试钩子优先，否则用编译期内嵌的那一把。
pub(crate) fn update_pubkey() -> Result<String, String> {
    resolve_pubkey(
        crate::dev_hooks::var("ZSSH_UPDATE_PUBKEY"),
        zssh_update::embedded_config(),
    )
}

fn resolve_pubkey(
    hook: Option<String>,
    embedded: Result<zssh_update::UpdaterConfig, String>,
) -> Result<String, String> {
    let key = match hook
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(key) => key,
        None => embedded
            .map(|config| config.pubkey)
            .map_err(|err| format!("{PUBKEY_UNAVAILABLE}：{err}"))?,
    };
    // 占位公钥验不了任何签名：与其让后面报一个看不懂的验签错误，不如在这里就明说。
    if zssh_update::is_placeholder_pubkey(&key) {
        return Err(SIGNING_UNCONFIGURED.to_owned());
    }
    Ok(key)
}

/// 进度节流：首个数据块和每 100ms 各上报一次，结束再补一次。
fn report_progress(
    sender: &Sender<DownloadEvent>,
    generation: u64,
    received: u64,
    total: Option<u64>,
    last_report: &mut Option<Instant>,
    force: bool,
) {
    let due = force || last_report.is_none_or(|at| at.elapsed() >= REPORT_INTERVAL);
    if due {
        let _ = sender.send(DownloadEvent::Progress {
            generation,
            received,
            total,
        });
        *last_report = Some(Instant::now());
    }
}

/// 总大小未知或为 0 时只显示已下载字节数。
fn progress_text(received: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!("已下载 {received} / {total}"),
        _ => format!("已下载 {received}"),
    }
}

fn file_summary(path: &Path, size: u64) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("（未知文件）");
    format!("文件：{name}（{size} 字节）")
}

fn update_cache_dir() -> PathBuf {
    crate::settings::data_dir().join(CACHE_DIR)
}

fn current_exe_args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// 生成安装计划；不启动任何进程。失败时返回可直接显示的中文原因。
fn build_install_plan(
    installer: &Path,
    cache_root: &Path,
    version: &str,
    restart: bool,
    current_exe_args: &[String],
    sha256: &str,
) -> Result<InstallPlan, String> {
    let request = InstallRequest {
        installer: installer.to_path_buf(),
        cache_root: cache_root.to_path_buf(),
        version: version.to_owned(),
        restart,
        current_exe_args: current_exe_args.to_vec(),
        sha256: sha256.to_owned(),
    };
    plan_install(&request).map_err(|err| err.to_string())
}

/// 等界面把「安装器已启动」画出来再退出应用；不碰安装器进程，它自己继续跑。
fn quit_after_paint(cx: &App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(EXIT_DELAY).await;
        cx.update(|app| app.quit());
    })
    .detach();
}

/// 取进程级的待安装队列。
fn pending_install() -> &'static Mutex<Option<InstallPlan>> {
    PENDING_INSTALL.get_or_init(|| Mutex::new(None))
}

/// 锁中毒只说明别的线程持锁时 panic 过，队列本身还能用，取回内部值继续。
fn locked_pending_install() -> MutexGuard<'static, Option<InstallPlan>> {
    match pending_install().lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// 排队等待退出后安装；已有计划直接覆盖，允许重复排队。
pub fn queue_install(plan: InstallPlan) {
    *locked_pending_install() = Some(plan);
}

/// 取走排队的计划；没有排队时返回 `None`。测试也用它检查队列。
fn take_pending_install() -> Option<InstallPlan> {
    locked_pending_install().take()
}

/// 清掉排队的计划；立即安装成功后调用，免得退出时再启动一次。
fn clear_pending_install() {
    *locked_pending_install() = None;
}

/// 排队等退出后安装的版本号（不带 `v`）；托盘据此显示「重启安装 vX」。没有排队时为 `None`。
pub fn pending_install_version() -> Option<String> {
    locked_pending_install()
        .as_ref()
        .map(|plan| plan.version.clone())
}

/// 托盘的「重启安装」：把排队的计划改成"装完重启 ZeeTerm"。返回是否真有计划可改。
/// 实际启动安装器仍然在程序退出时（`run_pending_install`），由调用方随后退出程序。
pub fn arm_pending_restart() -> bool {
    let mut slot = locked_pending_install();
    match slot.take() {
        Some(plan) => {
            *slot = Some(plan.with_restart(&current_exe_args()));
            true
        }
        None => false,
    }
}

/// 退出时启动排队好的安装器；由 `App::on_app_quit` 调用。
///
/// GPUI 给退出回调的时间很短，所以这里只做一次同步的 `ShellExecuteW`，
/// 不等待安装器。队列为空、启动失败都只记日志，不能 panic 也不能卡住退出。
pub fn run_pending_install() {
    let Some(plan) = take_pending_install() else {
        return;
    };
    match launch_installer(&plan) {
        Ok(()) => tracing::info!(
            version = %plan.version,
            program = %plan.program.display(),
            "退出时已启动安装器"
        ),
        Err(err) => tracing::warn!(
            version = %plan.version,
            program = %plan.program.display(),
            %err,
            "退出时启动安装器失败"
        ),
    }
}

/// 启动已校验的安装器。启动前一定先复验文件哈希：验签到这里可能隔了很久
/// （"退出后安装"），缓存目录又能被同用户的其他进程写入。
fn launch_installer(plan: &InstallPlan) -> Result<(), String> {
    launch_if_unchanged(plan, shell_execute_installer)
}

/// 复验通过才调用 `launch`；不一致或读不到文件就拒绝，一个字节的安装器都不启动。
/// `launch` 由调用方注入，测试里换成假实现。
fn launch_if_unchanged(
    plan: &InstallPlan,
    launch: impl FnOnce(&InstallPlan) -> Result<(), String>,
) -> Result<(), String> {
    zssh_update::verify_plan_file(plan)
        .map_err(|err| format!("安装包与验签时不一致，已拒绝启动：{err}"))?;
    launch(plan)
}

/// 用 `ShellExecuteW` 启动已校验的安装器。
///
/// 工作目录取安装器所在目录，参数按 Windows 命令行规则加引号；返回值大于 32
/// 才算成功，否则把错误码交给界面，程序继续运行。
#[cfg(windows)]
fn shell_execute_installer(plan: &InstallPlan) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    let program = wide(plan.program.as_os_str());
    let args = quote_args(&plan.args);
    let args = wide(OsStr::new(&args));
    // 安装计划里的路径已经规范化过，父目录一定存在；空父目录交给系统默认值。
    let working_dir = plan.program.parent().unwrap_or(Path::new(""));
    let working_dir = (!working_dir.as_os_str().is_empty()).then(|| wide(working_dir.as_os_str()));

    // SAFETY: 各宽字符串都以 0 结尾，并且在调用期间一直存活。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(program.as_ptr()),
            PCWSTR(args.as_ptr()),
            working_dir
                .as_deref()
                .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
            SW_SHOWNORMAL,
        )
    };
    // 文档：返回值大于 32 表示成功，否则是错误码。
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW 返回错误码 {code}"))
    }
}

#[cfg(not(windows))]
fn shell_execute_installer(_plan: &InstallPlan) -> Result<(), String> {
    Err("只支持在 Windows 上启动安装器".to_owned())
}

/// 按 Windows 命令行规则拼一组参数。
fn quote_args(args: &[String]) -> String {
    let mut line = String::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        push_quoted_arg(&mut line, arg);
    }
    line
}

/// 追加一个参数：空参数、含空白或引号的参数用双引号包起来。
///
/// 引号前的反斜杠要加倍再转义引号，结尾的反斜杠也要加倍，否则会被当成
/// 转义后面的结束引号。
fn push_quoted_arg(line: &mut String, arg: &str) {
    let needs_quotes = arg.is_empty() || arg.chars().any(|ch| ch.is_whitespace() || ch == '"');
    if !needs_quotes {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                push_repeated(line, '\\', backslashes * 2 + 1);
                line.push('"');
                backslashes = 0;
            }
            _ => {
                push_repeated(line, '\\', backslashes);
                line.push(ch);
                backslashes = 0;
            }
        }
    }
    push_repeated(line, '\\', backslashes * 2);
    line.push('"');
}

fn push_repeated(line: &mut String, ch: char, count: usize) {
    for _ in 0..count {
        line.push(ch);
    }
}

/// 从下载地址取文件名：只看最后一段，挡掉空名、`.`/`..` 和 Windows 非法字符。
fn download_file_name(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|err| format!("更新地址无效：{err}"))?;
    match parsed.scheme() {
        "http" | "https" | "file" => {}
        other => return Err(format!("不支持的更新地址协议：{other}")),
    }
    let raw = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    let name = raw.trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err("更新地址里没有可用的文件名".to_owned());
    }
    validate_file_name(name)?;
    Ok(name.to_owned())
}

/// 缓存目录里的落盘位置；文件名必须只有一个普通段，不能跳出缓存根目录。
fn cache_path(cache_root: &Path, name: &str) -> Result<PathBuf, String> {
    validate_file_name(name)?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!("更新包文件名不合法：{name}"));
    }
    Ok(cache_root.join(name))
}

fn validate_file_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("更新包文件名为空".to_owned());
    }
    if name.chars().any(|ch| {
        ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err(format!("更新包文件名不合法：{name}"));
    }
    Ok(())
}

/// 软件联网用的代理：调试钩子 `ZSSH_UPDATE_PROXY` 优先，其次是设置里的「软件网络」代理
/// （跟随系统代理 / SSH 默认代理 / 代理库里的某个 / 直连，和配置同步、下载 opencode 是同一个设置）。
/// `Ok(None)` 是直连；设置库打不开或代理配置有问题（例如指定的代理被删了）返回原因，不静默改成直连。
fn update_proxy() -> Result<Option<String>, String> {
    if let Some(hook) = crate::dev_hooks::var("ZSSH_UPDATE_PROXY")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(hook));
    }
    let db = zssh_core::store::Database::open(&crate::settings::data_dir().join("zssh.db"))
        .map_err(|err| format!("打开设置库失败：{err}"))?;
    proxy_from_settings(&db)
}

/// 设置库里的「软件网络」代理 -> 代理地址（含账号密码，不能打印）；`None` 是直连。
fn proxy_from_settings(db: &zssh_core::store::Database) -> Result<Option<String>, String> {
    zssh_core::sync::app_proxy::app_proxy(db)
        .map(|proxy| proxy.map(|proxy| proxy.url_with_credentials()))
        .map_err(|err| format!("软件网络的代理设置有问题：{err}"))
}

/// 不带代理的客户端构造器：超时、UA 和重定向规则都在这里，测试可以直接拿它连本地服务。
fn update_client_builder(allow_insecure: bool) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("ZeeTerm/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect_policy(allow_insecure))
}

fn update_client() -> Result<reqwest::Client, String> {
    let mut builder = update_client_builder(allow_insecure_sources());
    builder = match update_proxy()? {
        // 本机回环地址永远不走代理（调试用的本机夹具、以及误填的回环清单地址都直连）。
        Some(proxy) => builder.proxy(
            reqwest::Proxy::all(&proxy)
                .map_err(|err| format!("更新代理配置错误：{err}"))?
                .no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1")),
        ),
        // 直连：也不读环境变量里的代理，和软件网络的其他联网一致。
        None => builder.no_proxy(),
    };
    builder
        .build()
        .map_err(|err| format!("创建下载客户端失败：{err}"))
}

/// 当前版本：编译进来的包版本。调试钩子 `ZSSH_CURRENT_VERSION` 可以覆盖（为了测更新流程）。
pub(crate) fn current_version() -> String {
    resolve_current_version(crate::dev_hooks::var("ZSSH_CURRENT_VERSION"))
}

fn resolve_current_version(hook: Option<String>) -> String {
    hook.map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

/// 解析清单并选包；选出的安装包地址也要过协议检查。
fn evaluate_manifest(text: &str, current: &str, allow_insecure: bool) -> (String, Option<Package>) {
    match zssh_update::parse_manifest(text) {
        Ok(manifest) => {
            let outcome = manifest.select(current);
            if let Ok(Some(package)) = &outcome
                && let Err(err) = check_update_url(&package.url, allow_insecure)
            {
                return (format!("检查更新失败：{err}"), None);
            }
            let text = status_text(&outcome, manifest.notes.as_deref());
            (text, outcome.ok().flatten())
        }
        Err(err) => (status_text(&Err(err), None), None),
    }
}

/// 纯文本状态，方便单测；文件 I/O 留在 [`check_manifest`] 里。
fn status_text(outcome: &Result<Option<Package>, UpdateError>, notes: Option<&str>) -> String {
    match outcome {
        Ok(None) => "已是最新版本".to_owned(),
        Ok(Some(package)) => {
            let notes = notes
                .map(str::trim)
                .filter(|notes| !notes.is_empty())
                .unwrap_or("（无更新说明）");
            format!(
                "发现新版本 {}\n{}\n安装包：{}\n平台：{}",
                package.version, notes, package.url, package.platform_key
            )
        }
        Err(err) => format!("检查更新失败：{err}"),
    }
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
    // `#[test]` 属性宏，展开时直接撞递归上限。
    use super::{
        DownloadState, InstallState, advance_install, build_install_plan, cache_path,
        download_file_name, file_summary, progress_text, push_quoted_arg, queue_install,
        quote_args, take_pending_install,
    };
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};
    use zssh_update::{InstallPlan, Package, UpdateError};

    /// 待安装队列是进程级全局，这几个用例必须串行，否则「空队列」用例可能
    /// 捡到别的用例刚排进去的计划，真的去启动安装器。
    fn queue_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        match LOCK.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        }
    }

    fn sample_plan() -> InstallPlan {
        InstallPlan {
            program: PathBuf::from("C:\\cache\\zssh-setup.exe"),
            args: vec!["/P".to_owned(), "/UPDATE".to_owned()],
            restart: false,
            version: "0.3.0".to_owned(),
            sha256: "0".repeat(64),
        }
    }

    fn push_quoted(arg: &str) -> String {
        let mut line = String::new();
        push_quoted_arg(&mut line, arg);
        line
    }

    fn package() -> Package {
        Package {
            platform_key: "windows-x86_64-nsis".to_owned(),
            version: "0.3.0".to_owned(),
            url: "https://example.com/zssh-setup.exe".to_owned(),
            signature: "unused-in-window".to_owned(),
        }
    }

    #[test]
    fn no_update_says_latest() {
        assert_eq!(super::status_text(&Ok(None), None), "已是最新版本");
    }

    #[test]
    fn update_lists_version_notes_url_and_platform() {
        let text = super::status_text(&Ok(Some(package())), Some("修复若干问题"));
        assert!(text.contains("发现新版本 0.3.0"));
        assert!(text.contains("修复若干问题"));
        assert!(text.contains("https://example.com/zssh-setup.exe"));
        assert!(text.contains("windows-x86_64-nsis"));
    }

    #[test]
    fn update_without_notes_still_shows_package() {
        let text = super::status_text(&Ok(Some(package())), None);
        assert!(text.contains("（无更新说明）"));
        assert!(text.contains("安装包：https://example.com/zssh-setup.exe"));
    }

    #[test]
    fn error_keeps_update_error_text() {
        let text = super::status_text(&Err(UpdateError::NoPlatform), None);
        assert!(text.contains("没有适用于当前平台的安装包"));
    }

    #[test]
    fn url_file_name_uses_the_last_segment() {
        assert_eq!(
            download_file_name("https://example.com/a/b/zssh-setup.exe?token=1").unwrap(),
            "zssh-setup.exe"
        );
        assert_eq!(
            download_file_name("file:///C:/tmp/zssh-setup.exe").unwrap(),
            "zssh-setup.exe"
        );
    }

    #[test]
    fn url_without_a_safe_file_name_is_rejected() {
        for url in [
            "",
            "https://example.com/",
            "https://example.com",
            "https://example.com/dir/..",
            "ftp://example.com/zssh-setup.exe",
        ] {
            assert!(download_file_name(url).is_err(), "{url}");
        }
    }

    #[test]
    fn cache_path_rejects_paths_outside_the_cache() {
        let root = PathBuf::from("cache-root");
        assert_eq!(
            cache_path(&root, "zssh-setup.exe").unwrap(),
            root.join("zssh-setup.exe")
        );
        for name in [
            "",
            "..",
            "..\\evil.exe",
            "C:\\evil.exe",
            "sub\\evil.exe",
            "/etc/passwd",
        ] {
            assert!(cache_path(&root, name).is_err(), "{name}");
        }
    }

    #[test]
    fn progress_text_handles_known_and_unknown_totals() {
        assert_eq!(progress_text(10, Some(100)), "已下载 10 / 100");
        assert_eq!(progress_text(10, None), "已下载 10");
        assert_eq!(progress_text(0, Some(0)), "已下载 0");
        assert_eq!(progress_text(10, Some(0)), "已下载 10");
    }

    #[test]
    fn download_moves_from_downloading_to_verifying_to_verified() {
        let mut state = DownloadState::default();
        assert_eq!(state, DownloadState::Idle);

        state.start();
        state.progress(5, Some(10));
        assert_eq!(
            state,
            DownloadState::Downloading {
                received: 5,
                total: Some(10),
            }
        );

        state.begin_verify();
        assert_eq!(state, DownloadState::Verifying);

        state.verified(PathBuf::from("zssh-setup.exe"), 10, "ab".repeat(32));
        assert_eq!(
            state,
            DownloadState::Verified {
                path: PathBuf::from("zssh-setup.exe"),
                size: 10,
                sha256: "ab".repeat(32),
            }
        );
        assert!(!state.can_start());
    }

    #[test]
    fn failed_download_keeps_the_error_and_can_retry() {
        let mut state = DownloadState::default();
        state.start();
        state.fail("下载失败：连接被拒绝".to_owned());
        assert_eq!(
            state,
            DownloadState::Failed("下载失败：连接被拒绝".to_owned())
        );
        assert_eq!(state.text().as_deref(), Some("下载失败：连接被拒绝"));
        assert!(state.can_start());
    }

    #[test]
    fn cancel_marks_the_download_cancelled_and_can_retry() {
        let mut state = DownloadState::default();
        state.start();
        state.cancel();
        assert_eq!(state, DownloadState::Cancelled);
        assert_eq!(state.text().as_deref(), Some("已取消下载"));
        assert!(state.can_start());
    }

    #[test]
    fn file_summary_has_name_and_size() {
        let text = file_summary(Path::new("C:\\cache\\zssh-setup.exe"), 42);
        assert!(text.contains("zssh-setup.exe"));
        assert!(text.contains("42"));
    }

    #[test]
    fn install_plan_is_built_without_running_the_installer() {
        let root =
            std::env::temp_dir().join(format!("zssh-update-demo-{}-install", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时缓存目录");
        let installer = root.join("Z-SSH_0.3.0_x64-setup.exe");
        std::fs::write(&installer, b"fake installer").expect("写安装包");

        let plan = build_install_plan(
            &installer,
            &root,
            "0.3.0",
            true,
            &["--minimized".to_owned()],
            &zssh_update::sha256_hex(b"fake installer"),
        )
        .expect("安装计划应通过");

        let command = zssh_update::command_line(&plan);
        assert!(command.contains("setup.exe"));
        assert!(command.contains("/P"));
        assert!(command.contains("/UPDATE"));
        assert!(command.contains("/R"));
        assert!(command.contains("/ARGS"));
        assert!(command.contains("--minimized"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_needs_a_second_click_before_launching() {
        let plan = sample_plan();
        let mut launches = 0;

        // 第一次点击：只生成计划，不调用启动。
        let (state, launched) = advance_install(
            InstallState::Idle,
            || Ok(plan.clone()),
            |_| {
                launches += 1;
                Ok(())
            },
        );
        assert!(!launched);
        assert_eq!(launches, 0);
        assert!(state.awaiting_confirm());
        assert_eq!(state, InstallState::Confirming(plan.clone()));

        // 第二次点击：才真正把计划交给启动函数。
        let (state, launched) = advance_install(
            state,
            || Err("已经有计划，不该重新规划".to_owned()),
            |launched_plan| {
                assert_eq!(launched_plan, &plan);
                launches += 1;
                Ok(())
            },
        );
        assert!(launched);
        assert_eq!(launches, 1);
        assert_eq!(state, InstallState::Launched);
    }

    #[test]
    fn plan_failure_keeps_the_error_and_does_not_arm_confirmation() {
        let (state, launched) = advance_install(
            InstallState::Idle,
            || Err("安装包不存在".to_owned()),
            |_| panic!("计划失败时不能启动安装器"),
        );
        assert!(!launched);
        assert!(!state.awaiting_confirm());
        assert_eq!(
            state,
            InstallState::Failed("准备安装失败：安装包不存在".to_owned())
        );
    }

    #[test]
    fn launch_failure_reports_the_code_and_does_not_exit() {
        let (state, launched) = advance_install(
            InstallState::Confirming(sample_plan()),
            || Err("已经有计划，不该重新规划".to_owned()),
            |_| Err("ShellExecuteW 返回错误码 5".to_owned()),
        );
        assert!(!launched);
        assert_eq!(
            state,
            InstallState::Failed("启动安装器失败：ShellExecuteW 返回错误码 5".to_owned())
        );
    }

    #[test]
    fn quoting_handles_spaces_quotes_and_trailing_backslashes() {
        assert_eq!(push_quoted("--minimized"), "--minimized");
        assert_eq!(push_quoted(""), "\"\"");
        assert_eq!(
            push_quoted("C:\\Program Files\\Z-SSH"),
            "\"C:\\Program Files\\Z-SSH\""
        );
        assert_eq!(push_quoted("a\"b"), "\"a\\\"b\"");
        // 引号包住的结尾反斜杠要加倍，否则会被当成转义结束引号。
        assert_eq!(
            push_quoted("C:\\Program Files\\"),
            "\"C:\\Program Files\\\\\""
        );
        assert_eq!(
            quote_args(&[
                "/ARGS".to_owned(),
                "a b".to_owned(),
                "say \"hi\"".to_owned(),
            ]),
            "/ARGS \"a b\" \"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn queued_install_is_taken_once() {
        let _guard = queue_lock();
        // 上一个用例若中途失败，队列里可能还留着计划，先清掉。
        let _ = take_pending_install();

        queue_install(sample_plan());
        assert_eq!(take_pending_install(), Some(sample_plan()));
        assert_eq!(take_pending_install(), None);
    }

    #[test]
    fn queueing_again_replaces_the_pending_plan() {
        let _guard = queue_lock();
        let _ = take_pending_install();

        queue_install(sample_plan());
        let mut replacement = sample_plan();
        replacement.version = "0.4.0".to_owned();
        replacement.restart = true;
        queue_install(replacement.clone());

        assert_eq!(take_pending_install(), Some(replacement));
        assert_eq!(take_pending_install(), None);
    }

    #[test]
    fn running_with_an_empty_queue_does_not_launch_anything() {
        let _guard = queue_lock();
        let _ = take_pending_install();

        // 队列为空时 `run_pending_install` 连 `ShellExecuteW` 都不会碰。
        super::run_pending_install();

        assert_eq!(take_pending_install(), None);
    }

    // ---- 协议、大小、超时、重定向 ----

    use super::{
        DownloadFailure, MAX_REDIRECTS, ManifestSource, PUBKEY_UNAVAILABLE, SIGNING_UNCONFIGURED,
        allow_insecure_sources, append_capped, check_update_url, evaluate_manifest, fetch_bytes,
        launch_if_unchanged, redirect_allowed, resolve_current_version, resolve_manifest_sources,
        resolve_pubkey, update_client_builder, verify_and_store,
    };
    use std::io::{Read as _, Write as _};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试用 runtime")
            .block_on(future)
    }

    /// 不带系统代理的客户端：测试只连本机，不能被开发机上的系统代理截走。
    fn local_client(allow_insecure: bool) -> reqwest::Client {
        update_client_builder(allow_insecure)
            .no_proxy()
            .build()
            .expect("测试客户端")
    }

    /// 一次性本地 HTTP 服务：读完请求头后写出 `response`，`hold` 为真时再占着连接不放。
    fn serve_once(
        response: Vec<u8>,
        hold: Option<Duration>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("端口").port();
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut seen = Vec::new();
            let mut buffer = [0u8; 1024];
            while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => seen.extend_from_slice(&buffer[..read]),
                }
            }
            // 客户端中途断开时写会失败，这正是"超限中止"想要的结果，不算错误。
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            if let Some(hold) = hold {
                std::thread::sleep(hold);
            }
        });
        (format!("http://127.0.0.1:{port}/pkg.exe"), handle)
    }

    fn fetch(
        client: &reqwest::Client,
        url: &str,
        max_bytes: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, DownloadFailure> {
        let url = reqwest::Url::parse(url).expect("测试地址");
        let never = AtomicBool::new(false);
        block_on(fetch_bytes(
            client,
            &url,
            max_bytes,
            timeout,
            &never,
            |_, _, _| {},
        ))
    }

    fn failure_text(result: Result<Vec<u8>, DownloadFailure>) -> String {
        match result {
            Ok(bytes) => panic!("本该失败，却读到了 {} 字节", bytes.len()),
            Err(DownloadFailure::Cancelled) => "cancelled".to_owned(),
            Err(DownloadFailure::Failed(message)) => message,
        }
    }

    #[test]
    fn release_policy_rejects_http_and_file_sources() {
        for url in [
            "http://example.com/latest.json",
            "file:///C:/tmp/latest.json",
            "ftp://example.com/a.exe",
        ] {
            assert!(check_update_url(url, false).is_err(), "{url}");
        }
        assert!(check_update_url("https://example.com/latest.json", false).is_ok());
    }

    #[test]
    fn debug_policy_allows_local_sources_but_never_other_hosts_files() {
        assert!(check_update_url("http://127.0.0.1:1/a.exe", true).is_ok());
        assert!(check_update_url("file:///C:/tmp/a.exe", true).is_ok());
        // UNC 形式的 file 地址会触发 SMB 认证，调试版也不放。
        assert!(check_update_url("file://other-host/share/a.exe", true).is_err());
        assert!(check_update_url("ftp://example.com/a.exe", true).is_err());
    }

    /// release 构建里生效的就是 `false` 这一支（`cargo test --release` 时才会编进来）。
    #[cfg(not(debug_assertions))]
    #[test]
    fn this_release_build_rejects_http_and_file_sources() {
        assert!(!allow_insecure_sources());
        assert!(check_update_url("http://example.com/a.exe", allow_insecure_sources()).is_err());
        assert!(check_update_url("file:///C:/a.exe", allow_insecure_sources()).is_err());
    }

    #[test]
    fn insecure_sources_follow_the_build_kind() {
        assert_eq!(allow_insecure_sources(), cfg!(debug_assertions));
    }

    #[test]
    fn redirects_must_stay_on_https_and_be_limited() {
        assert!(redirect_allowed("https", 1, false).is_ok());
        assert!(redirect_allowed("http", 1, false).is_err());
        assert!(redirect_allowed("file", 1, false).is_err());
        assert!(redirect_allowed("http", 1, true).is_ok());
        assert!(redirect_allowed("https", MAX_REDIRECTS, false).is_ok());
        assert!(redirect_allowed("https", MAX_REDIRECTS + 1, false).is_err());
    }

    #[test]
    fn append_capped_stops_at_the_limit() {
        let mut buffer = Vec::new();
        assert!(append_capped(&mut buffer, b"abcd", 6).is_ok());
        assert!(append_capped(&mut buffer, b"ef", 6).is_ok());
        let err = append_capped(&mut buffer, b"g", 6).unwrap_err();
        assert!(err.contains("大小上限"), "{err}");
        assert_eq!(buffer, b"abcdef");
    }

    #[test]
    fn oversized_local_file_is_refused_before_reading_it() {
        let dir = std::env::temp_dir().join(format!("zssh-f2-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.exe");
        std::fs::write(&file, vec![7u8; 4096]).unwrap();
        let url = reqwest::Url::from_file_path(&file).expect("file 地址");
        let never = AtomicBool::new(false);
        let result = block_on(fetch_bytes(
            &local_client(true),
            &url,
            1024,
            Duration::from_secs(5),
            &never,
            |_, _, _| {},
        ));
        assert!(failure_text(result).contains("大小上限"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_download_within_the_limit_succeeds() {
        let body = b"installer bytes".to_vec();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let (url, server) = serve_once(response, None);
        let got = fetch(&local_client(true), &url, 1024, Duration::from_secs(5));
        assert_eq!(got.ok(), Some(body));
        let _ = server.join();
    }

    #[test]
    fn http_download_aborts_when_content_length_exceeds_the_limit() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999\r\nConnection: close\r\n\r\nxx".to_vec();
        let (url, server) = serve_once(response, None);
        let text = failure_text(fetch(
            &local_client(true),
            &url,
            1024,
            Duration::from_secs(5),
        ));
        assert!(text.contains("大小上限"), "{text}");
        let _ = server.join();
    }

    #[test]
    fn http_download_aborts_when_a_stream_without_length_keeps_going() {
        // 没有 Content-Length 的响应：只能靠累计字节数拦住。
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend(std::iter::repeat_n(b'x', 64 * 1024));
        let (url, server) = serve_once(response, None);
        let text = failure_text(fetch(
            &local_client(true),
            &url,
            1000,
            Duration::from_secs(5),
        ));
        assert!(text.contains("大小上限"), "{text}");
        let _ = server.join();
    }

    #[test]
    fn http_download_aborts_after_the_total_time_limit() {
        // 头发出去了，响应体一直不来：总时长上限必须能打断它。
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial".to_vec();
        let (url, server) = serve_once(response, Some(Duration::from_secs(3)));
        let started = std::time::Instant::now();
        let text = failure_text(fetch(
            &local_client(true),
            &url,
            1024,
            Duration::from_millis(300),
        ));
        assert!(started.elapsed() < Duration::from_secs(3), "{text}");
        assert!(text.contains("下载失败"), "{text}");
        let _ = server.join();
    }

    #[test]
    fn redirect_to_a_non_https_address_is_refused_by_the_client() {
        let (url, server) = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/other.exe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            None,
        );
        // 发行版配置（allow_insecure = false）：入口是调试用的 http，但跳转目标同样是 http，必须拒绝。
        let text = failure_text(fetch(
            &local_client(false),
            &url,
            1024,
            Duration::from_secs(5),
        ));
        assert!(text.contains("重定向"), "{text}");
        let _ = server.join();
    }

    // ---- 验签落盘、复验 ----

    fn keypair() -> (minisign::KeyPair, String) {
        use base64::Engine as _;
        let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("生成测试密钥");
        let text = kp.pk.to_box().expect("公钥文本").to_string();
        (kp, base64::engine::general_purpose::STANDARD.encode(text))
    }

    fn sign(kp: &minisign::KeyPair, data: &[u8], version: &str) -> String {
        use base64::Engine as _;
        let signed = minisign::sign(
            Some(&kp.pk),
            &kp.sk,
            data,
            Some(&format!("version:{version}")),
            Some("zssh test"),
        )
        .expect("签名");
        base64::engine::general_purpose::STANDARD.encode(signed.to_string())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-f2-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn signed_package(kp: &minisign::KeyPair, bytes: &[u8]) -> Package {
        Package {
            platform_key: "windows-x86_64-nsis".to_owned(),
            version: "0.3.0".to_owned(),
            url: "https://example.com/zssh-setup.exe".to_owned(),
            signature: sign(kp, bytes, "0.3.0"),
        }
    }

    #[test]
    fn verified_bytes_are_written_to_a_fresh_hash_named_file() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("store");
        let bytes = b"fake nsis installer bytes";
        let package = signed_package(&kp, bytes);

        let (path, size, sha256) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").expect("应通过");
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(sha256, zssh_update::sha256_hex(bytes));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(path.starts_with(cache.join(&sha256[..16])));

        // 同一个包再下一次：旧文件被替换成同样的内容，不会因为"已存在"失败。
        let again =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").expect("重下");
        assert_eq!(again.0, path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_file_planted_at_the_target_is_replaced_not_trusted() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("planted");
        let bytes = b"real installer";
        let package = signed_package(&kp, bytes);
        let dir = cache.join(&zssh_update::sha256_hex(bytes)[..16]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zssh-setup.exe"), b"planted").unwrap();

        let (path, _, _) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").expect("应通过");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn unverified_bytes_never_reach_the_cache() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("tampered");
        let package = signed_package(&kp, b"original");

        let err =
            verify_and_store(b"tampered", &package, &pubkey, &cache, "zssh-setup.exe").unwrap_err();
        assert!(err.contains("签名"), "{err}");
        // 缓存里一个文件、一个子目录都不该出现。
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);

        let (_, other_pubkey) = keypair();
        assert!(
            verify_and_store(
                b"original",
                &package,
                &other_pubkey,
                &cache,
                "zssh-setup.exe"
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn launch_is_refused_when_the_file_changed_after_verification() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("swap");
        let bytes = b"real installer";
        let package = signed_package(&kp, bytes);
        let (path, _, sha256) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").unwrap();
        let plan = build_install_plan(&path, &cache, "0.3.0", false, &[], &sha256).expect("计划");

        // 没动过：放行（注入假的启动函数，不会真的起进程）。
        let mut launched = 0;
        launch_if_unchanged(&plan, |_| {
            launched += 1;
            Ok(())
        })
        .expect("未改动应放行");
        assert_eq!(launched, 1);

        // 验签之后被换掉：拒绝，启动函数一次都不能被调用。
        std::fs::write(&path, b"malicious replacement").unwrap();
        let err = launch_if_unchanged(&plan, |_| panic!("被改过的安装包不能启动")).unwrap_err();
        assert!(err.contains("不一致"), "{err}");

        // 文件没了：同样拒绝。
        std::fs::remove_file(&path).unwrap();
        assert!(launch_if_unchanged(&plan, |_| panic!("不能启动")).is_err());
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn queued_install_is_rechecked_when_the_app_quits() {
        let _guard = queue_lock();
        let _ = take_pending_install();
        let (kp, pubkey) = keypair();
        let cache = temp_dir("queued-swap");
        let bytes = b"real installer";
        let package = signed_package(&kp, bytes);
        let (path, _, sha256) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").unwrap();
        let plan = build_install_plan(&path, &cache, "0.3.0", false, &[], &sha256).expect("计划");
        queue_install(plan);

        // 排队之后文件被换掉：退出回调里的复验必须拦下，不能走到 ShellExecuteW。
        std::fs::write(&path, b"swapped while waiting for quit").unwrap();
        super::run_pending_install();
        assert_eq!(take_pending_install(), None, "队列应已被取走");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ---- 版本、公钥、更新源 ----

    /// 读仓库根的文件（相对路径）。独立导出的原生版仓库里没有 `src-tauri/`，此时返回 `None`，
    /// 对应的比对跳过；读失败（不是"不存在"）照样 panic，不当成"没有"。
    fn repo_root_file(relative: &str) -> Option<String> {
        let path: std::path::PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", relative]
            .iter()
            .collect();
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => panic!("读 {} 失败：{err}", path.display()),
        }
    }

    fn workspace_version(manifest: &str) -> String {
        let section = manifest
            .split("[workspace.package]")
            .nth(1)
            .expect("Cargo.toml 应有 [workspace.package]");
        section
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("version")
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .map(|value| value.trim().trim_matches('"').to_owned())
            })
            .expect("workspace 应有版本号")
    }

    #[test]
    fn compiled_version_matches_the_stable_release_line() {
        assert_ne!(env!("CARGO_PKG_VERSION"), "0.0.0");
        // 版本号以 native 工作区为准；根 Cargo.toml、Tauri 版配置存在时才比对（文件不存在就跳过）。
        match repo_root_file("Cargo.toml") {
            Some(root) => assert_eq!(env!("CARGO_PKG_VERSION"), workspace_version(&root)),
            None => println!("跳过与根 Cargo.toml 的版本比对：文件不存在"),
        }
        match repo_root_file("src-tauri/tauri.conf.json") {
            Some(text) => {
                let conf: serde_json::Value =
                    serde_json::from_str(&text).expect("读取 tauri.conf.json");
                assert_eq!(Some(env!("CARGO_PKG_VERSION")), conf["version"].as_str());
            }
            None => println!(
                "跳过与 tauri.conf.json 的版本比对：没有 src-tauri/（独立导出的原生版仓库）"
            ),
        }
    }

    #[test]
    fn current_version_defaults_to_the_compiled_version() {
        assert_eq!(resolve_current_version(None), env!("CARGO_PKG_VERSION"));
        assert_eq!(
            resolve_current_version(Some("  ".to_owned())),
            env!("CARGO_PKG_VERSION")
        );
        // 钩子在调试构建里才会被 `dev_hooks` 放进来；这里只验证"给了值就用它"。
        assert_eq!(resolve_current_version(Some(" 0.1.0 ".to_owned())), "0.1.0");
    }

    #[test]
    fn the_public_key_comes_from_the_hook_or_the_config_and_a_placeholder_is_refused() {
        let configured = |pubkey: &str| zssh_update::UpdaterConfig {
            pubkey: pubkey.to_owned(),
            endpoints: vec!["https://example.com/latest.json".to_owned()],
        };
        assert_eq!(
            resolve_pubkey(None, Ok(configured("real-key"))).unwrap(),
            "real-key"
        );
        assert_eq!(
            resolve_pubkey(Some(" ".to_owned()), Ok(configured("real-key"))).unwrap(),
            "real-key"
        );
        assert_eq!(
            resolve_pubkey(Some(" custom ".to_owned()), Ok(configured("real-key"))).unwrap(),
            "custom"
        );
        let err = resolve_pubkey(None, Err("坏了".to_owned())).unwrap_err();
        assert!(err.starts_with(PUBKEY_UNAVAILABLE), "{err}");

        // 占位公钥：无论来自配置还是钩子，都明说"更新签名未配置"，不当成一把能用的钥匙。
        assert_eq!(
            resolve_pubkey(None, Ok(configured("PLACEHOLDER-not-set"))).unwrap_err(),
            SIGNING_UNCONFIGURED
        );
        assert_eq!(
            resolve_pubkey(Some("placeholder".to_owned()), Ok(configured("real-key"))).unwrap_err(),
            SIGNING_UNCONFIGURED
        );
        // 钩子给了真公钥时盖过配置里的占位值（调试构建里测更新流程靠它）。
        assert_eq!(
            resolve_pubkey(Some("test-key".to_owned()), Ok(configured("PLACEHOLDER"))).unwrap(),
            "test-key"
        );
    }

    #[test]
    fn the_embedded_config_has_a_real_key_or_refuses_every_update() {
        // 仓库里的 updater.json 在"占位"和"已填真公钥"两种状态下都应自洽：
        // 占位 -> 拿不到公钥（检查 / 下载 / 安装都会被挡住）；已填 -> 就是它。
        let config = zssh_update::embedded_config().expect("内嵌配置应能解析");
        match resolve_pubkey(None, zssh_update::embedded_config()) {
            Ok(key) => {
                assert!(config.signing_configured());
                assert_eq!(key, config.pubkey);
            }
            Err(message) => {
                assert!(!config.signing_configured());
                assert_eq!(message, SIGNING_UNCONFIGURED);
            }
        }
    }

    #[test]
    fn manifest_sources_come_from_the_embedded_endpoints_by_default() {
        let embedded = zssh_update::embedded_config().expect("内嵌配置");
        let sources = resolve_manifest_sources(None, zssh_update::embedded_config());
        assert_eq!(
            sources,
            embedded
                .endpoints
                .iter()
                .cloned()
                .map(ManifestSource::Url)
                .collect::<Vec<_>>()
        );
        assert!(resolve_manifest_sources(None, Err("坏了".to_owned())).is_empty());
        // 钩子给了本地路径 / URL：只用钩子，不再混入内嵌地址。
        assert_eq!(
            resolve_manifest_sources(
                Some("C:\\tmp\\latest.json".into()),
                zssh_update::embedded_config()
            ),
            vec![ManifestSource::File(PathBuf::from("C:\\tmp\\latest.json"))]
        );
        assert_eq!(
            resolve_manifest_sources(
                Some("http://127.0.0.1:1/latest.json".into()),
                zssh_update::embedded_config()
            ),
            vec![ManifestSource::Url(
                "http://127.0.0.1:1/latest.json".to_owned()
            )]
        );
        assert_eq!(
            resolve_manifest_sources(Some("".into()), zssh_update::embedded_config()).len(),
            embedded.endpoints.len()
        );
    }

    #[test]
    fn manifest_pointing_at_a_non_https_package_is_refused_in_release_mode() {
        let manifest = r#"{
            "version": "0.3.0",
            "platforms": { "windows-x86_64-nsis": { "url": "http://example.com/a.exe", "signature": "sig" } }
        }"#;
        let (text, package) = evaluate_manifest(manifest, "0.2.14", false);
        assert!(package.is_none());
        assert!(text.contains("https"), "{text}");
        // 调试配置放行同一份清单。
        let (_, package) = evaluate_manifest(manifest, "0.2.14", true);
        assert!(package.is_some());
    }
}

#[cfg(test)]
mod e2e_tests;
