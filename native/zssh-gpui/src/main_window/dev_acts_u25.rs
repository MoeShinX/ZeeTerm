//! 调试驱动里 V2-U25（终端右键菜单）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩两个空函数。
//! 真实的右键用 `click_text:<文字>|1|r[|shift]`（点聚焦窗格里那段文字的中间），这里只管摆状态和核对结果：
//!
//! - `term_select:<行1>,<列1>,<行2>,<列2>`：设选区（可见区坐标）；`assert_term_selection:some|none`；
//!   `assert_term_selected:<文字>`：选中的文字就是这段；
//! - `assert_term_search:closed` 或 `assert_term_search:<查询>|<计数>`：搜索条（右键菜单「搜索」打开）的状态；
//! - `assert_term_menu:closed` 或 `assert_term_menu:<项,项,…>`：右键菜单现在的条目，从上到下，`|` 是分隔线，
//!   名字后面加 `!` 表示置灰；
//! - `term_menu_pick:<名字>`：选菜单里的一项（和点它走同一个方法）；`term_menu_close`：不选任何项关掉；
//! - `term_paste_switch:on|off`（设置页「右键粘贴」开关）、`assert_term_paste_switch:on|off`（从磁盘重读设置）；
//! - `assert_term_focus:yes|no`：键盘焦点在不在终端上；`assert_panes:<个数>`：活动标签的窗格数；
//! - `clip_stash` / `clip_restore`：存下 / 还原剪贴板（验证会写剪贴板的动作之前之后用，别弄丢用户的内容）；
//!   `clip_set:<文字>`（支持反斜杠转义，写法同 `type:`，多行粘贴的验证要用）、`assert_clipboard:<文字|empty>`。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::{ClipboardItem, Window};
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::{Outcome, unescape};
    use crate::terminal_menu::MenuItem;

    const KNOWN: &[&str] = &[
        "term_select",
        "assert_term_selection",
        "assert_term_selected",
        "assert_term_menu",
        "assert_term_search",
        "term_menu_pick",
        "term_menu_close",
        "term_paste_switch",
        "assert_term_paste_switch",
        "assert_term_focus",
        "assert_panes",
        "clip_stash",
        "clip_restore",
        "clip_set",
        "assert_clipboard",
    ];

    thread_local! {
        /// `clip_stash` 存下的剪贴板内容。
        static STASH: std::cell::RefCell<Option<ClipboardItem>> =
            const { std::cell::RefCell::new(None) };
    }

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    fn check(ok: bool, what: String) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else {
            Err(format!("断言没有成立：{what}"))
        }
    }

    fn on_off(raw: &str) -> Result<bool, String> {
        match raw {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
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

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let ws = this.workspace.clone();
        match name {
            "term_select" => {
                let n: Vec<usize> = arg
                    .split(',')
                    .map(|part| part.trim().parse::<usize>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| "term_select 的参数要是 4 个数字")?;
                let [r1, c1, r2, c2] = n[..] else {
                    return Err("term_select 的参数要是 4 个数字".to_owned());
                };
                ws.update(cx, |ws, cx| ws.dev_select((r1, c1), (r2, c2), cx));
                Ok(Outcome::Done)
            }
            "assert_term_selection" => {
                let have = !ws.read(cx).dev_selected_text().is_empty();
                match arg {
                    "some" => check(have, "没有选区".to_owned()),
                    "none" => check(!have, "还有选区".to_owned()),
                    _ => Err("参数只能是 some 或 none".to_owned()),
                }
            }
            "assert_term_selected" => {
                let have = ws.read(cx).dev_selected_text();
                check(have == arg, format!("选中的是 {have:?}"))
            }
            "assert_term_menu" => {
                let entries = ws.read(cx).dev_menu_entries();
                if arg == "closed" {
                    return check(entries.is_none(), "菜单还开着".to_owned());
                }
                let Some(entries) = entries else {
                    return Err("断言没有成立：菜单没有弹出".to_owned());
                };
                let have: Vec<String> = entries
                    .iter()
                    .map(|entry| {
                        if entry.item == MenuItem::Separator {
                            "|".to_owned()
                        } else if entry.enabled {
                            entry.label.to_owned()
                        } else {
                            format!("{}!", entry.label)
                        }
                    })
                    .collect();
                let have = have.join(",");
                check(have == arg, format!("菜单现在是 {have}"))
            }
            "assert_term_search" => {
                // `closed`，或 `<查询>|<计数>`（如 `error|2/3`）；条件暂时不成立会重试到步骤超时。
                let state = ws.read(cx).dev_search_state();
                let have = match &state {
                    None => "closed".to_owned(),
                    Some((query, count)) => format!("{query}|{count}"),
                };
                // 打开 / 重搜在下一个节拍才到位：不对先等，超时才算失败。
                Ok(if have == arg {
                    Outcome::Done
                } else {
                    Outcome::Pending
                })
            }
            "term_menu_pick" => {
                let mut result = Ok(());
                ws.update(cx, |ws, cx| result = ws.dev_menu_pick(arg, window, cx));
                result.map(|()| Outcome::Done)
            }
            "term_menu_close" => {
                ws.update(cx, |ws, cx| ws.dev_menu_close(window, cx));
                Ok(Outcome::Done)
            }
            "term_paste_switch" => {
                let on = on_off(arg)?;
                ws.update(cx, |ws, cx| ws.dev_right_click_paste(on, cx));
                Ok(Outcome::Done)
            }
            "assert_term_paste_switch" => {
                let want = on_off(arg)?;
                // 从磁盘重读，不看内存里的值（先等后台把待写的写完）。
                crate::settings_store::flush_to_disk();
                let have = crate::settings::UiSettings::load().right_click_paste;
                check(have == want, format!("磁盘上是 {have}"))
            }
            "assert_term_focus" => {
                let have = ws.read(cx).dev_terminal_focused(window);
                match arg {
                    "yes" => check(have, "焦点不在终端上".to_owned()),
                    "no" => check(!have, "焦点还在终端上".to_owned()),
                    _ => Err("参数只能是 yes 或 no".to_owned()),
                }
            }
            "assert_panes" => {
                let want: usize = arg.trim().parse().map_err(|_| "个数不是数字")?;
                let have = ws.read(cx).pane_count();
                check(have == want, format!("现在有 {have} 个窗格"))
            }
            "clip_stash" => {
                STASH.with(|slot| *slot.borrow_mut() = cx.read_from_clipboard());
                Ok(Outcome::Done)
            }
            "clip_restore" => {
                match STASH.with(|slot| slot.borrow_mut().take()) {
                    Some(item) => cx.write_to_clipboard(item),
                    // 原来剪贴板是空的（或读不出文字）：写个空串，免得留着测试用的内容。
                    None => cx.write_to_clipboard(ClipboardItem::new_string(String::new())),
                }
                Ok(Outcome::Done)
            }
            "clip_set" => {
                cx.write_to_clipboard(ClipboardItem::new_string(unescape(arg)));
                Ok(Outcome::Done)
            }
            _ => {
                // assert_clipboard
                let have = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                let want = if arg == "empty" { "" } else { arg };
                check(have == want, format!("剪贴板里是 {have:?}"))
            }
        }
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
