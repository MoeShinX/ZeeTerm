//! 调试驱动里和命令栏、撰写栏、AI 面板、文件停靠面板有关的 `act:` 动作（G07c）。
//! 动作都走和界面按钮同一批方法；`assert_*` 检查结果，不对就让脚本失败。日志里只写动作名和结果。

use ::gpui::Window;
use gpui_kit::*;

use super::MainWindow;
use super::dev_script::Outcome;
use super::modal::Modal;
use super::quick_commands::{Field as QuickField, QuickCommand};

pub const KNOWN: &[&str] = &[
    "bar_quick",
    "bar_compose",
    "qc_add",
    "qc_run",
    "qc_delete",
    "qc_panel",
    "qc_form",
    "qc_ask_delete",
    "assert_qc_error",
    "qc_dialog",
    "assert_qc_form",
    "assert_qc_flash",
    "assert_qc_selected",
    "qc_move",
    "assert_qc_order",
    "compose_set",
    "compose_send",
    "compose_history",
    "assert_qc_count",
    "assert_bar",
    "assert_compose_text",
    "pane_expect",
    "pane_count",
    "pane_exactly",
    "compose_focus",
    "broadcast",
    "confirm_multiline",
];

fn fail<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

fn on_off(raw: &str) -> Result<bool, String> {
    match raw {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => fail("参数只能是 on 或 off"),
    }
}

/// 不认识的动作返回 `None`，让上一层报错。
pub(super) fn run(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Option<Result<Outcome, String>> {
    let mut parts = spec.splitn(5, ':');
    let name = parts.next().unwrap_or("");
    if !is_known(name) {
        return None;
    }
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let d = parts.next().unwrap_or("");
    if is_dock_act(name) {
        return Some(run_dock(this, name, [a, b, c, d], window, cx));
    }
    Some(run_inner(this, name, [a, b, c, d], window, cx))
}

fn find_command<'a>(this: &'a MainWindow, name: &str) -> Option<&'a QuickCommand> {
    this.store
        .bars()
        .commands
        .iter()
        .find(|item| item.name == name)
}

fn run_inner(
    this: &mut MainWindow,
    name: &str,
    [a, b, c, d]: [&str; 4],
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    match name {
        "bar_quick" => {
            let want = on_off(a)?;
            if this.quick_bar_open() != want {
                this.toggle_quick_bar(window, cx);
            }
        }
        "bar_compose" => {
            let want = on_off(a)?;
            if this.compose_bar_open() != want {
                this.toggle_compose_bar(window, cx);
            }
        }
        // qc_add:<名称>:<命令>[:<分组>[:<颜色>]]；命令里的 `\n` 当换行。
        "qc_add" => {
            let mut list = this.store.bars().commands.clone();
            list.push(QuickCommand {
                id: format!("dev-{}", list.len() + 1),
                name: a.to_owned(),
                command: b.replace("\\n", "\n"),
                group: (!c.is_empty()).then(|| c.to_owned()),
                enter: true,
                color: (!d.is_empty()).then(|| d.to_owned()),
            });
            this.save_quick_commands(&list, window, cx);
        }
        "qc_delete" => {
            let id = find_command(this, a)
                .map(|command| command.id.clone())
                .ok_or("没有这个名字的快捷命令")?;
            this.do_delete_quick_command(&id, window, cx);
        }
        // qc_run:<名称>[:all]：和点命令按钮（Ctrl+单击）同一个方法。
        "qc_run" => {
            let id = find_command(this, a)
                .map(|command| command.id.clone())
                .ok_or("没有这个名字的快捷命令")?;
            this.run_quick_command(&id, b == "all", window, cx);
        }
        "qc_panel" => match a {
            "toggle" => this.toggle_quick_panel(window, cx),
            "new" => this.open_quick_new(window, cx),
            "edit" => {
                let id = find_command(this, b)
                    .map(|command| command.id.clone())
                    .ok_or("没有这个名字的快捷命令")?;
                this.open_quick_edit(&id, window, cx);
            }
            _ => return fail("qc_panel 的参数是 toggle / new / edit:<名称>"),
        },
        // qc_form:<名称>:<命令>:<分组>[:nosave]：在已经打开的新建 / 编辑对话框里填字段，再按「保存」同一个方法保存。
        "qc_form" => {
            if !matches!(this.modal, Some(Modal::QuickEdit(_))) {
                return fail("新建 / 编辑对话框现在没开着");
            }
            let (name, command, group) = (a.to_owned(), b.replace("\\n", "\n"), c.to_owned());
            let ui = &this.quick_ui;
            let (name_input, group_input, command_input) =
                (ui.name.clone(), ui.group.clone(), ui.command.clone());
            name_input.update(cx, |input, cx| input.set_value(name, window, cx));
            group_input.update(cx, |input, cx| input.set_value(group, window, cx));
            command_input.update(cx, |input, cx| input.set_value(command, window, cx));
            // 第四段写 nosave：只填字段不保存（再用真实按键 Ctrl+Enter / Esc 去存或取消）。
            if d != "nosave" {
                this.quick_save(window, cx);
            }
        }
        "qc_ask_delete" => {
            let id = find_command(this, a)
                .map(|command| command.id.clone())
                .ok_or("没有这个名字的快捷命令")?;
            this.ask_delete_quick_command(&id, cx);
        }
        // qc_move:<名称>:up|down：和面板里的上移 / 下移按钮同一个方法（只在分组内动）。
        "qc_move" => {
            let id = find_command(this, a)
                .map(|command| command.id.clone())
                .ok_or("没有这个名字的快捷命令")?;
            this.quick_move(&id, if b == "up" { -1 } else { 1 }, window, cx);
        }
        // assert_qc_order:<名称,名称,…>：命令在设置里的整体顺序。
        "assert_qc_order" => {
            let have: Vec<String> = this
                .store
                .bars()
                .commands
                .iter()
                .map(|command| command.name.clone())
                .collect();
            if have.join(",") != a {
                return fail("快捷命令的顺序和预期不同");
            }
        }
        // assert_qc_error:<文字>[:<字段>]：保存被拒时对话框里显示的原因包含这段文字（可选再指定
        // 出错的字段 name / group / command / form）。
        "assert_qc_error" => {
            let Some(Modal::QuickEdit(dialog)) = &this.modal else {
                return fail("新建 / 编辑对话框现在没开着");
            };
            let Some((field, shown)) = &dialog.error else {
                return fail("对话框里没有错误提示");
            };
            if !shown.contains(a) {
                return fail("对话框里没有预期的错误提示");
            }
            let at = match field {
                QuickField::Name => "name",
                QuickField::Group => "group",
                QuickField::Command => "command",
                QuickField::Form => "form",
            };
            if !b.is_empty() && at != b {
                return fail(format!("错误挂在 {at} 上，不是 {b}"));
            }
        }
        // qc_dialog:color:<颜色 id 或空>  /  qc_dialog:enter:on|off：改对话框里的颜色 / 开关
        // （和点色点、拨开关同一份状态）。
        "qc_dialog" => {
            let Some(Modal::QuickEdit(dialog)) = &mut this.modal else {
                return fail("新建 / 编辑对话框现在没开着");
            };
            match a {
                "color" => dialog.color = (!b.is_empty()).then(|| b.to_owned()),
                "enter" => dialog.enter = on_off(b)?,
                _ => return fail("qc_dialog 的参数是 color:<色> / enter:on|off"),
            }
            cx.notify();
        }
        // assert_qc_form:<字段>:<值>：对话框里这个字段现在的值。字段 = title / name / group /
        // command / color / enter / editing（正在编辑哪一条的名称，新建是空）/ focus（name / command）。
        "assert_qc_form" => {
            let Some(Modal::QuickEdit(dialog)) = &this.modal else {
                return fail("新建 / 编辑对话框现在没开着");
            };
            let want = b.replace("\\n", "\n");
            let have = match a {
                "title" => super::quick_commands::dialog_title(dialog.id.is_some()).to_owned(),
                "name" => this.quick_ui.name.read(cx).value().to_string(),
                "group" => this.quick_ui.group.read(cx).value().to_string(),
                "command" => this.quick_ui.command.read(cx).value().to_string(),
                "color" => dialog.color.clone().unwrap_or_default(),
                "enter" => (if dialog.enter { "on" } else { "off" }).to_owned(),
                "editing" => dialog
                    .id
                    .as_deref()
                    .and_then(|id| this.store.bars().commands.iter().find(|c| c.id == id))
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                "focus" => {
                    let name = this.quick_ui.name.read(cx).focus_handle(cx);
                    let command = this.quick_ui.command.read(cx).focus_handle(cx);
                    if name.is_focused(window) {
                        "name"
                    } else if command.is_focused(window) {
                        "command"
                    } else {
                        "other"
                    }
                    .to_owned()
                }
                _ => {
                    return fail(
                        "字段是 title / name / group / command / color / enter / editing / focus",
                    );
                }
            };
            if have != want {
                return fail(format!("对话框里 {a} 是 {have:?}，不是 {want:?}"));
            }
        }
        // assert_qc_flash:<名称 或 none>：保存后正在高亮的是哪一条。
        "assert_qc_flash" => {
            let now = this
                .quick_ui
                .flash
                .as_deref()
                .and_then(|id| this.store.bars().commands.iter().find(|c| c.id == id))
                .map_or("none", |c| c.name.as_str());
            if now != a {
                return fail(format!("现在高亮的是 {now}，不是 {a}"));
            }
        }
        // assert_qc_selected:<名称 或 none>：列表里单击选中的是哪一条。
        "assert_qc_selected" => {
            let now = this
                .quick_ui
                .selected
                .as_deref()
                .and_then(|id| this.store.bars().commands.iter().find(|c| c.id == id))
                .map_or("none", |c| c.name.as_str());
            if now != a {
                return fail(format!("现在选中的是 {now}，不是 {a}"));
            }
        }
        "compose_set" => {
            let text = a.replace("\\n", "\n");
            this.bars
                .compose
                .update(cx, |state, cx| state.set_value(text, window, cx));
        }
        // compose_send[:ctrl]：和在输入框里按 Enter（Ctrl+Enter）同一个方法。
        "compose_send" => this.compose_submit(a == "ctrl", window, cx),
        // compose_history:up|down：和输入框里按 ↑ ↓ 同一个方法。
        "compose_history" => {
            let handled = this.compose_history_key(a == "up", cx);
            this.apply_pending_compose_text(window, cx);
            if !handled {
                return fail("这时 ↑ ↓ 不翻历史");
            }
        }
        "broadcast" => {
            if on_off(a)? != this.workspace.read(cx).broadcast_on() {
                this.workspace.update(cx, |ws, cx| ws.toggle_broadcast(cx));
            }
        }
        "confirm_multiline" => {
            if on_off(a)? != this.workspace.read(cx).settings().confirm_multiline {
                this.workspace
                    .update(cx, |ws, cx| ws.toggle_confirm_multiline(cx));
            }
        }
        "assert_qc_count" => {
            let want: usize = a.parse().map_err(|_| "个数不是数字")?;
            let have = this.store.bars().commands.len();
            if have != want {
                return fail(format!("快捷命令有 {have} 条，不是 {want}"));
            }
        }
        // assert_bar:quick|compose:on|off  /  assert_bar:quick_height:<n>  /  assert_bar:compose_height:<n>
        "assert_bar" => match (a, b) {
            ("quick", state) => {
                if this.quick_bar_open() != on_off(state)? {
                    return fail("命令栏开关和预期不同");
                }
            }
            ("compose", state) => {
                if this.compose_bar_open() != on_off(state)? {
                    return fail("撰写栏开关和预期不同");
                }
            }
            // 文件面板里本地栏收起的设置（等节拍把视图里的值存进设置表）。
            ("local_hidden", state) => {
                if this.store.bars().sftp_local_hidden != on_off(state)? {
                    return Ok(Outcome::Pending);
                }
            }
            ("quick_height", n) => {
                let want: f32 = n.parse().map_err(|_| "高度不是数字")?;
                let have = this.quick_height_now();
                if (have - want).abs() > 0.5 {
                    return fail(format!("命令栏高度是 {have}，不是 {want}"));
                }
            }
            ("compose_height", n) => {
                let want: f32 = n.parse().map_err(|_| "高度不是数字")?;
                let have = this.store.bars().compose_height;
                if (have - want).abs() > 0.5 {
                    return fail(format!("撰写栏设置的高度是 {have}，不是 {want}"));
                }
            }
            _ => return fail("assert_bar 的参数不对"),
        },
        "assert_compose_text" => {
            let have = this.bars.compose.read(cx).value().to_string();
            if have != a.replace("\\n", "\n") {
                return fail("撰写栏里的内容和预期不同");
            }
        }
        // pane_expect:<窗格下标>:<文字>：这个窗格的屏幕上有这段文字；`!` 开头表示没有。
        "pane_expect" => {
            let index: usize = a.parse().map_err(|_| "窗格下标不是数字")?;
            let texts = this.workspace.read(cx).pane_texts();
            let Some(screen) = texts.get(index) else {
                return Ok(Outcome::Pending);
            };
            let (negate, needle) = match b.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, b),
            };
            // 参数里 `:` 后面的部分也属于要找的文字。
            let needle = if c.is_empty() {
                needle.to_owned()
            } else {
                format!("{needle}:{c}")
            };
            let found = screen.contains(&needle);
            if negate {
                if found {
                    return fail("这个窗格的屏幕上不该有这段文字");
                }
            } else if !found {
                return Ok(Outcome::Pending);
            }
        }
        "pane_count" => {
            let want: usize = a.parse().map_err(|_| "个数不是数字")?;
            let have = this.workspace.read(cx).pane_count();
            if have != want {
                return Ok(Outcome::Pending);
            }
        }
        // pane_exactly:<n>:<文字>：恰好 n 个窗格的屏幕上有这段文字（等到成立；写 `!` 前缀表示「现在就」要成立）。
        "pane_exactly" => {
            let want: usize = a.parse().map_err(|_| "个数不是数字")?;
            let needle = if c.is_empty() {
                b.to_owned()
            } else {
                format!("{b}:{c}")
            };
            let have = this
                .workspace
                .read(cx)
                .pane_texts()
                .iter()
                .filter(|screen| screen.contains(&needle))
                .count();
            if have != want {
                return Ok(Outcome::Pending);
            }
        }
        "compose_focus" => {
            let handle = this.bars.compose.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        _ => return fail("还没有实现"),
    }
    Ok(Outcome::Done)
}

// ---- AI 面板和文件面板 ----

const DOCK_KNOWN: &[&str] = &[
    "ai_panel",
    "sftp_dock",
    "overview",
    "ai_text",
    "ai_send",
    "ai_wait_reply",
    "ai_assert",
    "ai_pin",
    "ai_unpin",
    "ai_mode",
    "ai_clear",
    "ai_rebind",
    "ai_reopen",
    "ai_usage",
    "sftp_open_tab",
    "sftp_undock",
    "sftp_close_tab",
    "sftp_wait",
    "sftp_assert",
    "sftp_upload",
    "sftp_download",
    "sftp_cd",
    "sftp_local",
    "sftp_hide_local",
    "sftp_layout",
    "sftp_follow",
    "sftp_ui",
    "files_move",
    "file_window_setting",
    "files_assert",
    "files_dup",
    "files_resize",
    "files_back",
    "files_close_tab",
    "files_ui",
    "assert_quit",
    "quit_ask",
    "assert_dock_width",
    "assert_docks",
    "assert_sftp_tabs",
];

pub fn is_dock_act(name: &str) -> bool {
    DOCK_KNOWN.contains(&name)
}

pub fn is_known(name: &str) -> bool {
    KNOWN.contains(&name) || is_dock_act(name)
}

fn active_ai(
    this: &MainWindow,
    _cx: &App,
) -> Option<::gpui::Entity<crate::agent_chat_demo::AgentChatDemo>> {
    this.docks.ai.clone()
}

/// 文件视图：先看当前显示的 SFTP 标签，再看当前终端标签主机的停靠视图。
fn active_sftp(this: &MainWindow, cx: &App) -> Option<::gpui::Entity<crate::sftp_demo::SftpDemo>> {
    if let Some(view) = this.active_sftp_view() {
        return Some(view);
    }
    let host = this.workspace.read(cx).active_host_id()?;
    this.docks.sftp.get(&host).cloned()
}

fn host_by_name(this: &MainWindow, name: &str) -> Result<i64, String> {
    let name = name.strip_prefix("@").unwrap_or(name);
    this.store
        .hosts()
        .iter()
        .find(|host| host.name == name)
        .map(|host| host.id)
        .ok_or_else(|| "没有这个名字的主机".to_owned())
}

pub(super) fn run_dock(
    this: &mut MainWindow,
    name: &str,
    [a, b, c, _d]: [&str; 4],
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    match name {
        "ai_panel" => {
            if on_off(a)? != this.docks.ai_open {
                this.toggle_ai_panel(window, cx);
            }
        }
        "sftp_dock" => {
            let tab = this.workspace.read(cx).active_tab_id();
            if on_off(a)? != this.docks.sftp_open_for(tab) {
                this.toggle_sftp_dock(window, cx);
            }
        }
        "overview" => {
            if on_off(a)? != this.overview_open(cx) {
                this.toggle_overview(cx);
            }
        }
        "ai_text" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            let text = a.replace("\\n", "\n");
            chat.update(cx, |chat, cx| chat.set_input_text(&text, window, cx));
        }
        "ai_send" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            // 真实模式会拉起真实智能体并调用模型：只在显式 ZSSH_AGENT_E2E=1 的自动化里发；
            // 设了假世界（ZSSH_AGENT_FAKE）时拉起的是假驱动，不受这个限制。
            // 内置助手配了进程内的假接口（ZSSH_BUILTIN_FAKE）时不联网，也放行。
            if chat.read_with(cx, |chat, _| chat.is_real())
                && crate::agent_setup::fake::world().is_none()
                && !(crate::agent_setup::builtin_fake::transport().is_some()
                    && chat.read_with(cx, |chat, _| chat.agent_is_builtin()))
                && crate::dev_hooks::var("ZSSH_AGENT_E2E").as_deref() != Some("1")
            {
                return fail("真实对话要设置 ZSSH_AGENT_E2E=1 才会发送（会拉起真实智能体）");
            }
            chat.update(cx, |chat, cx| chat.send_now(window, cx));
        }
        // ai_wait_reply[:最少消息数]：回合结束且有助手回复；失败提示出现就报错。
        "ai_wait_reply" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            let need: usize = a.parse().unwrap_or(2);
            let (running, count, error) = chat.read_with(cx, |chat, _| {
                (
                    chat.is_turn_running(),
                    chat.message_count(),
                    chat.footer_error_text(),
                )
            });
            if let Some(error) = error {
                return fail(format!("AI 面板底部有错误：{error}"));
            }
            if running || count < need {
                return Ok(Outcome::Pending);
            }
        }
        // ai_assert:targets:<名字,名字> / reply_has:<文字> / mode:<manual|readonly|auto> / input:<文字> / messages:<n>
        "ai_assert" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            match a {
                "targets" => {
                    let ids = chat.read_with(cx, |chat, _| chat.target_ids());
                    let mut names: Vec<String> = ids
                        .iter()
                        .filter_map(|id| this.store.host(*id).map(|host| host.name.clone()))
                        .collect();
                    let mut want: Vec<String> = b
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    names.sort();
                    want.sort();
                    if names != want {
                        return fail(format!("AI 目标是 {} 台，和预期不同", names.len()));
                    }
                }
                "reply_has" => {
                    let text = chat.read_with(cx, |chat, _| chat.last_assistant_text());
                    if !text.is_some_and(|text| text.contains(b)) {
                        return fail("最后一条助手回复里没有预期的文字");
                    }
                }
                "mode" => {
                    let mode = chat.read_with(cx, |chat, _| chat.exec_mode());
                    let have = match mode {
                        zssh_core::ai::bridge::ExecMode::Manual => "manual",
                        zssh_core::ai::bridge::ExecMode::ReadOnly => "readonly",
                        zssh_core::ai::bridge::ExecMode::Auto => "auto",
                    };
                    if have != b {
                        return fail(format!("执行模式是 {have}，不是 {b}"));
                    }
                }
                "input" => {
                    let have = chat.read_with(cx, |chat, cx| chat.input_text(cx));
                    if have != b.replace("\\n", "\n") {
                        return fail("AI 输入框里的内容和预期不同");
                    }
                }
                // anchor:<主机名|none>：对话绑定的主机（none = 还没锁定）。
                "anchor" => {
                    let have = chat.read_with(cx, |chat, _| chat.anchor_name());
                    let want = (b != "none").then(|| b.to_owned());
                    if have != want {
                        return fail("对话绑定的主机和预期不同");
                    }
                }
                // banner:<文字|none>：目标栏下面那一行提示（none = 不显示）。
                "banner" => {
                    let have = chat.read_with(cx, |chat, _| chat.banner_text());
                    let want = (b != "none").then(|| b.to_owned());
                    if have != want {
                        return fail(format!(
                            "目标栏下面的提示和预期不同：{}",
                            have.unwrap_or_else(|| "（没有）".to_owned())
                        ));
                    }
                }
                // usage:<文字>：标题栏「上下文」指示器上的字，如 `上下文 3%` / `上下文 —`。
                "usage" => {
                    let have = chat.read_with(cx, |chat, _| chat.usage_text());
                    if have != b {
                        return fail(format!("上下文指示器是「{have}」，和预期不同"));
                    }
                }
                // usage_level:<none|normal|warning|danger>：进度条的颜色档。
                "usage_level" => {
                    let have = chat.read_with(cx, |chat, _| chat.usage_level_name());
                    if have != b {
                        return fail(format!("上下文进度条颜色档是 {have}，不是 {b}"));
                    }
                }
                // chips:<文字|文字>：目标栏每个标签的文字，按顺序（空 = 没有目标）。
                "chips" => {
                    let have = chat.read_with(cx, |chat, _| chat.target_chip_texts());
                    let want: Vec<String> = b
                        .split('|')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    if have != want {
                        return fail(format!("目标栏标签是 {}，和预期不同", have.join("|")));
                    }
                }
                "messages" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let have = chat.read_with(cx, |chat, _| chat.message_count());
                    if have != want {
                        return fail(format!("对话里有 {have} 条消息，不是 {want}"));
                    }
                }
                _ => return fail("ai_assert 的参数不对"),
            }
        }
        "ai_pin" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            let id = host_by_name(this, a)?;
            chat.update(cx, |chat, cx| chat.pin_host(id, cx));
        }
        "ai_unpin" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            let id = host_by_name(this, a)?;
            chat.update(cx, |chat, cx| chat.unpin_host(id, cx));
        }
        "ai_mode" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            let mode = match a {
                "manual" => zssh_core::ai::bridge::ExecMode::Manual,
                "readonly" => zssh_core::ai::bridge::ExecMode::ReadOnly,
                "auto" => zssh_core::ai::bridge::ExecMode::Auto,
                _ => return fail("模式只能是 manual / readonly / auto"),
            };
            chat.update(cx, |chat, cx| chat.set_mode_explicit(mode, cx));
        }
        // 点目标栏下面的「改用 <当前>」/「改用当前主机」。
        "ai_rebind" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            chat.update(cx, |chat, cx| chat.rebind_to_current(cx));
        }
        // ai_usage:open|close：开 / 关标题栏「上下文」的明细卡片。
        "ai_usage" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            let open = match a {
                "open" => true,
                "close" => false,
                _ => return fail("ai_usage 只能是 open / close"),
            };
            chat.update(cx, |chat, cx| chat.set_usage_open(open, cx));
        }
        // 点「重新连接 <主机>」。
        "ai_reopen" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            chat.update(cx, |chat, cx| chat.reopen_anchor_host(cx));
        }
        "ai_clear" => {
            let chat = active_ai(this, cx).ok_or("AI 面板还没打开过")?;
            chat.update(cx, |chat, cx| chat.clear_conversation(cx));
        }
        // sftp_open_tab[:@主机]：不写主机就用当前终端标签（复用它的连接）。
        "sftp_open_tab" => {
            if a.is_empty() {
                let index = this
                    .workspace
                    .read(cx)
                    .active_index()
                    .ok_or("没有终端标签")?;
                this.open_sftp_from_tab(index, window, cx);
            } else {
                let host = host_by_name(this, a)?;
                this.open_sftp(host, window, cx);
            }
        }
        "sftp_undock" => {
            let host = this
                .workspace
                .read(cx)
                .active_host_id()
                .ok_or("当前标签没有主机")?;
            this.undock_sftp(host, window, cx);
        }
        "sftp_close_tab" => {
            let id = this.docks.sftp_tabs.last().map(|tab| tab.id);
            if let Some(id) = id {
                this.close_sftp_tab(id, window, cx);
            }
        }
        "sftp_wait" => {
            let Some(view) = active_sftp(this, cx) else {
                return Ok(Outcome::Pending);
            };
            let (ready, failed) = view.read_with(cx, |view, _| (view.is_ready(), view.is_failed()));
            if failed {
                return fail("文件视图连接失败");
            }
            if !ready {
                return Ok(Outcome::Pending);
            }
        }
        // sftp_assert:has:<名字> / hasnt:<名字> / cwd:<路径前缀> / done:<n> / hidden:on|off
        "sftp_assert" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            match a {
                "has" => {
                    let names = view.read_with(cx, |view, _| view.remote_names());
                    if !names.iter().any(|name| name == b) {
                        return Ok(Outcome::Pending);
                    }
                }
                "hasnt" => {
                    let names = view.read_with(cx, |view, _| view.remote_names());
                    if names.iter().any(|name| name == b) {
                        return Ok(Outcome::Pending);
                    }
                }
                "cwd" => {
                    let cwd = view.read_with(cx, |view, _| view.remote_cwd().to_owned());
                    if !cwd.starts_with(b) {
                        return fail("远端目录和预期不同");
                    }
                }
                "done" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let (done, failed, running) =
                        view.read_with(cx, |view, _| view.transfer_counts());
                    if failed > 0 {
                        return fail("有传输失败");
                    }
                    if done < want || running > 0 {
                        return Ok(Outcome::Pending);
                    }
                }
                "hidden" => {
                    let hidden = view.read_with(cx, |view, _| view.local_hidden());
                    if hidden != on_off(b)? {
                        return fail("本地栏收起状态和预期不同");
                    }
                }
                // follow:on|off：「跟随终端目录」开关现在的样子（轮询是异步的，对不上先等）
                "follow" => {
                    let follow = view.read_with(cx, |view, _| view.follow());
                    if follow != on_off(b)? {
                        return Ok(Outcome::Pending);
                    }
                }
                _ => return fail("sftp_assert 的参数不对"),
            }
        }
        "sftp_upload" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            // 本地路径里有盘符冒号：后面两段拼回去。
            let path = [a, b, c]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(":");
            let result = view.update(cx, |view, cx| view.script_upload(&path, cx));
            result?;
        }
        "sftp_download" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            let result = view.update(cx, |view, cx| view.script_download(a, cx));
            result?;
        }
        "sftp_cd" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            view.update(cx, |view, cx| view.script_remote_dir(a, cx));
        }
        "sftp_local" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            let path = [a, b, c]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(":");
            view.update(cx, |view, cx| {
                view.script_local_dir(std::path::Path::new(&path), cx);
            });
        }
        // sftp_ui:<命令>:<参数…>：文件视图的界面动作和检查，见 `sftp_demo/script.rs`。
        "sftp_ui" => {
            let Some(view) = active_sftp(this, cx) else {
                return Ok(Outcome::Pending);
            };
            let args = [a, b, c, _d];
            // 第一个参数是命令，其余是命令的参数（空的丢掉尾部）。
            let (command, rest) = (a, &args[1..]);
            let used = rest
                .iter()
                .rposition(|s| !s.is_empty())
                .map_or(0, |i| i + 1);
            let rest: Vec<&str> = rest[..used].to_vec();
            let done = view.update(cx, |view, cx| view.script_ui(command, &rest, window, cx))?;
            if !done {
                return Ok(Outcome::Pending);
            }
        }
        // sftp_follow:on|off：开关「跟随终端目录」（和面板里的勾选框同一个方法）
        "sftp_follow" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            let on = on_off(a)?;
            view.update(cx, |view, cx| view.set_follow(on, cx));
        }
        // files_move：把当前显示的 SFTP 标签移到独立文件窗口（和标签右键「移到文件窗口」同一个方法）
        "files_move" => {
            let id = this.docks.active_sftp.ok_or("当前没有显示 SFTP 标签")?;
            this.move_sftp_tab_to_file_window(id, window, cx);
        }
        // files_dup[:<标签下标>]：复制文件窗口里的标签（和标签右键「复制标签」同一个方法）；不写下标就复制当前标签
        "files_dup" => {
            let handle = crate::file_window::window_handle(cx).ok_or("文件窗口没有开")?;
            let index: Option<usize> = if a.is_empty() {
                None
            } else {
                Some(a.parse().map_err(|_| "下标不是数字")?)
            };
            let found = handle
                .update(cx, |files, window, cx| {
                    let id = match index {
                        Some(index) => files.id_at(index),
                        None => files.active_id(),
                    };
                    id.map(|id| files.duplicate_tab(id, window, cx)).is_some()
                })
                .map_err(|_| "文件窗口已经关闭")?;
            if !found {
                return fail("没有这个标签");
            }
        }
        // files_resize:<宽>:<高>：把文件窗口调成这个大小（和用户拖边框同一条路径：窗口大小变了 -> 记住）
        "files_resize" => {
            let handle = crate::file_window::window_handle(cx).ok_or("文件窗口没有开")?;
            let width: f32 = a.parse().map_err(|_| "宽不是数字")?;
            let height: f32 = b.parse().map_err(|_| "高不是数字")?;
            handle
                .update(cx, |_, window, _| {
                    window.resize(::gpui::size(::gpui::px(width), ::gpui::px(height)));
                })
                .map_err(|_| "文件窗口已经关闭")?;
        }
        // files_assert:open|closed / tabs:<n> / unfinished:<n> / active:<下标> / cwds:<路径,路径…>（每个标签的远端目录，按标签顺序）
        // / status:<文字>（当前标签状态栏里有这段话）
        "files_assert" => {
            let handle = crate::file_window::window_handle(cx);
            match (a, handle) {
                ("open", Some(_)) | ("closed", None) => {}
                ("open", None) => return Ok(Outcome::Pending),
                ("closed", Some(_)) => return Ok(Outcome::Pending),
                ("tabs", Some(handle)) => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let have = handle
                        .read_with(cx, |files, cx| files.tab_summary(cx).len())
                        .map_err(|_| "文件窗口已经关闭")?;
                    if have != want {
                        return Ok(Outcome::Pending);
                    }
                }
                // size:<宽>:<高>：文件窗口现在的大小（容差 2 像素）
                ("size", Some(handle)) => {
                    let width: f32 = b.parse().map_err(|_| "宽不是数字")?;
                    let height: f32 = c.parse().map_err(|_| "高不是数字")?;
                    let have = handle
                        .update(cx, |_, window, _| {
                            let bounds = window.bounds();
                            (f32::from(bounds.size.width), f32::from(bounds.size.height))
                        })
                        .map_err(|_| "文件窗口已经关闭")?;
                    if (have.0 - width).abs() > 2.0 || (have.1 - height).abs() > 2.0 {
                        return Ok(Outcome::Pending);
                    }
                }
                ("active", Some(handle)) => {
                    let want: usize = b.parse().map_err(|_| "下标不是数字")?;
                    let have = handle
                        .read_with(cx, |files, _| files.active_index())
                        .map_err(|_| "文件窗口已经关闭")?;
                    if have != Some(want) {
                        return Ok(Outcome::Pending);
                    }
                }
                ("cwds", Some(handle)) => {
                    let have = handle
                        .read_with(cx, |files, cx| files.tab_remote_dirs(cx))
                        .map_err(|_| "文件窗口已经关闭")?;
                    let want: Vec<&str> = b.split(',').collect();
                    if have.iter().map(String::as_str).collect::<Vec<_>>() != want {
                        return Ok(Outcome::Pending);
                    }
                }
                ("status", Some(handle)) => {
                    let have = handle
                        .read_with(cx, |files, cx| files.active_status(cx))
                        .map_err(|_| "文件窗口已经关闭")?;
                    if !have.is_some_and(|text| text.contains(b)) {
                        return Ok(Outcome::Pending);
                    }
                }
                ("unfinished", Some(handle)) => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let have = handle
                        .read_with(cx, |files, cx| {
                            files.tab_summary(cx).iter().map(|tab| tab.2).sum::<usize>()
                        })
                        .map_err(|_| "文件窗口已经关闭")?;
                    if have != want {
                        return Ok(Outcome::Pending);
                    }
                }
                _ => return fail("files_assert 的参数不对或文件窗口没有开"),
            }
        }
        // file_window_setting:merge|close：写独立文件窗口的"关闭时"设置（`file_window_close`，写的是
        // `ZSSH_DATA_DIR` 指向的库；设置页还没有这一项）
        "file_window_setting" => {
            let value = match a {
                "merge" | "close" => a,
                _ => return fail("只能是 merge 或 close"),
            };
            let db = zssh_core::store::Database::open(&crate::settings::data_dir().join("zssh.db"))
                .map_err(|err| err.to_string())?;
            db.setting_set("file_window_close", value)
                .map_err(|err| err.to_string())?;
        }
        // files_back：由 `dev_script` 在主窗口之外执行（见那里的说明），走不到这里。
        "files_back" => return fail("files_back 只能由脚本驱动执行"),
        // files_close_tab：关文件窗口里当前的标签（有传输会出确认条）
        "files_close_tab" => {
            let handle = crate::file_window::window_handle(cx).ok_or("文件窗口没有开")?;
            handle
                .update(cx, |files, window, cx| {
                    if let Some(id) = files.active_id() {
                        files.close_tab(id, window, cx);
                    }
                })
                .map_err(|_| "文件窗口已经关闭")?;
        }
        // files_ui:<命令>:<参数…>：对文件窗口里当前标签的视图做界面动作和检查（同 `sftp_ui`）
        "files_ui" => {
            let Some(handle) = crate::file_window::window_handle(cx) else {
                return Ok(Outcome::Pending);
            };
            let args = [a, b, c, _d];
            let (command, rest) = (a, &args[1..]);
            let used = rest
                .iter()
                .rposition(|s| !s.is_empty())
                .map_or(0, |i| i + 1);
            let rest: Vec<&str> = rest[..used].to_vec();
            let done = handle
                .update(cx, |files, window, cx| {
                    let Some(view) = files.active_view() else {
                        return Ok(false);
                    };
                    view.update(cx, |view, cx| view.script_ui(command, &rest, window, cx))
                })
                .map_err(|_| "文件窗口已经关闭")??;
            if !done {
                return Ok(Outcome::Pending);
            }
        }
        // sftp_layout:dock|tab：把当前文件视图换成停靠面板 / 标签的布局（没有真实终端连接时用来看停靠面板的排版）
        "sftp_layout" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            let dock = match a {
                "dock" => true,
                "tab" => false,
                _ => return fail("只能是 dock 或 tab"),
            };
            view.update(cx, |view, cx| view.set_dock_layout(dock, cx));
        }
        "sftp_hide_local" => {
            let view = active_sftp(this, cx).ok_or("没有文件视图")?;
            let hidden = on_off(a)?;
            view.update(cx, |view, cx| view.set_local_hidden(hidden, cx));
        }
        // assert_quit:none|prompt：现在退出要不要先问。
        "assert_quit" => {
            let asks = super::close_guard::quit_prompt(crate::install_state::snapshot()).is_some();
            match (a, asks) {
                ("none", false) | ("prompt", true) => {}
                _ => return fail("退出确认和预期不同"),
            }
        }
        // quit_ask:none|prompt：走和关闭按钮 / 托盘「退出」同一个 `request_quit`；
        // 有进行中的工作就出确认框（并不真的退出）。
        "quit_ask" => {
            let can_quit = this.request_quit(cx);
            match (a, can_quit) {
                ("none", true) | ("prompt", false) => {}
                _ => return fail("退出确认和预期不同"),
            }
        }
        // assert_dock_width:ai|sftp:<最小>:<最大>
        "assert_dock_width" => {
            let layout = this.dock_layout_now(cx);
            let width = match a {
                "ai" => layout.ai_width,
                "sftp" => layout.sftp_width,
                _ => return fail("只能是 ai 或 sftp"),
            };
            let min: f32 = b.parse().map_err(|_| "最小值不是数字")?;
            let max: f32 = c.parse().map_err(|_| "最大值不是数字")?;
            if !(min..=max).contains(&width) {
                return fail(format!("面板宽度是 {width}，不在 {min}~{max} 里"));
            }
        }
        "assert_docks" => {
            let want = (on_off(a)?, on_off(b)?);
            let tab = this.workspace.read(cx).active_tab_id();
            let have = (this.docks.ai_open, this.docks.sftp_open_for(tab));
            if have != want {
                return fail(format!("AI / 文件面板是 {have:?}，不是 {want:?}"));
            }
        }
        "assert_sftp_tabs" => {
            let want: usize = a.parse().map_err(|_| "个数不是数字")?;
            if this.docks.sftp_tabs.len() != want {
                return fail(format!("SFTP 标签有 {} 个", this.docks.sftp_tabs.len()));
            }
        }
        _ => return fail("还没有实现"),
    }
    Ok(Outcome::Done)
}
