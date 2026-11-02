//! 调试驱动（`ZSSH_MAIN_SCRIPT` 里的 `act:` 步骤）里和编辑器、恢复提示、外部编辑有关的动作。只在 debug 构建里
//! 通过 `dev_hooks` 读到脚本，release 里这条路径不存在。
//!
//! 写法 `act:<动作>[:参数…]`；参数里可以带 `:`（只拆第一个冒号），文字里的 `\n` `\t` `\\` 会还原。
//! 动作都走和界面按钮 / 快捷键同一批方法；`*_assert_*` 检查结果，不对就让脚本失败；
//! 需要等后台结果的（打开、保存、冲突）返回「还没好」，驱动每 100 ms 重试一次，最长 40 秒。
//!
//! | 动作 | 作用 |
//! |---|---|
//! | `edit_open:<@主机>:<路径>` | 在主窗口开编辑器标签（自己拨号，和从恢复提示打开同一条路） |
//! | `edit_wait_ready` | 等当前编辑器读完 |
//! | `edit_set_text` / `edit_insert` / `edit_select:<起>-<止>` | 像用户那样改文字 / 选区 |
//! | `edit_cmd:<命令>` | `save` `comment` `up` `down` `copy_up` `copy_down` `delete_line` `select_line` `blank_line` `bracket` `next_occurrence` `wrap` `search` `replace` `find_next` |
//! | `edit_goto:<行[:列]>` | 走跳行条（打开 → 填 → 回车） |
//! | `edit_conflict:<覆盖|放弃|稍后|重开|另存为:<名字>>`、`edit_owner:<允许|原地|取消>`、`edit_draft:<恢复|丢弃>` | 提示条上的按钮 |
//! | `edit_close` / `unsaved_answer:<save|discard>` | 关标签、回答未保存确认 |
//! | `edit_assert_*` | 文本 / 未保存 / 状态栏 / 语言 / 光标 / 编码换行 / 状态 / 标签数 |
//! | `recovery_*` | 恢复提示 |

use ::gpui::Window;
use gpui_kit::*;

use super::MainWindow;
use super::dev_script::Outcome;
use super::modal::Modal;
use crate::remote_editor_demo::recovery::Action;
use crate::remote_editor_demo::{ChannelSource, OpenSpec, Phase};

const KNOWN: &[&str] = &[
    "edit_open",
    "edit_wait_ready",
    "edit_set_text",
    "edit_insert",
    "edit_select",
    "edit_cmd",
    "edit_goto",
    "edit_conflict",
    "edit_owner",
    "edit_draft",
    "edit_close",
    "edit_select_tab",
    "edit_reconnect",
    "edit_assert_text",
    "edit_assert_has",
    "edit_assert_unsaved",
    "edit_assert_status",
    "edit_assert_tabs",
    "edit_wait_tabs",
    "edit_assert_title",
    "edit_assert_lang",
    "edit_assert_cursor",
    "edit_assert_format",
    "edit_assert_state",
    "edit_assert_goto",
    "edit_wait_state",
    "edit_wait_text",
    "edit_wait_modal",
    "edit_wait_status",
    "edit_expect_failed",
    "unsaved_answer",
    "recovery_assert",
    "recovery_act",
    "recovery_wait",
    "recovery_wait_conflict",
    "recovery_wait_note",
    "recovery_wait_gone",
    "recovery_reopen_scan",
];

pub fn is_known(name: &str) -> bool {
    KNOWN.contains(&name)
}

fn fail<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

/// 还原参数里的转义。
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
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

fn on_off(raw: &str) -> Result<bool, String> {
    match raw {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => fail("参数只能是 on 或 off"),
    }
}

/// 不认识的动作返回 `None`，让上一层继续找。
pub(super) fn run(
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

/// 当前显示的编辑器（没有就失败）。
fn active_editor(
    this: &MainWindow,
) -> Result<::gpui::Entity<crate::remote_editor_demo::RemoteEditor>, String> {
    this.active_editor_view()
        .ok_or_else(|| "当前没有显示编辑器标签".to_owned())
}

fn host_by_ref(this: &MainWindow, raw: &str) -> Result<(i64, String), String> {
    let name = raw.strip_prefix('@').unwrap_or(raw);
    this.store
        .hosts()
        .iter()
        .find(|host| host.name == name)
        .map(|host| (host.id, host.name.clone()))
        .ok_or_else(|| "没有这个名字的主机".to_owned())
}

fn run_inner(
    this: &mut MainWindow,
    name: &str,
    arg: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    let done = Ok(Outcome::Done);
    match name {
        "edit_open" => {
            let (host, path) = arg.split_once(':').ok_or("写法：edit_open:@主机:/路径")?;
            let (host_id, host_label) = host_by_ref(this, host)?;
            this.open_editor_tab(
                OpenSpec {
                    host_id,
                    host_label,
                    path: path.to_owned(),
                    source: ChannelSource::Dial,
                },
                window,
                cx,
            );
            done
        }
        "edit_wait_ready" => {
            let view = active_editor(this)?;
            let (phase, status) =
                view.read_with(cx, |v, _| (v.phase(), v.status_text().to_owned()));
            match phase {
                Phase::Ready => done,
                Phase::Failed => fail(format!("编辑器没打开：{status}")),
                _ => Ok(Outcome::Pending),
            }
        }
        "edit_set_text" => {
            let text = unescape(arg);
            active_editor(this)?.update(cx, |v, cx| v.script_set_text(&text, window, cx));
            done
        }
        "edit_insert" => {
            let text = unescape(arg);
            active_editor(this)?.update(cx, |v, cx| v.script_insert(&text, window, cx));
            done
        }
        "edit_select" => {
            let (from, to) = arg.split_once('-').ok_or("写法：edit_select:起-止")?;
            let range = from.parse().map_err(|_| "起点不是数字")?
                ..to.parse().map_err(|_| "止点不是数字")?;
            active_editor(this)?.update(cx, |v, cx| v.script_select(range, cx));
            done
        }
        "edit_cmd" => {
            let view = active_editor(this)?;
            view.update(cx, |v, cx| match arg {
                "save" => {
                    v.save(cx);
                    Ok(())
                }
                "comment" => {
                    v.toggle_comment(window, cx);
                    Ok(())
                }
                "up" => {
                    v.move_lines(true, window, cx);
                    Ok(())
                }
                "down" => {
                    v.move_lines(false, window, cx);
                    Ok(())
                }
                "copy_up" => {
                    v.copy_lines(true, window, cx);
                    Ok(())
                }
                "copy_down" => {
                    v.copy_lines(false, window, cx);
                    Ok(())
                }
                "delete_line" => {
                    v.delete_lines(window, cx);
                    Ok(())
                }
                "select_line" => {
                    v.select_line(cx);
                    Ok(())
                }
                "blank_line" => {
                    v.insert_line_below(window, cx);
                    Ok(())
                }
                "bracket" => {
                    v.jump_to_bracket(cx);
                    Ok(())
                }
                "next_occurrence" => {
                    v.select_next_occurrence(cx);
                    Ok(())
                }
                "wrap" => {
                    v.toggle_wrap(window, cx);
                    Ok(())
                }
                "search" => {
                    v.open_search(false, window, cx);
                    Ok(())
                }
                "replace" => {
                    v.open_search(true, window, cx);
                    Ok(())
                }
                "find_next" => {
                    v.find_step(true, window, cx);
                    Ok(())
                }
                _ => Err("不认识的编辑命令".to_owned()),
            })?;
            done
        }
        "edit_goto" => {
            active_editor(this)?.update(cx, |v, cx| v.script_goto(arg, window, cx));
            done
        }
        "edit_conflict" => {
            let view = active_editor(this)?;
            let (what, rest) = arg.split_once(':').unwrap_or((arg, ""));
            view.update(cx, |v, cx| match what {
                "覆盖" => {
                    v.overwrite_remote(cx);
                    Ok(())
                }
                "放弃" => {
                    v.discard_my_changes(cx);
                    Ok(())
                }
                "稍后" => {
                    v.defer_conflict(cx);
                    Ok(())
                }
                "重开" => {
                    v.reopen_conflict(cx);
                    Ok(())
                }
                "另存为" => {
                    v.script_conflict_name(rest, window, cx);
                    v.save_conflict_as(cx);
                    Ok(())
                }
                _ => Err("冲突动作只能是 覆盖 / 放弃 / 稍后 / 重开 / 另存为".to_owned()),
            })?;
            done
        }
        "edit_owner" => {
            let view = active_editor(this)?;
            view.update(cx, |v, cx| match arg {
                "允许" => {
                    v.save_allowing_owner_change(cx);
                    Ok(())
                }
                "原地" => {
                    v.save_in_place(cx);
                    Ok(())
                }
                "取消" => {
                    v.cancel_owner_prompt(cx);
                    Ok(())
                }
                _ => Err("属主动作只能是 允许 / 原地 / 取消".to_owned()),
            })?;
            done
        }
        "edit_draft" => {
            let view = active_editor(this)?;
            view.update(cx, |v, cx| match arg {
                "恢复" => {
                    v.restore_draft(window, cx);
                    Ok(())
                }
                "丢弃" => {
                    v.discard_draft(cx);
                    Ok(())
                }
                _ => Err("草稿动作只能是 恢复 / 丢弃".to_owned()),
            })?;
            done
        }
        "edit_reconnect" => {
            active_editor(this)?.update(cx, |v, cx| v.reconnect(cx));
            done
        }
        "edit_close" => {
            let id = this.docks.active_editor.ok_or("当前没有显示编辑器标签")?;
            this.close_editor_tab(id, window, cx);
            done
        }
        "edit_select_tab" => {
            let index: usize = arg.parse().map_err(|_| "下标不是数字")?;
            let id = this
                .docks
                .editor_tabs
                .get(index)
                .map(|tab| tab.id)
                .ok_or("没有这个编辑器标签")?;
            this.select_editor_tab(id, window, cx);
            done
        }
        "unsaved_answer" => {
            match &this.modal {
                Some(Modal::Unsaved(_)) => {}
                _ => return fail("现在没有未保存确认框"),
            }
            match arg {
                "save" => this.answer_unsaved(true, window, cx),
                "discard" => this.answer_unsaved(false, window, cx),
                _ => return fail("只能是 save 或 discard"),
            }
            done
        }
        "edit_assert_text" => {
            let want = unescape(arg);
            let got = active_editor(this)?.read_with(cx, |v, cx| v.current_text(cx));
            if got != want {
                return fail(format!(
                    "编辑器文本不对：{} 字节，期望 {} 字节",
                    got.len(),
                    want.len()
                ));
            }
            done
        }
        "edit_wait_text" => {
            let want = unescape(arg);
            let got = active_editor(this)?.read_with(cx, |v, cx| v.current_text(cx));
            if got == want {
                done
            } else {
                Ok(Outcome::Pending)
            }
        }
        "edit_assert_has" => {
            let want = unescape(arg);
            let got = active_editor(this)?.read_with(cx, |v, cx| v.current_text(cx));
            if !got.contains(&want) {
                return fail("编辑器文本里没有这段内容");
            }
            done
        }
        "edit_assert_unsaved" => {
            let want = on_off(arg)?;
            let view = active_editor(this)?;
            let got = view.update(cx, |v, cx| v.has_unsaved(cx));
            if got != want {
                return fail(format!("未保存标记是 {got}，期望 {want}"));
            }
            done
        }
        "edit_assert_status" | "edit_wait_status" => {
            let want = unescape(arg);
            let status = active_editor(this)?.read_with(cx, |v, _| v.status_text().to_owned());
            if status.contains(&want) {
                done
            } else if name == "edit_wait_status" {
                Ok(Outcome::Pending)
            } else {
                fail(format!("状态栏文字是「{status}」，不含「{want}」"))
            }
        }
        "edit_assert_tabs" | "edit_wait_tabs" => {
            let want: usize = arg.parse().map_err(|_| "个数不是数字")?;
            let got = this.docks.editor_tabs.len();
            if got == want {
                done
            } else if name == "edit_wait_tabs" {
                Ok(Outcome::Pending)
            } else {
                fail(format!("编辑器标签有 {got} 个"))
            }
        }
        "edit_assert_title" => {
            let want = arg.to_owned();
            let got = active_editor(this)?.read_with(cx, |v, _| v.file_name());
            if got != want {
                return fail(format!("标签标题是「{got}」"));
            }
            done
        }
        "edit_assert_lang" => {
            let want = arg.to_owned();
            let got = active_editor(this)?.read_with(cx, |v, _| v.lang().display().to_owned());
            if got != want {
                return fail(format!("语言是「{got}」"));
            }
            done
        }
        "edit_assert_cursor" => {
            let (line, column) = arg
                .split_once(':')
                .ok_or("写法：edit_assert_cursor:行:列")?;
            let (line, column): (usize, usize) = (
                line.parse().map_err(|_| "行不是数字")?,
                column.parse().map_err(|_| "列不是数字")?,
            );
            let view = active_editor(this)?;
            view.update(cx, |v, cx| v.script_refresh(cx));
            let info = view.read_with(cx, |v, _| v.cursor_info());
            if (info.line, info.column) != (line, column) {
                return fail(format!("光标在 {} 行 {} 列", info.line, info.column));
            }
            done
        }
        "edit_assert_format" => {
            let (encoding, eol) = arg
                .split_once(':')
                .ok_or("写法：edit_assert_format:utf8|bom:lf|crlf")?;
            let (bom, crlf) = active_editor(this)?.read_with(cx, |v, _| {
                (v.format().bom, v.format().eol.label() == "CRLF")
            });
            if bom != (encoding == "bom") || crlf != (eol == "crlf") {
                return fail(format!("编码换行是 bom={bom} crlf={crlf}"));
            }
            done
        }
        "edit_assert_state" | "edit_wait_state" => {
            let view = active_editor(this)?;
            let state = view.read_with(cx, |v, _| v.state_name());
            if state == arg {
                done
            } else if name == "edit_wait_state" {
                Ok(Outcome::Pending)
            } else {
                fail(format!("编辑器状态是 {state}，不是 {arg}"))
            }
        }
        "edit_assert_goto" => {
            let want = on_off(arg)?;
            let got = active_editor(this)?.read_with(cx, |v, _| v.goto_visible());
            if got != want {
                return fail(format!("跳行条开着：{got}"));
            }
            done
        }
        "edit_expect_failed" => {
            let want = unescape(arg);
            let (phase, status) =
                active_editor(this)?.read_with(cx, |v, _| (v.phase(), v.status_text().to_owned()));
            match phase {
                Phase::Failed => {
                    if status.contains(&want) {
                        done
                    } else {
                        fail(format!("打开失败的原因是「{status}」，不含「{want}」"))
                    }
                }
                Phase::Ready => fail("这个文件居然打开了"),
                _ => Ok(Outcome::Pending),
            }
        }
        "edit_wait_modal" => {
            let now = super::dev_acts::modal_name(this);
            if now == arg {
                done
            } else {
                Ok(Outcome::Pending)
            }
        }
        // ---- 恢复提示 ----
        "recovery_wait" => match &this.modal {
            Some(Modal::Recovery(ui)) if !ui.items.is_empty() => done,
            _ => Ok(Outcome::Pending),
        },
        // recovery_wait_conflict:<文件名> / recovery_wait_note:<文件名>:<说明里的字> / recovery_wait_gone:<文件名>
        "recovery_wait_conflict" | "recovery_wait_note" | "recovery_wait_gone" => {
            let (file, rest) = arg.split_once(':').unwrap_or((arg, ""));
            let Some(Modal::Recovery(ui)) = &this.modal else {
                // 提示已经关了：只有「等这一项消失」算成功。
                return if name == "recovery_wait_gone" {
                    done
                } else {
                    fail("现在没有恢复提示")
                };
            };
            let item = ui.items.iter().find(|item| item.file_name == file);
            let ready = match (name, item) {
                ("recovery_wait_gone", None) => true,
                ("recovery_wait_gone", Some(_)) => false,
                (_, None) => return fail("没有这一项"),
                ("recovery_wait_conflict", Some(item)) => ui.conflicts.contains(&item.id),
                (_, Some(item)) => ui
                    .notes
                    .get(&item.id)
                    .is_some_and(|(_, text)| text.contains(rest)),
            };
            if ready { done } else { Ok(Outcome::Pending) }
        }
        "recovery_assert" => {
            let want: usize = arg.parse().map_err(|_| "个数不是数字")?;
            let got = match &this.modal {
                Some(Modal::Recovery(ui)) => ui.items.len(),
                _ => 0,
            };
            if got != want {
                return fail(format!("恢复提示里有 {got} 项"));
            }
            done
        }
        // recovery_act:<序号>:<reupload|overwrite|save_local|discard|open>
        "recovery_act" => {
            let (index, what) = arg.split_once(':').ok_or("写法：recovery_act:序号:动作")?;
            let id = match &this.modal {
                Some(Modal::Recovery(ui)) => {
                    // 序号（从 0 起），或者文件名（同一秒落的草稿排序不稳定，场景里按文件名点）。
                    let found = match index.parse::<usize>() {
                        Ok(index) => ui.items.get(index),
                        Err(_) => ui.items.iter().find(|item| item.file_name == index),
                    };
                    found.map(|item| item.id.clone()).ok_or("没有这一项")?
                }
                _ => return fail("现在没有恢复提示"),
            };
            match what {
                "open" => this.recovery_open_in_editor(&id, window, cx),
                "reupload" => this.recovery_act(&id, Action::Reupload, window, cx),
                "overwrite" => this.recovery_act(&id, Action::ReuploadOverwrite, window, cx),
                "save_local" => this.recovery_act(&id, Action::SaveLocal, window, cx),
                // 放弃要点两次，和界面上一样。
                "discard" => {
                    this.recovery_act(&id, Action::Discard, window, cx);
                    this.recovery_act(&id, Action::Discard, window, cx);
                }
                _ => return fail("不认识的恢复动作"),
            }
            done
        }
        "recovery_reopen_scan" => {
            this.recovery_scanned = false;
            done
        }
        _ => fail("还没有实现"),
    }
}
