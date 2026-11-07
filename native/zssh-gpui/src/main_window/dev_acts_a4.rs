//! 调试驱动里 V2-A4（设置页"内置助手"接口管理、添加 / 编辑对话框）要用的 `act:` 动作。只在 debug 构建里
//! 编进去；release 里这里只剩两个空函数。按钮都走和界面同一批方法，`aib_assert` 不对就让脚本失败：
//!
//! - `aib_seed:<名称>|<openai|anthropic>|<Base URL>|<Key>|<默认模型>[|default][|model:<名>][|notools:<名>]`：绕过对话框
//!   直接往库里写一个接口（造场景用；`default` 同时设为默认接口，`model:` 加一个手填模型，`notools:` 加一个
//!   手动标成不支持工具调用的模型）；`aib_clear`：删光所有接口；
//! - `aib_agent:<接口名>`：点智能体下拉里内置助手的这个接口；
//! - `aib_panel:<label|menu|no_tools|window|reply_lacks>:<值>`：AI 面板里内置助手相关的检查（不成立就等，超时失败）；
//! - `aib_dialog:new` / `aib_dialog:edit:<名称>` / `aib_dialog:close`：开 / 关添加（编辑）对话框；
//! - `aib_fill:<name|url|key|model|extra|context|output|pick_query>:<文字>`：往对话框的输入框里填字
//!   （`extra` 里的 `\n` 是换行）；`aib_kind:<openai|anthropic>`；`aib_eye`：点 Key 框的小眼睛；
//! - `aib_advanced:<on|off>`；`aib_thinking:<自动|不支持|开关|等级|预算>`；`aib_force:<tools|image>:<auto|yes|no>`；
//!   `aib_option:<max_completion|echo|thinking>:<auto|on|off>`、`aib_option:usage:<on|off>`；
//!   `aib_pick:<条目键>`（改选匹配）；`aib_pick_model:<模型名>`（点"默认模型"旁下拉里的一项）；
//! - `aib_test`：点"测试"；`aib_wait_test`：等测试结束；`aib_save`：点"保存"；`aib_scroll:<top|bottom>`：对话框滚到顶 / 底；
//! - `aib_default:<名称>`：点某行的"设为默认"；`aib_ask_delete:<名称>`：点某行的"删除"（再用 `act:confirm` 确认）；
//! - `aib_catalog:refresh` / `aib_catalog:auto` / `aib_wait_catalog`：模型资料库的"立即更新" / 自动更新开关；
//! - `aib_assert:<项>:<值>`：`rows`（行数）、`row_has:<名称>|<文字>`、`default:<名称>`、`test:<ok|fail|none|running>`、
//!   `test_has:<文字>`、`caps_has:<文字>`、`error_has:<文字>`、`key_text:<空串|非空>`、`key_masked:<yes|no>`、
//!   `catalog_has:<文字>`、`auto:<on|off>`、`models_has:<模型名>`、`title:<添加接口|编辑接口>`、
//!   `field:<name|url|model|extra|context|output>:<文字>`、`url_placeholder:<文字>`。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::ai::agent::builtin::config::{
        AiEndpointDraft, EndpointKind, OpenAiOptionsConfig, Tri,
    };

    use super::super::MainWindow;
    use super::super::ai_endpoint_dialog::{
        AiEndpointDialog, Force, kind_label, thinking_choices, thinking_from_choice,
    };
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use crate::agent_setup::builtin::{self, caps_summary, source_label};

    const KNOWN: &[&str] = &[
        "aib_seed",
        "aib_clear",
        "aib_dialog",
        "aib_fill",
        "aib_kind",
        "aib_eye",
        "aib_advanced",
        "aib_thinking",
        "aib_force",
        "aib_option",
        "aib_pick",
        "aib_pick_model",
        "aib_override",
        "aib_test",
        "aib_fetch_models",
        "aib_wait_test",
        "aib_save",
        "aib_scroll",
        "aib_default",
        "aib_ask_delete",
        "aib_catalog",
        "aib_wait_catalog",
        "aib_panel",
        "aib_agent",
        "aib_assert",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    fn fail<T>(message: impl Into<String>) -> Result<T, String> {
        Err(message.into())
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(run_inner(this, name, a, b, window, cx))
    }

    fn dialog(this: &MainWindow) -> Result<&AiEndpointDialog, String> {
        match &this.modal {
            Some(Modal::AiEndpoint(dialog)) => Ok(dialog),
            _ => fail("添加 / 编辑接口的对话框没开着"),
        }
    }

    fn dialog_mut(this: &mut MainWindow) -> Result<&mut AiEndpointDialog, String> {
        match &mut this.modal {
            Some(Modal::AiEndpoint(dialog)) => Ok(dialog),
            _ => fail("添加 / 编辑接口的对话框没开着"),
        }
    }

    fn row_id(this: &MainWindow, name: &str) -> Result<i64, String> {
        this.ai_builtin
            .rows
            .iter()
            .find(|row| row.name == name)
            .map(|row| row.id)
            .ok_or_else(|| "列表里没有这个名字的接口".to_owned())
    }

    fn kind_of(raw: &str) -> Result<EndpointKind, String> {
        match raw {
            "openai" => Ok(EndpointKind::OpenAiChat),
            "anthropic" => Ok(EndpointKind::Anthropic),
            _ => fail("类型只能是 openai / anthropic"),
        }
    }

    fn tri_of(raw: &str) -> Result<Tri, String> {
        match raw {
            "auto" => Ok(Tri::Auto),
            "on" => Ok(Tri::On),
            "off" => Ok(Tri::Off),
            _ => fail("只能是 auto / on / off"),
        }
    }

    fn check(ok: bool, what: String) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else {
            fail(format!("断言没有成立：{what}"))
        }
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        a: &str,
        b: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "aib_seed" => {
                // 地址里有冒号，被按冒号切开了：拼回去再按竖线分字段。
                let arg = if b.is_empty() {
                    a.to_owned()
                } else {
                    format!("{a}:{b}")
                };
                let fields: Vec<&str> = arg.split('|').collect();
                seed(this, &fields, cx)
            }
            "aib_clear" => {
                let db = crate::host_db::open_database().map_err(|e| e.to_string())?;
                for endpoint in db.ai_endpoint_list().map_err(|e| e.to_string())? {
                    db.ai_endpoint_delete(endpoint.id)
                        .map_err(|e| e.to_string())?;
                }
                builtin::reload();
                this.ai_builtin.reload_rows();
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_dialog" => {
                match a {
                    "new" => this.open_ai_endpoint_dialog(None, window, cx),
                    "edit" => {
                        let id = row_id(this, b)?;
                        this.open_ai_endpoint_dialog(Some(id), window, cx);
                    }
                    "close" => this.close_modal(window, cx),
                    _ => return fail("aib_dialog:new|edit:<名称>|close"),
                }
                Ok(Outcome::Done)
            }
            "aib_fill" => {
                let text = b.replace("\\n", "\n");
                let d = dialog(this)?;
                match a {
                    "name" => d.name.update(cx, |i, cx| i.set_value(text, window, cx)),
                    "url" => d.base_url.update(cx, |i, cx| i.set_value(text, window, cx)),
                    "key" => d.api_key.update(cx, |i, cx| i.set_value(text, window, cx)),
                    "model" => d
                        .default_model
                        .update(cx, |i, cx| i.set_value(text, window, cx)),
                    "extra" => d
                        .extra_models
                        .update(cx, |i, cx| i.set_value(text, window, cx)),
                    "context" => d
                        .cap_context
                        .update(cx, |i, cx| i.set_value(text, window, cx)),
                    "output" => d
                        .cap_output
                        .update(cx, |i, cx| i.set_value(text, window, cx)),
                    "pick_query" => d
                        .pick_query
                        .update(cx, |i, cx| i.set_value(text, window, cx)),
                    _ => return fail("aib_fill 的字段不认识"),
                }
                // 和手敲一样：默认模型换了，覆盖项、测试结果跟着新模型换（程序里 set_value 不发"变了"事件）。
                if a == "model" {
                    this.ai_dialog_model_changed(window, cx);
                }
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_kind" => {
                let kind = kind_of(a)?;
                dialog(this)?;
                this.ai_dialog_set_kind(kind, window, cx);
                Ok(Outcome::Done)
            }
            "aib_eye" => {
                dialog(this)?;
                this.ai_dialog_toggle_key(window, cx);
                Ok(Outcome::Done)
            }
            "aib_advanced" => {
                let on = match a {
                    "on" => true,
                    "off" => false,
                    _ => return fail("on / off"),
                };
                dialog_mut(this)?.advanced = on;
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_thinking" => {
                let index = thinking_choices()
                    .iter()
                    .position(|label| *label == a)
                    .ok_or("思考只能是 自动 / 不支持 / 开关 / 等级 / 预算")?;
                let d = dialog_mut(this)?;
                let next = thinking_from_choice(index, &d.cap_fields.thinking);
                d.cap_fields.thinking = next;
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_force" => {
                let force = match b {
                    "auto" => Force::Auto,
                    "yes" => Force::Yes,
                    "no" => Force::No,
                    _ => return fail("auto / yes / no"),
                };
                let d = dialog_mut(this)?;
                match a {
                    "tools" => d.cap_fields.tools = force,
                    "image" => d.cap_fields.image = force,
                    _ => return fail("tools / image"),
                }
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_option" => {
                let d = dialog_mut(this)?;
                match a {
                    "max_completion" => d.options.max_completion_tokens = tri_of(b)?,
                    "echo" => d.options.echo_reasoning_content = tri_of(b)?,
                    "thinking" => d.options.thinking_toggle = tri_of(b)?,
                    "usage" => d.options.include_usage = b == "on",
                    _ => return fail("max_completion / echo / thinking / usage"),
                }
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_pick" => {
                let key = b.to_owned();
                let d = dialog_mut(this)?;
                d.cap_fields.pick = (!key.is_empty()).then_some(key);
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_pick_model" => {
                dialog(this)?;
                this.ai_dialog_pick_model(a.to_owned(), window, cx);
                Ok(Outcome::Done)
            }
            // aib_override:<模型>：在高级里的「要覆盖的模型」下拉里选一个（V2-A5）。
            "aib_override" => {
                dialog(this)?;
                this.ai_dialog_pick_override_model(a.to_owned(), window, cx);
                Ok(Outcome::Done)
            }
            "aib_test" => {
                dialog(this)?;
                this.ai_endpoint_test(window, cx);
                Ok(Outcome::Done)
            }
            "aib_fetch_models" => {
                dialog(this)?;
                this.ai_endpoint_fetch_models(window, cx);
                Ok(Outcome::Done)
            }
            "aib_wait_test" => {
                if dialog(this)?.test.running {
                    return Ok(Outcome::Pending);
                }
                Ok(Outcome::Done)
            }
            "aib_scroll" => {
                let d = dialog(this)?;
                match a {
                    "bottom" => d.scroll.scroll_to_bottom(),
                    "top" => d
                        .scroll
                        .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(0.0))),
                    other => {
                        let px: f32 = other
                            .parse()
                            .map_err(|_| "aib_scroll:bottom|top|<像素>".to_owned())?;
                        d.scroll
                            .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(-px)));
                    }
                }
                cx.notify();
                Ok(Outcome::Done)
            }
            "aib_save" => {
                dialog(this)?;
                this.ai_endpoint_save(window, cx);
                Ok(Outcome::Done)
            }
            "aib_default" => {
                let id = row_id(this, a)?;
                this.ai_set_default_endpoint(id, window, cx);
                Ok(Outcome::Done)
            }
            "aib_ask_delete" => {
                let id = row_id(this, a)?;
                this.ask_delete_ai_endpoint(id, cx);
                Ok(Outcome::Done)
            }
            "aib_catalog" => {
                match a {
                    "refresh" => this.ai_catalog_refresh(window, cx),
                    "auto" => this.ai_set_auto_update(window, cx),
                    _ => return fail("aib_catalog:refresh|auto"),
                }
                Ok(Outcome::Done)
            }
            "aib_wait_catalog" => {
                if this.ai_builtin.catalog_busy {
                    return Ok(Outcome::Pending);
                }
                Ok(Outcome::Done)
            }
            "aib_panel" => panel_check(this, a, b, cx),
            // aib_agent:<接口名>：在 AI 面板的智能体下拉里点内置助手的这一项。
            "aib_agent" => {
                let chat = this
                    .docks
                    .ai
                    .clone()
                    .ok_or_else(|| "AI 面板还没打开过".to_owned())?;
                let target = if b.is_empty() {
                    a.to_owned()
                } else {
                    format!("{a}:{b}")
                };
                chat.update(cx, |chat, cx| {
                    chat.switch_agent_by_id(&format!("builtin:{target}"), cx)
                })?;
                Ok(Outcome::Done)
            }
            "aib_assert" => assert_what(this, a, b, cx),
            _ => fail("不认识的动作"),
        }
    }

    /// AI 面板里和内置助手有关的检查（等条件成立：不成立返回 Pending，脚本超时就失败）。
    fn panel_check(
        this: &MainWindow,
        what: &str,
        value: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let chat = this
            .docks
            .ai
            .clone()
            .ok_or_else(|| "AI 面板还没打开过".to_owned())?;
        let ok = match what {
            "label" => chat.read_with(cx, |chat, _| chat.dev_agent_label()) == value,
            "menu" => {
                chat.read_with(cx, |chat, _| chat.dev_builtin_menu())
                    .join("|")
                    == value
            }
            "no_tools" => {
                chat.read_with(cx, |chat, _| chat.dev_no_tools_hint()) == (value == "yes")
            }
            "window" => {
                let have = chat.read_with(cx, |chat, _| chat.dev_usage_window());
                let want: Option<(u64, bool)> = match value {
                    "none" => None,
                    other => {
                        let (n, est) = other.split_once(',').ok_or("window:<n>,<est|exact>")?;
                        Some((n.parse().map_err(|_| "窗口不是数字")?, est == "est"))
                    }
                };
                have == want
            }
            "reply_lacks" => !chat
                .read_with(cx, |chat, _| chat.last_assistant_text())
                .unwrap_or_default()
                .contains(value),
            _ => return fail("aib_panel:label|menu|no_tools|window|reply_lacks"),
        };
        if ok {
            Ok(Outcome::Done)
        } else {
            Ok(Outcome::Pending)
        }
    }

    fn seed(
        this: &mut MainWindow,
        fields: &[&str],
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        if fields.len() < 5 {
            return fail("aib_seed:<名称>|<类型>|<Base URL>|<Key>|<默认模型>[|default]");
        }
        let draft = AiEndpointDraft {
            id: None,
            name: fields[0].to_owned(),
            kind: kind_of(fields[1])?,
            base_url: fields[2].to_owned(),
            api_key: (!fields[3].is_empty()).then(|| fields[3].to_owned()),
            default_model: (!fields[4].is_empty()).then(|| fields[4].to_owned()),
            models: Vec::new(),
            overrides: Default::default(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: None,
        };
        let db = crate::host_db::open_database().map_err(|e| e.to_string())?;
        let mut draft = draft;
        let mut make_default = false;
        for extra in fields.iter().skip(5) {
            if *extra == "default" {
                make_default = true;
            } else if let Some(model) = extra.strip_prefix("notools:") {
                // 给这个模型加一条"不支持工具调用"的手动覆盖（造"只能聊天"的场景）。
                draft.models.push(model.to_owned());
                draft.overrides.insert(
                    model.to_owned(),
                    zssh_core::ai::agent::builtin::catalog::CapsOverride {
                        tool_call: Some(false),
                        ..Default::default()
                    },
                );
            } else if let Some(model) = extra.strip_prefix("model:") {
                draft.models.push(model.to_owned());
            }
        }
        let saved = db.ai_endpoint_save(&draft).map_err(|e| e.to_string())?;
        if make_default {
            db.set_ai_builtin_default(Some((saved.id, saved.default_model.as_deref())))
                .map_err(|e| e.to_string())?;
        }
        builtin::reload();
        this.ai_builtin.reload_rows();
        cx.notify();
        Ok(Outcome::Done)
    }

    fn assert_what(
        this: &MainWindow,
        what: &str,
        value: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "rows" => {
                let want: usize = value.parse().map_err(|_| "个数不是数字")?;
                check(
                    this.ai_builtin.rows.len() == want,
                    format!("接口行数 {} 不是 {want}", this.ai_builtin.rows.len()),
                )
            }
            "row_has" => {
                let (name, text) = value.split_once('|').ok_or("row_has:<名称>|<文字>")?;
                let row = this
                    .ai_builtin
                    .rows
                    .iter()
                    .find(|row| row.name == name)
                    .ok_or("列表里没有这个名字的接口")?;
                let detail = format!(
                    "{} {} {}",
                    kind_label(row.kind),
                    super::super::ai_builtin::row_detail(row),
                    if row.is_default { "默认接口" } else { "" }
                );
                check(detail.contains(text), format!("接口行没有包含「{text}」"))
            }
            // override_target:<模型>：手动覆盖现在对应哪个模型；override_ctx:<模型>|<n>：那个模型的手动上下文窗口。
            "override_target" => {
                let d = dialog(this)?;
                check(
                    d.override_model == value,
                    format!("覆盖目标是「{}」", d.override_model),
                )
            }
            "override_ctx" => {
                let (model, n) = value.split_once('|').ok_or("override_ctx:<模型>|<n>")?;
                let want: u32 = n.parse().map_err(|_| "窗口不是数字")?;
                let d = dialog(this)?;
                let have = d.manual_for(model, cx).and_then(|item| item.context_window);
                check(have == Some(want), format!("{model} 的手动窗口是 {have:?}"))
            }
            "default" => {
                let row = this
                    .ai_builtin
                    .rows
                    .iter()
                    .find(|row| row.is_default)
                    .map(|row| row.name.as_str());
                check(
                    row == (value != "none").then_some(value),
                    format!("默认接口是 {row:?}"),
                )
            }
            "test" => {
                let d = dialog(this)?;
                let have = if d.test.running {
                    "running"
                } else {
                    match &d.test.result {
                        None => "none",
                        Some(result) if result.ok => "ok",
                        Some(_) => "fail",
                    }
                };
                check(have == value, format!("测试状态是 {have}，不是 {value}"))
            }
            "test_has" => {
                let d = dialog(this)?;
                let text = d
                    .test
                    .result
                    .as_ref()
                    .map(|r| r.lines.join("\n"))
                    .unwrap_or_default();
                check(text.contains(value), format!("测试结果里没有「{value}」"))
            }
            "caps_has" => {
                let d = dialog(this)?;
                let (model, caps) = d
                    .caps_now(&builtin::catalog(), cx)
                    .ok_or("没填默认模型，没有识别结果")?;
                let mut text = format!(
                    "识别为：{}（来源：{}）",
                    if caps.display_name.is_empty() {
                        model
                    } else {
                        caps.display_name.clone()
                    },
                    source_label(caps.source)
                );
                for (name, line) in caps_summary(&caps) {
                    text.push_str(&format!("\n{name} {line}"));
                }
                check(
                    text.contains(value),
                    format!("识别结果里没有「{value}」：{text}"),
                )
            }
            "error_has" => {
                let d = dialog(this)?;
                let text = d
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_default();
                check(
                    text.contains(value),
                    format!("表单错误里没有「{value}」：{text}"),
                )
            }
            "key_text" => {
                let d = dialog(this)?;
                let empty = d.api_key.read(cx).value().is_empty();
                check(
                    empty == (value == "空串"),
                    "Key 框的内容和预期不同".to_owned(),
                )
            }
            "key_masked" => {
                let d = dialog(this)?;
                check(
                    d.key_shown == (value == "no"),
                    "Key 框的显示 / 隐藏状态和预期不同".to_owned(),
                )
            }
            "catalog_has" => check(
                this.ai_builtin.catalog_line.contains(value),
                format!("资料库那行是「{}」", this.ai_builtin.catalog_line),
            ),
            "auto" => check(
                this.ai_builtin.auto_update == (value == "on"),
                "自动更新开关和预期不同".to_owned(),
            ),
            "models_has" => {
                let d = dialog(this)?;
                check(
                    d.model_choices(cx).iter().any(|m| m == value),
                    format!("模型下拉里没有 {value}"),
                )
            }
            "title" => {
                let d = dialog(this)?;
                let have = if d.id.is_some() {
                    "编辑接口"
                } else {
                    "添加接口"
                };
                check(have == value, format!("对话框标题是 {have}"))
            }
            "field" => {
                let (field, want) = value.split_once(':').ok_or("field:<项>:<文字>")?;
                let d = dialog(this)?;
                let have = match field {
                    "name" => d.name.read(cx).value().to_string(),
                    "url" => d.base_url.read(cx).value().to_string(),
                    "model" => d.default_model.read(cx).value().to_string(),
                    "extra" => d.extra_models.read(cx).value().to_string(),
                    "context" => d.cap_context.read(cx).value().to_string(),
                    "output" => d.cap_output.read(cx).value().to_string(),
                    _ => return fail("field 的项不认识"),
                };
                check(
                    have == want.replace("\\n", "\n"),
                    format!("{field} 是「{have}」"),
                )
            }
            "url_placeholder" => {
                let d = dialog(this)?;
                check(
                    super::super::ai_endpoint_dialog::example_url(d.kind) == value,
                    format!(
                        "示例地址是 {}",
                        super::super::ai_endpoint_dialog::example_url(d.kind)
                    ),
                )
            }
            _ => fail("aib_assert 的项不认识"),
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
