//! 调试驱动里 V2-U47（快捷命令栏展开成面板）要用的 `act:` 动作。只在 debug 构建里编进去（模块本身在
//! `mod.rs` 里被门掉）。动作走和界面操作同一批方法（展开 / 收起、选分组、搜索框），截图脚本用。
//!
//! - `offline_tab`：开一个离线演示标签（不连任何主机、键入内容本地回显）并选中，截图用；
//! - `quick_expand:<on|off>`：展开 / 收起命令栏（和点栏上的按钮一样）；
//! - `quick_search:<文字>`：往展开面板的搜索框里填文字；
//! - `quick_group:<分组名|all|ungrouped>`：选当前分组（和点面板左边那一列 / 栏上的下拉一样）；
//! - `assert_quick_panel:<on|off>`：展开面板现在显示着没有；
//! - `assert_quick_shown:<个数>`：当前分组 + 搜索词下面板右边显示几条命令。
//!
//! V2-U48（命令历史面板）：
//!
//! - `hist_host:<主机编号>`：给活动窗格挂一个主机编号（离线演示窗格才有历史目标，截图用）；
//! - `hist_seed:<主机编号>:<命令>`：往 `host:<编号>` 这个历史桶里记一条（和窗格自己采集同一个入口，
//!   同样过滤前导空格、敏感命令等）；
//! - `hist_panel:<on|off>`：开 / 关命令历史面板（和点状态栏按钮一样）；
//! - `hist_search:<文字>`：往面板的搜索框里填文字；
//! - `assert_hist_rows:<个数>`：面板搜索过滤后显示几条。

use ::gpui::Window;
use gpui_kit::*;

use super::MainWindow;
use super::dev_script::Outcome;
use super::quick_commands;

const KNOWN: &[&str] = &[
    "offline_tab",
    "quick_expand",
    "quick_search",
    "quick_group",
    "assert_quick_panel",
    "assert_quick_shown",
    "hist_host",
    "hist_seed",
    "hist_panel",
    "hist_search",
    "assert_hist_rows",
];

pub fn is_known(name: &str) -> bool {
    KNOWN.contains(&name)
}

pub(in crate::main_window) fn run(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Option<Result<Outcome, String>> {
    let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
    if !is_known(name) {
        return None;
    }
    Some(run_inner(this, name, arg, window, cx))
}

fn on_off(raw: &str) -> Result<bool, String> {
    match raw {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("参数只能是 on 或 off".to_owned()),
    }
}

fn run_inner(
    this: &mut MainWindow,
    name: &str,
    arg: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    match name {
        "offline_tab" => {
            this.workspace.update(cx, |ws, cx| {
                ws.dev_offline_tab(cx);
                let last = ws.tab_count().saturating_sub(1);
                ws.select_tab(last, cx);
            });
            this.show_view(super::nav::MainView::Terminal, window, cx);
        }
        "quick_expand" => {
            if this.bar_settings().quick_expanded != on_off(arg)? {
                this.toggle_quick_expanded(window, cx);
            }
        }
        "quick_search" => {
            let text = arg.to_owned();
            this.bars
                .quick_search
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        "quick_group" => {
            let key = match arg {
                "all" => "",
                "ungrouped" => quick_commands::UNGROUPED_KEY,
                other => other,
            };
            this.set_quick_group(key, window, cx);
        }
        "assert_quick_panel" => {
            if this.quick_panel_shown() != on_off(arg)? {
                return Ok(Outcome::Pending);
            }
        }
        "assert_quick_shown" => {
            let want: usize = arg.parse().map_err(|_| "个数不是数字")?;
            let settings = this.bar_settings();
            let group = quick_commands::effective_group(&settings.commands, &settings.group);
            let query = this.bars.quick_search.read(cx).value().to_string();
            let have = quick_commands::panel_shown(&settings.commands, &group, &query).len();
            if have != want {
                return Err(format!("面板里显示 {have} 条，不是 {want}"));
            }
        }
        "hist_host" => {
            let id: i64 = arg.parse().map_err(|_| "主机编号不是数字")?;
            this.workspace.update(cx, |ws, cx| {
                ws.dev_set_host_id(id);
                cx.notify();
            });
        }
        "hist_seed" => {
            let (id, command) = arg.split_once(':').ok_or("要写成 <主机编号>:<命令>")?;
            let id: i64 = id.parse().map_err(|_| "主机编号不是数字")?;
            crate::terminal_workspace::TerminalWorkspace::dev_record_history(
                &format!("host:{id}"),
                command,
            );
        }
        "hist_panel" => {
            if this.history_panel_visible() != on_off(arg)? {
                this.toggle_history_panel(window, cx);
            }
        }
        "hist_search" => {
            let text = arg.to_owned();
            this.history_ui
                .search
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        "assert_hist_rows" => {
            let want: usize = arg.parse().map_err(|_| "个数不是数字")?;
            let query = this.history_ui.search.read(cx).value().to_string();
            let have = super::history_panel::filter(this.history_ui.entries(), &query).len();
            if have != want {
                return Ok(Outcome::Pending);
            }
        }
        _ => return Err("没做的动作".to_owned()),
    }
    Ok(Outcome::Done)
}
