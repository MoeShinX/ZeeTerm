//! 调试用的程序内驱动：按 `ZSSH_MAIN_SCRIPT` 里写的步骤在主窗口里依次执行，结果写到
//! `ZSSH_MAIN_LOG` 指定的文件。只在 debug 构建里生效（见 `dev_hooks`），自动化验收和截图
//! 用它代替手点桌面。
//!
//! 步骤用分号隔开，`名字` 或 `名字:参数`：
//! - `open:<主机id>` 像在主机树里双击那样连一台主机；`new_tab` 开空白标签；
//! - `wait_live` 等活动窗格连上；`wait_disconnected` 等活动标签显示已断开；
//! - `type:<文字>` 往活动窗格打字（`\r` 是回车）；`expect:<文字>` 等屏幕上出现这段文字；
//!   `expect_count:<n>:<文字>` 等活动标签（含分屏）的屏幕上至少有 n 行带这段文字；
//! - `split[:x|y]`（Ctrl+Shift+D / E）、`duplicate:<下标>`、`close:<下标>`、`select:<下标>`、`cycle`、`reconnect:<下标>`；
//! - `view:welcome|vault|terminal|settings`、`settings`（齿轮开关）；
//! - `key:<按键>` 走真实的按键分发路径（如 `key:ctrl-shift-t`、`key:escape`）；
//!   `click:<x>,<y>[,<次数>[,l|m|r]]` 走真实的鼠标分发路径（先移动再按下抬起，坐标是窗口内逻辑像素）；
//! - `assert_view:<页>`、`assert_tabs:<个数>`、`assert_status:<标签下标>:<connected|disconnected|connecting|empty>`
//!   检查结果，不对就让脚本失败；
//! - `tree_width:<像素>`、`toggle_tree`、`toggle_group:<键>`、`search:<文字>`、`theme:dark|light`
//!   走和界面操作同一批方法；`layout` 把布局状态写进日志；
//! - `down:<x>,<y>` / `move:<x>,<y>` / `up:<x>,<y>` 分开的鼠标按下 / 移动 / 抬起（拖放用，中间可以 `sleep`
//!   让界面画一帧、`shot` 截图）；
//! - `act:<动作>[:参数…]` 调主机页 / 面板 / 对话框上的方法并检查结果，见 `dev_acts`；
//! - `dump` 把标签列表（标题、状态）写进日志；`sleep:<毫秒>`；
//! - `shot:<名字>` 先做隐私检查（禁用片段取自 `ZSSH_SHOT_FORBID`，分号隔开），通过后写
//!   `<ZSSH_SHOT_DIR>/<名字>.ready`，等外面截完图写出 `<名字>.done` 再继续；
//! - `quit` 结束进程。
//!
//! 日志里只写步骤名、结果和标签标题 / 状态，不写屏幕内容，也不写任何禁用片段的原文。

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ::gpui::{AnyWindowHandle, App, Entity, Window};
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use crate::split_tree::Axis;
use crate::terminal_workspace::TabStatus;

/// 一个步骤。
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Open(i64),
    NewTab,
    WaitLive,
    WaitDisconnected,
    Type(String),
    Expect(String),
    ExpectCount(usize, String),
    /// 拆分聚焦的窗格：`split` / `split:x` 左右，`split:y` 上下。
    Split(Axis),
    Duplicate(usize),
    Close(usize),
    Select(usize),
    Cycle,
    Reconnect(usize),
    View(MainView),
    Settings,
    Dump,
    Layout,
    OpenHosts,
    Key(String),
    Click {
        x: f32,
        y: f32,
        count: usize,
        button: char,
        /// 按着 Ctrl / Shift 点击（多选）。
        ctrl: bool,
        shift: bool,
    },
    /// `click_text:<文字>[|次数[|l|m|r[|ctrl+shift]]]`：点聚焦窗格里第一次出现这段文字的中间（文字还没出现就等）。
    /// 走和 `click` 同一条鼠标分发路径，所以右键它就是真实的右键。
    ClickText {
        text: String,
        count: usize,
        button: char,
        ctrl: bool,
        shift: bool,
    },
    /// 资源管理器拖文件进窗口：走 gpui 的 `FileDrop`（进入 → 移动 → 松手）。多个路径用 `|` 隔开。
    FileDrop {
        x: f32,
        y: f32,
        paths: Vec<String>,
        /// 拖进来之后又拖走（Esc / 拖出窗口）：不该有任何东西被传。
        cancel: bool,
    },
    /// 鼠标按下（`d`）/ 移动（`m`，按下过就带着左键）/ 抬起（`u`）。
    Mouse {
        kind: char,
        x: f32,
        y: f32,
    },
    /// `files:<步骤>`：把按键 / 点击 / 鼠标 / 文件拖放发给独立文件窗口，而不是主窗口。
    Files(Box<Step>),
    /// `termwin:<步骤>`：把按键 / 点击 / 鼠标发给第一个独立终端窗口（V2-U1，拖标签弹出的窗口）。
    TermWin(Box<Step>),
    /// 主机页 / 面板 / 对话框上的动作，见 `dev_acts`。
    Act(String),
    AssertView(MainView),
    AssertTabs(usize),
    AssertStatus(usize, TabStatus),
    TreeWidth(f32),
    ToggleTree,
    ToggleGroup(String),
    Search(String),
    Theme(bool),
    Sleep(u64),
    Shot(String),
    Quit,
}

/// 解析脚本；任何一步写错都整体拒绝，免得跑到一半才发现。
pub fn parse_script(raw: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for part in raw.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(inner) = part.strip_prefix("termwin:") {
            let mut parsed = parse_script(inner)?;
            let step = match (parsed.pop(), parsed.is_empty()) {
                (Some(step @ (Step::Key(_) | Step::Click { .. } | Step::Mouse { .. })), true) => {
                    step
                }
                _ => {
                    return Err("termwin: 后面只能跟 key / click / down / move / up".to_owned());
                }
            };
            steps.push(Step::TermWin(Box::new(step)));
            continue;
        }
        if let Some(inner) = part.strip_prefix("files:") {
            let mut parsed = parse_script(inner)?;
            let step = match (parsed.pop(), parsed.is_empty()) {
                (
                    Some(
                        step @ (Step::Key(_)
                        | Step::Click { .. }
                        | Step::Mouse { .. }
                        | Step::FileDrop { .. }),
                    ),
                    true,
                ) => step,
                _ => {
                    return Err(
                        "files: 后面只能跟 key / click / down / move / up / filedrop".to_owned(),
                    );
                }
            };
            steps.push(Step::Files(Box::new(step)));
            continue;
        }
        let (name, arg) = match part.split_once(':') {
            Some((name, arg)) => (name.trim(), Some(arg)),
            None => (part, None),
        };
        let need = |what: &str| arg.ok_or_else(|| format!("步骤 {name} 需要{what}"));
        let number = |what: &str| -> Result<usize, String> {
            need(what)?
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("步骤 {name} 的{what}不是数字"))
        };
        let step = match name {
            "open" => Step::Open(
                need("主机 id")?
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "步骤 open 的主机 id 不是数字".to_owned())?,
            ),
            "new_tab" => Step::NewTab,
            "wait_live" => Step::WaitLive,
            "wait_disconnected" => Step::WaitDisconnected,
            "type" => Step::Type(unescape(need("要打的文字")?)),
            "expect" => Step::Expect(need("要等的文字")?.to_owned()),
            "expect_count" => {
                let raw = need("个数和文字")?;
                let (count, text) = raw
                    .split_once(':')
                    .ok_or_else(|| "步骤 expect_count 的格式是 个数:文字".to_owned())?;
                let count = count
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "步骤 expect_count 的个数不是数字".to_owned())?;
                Step::ExpectCount(count, text.to_owned())
            }
            "split" => Step::Split(match arg.map(str::trim) {
                None | Some("") | Some("x") => Axis::X,
                Some("y") => Axis::Y,
                Some(_) => return Err("步骤 split 的方向只能是 x 或 y".to_owned()),
            }),
            "duplicate" => Step::Duplicate(number("标签下标")?),
            "close" => Step::Close(number("标签下标")?),
            "select" => Step::Select(number("标签下标")?),
            "cycle" => Step::Cycle,
            "reconnect" => Step::Reconnect(number("标签下标")?),
            "view" => Step::View(MainView::parse(need("页面名")?.trim()).ok_or_else(|| {
                "步骤 view 的页面名只能是 welcome / vault / terminal / settings".to_owned()
            })?),
            "settings" => Step::Settings,
            "dump" => Step::Dump,
            "layout" => Step::Layout,
            "open_hosts" => Step::OpenHosts,
            "key" => Step::Key(need("按键")?.trim().to_owned()),
            "click" => {
                let raw = need("坐标")?;
                let mut parts = raw.split(',').map(str::trim);
                let coord = |value: Option<&str>| -> Result<f32, String> {
                    value
                        .and_then(|v| v.parse::<f32>().ok())
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| "步骤 click 的坐标不是数字".to_owned())
                };
                let (x, y) = (coord(parts.next())?, coord(parts.next())?);
                let count = match parts.next() {
                    Some(raw) => raw
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=3).contains(n))
                        .ok_or_else(|| "步骤 click 的次数只能是 1 到 3".to_owned())?,
                    None => 1,
                };
                let button = match parts.next() {
                    None | Some("l") => 'l',
                    Some("m") => 'm',
                    Some("r") => 'r',
                    Some(_) => return Err("步骤 click 的按键只能是 l / m / r".to_owned()),
                };
                let (mut ctrl, mut shift) = (false, false);
                if let Some(raw) = parts.next() {
                    for part in raw.split('+') {
                        match part.trim() {
                            "ctrl" => ctrl = true,
                            "shift" => shift = true,
                            _ => return Err("步骤 click 的修饰键只能是 ctrl / shift".to_owned()),
                        }
                    }
                }
                Step::Click {
                    x,
                    y,
                    count,
                    button,
                    ctrl,
                    shift,
                }
            }
            "click_text" => {
                let raw = unescape(need("文字")?);
                let mut parts = raw.split('|');
                let text = parts.next().unwrap_or("").to_owned();
                if text.is_empty() {
                    return Err("步骤 click_text 需要文字".to_owned());
                }
                let count = match parts.next() {
                    Some(raw) => raw
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=3).contains(n))
                        .ok_or_else(|| "步骤 click_text 的次数只能是 1 到 3".to_owned())?,
                    None => 1,
                };
                let button = match parts.next().map(str::trim) {
                    None | Some("l") => 'l',
                    Some("m") => 'm',
                    Some("r") => 'r',
                    Some(_) => return Err("步骤 click_text 的按键只能是 l / m / r".to_owned()),
                };
                let (mut ctrl, mut shift) = (false, false);
                if let Some(raw) = parts.next() {
                    for part in raw.split('+') {
                        match part.trim() {
                            "ctrl" => ctrl = true,
                            "shift" => shift = true,
                            _ => {
                                return Err(
                                    "步骤 click_text 的修饰键只能是 ctrl / shift".to_owned()
                                );
                            }
                        }
                    }
                }
                Step::ClickText {
                    text,
                    count,
                    button,
                    ctrl,
                    shift,
                }
            }
            "filedrop" | "filedrop_cancel" => {
                let raw = need("坐标和路径")?;
                let mut parts = raw.splitn(3, ',');
                let mut coord = || -> Result<f32, String> {
                    parts
                        .next()
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| "步骤 filedrop 的坐标不是数字".to_owned())
                };
                let (x, y) = (coord()?, coord()?);
                let paths: Vec<String> = parts
                    .next()
                    .unwrap_or_default()
                    .split('|')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect();
                if paths.is_empty() {
                    return Err("步骤 filedrop 需要至少一个路径".to_owned());
                }
                Step::FileDrop {
                    x,
                    y,
                    paths,
                    cancel: name == "filedrop_cancel",
                }
            }
            "down" | "move" | "up" => {
                let raw = need("坐标")?;
                let mut parts = raw.split(',').map(str::trim);
                let coord = |value: Option<&str>| -> Result<f32, String> {
                    value
                        .and_then(|v| v.parse::<f32>().ok())
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| format!("步骤 {name} 的坐标不是数字"))
                };
                let (x, y) = (coord(parts.next())?, coord(parts.next())?);
                let kind = match name {
                    "down" => 'd',
                    "move" => 'm',
                    _ => 'u',
                };
                Step::Mouse { kind, x, y }
            }
            "act" => {
                let spec = need("动作")?.to_owned();
                super::dev_acts::validate(&spec)?;
                Step::Act(spec)
            }
            "assert_view" => {
                Step::AssertView(MainView::parse(need("页面名")?.trim()).ok_or_else(|| {
                    "步骤 assert_view 的页面名只能是 welcome / vault / terminal / settings"
                        .to_owned()
                })?)
            }
            "assert_tabs" => Step::AssertTabs(number("标签个数")?),
            "assert_status" => {
                let raw = need("下标和状态")?;
                let (index, status) = raw
                    .split_once(':')
                    .ok_or_else(|| "步骤 assert_status 的格式是 下标:状态".to_owned())?;
                let index = index
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "步骤 assert_status 的下标不是数字".to_owned())?;
                let status = match status.trim() {
                    "connected" => TabStatus::Connected,
                    "disconnected" => TabStatus::Disconnected,
                    "connecting" => TabStatus::Connecting,
                    "empty" => TabStatus::Empty,
                    _ => return Err("步骤 assert_status 的状态不认识".to_owned()),
                };
                Step::AssertStatus(index, status)
            }
            "tree_width" => Step::TreeWidth(
                need("宽度")?
                    .trim()
                    .parse::<f32>()
                    .map_err(|_| "步骤 tree_width 的宽度不是数字".to_owned())?,
            ),
            "toggle_tree" => Step::ToggleTree,
            "toggle_group" => Step::ToggleGroup(need("分组键")?.trim().to_owned()),
            "search" => Step::Search(need("搜索文字")?.to_owned()),
            "theme" => match need("dark 或 light")?.trim() {
                "dark" => Step::Theme(true),
                "light" => Step::Theme(false),
                _ => return Err("步骤 theme 只能是 dark 或 light".to_owned()),
            },
            "sleep" => Step::Sleep(number("毫秒数")? as u64),
            "shot" => {
                let label = need("截图名")?.trim().to_owned();
                if label.is_empty()
                    || !label
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Err("步骤 shot 的名字只能用字母数字和 - _".to_owned());
                }
                Step::Shot(label)
            }
            "quit" => Step::Quit,
            other => return Err(format!("不认识的步骤：{other}")),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// `\r`、`\n`、`\t`、`\e`（ESC）、`\s`（空格）、`\\` 转义；别的反斜杠原样保留。
pub(super) fn unescape(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            // ESC：往终端打转义序列（比如 `\e[?1002h` 打开鼠标上报）用。
            Some('e') => out.push('\x1b'),
            // 步骤两头的空格会被去掉：结尾要留空格时写成 `\s`。
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub(super) enum Outcome {
    Done,
    /// 条件还没成立，稍后再看。
    Pending,
    Failed(String),
}

/// 单步等待的上限。
const STEP_TIMEOUT: Duration = Duration::from_secs(40);
/// 等外面截图的上限。
const SHOT_TIMEOUT: Duration = Duration::from_secs(180);

pub(super) fn log_line(path: &Option<PathBuf>, line: &str) {
    if let Some(path) = path
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// 读环境变量并启动驱动；没设 `ZSSH_MAIN_SCRIPT`（或 release 构建）什么都不做。
pub fn spawn_from_env(view: Entity<MainWindow>, handle: AnyWindowHandle, cx: &mut App) {
    let Some(raw) = crate::dev_hooks::var("ZSSH_MAIN_SCRIPT") else {
        return;
    };
    let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(PathBuf::from);
    let shot_dir = crate::dev_hooks::var("ZSSH_SHOT_DIR").map(PathBuf::from);
    let forbidden: Vec<String> = crate::dev_hooks::var("ZSSH_SHOT_FORBID")
        .map(|raw| raw.split(';').map(str::to_owned).collect())
        .unwrap_or_default();
    let steps = match parse_script(&raw) {
        Ok(steps) => steps,
        Err(err) => {
            log_line(&log, &format!("脚本解析失败：{err}"));
            return;
        }
    };
    cx.spawn(async move |cx| {
        // 等窗口画出第一帧、主机库读完。
        cx.background_executor()
            .timer(Duration::from_millis(800))
            .await;
        for (number, step) in steps.iter().enumerate() {
            if let Step::Sleep(ms) = step {
                cx.background_executor()
                    .timer(Duration::from_millis(*ms))
                    .await;
                log_line(&log, &format!("[{number}] {step:?} ok"));
                continue;
            }
            let started = Instant::now();
            let limit = if matches!(step, Step::Shot(_)) {
                SHOT_TIMEOUT
            } else {
                STEP_TIMEOUT
            };
            let mut shot_ready = false;
            let result = loop {
                // 发给文件窗口的按键 / 点击：文件窗口的操作（比如右键菜单「移回主窗口」）会去更新主窗口，
                // 所以不能包在主窗口自己的 `update` 里（真实操作时主窗口也没有被借用）。
                if let Step::TermWin(inner) = step {
                    match cx.update(|cx| to_term_window(inner, cx)) {
                        Outcome::Failed(reason) => break Err(reason),
                        _ => break Ok(()),
                    }
                }
                if let Step::Files(inner) = step {
                    match cx.update(|cx| to_file_window(inner, cx)) {
                        Outcome::Failed(reason) => break Err(reason),
                        _ => break Ok(()),
                    }
                }
                // 文件窗口把标签交回主窗口：同上。
                if let Step::Act(spec) = step
                    && spec == "files_back"
                {
                    let outcome = cx.update(|cx| match crate::file_window::window_handle(cx) {
                        Some(files) => {
                            let moved = files.update(cx, |files, window, cx| {
                                if let Some(id) = files.active_id() {
                                    files.move_to_main(id, window, cx);
                                }
                            });
                            match moved {
                                Ok(()) => Outcome::Done,
                                Err(_) => Outcome::Failed("文件窗口已经关闭".to_owned()),
                            }
                        }
                        None => Outcome::Failed("文件窗口没有开".to_owned()),
                    });
                    match outcome {
                        Outcome::Failed(reason) => break Err(reason),
                        _ => break Ok(()),
                    }
                }
                let outcome = handle.update(cx, |_, window, cx| match step {
                    // 按键和点击会触发界面上的监听器，监听器里要更新 MainWindow，所以不能包在
                    // `view.update` 里（那样 MainWindow 已经处在被借用的状态）。
                    Step::Key(spec) => {
                        match ::gpui::Keystroke::parse_with_os(
                            spec,
                            ::gpui::OperatingSystem::Windows,
                        ) {
                            Ok(keystroke) => {
                                window.dispatch_keystroke(keystroke, cx);
                                Outcome::Done
                            }
                            Err(_) => Outcome::Failed("不认识的按键写法".to_owned()),
                        }
                    }
                    Step::Click {
                        x,
                        y,
                        count,
                        button,
                        ctrl,
                        shift,
                    } => {
                        click(window, cx, *x, *y, *count, *button, *ctrl, *shift);
                        Outcome::Done
                    }
                    Step::ClickText {
                        text,
                        count,
                        button,
                        ctrl,
                        shift,
                    } => {
                        let center = view.read(cx).workspace.read(cx).dev_text_center(text);
                        match center {
                            Some((x, y)) => {
                                click(window, cx, x, y, *count, *button, *ctrl, *shift);
                                Outcome::Done
                            }
                            None => Outcome::Pending,
                        }
                    }
                    Step::FileDrop {
                        x,
                        y,
                        paths,
                        cancel,
                    } => {
                        file_drop(window, cx, *x, *y, paths, *cancel);
                        Outcome::Done
                    }
                    Step::Files(inner) => to_file_window(inner, cx),
                    Step::TermWin(inner) => to_term_window(inner, cx),
                    Step::Mouse { kind, x, y } => {
                        mouse(window, cx, *kind, *x, *y);
                        Outcome::Done
                    }
                    _ => view.update(cx, |this, cx| {
                        run_step(
                            this,
                            step,
                            window,
                            cx,
                            &shot_dir,
                            &forbidden,
                            &mut shot_ready,
                        )
                    }),
                });
                match outcome {
                    Ok(Outcome::Done) => break Ok(()),
                    Ok(Outcome::Failed(reason)) => break Err(reason),
                    Ok(Outcome::Pending) => {
                        if started.elapsed() > limit {
                            break Err("等待超时".to_owned());
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                    }
                    Err(_) => break Err("窗口已经关闭".to_owned()),
                }
            };
            match result {
                Ok(()) => log_line(&log, &format!("[{number}] {step:?} ok")),
                Err(reason) => {
                    log_line(&log, &format!("[{number}] {step:?} FAIL {reason}"));
                    // 失败就停：后面的步骤依赖前面的结果，继续跑只会产生一堆假失败。
                    let _ = handle.update(cx, |_, window, cx| {
                        view.update(cx, |this, cx| this.on_close(window, cx))
                    });
                    crate::settings_store::flush_to_disk();
                    std::process::exit(2);
                }
            }
            if matches!(step, Step::Quit) {
                break;
            }
        }
        log_line(&log, "脚本结束");
    })
    .detach();
}

fn run_step(
    this: &mut MainWindow,
    step: &Step,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
    shot_dir: &Option<PathBuf>,
    forbidden: &[String],
    shot_ready: &mut bool,
) -> Outcome {
    let ws = this.workspace.clone();
    match step {
        Step::Open(id) => {
            if this.store.host(*id).is_none() {
                return Outcome::Failed("主机库里没有这个主机 id".to_owned());
            }
            this.open_host(*id, window, cx);
            Outcome::Done
        }
        Step::NewTab => {
            this.new_tab(window, cx);
            Outcome::Done
        }
        Step::WaitLive => {
            if ws.read(cx).active_pane_is_live() {
                Outcome::Done
            } else {
                Outcome::Pending
            }
        }
        Step::WaitDisconnected => {
            let ws = ws.read(cx);
            let disconnected = ws
                .active_index()
                .and_then(|at| ws.tab_infos().get(at).map(|info| info.status))
                == Some(TabStatus::Disconnected);
            if disconnected {
                Outcome::Done
            } else {
                Outcome::Pending
            }
        }
        Step::Type(text) => {
            ws.update(cx, |ws, cx| ws.type_text(text, cx));
            Outcome::Done
        }
        Step::Expect(text) => {
            let found = ws
                .read(cx)
                .visible_text()
                .iter()
                .any(|line| line.contains(text.as_str()));
            if found {
                Outcome::Done
            } else {
                Outcome::Pending
            }
        }
        Step::ExpectCount(count, text) => {
            let seen = ws
                .read(cx)
                .visible_text()
                .iter()
                .filter(|line| line.contains(text.as_str()))
                .count();
            if seen >= *count {
                Outcome::Done
            } else {
                Outcome::Pending
            }
        }
        Step::Split(axis) => {
            let done = ws.update(cx, |ws, cx| ws.split_active(*axis, cx));
            if done {
                Outcome::Done
            } else {
                Outcome::Failed("没有可拆的窗格".to_owned())
            }
        }
        Step::Duplicate(index) => {
            let before = ws.read(cx).tab_count();
            this.duplicate_tab(*index, window, cx);
            if ws.read(cx).tab_count() == before + 1 {
                Outcome::Done
            } else {
                Outcome::Failed("没有复制出新标签".to_owned())
            }
        }
        Step::Close(index) => {
            let before = ws.read(cx).tab_count();
            this.close_tab(*index, window, cx);
            if ws.read(cx).tab_count() + 1 == before {
                Outcome::Done
            } else {
                Outcome::Failed("标签没有关掉".to_owned())
            }
        }
        Step::Select(index) => {
            this.select_tab(*index, window, cx);
            Outcome::Done
        }
        Step::Cycle => {
            this.cycle_tab(1, window, cx);
            Outcome::Done
        }
        Step::Reconnect(index) => {
            this.reconnect_tab(*index, window, cx);
            Outcome::Done
        }
        Step::View(view) => {
            this.show_view(*view, window, cx);
            Outcome::Done
        }
        Step::Settings => {
            this.toggle_settings(window, cx);
            Outcome::Done
        }
        Step::Dump => {
            // 只写标题和状态；标题是主机名，不含地址。
            let lines: Vec<String> = ws
                .read(cx)
                .tab_infos()
                .iter()
                .map(|info| format!("{}:{:?}", info.title, info.status))
                .collect();
            let active = ws.read(cx).active_index();
            let rows = this.tree_row_count(cx);
            log_line(
                &crate::dev_hooks::var("ZSSH_MAIN_LOG").map(PathBuf::from),
                &format!(
                    "tabs active={active:?} [{}] view={} tree_rows={rows} tree_cursor={:?} launcher_cursor={}",
                    lines.join(", "),
                    this.nav.view.as_str(),
                    this.tree.cursor_host,
                    this.launcher_cursor
                ),
            );
            Outcome::Done
        }
        Step::Layout => {
            let snapshot = this.layout_snapshot();
            let size = window.viewport_size();
            log_line(
                &crate::dev_hooks::var("ZSSH_MAIN_LOG").map(PathBuf::from),
                &format!(
                    "layout view={:?} tree_width={} collapsed={} expanded={:?} window={}x{}",
                    snapshot.view,
                    snapshot.tree_width,
                    snapshot.tree_collapsed,
                    snapshot.tree_expanded,
                    f32::from(size.width).round(),
                    f32::from(size.height).round()
                ),
            );
            Outcome::Done
        }
        // 按键、点击、鼠标分步在外层处理（见 `spawn_from_env`），不会走到这里。
        Step::Key(_)
        | Step::Click { .. }
        | Step::ClickText { .. }
        | Step::Mouse { .. }
        | Step::FileDrop { .. }
        | Step::Files(_)
        | Step::TermWin(_) => Outcome::Done,
        Step::Act(spec) => super::dev_acts::run(this, spec, window, cx),
        Step::OpenHosts => {
            crate::hosts_demo::open_hosts_window(cx);
            Outcome::Done
        }
        Step::AssertView(view) => {
            if this.nav.view == *view {
                Outcome::Done
            } else {
                Outcome::Failed(format!(
                    "当前页是 {}，不是 {}",
                    this.nav.view.as_str(),
                    view.as_str()
                ))
            }
        }
        Step::AssertTabs(count) => {
            let now = ws.read(cx).tab_count();
            if now == *count {
                Outcome::Done
            } else {
                Outcome::Failed(format!("标签数是 {now}，不是 {count}"))
            }
        }
        Step::AssertStatus(index, status) => {
            let infos = ws.read(cx).tab_infos();
            match infos.get(*index) {
                Some(info) if info.status == *status => Outcome::Done,
                // 状态可能还在变（连接中 → 已连接），给它一点时间。
                Some(_) => Outcome::Pending,
                None => Outcome::Failed("没有这个标签".to_owned()),
            }
        }
        Step::TreeWidth(width) => {
            this.tree.width = crate::window_state::clamp_tree_width(
                *width,
                f32::from(window.viewport_size().width),
            );
            this.save_layout();
            cx.notify();
            Outcome::Done
        }
        Step::ToggleTree => {
            this.toggle_tree(cx);
            Outcome::Done
        }
        Step::ToggleGroup(key) => {
            this.toggle_group(key.clone(), cx);
            Outcome::Done
        }
        Step::Search(text) => {
            this.tree_query
                .update(cx, |input, cx| input.set_value(text.clone(), window, cx));
            Outcome::Done
        }
        Step::Theme(dark) => {
            ws.update(cx, |ws, cx| ws.set_theme(*dark, cx));
            Outcome::Done
        }
        // 睡眠由外层的异步循环承担，不会走到这里。
        Step::Sleep(_) => Outcome::Done,
        Step::Shot(name) => {
            let Some(dir) = shot_dir else {
                return Outcome::Failed("没设置 ZSSH_SHOT_DIR".to_owned());
            };
            if !*shot_ready {
                let hits = this.privacy_hits(forbidden, cx);
                if !hits.is_empty() {
                    // 不打印命中的原文（那正是不能出现的东西），只报个数。
                    return Outcome::Failed(format!(
                        "隐私检查命中 {} 项（{}），不截图",
                        hits.len(),
                        hits.join("、")
                    ));
                }
                if std::fs::create_dir_all(dir).is_err()
                    || std::fs::write(dir.join(format!("{name}.ready")), b"ok").is_err()
                {
                    return Outcome::Failed("写不了截图标记文件".to_owned());
                }
                *shot_ready = true;
            }
            if dir.join(format!("{name}.done")).exists() {
                Outcome::Done
            } else {
                Outcome::Pending
            }
        }
        Step::Quit => {
            this.on_close(window, cx);
            crate::settings_store::flush_to_disk();
            std::process::exit(0);
        }
    }
}

/// 先把指针移到那里，再按下抬起 `count` 次（和真实鼠标点击同一条分发路径）。
#[allow(clippy::too_many_arguments)]
fn click(
    window: &mut Window,
    cx: &mut App,
    x: f32,
    y: f32,
    count: usize,
    button: char,
    ctrl: bool,
    shift: bool,
) {
    use ::gpui::{
        Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, point,
        px,
    };
    let modifiers = Modifiers {
        control: ctrl,
        shift,
        ..Modifiers::default()
    };
    let position = point(px(x), px(y));
    let button = match button {
        'm' => MouseButton::Middle,
        'r' => MouseButton::Right,
        _ => MouseButton::Left,
    };
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
    for n in 1..=count {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count: n,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers,
                click_count: n,
            }),
            cx,
        );
    }
}

/// `termwin:` 步骤：同样的按键 / 点击 / 鼠标，发给第一个独立终端窗口。
fn to_term_window(step: &Step, cx: &mut App) -> Outcome {
    let Some(handle) = crate::term_window::handles(cx).into_iter().next() else {
        return Outcome::Failed("没有独立终端窗口".to_owned());
    };
    // 同 `to_file_window`：用不带根视图类型的句柄，事件处理器里才能更新窗口自己的视图。
    let handle: AnyWindowHandle = handle.into();
    let result = handle.update(cx, |_, window, cx| match step {
        Step::Key(spec) => {
            match ::gpui::Keystroke::parse_with_os(spec, ::gpui::OperatingSystem::Windows) {
                Ok(keystroke) => {
                    window.dispatch_keystroke(keystroke, cx);
                    Outcome::Done
                }
                Err(_) => Outcome::Failed("不认识的按键写法".to_owned()),
            }
        }
        Step::Click {
            x,
            y,
            count,
            button,
            ctrl,
            shift,
        } => {
            click(window, cx, *x, *y, *count, *button, *ctrl, *shift);
            Outcome::Done
        }
        Step::Mouse { kind, x, y } => {
            mouse(window, cx, *kind, *x, *y);
            Outcome::Done
        }
        _ => Outcome::Failed("独立终端窗口不支持这个步骤".to_owned()),
    });
    result.unwrap_or_else(|_| Outcome::Failed("独立终端窗口已经关闭".to_owned()))
}

/// `files:` 步骤：同样的按键 / 点击 / 鼠标 / 文件拖放，发给独立文件窗口。
fn to_file_window(step: &Step, cx: &mut App) -> Outcome {
    let Some(handle) = crate::file_window::window_handle(cx) else {
        return Outcome::Failed("文件窗口没有开".to_owned());
    };
    // 用不带根视图类型的句柄：带类型的 `update` 会把窗口的根视图（文件窗口）借出来，事件处理器里
    // 再更新它就是重复借用。
    let handle: AnyWindowHandle = handle.into();
    let result = handle.update(cx, |_, window, cx| match step {
        Step::Key(spec) => {
            match ::gpui::Keystroke::parse_with_os(spec, ::gpui::OperatingSystem::Windows) {
                Ok(keystroke) => {
                    window.dispatch_keystroke(keystroke, cx);
                    Outcome::Done
                }
                Err(_) => Outcome::Failed("不认识的按键写法".to_owned()),
            }
        }
        Step::Click {
            x,
            y,
            count,
            button,
            ctrl,
            shift,
        } => {
            click(window, cx, *x, *y, *count, *button, *ctrl, *shift);
            Outcome::Done
        }
        Step::Mouse { kind, x, y } => {
            mouse(window, cx, *kind, *x, *y);
            Outcome::Done
        }
        Step::FileDrop {
            x,
            y,
            paths,
            cancel,
        } => {
            file_drop(window, cx, *x, *y, paths, *cancel);
            Outcome::Done
        }
        _ => Outcome::Failed("文件窗口不支持这个步骤".to_owned()),
    });
    result.unwrap_or_else(|_| Outcome::Failed("文件窗口已经关闭".to_owned()))
}

/// 资源管理器拖文件进来：和系统给 gpui 的事件同一个入口（`FileDrop`），经 gpui 翻译成"带着外部路径的
/// 拖动"，落点上的 `on_drop::<ExternalPaths>` 照常触发。没法模拟的只有 Windows 自己的 OLE 拖放。
fn file_drop(window: &mut Window, cx: &mut App, x: f32, y: f32, paths: &[String], cancel: bool) {
    use ::gpui::{ExternalPaths, FileDropEvent, PlatformInput, point, px};
    let position = point(px(x), px(y));
    let mut external = ExternalPaths::default();
    for path in paths {
        external.0.push(PathBuf::from(path));
    }
    window.dispatch_event(
        PlatformInput::FileDrop(FileDropEvent::Entered {
            position,
            paths: external,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::FileDrop(FileDropEvent::Pending { position }),
        cx,
    );
    // 拖进来又拖走：只发"离开"，不发"松手"。
    let last = if cancel {
        FileDropEvent::Exited
    } else {
        FileDropEvent::Submit { position }
    };
    window.dispatch_event(PlatformInput::FileDrop(last), cx);
}

/// 有没有按着左键（`down` 之后的 `move` 要带着左键）。
static LEFT_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 分步的鼠标：`d` 先移到那里再按下，`m` 移动（按下过就带着左键，拖放靠它），`u` 在那里抬起。
fn mouse(window: &mut Window, cx: &mut App, kind: char, x: f32, y: f32) {
    use ::gpui::{
        Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, point,
        px,
    };
    use std::sync::atomic::Ordering;
    let position = point(px(x), px(y));
    let pressed = LEFT_DOWN.load(Ordering::Relaxed);
    match kind {
        'd' => {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            LEFT_DOWN.store(true, Ordering::Relaxed);
        }
        'm' => {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: pressed.then_some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                }),
                cx,
            );
        }
        _ => {
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
            LEFT_DOWN.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{Step, parse_script, unescape};
    use crate::main_window::nav::MainView;
    use crate::split_tree::Axis;

    #[test]
    fn a_script_parses_into_steps() {
        let steps = parse_script(
            "open:1; wait_live ;type:PS1='$ '\\r;split;duplicate:0;close:1;view:settings;shot:main-dark;quit",
        )
        .expect("合法脚本");
        assert_eq!(
            steps,
            vec![
                Step::Open(1),
                Step::WaitLive,
                Step::Type("PS1='$ '\r".to_owned()),
                Step::Split(Axis::X),
                Step::Duplicate(0),
                Step::Close(1),
                Step::View(MainView::Settings),
                Step::Shot("main-dark".to_owned()),
                Step::Quit,
            ]
        );
    }

    #[test]
    fn split_takes_an_optional_direction() {
        assert_eq!(
            parse_script("split:y; split:x; split").expect("合法脚本"),
            vec![
                Step::Split(Axis::Y),
                Step::Split(Axis::X),
                Step::Split(Axis::X)
            ]
        );
        assert!(parse_script("split:diagonal").is_err());
    }

    #[test]
    fn click_text_takes_text_count_button_and_modifiers() {
        assert_eq!(
            parse_script("click_text:https://example.com/docs|1|r|shift").expect("合法脚本"),
            vec![Step::ClickText {
                text: "https://example.com/docs".to_owned(),
                count: 1,
                button: 'r',
                ctrl: false,
                shift: true,
            }]
        );
        // 只写文字时是单击左键。
        assert_eq!(
            parse_script("click_text:hello").expect("合法脚本"),
            vec![Step::ClickText {
                text: "hello".to_owned(),
                count: 1,
                button: 'l',
                ctrl: false,
                shift: false,
            }]
        );
        for bad in [
            "click_text",
            "click_text:",
            "click_text:a|4",
            "click_text:a|1|x",
            "click_text:a|1|r|alt",
        ] {
            assert!(parse_script(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bad_steps_reject_the_whole_script() {
        for bad in [
            "open",
            "open:x",
            "frobnicate",
            "view:nowhere",
            "close:-1",
            "shot:../escape",
            "shot:",
            "sleep:soon",
        ] {
            assert!(parse_script(bad).is_err(), "{bad} 应当被拒绝");
        }
        assert_eq!(parse_script("  ;; ").expect("空脚本"), Vec::new());
    }

    #[test]
    fn escapes_only_cover_the_known_ones() {
        assert_eq!(unescape(r"a\rb\nc\\d\xe"), "a\rb\nc\\d\\xe");
        assert_eq!(unescape("tail\\"), "tail\\");
        assert_eq!(unescape(r"\e[?1002h"), "\x1b[?1002h");
    }
}
