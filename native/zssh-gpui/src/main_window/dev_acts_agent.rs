//! 调试驱动里和 AI 智能体有关的 `act:` 动作（G10a）：面板里换智能体、选模型 / 强度、断言可用状态和
//! 提示；设置页 AI 一节里的路径、检测、下载。动作都走和界面按钮同一批方法；`ai_*` / `agents_*` 的
//! 断言不对就让脚本失败。日志里只写动作名和结果。
//!
//! 配合 `ZSSH_AGENT_FAKE`（假世界，见 `agent_setup::fake`）使用：不拉任何真实智能体进程。

use ::gpui::{Entity, Window};
use gpui_kit::*;

use super::MainWindow;
use super::dev_script::{Outcome, log_line};
use crate::agent_setup::AgentKind;
use crate::agents_demo::AgentsDemo;

const KNOWN: &[&str] = &[
    // ai_agent:<id> / ai_model:<名字> / ai_effort:<名字> / ai_reset_agent_check
    "ai_agent",
    "ai_model",
    "ai_effort",
    // ai_check:<what>:<值>
    "ai_check",
    "ai_refuse",
    "ai_open_settings",
    "ai_history",
    // G10b：工具卡片、审批、停止、思考指示、标签活动点、主机记忆框
    "ai_stop",
    "ai_queue",
    "ai_layout",
    "ai_scroll",
    "ai_wait",
    "ai_answer",
    "ai_card",
    "ai_thinking",
    "tab_ai_dot",
    "form_memory_has",
    "panel_scroll",
    "settings_scroll",
    // agents_*：设置页 AI 一节
    "agents_text",
    "agents_manual",
    "agents_path",
    "agents_probe",
    "agents_recheck",
    "agents_download",
    "agents_check",
    "agents_default",
    "agents_exec_mode",
    "agents_clear_history",
    "agents_wait_status",
];

pub fn is_known(name: &str) -> bool {
    KNOWN.contains(&name)
}

fn fail<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

fn kind_of(raw: &str) -> Result<AgentKind, String> {
    AgentKind::parse(raw).ok_or_else(|| "智能体只能是 opencode / codex / claude".to_owned())
}

fn active_ai(
    this: &MainWindow,
    cx: &::gpui::App,
) -> Result<Entity<crate::agent_chat_demo::AgentChatDemo>, String> {
    let _ = cx;
    this.docks
        .ai
        .clone()
        .ok_or_else(|| "AI 面板还没打开过".to_owned())
}

fn agents_view(this: &MainWindow) -> Result<Entity<AgentsDemo>, String> {
    this.agent_settings
        .clone()
        .ok_or_else(|| "设置页的 AI 一节还没画出来（先 view:settings 再 sleep）".to_owned())
}

/// 不认识的动作返回 `None`，让上一层继续找。
pub(super) fn run(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Option<Result<Outcome, String>> {
    let mut parts = spec.splitn(4, ':');
    let name = parts.next().unwrap_or("");
    if !is_known(name) {
        return None;
    }
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    Some(run_inner(this, name, [a, b, c], window, cx))
}

fn run_inner(
    this: &mut MainWindow,
    name: &str,
    [a, b, c]: [&str; 3],
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    match name {
        // ---- AI 面板 ----
        "ai_agent" => {
            let chat = active_ai(this, cx)?;
            chat.update(cx, |chat, cx| chat.switch_agent_by_id(a, cx))?;
        }
        // ai_refuse:<id>：切换必须被拒绝（这一项在下拉里是灰的）；没被拒绝就让脚本失败。
        "ai_refuse" => {
            let chat = active_ai(this, cx)?;
            let result = chat.update(cx, |chat, cx| chat.switch_agent_by_id(a, cx));
            if result.is_ok() {
                return fail("不可用的智能体居然切过去了");
            }
        }
        // ai_open_settings：面板里「去设置」/「下载」按钮点下去做的事（主窗口下一个节拍打开设置页）。
        "ai_open_settings" => crate::agent_setup::request_settings(),
        // settings_scroll:bottom|top|<像素>：设置页滚到底 / 顶 / 往下 N 像素（AI 一节在最下面）。
        "settings_scroll" => {
            match a {
                "bottom" => this.settings_scroll.scroll_to_bottom(),
                "top" => this
                    .settings_scroll
                    .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(0.0))),
                other => {
                    let px: f32 = other
                        .parse()
                        .map_err(|_| "settings_scroll 只能是 bottom / top / 像素数")?;
                    this.settings_scroll
                        .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(-px)));
                }
            }
            cx.notify();
        }
        "ai_model" => {
            let chat = active_ai(this, cx)?;
            chat.update(cx, |chat, cx| chat.pick_model_by_label(a, cx))?;
        }
        "ai_effort" => {
            let chat = active_ai(this, cx)?;
            chat.update(cx, |chat, cx| chat.pick_effort_by_label(a, cx))?;
        }
        // ---- G10b：工具卡片 / 审批 / 停止 ----
        // ai_stop：点「停止」。
        "ai_stop" => {
            let chat = active_ai(this, cx)?;
            chat.update(cx, |chat, cx| chat.dev_stop(cx));
        }
        // V2-U1：ai_layout:send_inside 发送按钮完整在面板里（还没量到就等下一帧）。
        "ai_layout" => {
            let chat = active_ai(this, cx)?;
            match a {
                "send_inside" => {
                    let Some(inside) = chat.read_with(cx, |chat, _| chat.dev_send_inside_panel())
                    else {
                        return Ok(Outcome::Pending);
                    };
                    if let Some((panel, send, gap)) =
                        chat.read_with(cx, |chat, _| chat.dev_send_geometry())
                    {
                        log_line(
                            &crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from),
                            &format!(
                                "ai_layout panel={panel:.0} send={send:.0} right_gap={gap:.0}"
                            ),
                        );
                    }
                    if !inside {
                        return fail("发送按钮跑到了面板外面");
                    }
                }
                _ => return fail("ai_layout 只有 send_inside"),
            }
        }
        // V2-U1 待发送队列：ai_queue:len:<n> / text:<第几条>:<文字> / edit:<第几条>:<文字> /
        //   begin_edit:<第几条> / cancel_edit / editing:yes|no / remove:<第几条> / interrupt / send_now /
        //   held:yes|no。文字里可以带冒号（取最后一段）。
        "ai_queue" => {
            let chat = active_ai(this, cx)?;
            let yes_no = |raw: &str| match raw {
                "yes" => Ok(true),
                "no" => Ok(false),
                _ => Err("参数只能是 yes 或 no".to_owned()),
            };
            let nth = |raw: &str| -> Result<usize, String> {
                raw.parse().map_err(|_| "第几条不是数字".to_owned())
            };
            match a {
                "len" => {
                    let want = nth(b)?;
                    let have = chat.read_with(cx, |chat, _| chat.dev_queue_texts().len());
                    if have != want {
                        return fail(format!("队列里有 {have} 条，不是 {want}"));
                    }
                }
                "text" => {
                    let index = nth(b)?;
                    let texts = chat.read_with(cx, |chat, _| chat.dev_queue_texts());
                    let want = c.replace("\\n", "\n");
                    if texts.get(index) != Some(&want) {
                        return fail(format!("队列第 {index} 条的内容和预期不同"));
                    }
                }
                "edit" => {
                    let index = nth(b)?;
                    let text = c.replace("\\n", "\n");
                    chat.update(cx, |chat, cx| chat.dev_queue_edit(index, &text, window, cx))?;
                }
                "begin_edit" => {
                    let index = nth(b)?;
                    chat.update(cx, |chat, cx| chat.dev_queue_begin_edit(index, window, cx))?;
                }
                "cancel_edit" => chat.update(cx, |chat, cx| chat.dev_queue_cancel_edit(cx)),
                "editing" => {
                    let want = yes_no(b)?;
                    if chat.read_with(cx, |chat, _| chat.dev_queue_editing()) != want {
                        return fail("编辑状态和预期不同");
                    }
                }
                "remove" => {
                    let index = nth(b)?;
                    chat.update(cx, |chat, cx| chat.dev_queue_remove(index, cx))?;
                }
                "interrupt" => chat.update(cx, |chat, cx| chat.dev_queue_interrupt(cx))?,
                "send_now" => chat.update(cx, |chat, cx| chat.dev_queue_send_now(cx)),
                "held" => {
                    let want = yes_no(b)?;
                    if chat.read_with(cx, |chat, _| chat.dev_queue_held()) != want {
                        return fail("队列的「已停止」状态和预期不同");
                    }
                }
                _ => return fail("ai_queue 的子命令不认识"),
            }
        }
        // ai_scroll：消息区滚到最新（截图前用）。
        "ai_scroll" => {
            let chat = active_ai(this, cx)?;
            chat.update(cx, |chat, cx| chat.dev_scroll_bottom(cx));
        }
        // ai_wait:approval|no_approval|idle|running|cards:<n>：等到条件成立。
        "ai_wait" => {
            let chat = active_ai(this, cx)?;
            let (pending, (cards, running, _)) = chat.read_with(cx, |chat, _| {
                (chat.dev_pending_approvals(), chat.dev_card_counts())
            });
            let turn_running = chat.read_with(cx, |chat, _| chat.is_turn_running());
            let ready = match a {
                "approval" => pending > 0,
                "no_approval" => pending == 0,
                "idle" => !turn_running,
                "running" => running > 0,
                "cards" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    cards >= want
                }
                _ => return fail("ai_wait 只能是 approval / no_approval / idle / running / cards"),
            };
            if !ready {
                return Ok(Outcome::Pending);
            }
        }
        // ai_answer:allow|deny：点第一张等批准的确认卡片上的按钮。
        "ai_answer" => {
            let chat = active_ai(this, cx)?;
            let allow = match a {
                "allow" => true,
                "deny" => false,
                _ => return fail("ai_answer 只能是 allow / deny"),
            };
            chat.update(cx, |chat, cx| chat.dev_answer_approval(allow, cx))?;
        }
        // ai_card:visible_has:<文字> / visible_no:<文字> / count:<n> / groups:<n> / toggle_group:<第几个>
        //   / view_terminal：看得见的工具卡片文字、卡片和分组个数、展开分组、点「查看终端」。
        "ai_card" => {
            let chat = active_ai(this, cx)?;
            // 文字里可能带冒号：把后面的段拼回去。
            let text = if c.is_empty() {
                b.to_owned()
            } else {
                format!("{b}:{c}")
            };
            match a {
                "visible_has" => {
                    let seen = chat.read_with(cx, |chat, _| chat.dev_visible_cards_text());
                    if !seen.contains(&text) {
                        return Ok(Outcome::Pending);
                    }
                }
                "visible_no" => {
                    let seen = chat.read_with(cx, |chat, _| chat.dev_visible_cards_text());
                    if seen.contains(&text) {
                        return Ok(Outcome::Pending);
                    }
                }
                "count" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let (cards, _, _) = chat.read_with(cx, |chat, _| chat.dev_card_counts());
                    if cards != want {
                        return Ok(Outcome::Pending);
                    }
                }
                "groups" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let (_, _, groups) = chat.read_with(cx, |chat, _| chat.dev_card_counts());
                    if groups != want {
                        return Ok(Outcome::Pending);
                    }
                }
                "toggle_group" => {
                    let nth: usize = b.parse().map_err(|_| "第几个不是数字")?;
                    chat.update(cx, |chat, cx| chat.dev_toggle_group(nth, cx))?;
                }
                "view_terminal" => {
                    chat.update(cx, |chat, cx| chat.dev_view_terminal(cx))?;
                }
                _ => return fail("ai_card 的参数不对"),
            }
        }
        // ai_thinking:on|off / retry:<文字>：「思考中」指示和重试提示。
        "ai_thinking" => {
            let chat = active_ai(this, cx)?;
            let (shown, retry) = chat.read_with(cx, |chat, _| chat.dev_thinking());
            match a {
                "on" if !shown => return Ok(Outcome::Pending),
                "off" if shown => return Ok(Outcome::Pending),
                "on" | "off" => {}
                "retry" => {
                    if !retry.is_some_and(|text| text.contains(b)) {
                        return Ok(Outcome::Pending);
                    }
                }
                _ => return fail("ai_thinking 只能是 on / off / retry"),
            }
        }
        // tab_ai_dot:<标签下标>:on|off：标签上「AI 正在这个终端里执行」的点。
        "tab_ai_dot" => {
            let index: usize = a.parse().map_err(|_| "下标不是数字")?;
            let infos = this.workspace.read(cx).tab_infos();
            let info = infos.get(index).ok_or("没有这个标签")?;
            if info.ai_active != (b == "on") {
                return Ok(Outcome::Pending);
            }
        }
        // panel_scroll:bottom|top：右侧面板内容区滚到底 / 顶。
        "panel_scroll" => {
            match a {
                "bottom" => this.panel_scroll.scroll_to_bottom(),
                "top" => this
                    .panel_scroll
                    .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(0.0))),
                _ => return fail("panel_scroll 只能是 bottom / top"),
            }
            cx.notify();
        }
        // form_memory_has:<文字>：主机属性对话框里记忆框当前显示的内容。
        "form_memory_has" => {
            let input = this
                .host_form
                .as_ref()
                .and_then(|form| form.memory.as_ref())
                .map(|memory| memory.input.clone())
                .ok_or("主机属性里没有记忆小节")?;
            let value = input.read(cx).value().to_string();
            let text = if c.is_empty() {
                a.to_owned()
            } else {
                format!("{a}:{c}")
            };
            if !value.contains(&text) {
                return Ok(Outcome::Pending);
            }
        }

        // ai_history:open | ai_history:continue:<第几条，从 0 数> | ai_history:count:<n>
        "ai_history" => {
            let chat = active_ai(this, cx)?;
            match a {
                "open" => chat.update(cx, |chat, cx| chat.toggle_history_panel(cx)),
                "continue" => {
                    let index: usize = b.parse().map_err(|_| "第几条不是数字")?;
                    chat.update(cx, |chat, cx| chat.continue_history_nth(index, cx))?;
                }
                "count" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let have = chat.read_with(cx, |chat, _| chat.history_count());
                    if have != want {
                        return fail(format!("历史里有 {have} 条，不是 {want}"));
                    }
                }
                _ => return fail("ai_history 只能是 open / continue / count"),
            }
        }
        // ai_check:agent:<id> / available:<ready|missing|old|login|checking> / blocked:<yes|no>
        //   / text_has:<文字> / text_lacks:<文字> / models:<个数> / model_has:<名字> / model:<名字>
        //   / efforts:<名字,名字> / effort:<名字> / sent_model:<id|none> / shows:<too_old|login_unknown|unavailable>:<yes|no>
        "ai_check" => {
            let chat = active_ai(this, cx)?;
            match a {
                "agent" => {
                    let have = chat.read_with(cx, |chat, _| chat.agent_id());
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "available" => {
                    let have = chat.read_with(cx, |chat, _| chat.availability_name());
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "blocked" => {
                    let have = chat.read_with(cx, |chat, _| chat.is_blocked());
                    if have != (b == "yes") {
                        return Ok(Outcome::Pending);
                    }
                }
                "text_has" => {
                    let text = chat.read_with(cx, |chat, _| chat.notices_text());
                    if !text.contains(b) {
                        return Ok(Outcome::Pending);
                    }
                }
                "text_lacks" => {
                    let text = chat.read_with(cx, |chat, _| chat.notices_text());
                    if text.contains(b) {
                        return Ok(Outcome::Pending);
                    }
                }
                "models" => {
                    let want: usize = b.parse().map_err(|_| "个数不是数字")?;
                    let have = chat.read_with(cx, |chat, _| chat.model_labels().len());
                    if have != want {
                        return Ok(Outcome::Pending);
                    }
                }
                "model_has" => {
                    let labels = chat.read_with(cx, |chat, _| chat.model_labels());
                    if !labels.iter().any(|label| label == b) {
                        return Ok(Outcome::Pending);
                    }
                }
                "model" => {
                    let have = chat.read_with(cx, |chat, _| chat.model_label_now());
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "efforts" => {
                    let have = chat.read_with(cx, |chat, _| chat.effort_labels()).join(",");
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "effort" => {
                    let have = chat.read_with(cx, |chat, _| chat.effort_label_now());
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "sent_model" => {
                    let have = chat
                        .read_with(cx, |chat, _| chat.model_for_send_now())
                        .unwrap_or_else(|| "none".to_owned());
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "shows" => {
                    let have = chat.read_with(cx, |chat, _| chat.shows(b));
                    if have != (c == "yes") {
                        return Ok(Outcome::Pending);
                    }
                }
                _ => return fail("ai_check 的参数不对"),
            }
        }

        // ---- 设置页 AI 一节 ----
        // agents_text:<智能体>:<文字>：这个块的文字里要有这段（agents_text:<智能体>:!<文字> 表示不能有）
        "agents_text" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            let text = view.read_with(cx, |view, _| view.dev_text(kind));
            match b.strip_prefix('!') {
                Some(absent) if text.contains(absent) => return fail("块里不该有这段文字"),
                Some(_) => {}
                None if !text.contains(b) => return fail("块里没有预期的文字"),
                None => {}
            }
        }
        "agents_manual" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            let on = match b {
                "on" => true,
                "off" => false,
                _ => return fail("参数只能是 on 或 off"),
            };
            view.update(cx, |view, cx| view.dev_set_manual(kind, on, window, cx));
        }
        "agents_path" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            let text = if c.is_empty() {
                b.to_owned()
            } else {
                format!("{b}:{c}")
            };
            view.update(cx, |view, cx| view.dev_type_path(kind, &text, window, cx));
        }
        "agents_probe" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            view.update(cx, |view, cx| view.dev_probe_and_save(kind, cx));
        }
        "agents_recheck" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            view.update(cx, |view, cx| view.dev_recheck(kind, cx));
        }
        // agents_download:start|confirm|cancel
        "agents_download" => {
            let view = agents_view(this)?;
            view.update(cx, |view, cx| view.dev_download(a, cx))?;
        }
        // agents_check:state:<idle|fetching|confirm|running> / saved:<智能体>:<有|无> / manual:<智能体>:<yes|no>
        //   / download_text:<文字> / clear_note:<文字> / default:<id> / exec_mode:<值>
        "agents_check" => {
            let view = agents_view(this)?;
            match a {
                "state" => {
                    let have = view.read_with(cx, |view, _| view.dev_download_state());
                    // 下载流程是异步的：没到就等（单步最多等 40 秒）。
                    if have != b {
                        return Ok(Outcome::Pending);
                    }
                }
                "download_text" => {
                    let text = view.read_with(cx, |view, _| view.dev_download_text());
                    if !text.contains(b) {
                        return Ok(Outcome::Pending);
                    }
                }
                "saved" => {
                    let kind = kind_of(b)?;
                    let have = view.read_with(cx, |view, _| view.dev_saved_path(kind));
                    match (c, have) {
                        ("none", None) => {}
                        ("none", Some(_)) => return fail("不该保存过路径"),
                        (want, Some(have)) if have == want => {}
                        (_, other) => return fail(format!("保存的路径是 {other:?}")),
                    }
                }
                "manual" => {
                    let kind = kind_of(b)?;
                    let have = view.read_with(cx, |view, _| view.dev_is_manual(kind));
                    if have != (c == "yes") {
                        return fail(format!("手动模式是 {have}"));
                    }
                }
                "clear_note" => {
                    let note = view.read_with(cx, |view, _| view.dev_clear_note());
                    if !note.is_some_and(|note| note.contains(b)) {
                        return fail("清空历史的提示不对");
                    }
                }
                "default" => {
                    let have = view.read_with(cx, |view, _| view.dev_default_agent());
                    if have != b {
                        return fail(format!("默认智能体是 {have}，不是 {b}"));
                    }
                }
                "exec_mode" => {
                    let have = view.read_with(cx, |view, _| view.dev_exec_mode());
                    if have != b {
                        return fail(format!("执行模式是 {have}，不是 {b}"));
                    }
                }
                _ => return fail("agents_check 的参数不对"),
            }
        }
        // 等某个智能体块的文字里出现这段（检测是异步的）。
        "agents_wait_status" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            let text = view.read_with(cx, |view, _| view.dev_text(kind));
            if !text.contains(b) {
                return Ok(Outcome::Pending);
            }
        }
        "agents_default" => {
            let view = agents_view(this)?;
            let kind = kind_of(a)?;
            view.update(cx, |view, cx| view.dev_set_default_agent(kind, cx));
        }
        "agents_exec_mode" => {
            let view = agents_view(this)?;
            view.update(cx, |view, cx| view.dev_set_exec_mode(a, cx));
        }
        "agents_clear_history" => {
            let view = agents_view(this)?;
            view.update(cx, |view, cx| view.dev_clear_history(cx));
        }
        _ => return fail("不认识的动作"),
    }
    Ok(Outcome::Done)
}
