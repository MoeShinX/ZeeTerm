//! 调试驱动里 V2-A5（Tokenway 一键连接）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里只剩
//! 两个空函数。按钮都走和界面同一批方法，断言不对就让脚本失败（`tw_wait` 开头的是"等到成立，超时失败"）。
//! 要配 `ZSSH_TOKENWAY_FAKE=1`（本机假 Tokenway，见 `agent_setup::tokenway`）：授权页自动点"授权"，
//! 不联网、不打开真实浏览器。
//!
//! - `tw_picker:open|close`；`tw_pick:<tokenway|openai|anthropic|deepseek|openrouter|ollama|custom>`：点预设列表里的一行；
//! - `tw_device:<设备名>`；`tw_connect` / `tw_cancel` / `tw_paste` / `tw_register` / `tw_close`：连接对话框里的按钮；
//! - `tw_disconnect:<接口名>`：点设置列表那一行的「断开」（再用 `act:confirm` 确认）；
//! - `tw_fake:approve:<auto|hold|deny>`、`tw_fake:release`、`tw_fake:balance:<可用分>:<总分>`、`tw_fake:topup:<on|off>`、
//!   `tw_fake:revoke_all`、`tw_fake:status:<状态码|none>`：摆弄假 Tokenway；
//! - `tw_assert:<项>:<值>`：`modal`（`ai_presets|tokenway|ai_endpoint|confirm|none`）、`phase`（`idle|waiting|exchanging|failed`）、
//!   `error_has`、`note_has`、`preset_first`（第一行是 Tokenway 且带"推荐"）、`preset_rows`（行数）、`preset_names`（逗号分隔）、
//!   `endpoint`（`<名称>|<preset_id>|<有Key yes/no>`）、`key_matches_fake`（保险库里的 Key = 假服务器最近签发的，值 yes）、
//!   `fake_requests`（假服务器收到的请求，值 `<包含的文字>=<次数>`）、`fake_opened`（"打开"过的页面包含值）、`device_sent`（假服务器收到的设备名）、
//!   `fake_key_revoked`（yes/no）、`dialog_field`（`device` 输入框的内容）；
//! - `tw_wait:<项>:<值>`：同上但等到成立；另有 `panel_balance`（标题栏余额的文字）、`panel_balance_none`、`panel_cards`（卡片种类，逗号分隔）、
//!   `row_balance`（设置页某行的余额文字，值 `<名称>|<文字>`）；
//! - `tw_card:<topup|reconnect>`：点 AI 面板里提示卡上的按钮；`tw_balance:topup`：点标题栏上的「充值」。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::ai_presets::phase_name;
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use crate::agent_setup::presets::preset_rows;
    use crate::agent_setup::tokenway;

    const KNOWN: &[&str] = &[
        "tw_picker",
        "tw_pick",
        "tw_device",
        "tw_connect",
        "tw_cancel",
        "tw_paste",
        "tw_register",
        "tw_close",
        "tw_disconnect",
        "tw_fake",
        "tw_assert",
        "tw_wait",
        "tw_card",
        "tw_balance",
    ];

    /// 活动标签自己的那条 AI 对话（每标签一条；面板没开 / 标签没建过就没有）。
    fn active_tab_chat(
        this: &MainWindow,
        cx: &App,
    ) -> Option<::gpui::Entity<crate::agent_chat::AgentChatDemo>> {
        let tab = this.workspace.read(cx).active_tab_id()?;
        this.docks.ai.get(&tab).cloned()
    }

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

    fn dialog(this: &MainWindow) -> Result<&super::super::ai_presets::TokenwayDialog, String> {
        match &this.modal {
            Some(Modal::Tokenway(dialog)) => Ok(dialog),
            _ => fail("连接 Tokenway 的对话框没开着"),
        }
    }

    fn fake() -> Result<&'static tokenway::Fake, String> {
        tokenway::fake().ok_or_else(|| "没有设 ZSSH_TOKENWAY_FAKE（或假服务没起来）".to_owned())
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
            "tw_picker" => {
                match a {
                    "open" => this.open_ai_presets(window, cx),
                    "close" => this.close_modal(window, cx),
                    _ => return fail("tw_picker:open|close"),
                }
                Ok(Outcome::Done)
            }
            "tw_pick" => {
                let title = match a {
                    "tokenway" => "Tokenway",
                    "openai" => "OpenAI",
                    "anthropic" => "Anthropic",
                    "deepseek" => "DeepSeek",
                    "openrouter" => "OpenRouter",
                    "ollama" => "Ollama",
                    "custom" => "自定义",
                    _ => return fail("tw_pick 的预设不认识"),
                };
                let rows = preset_rows(tokenway::tokenway_preset().as_ref());
                let index = rows
                    .iter()
                    .position(|row| row.title() == title)
                    .ok_or("预设列表里没有这一行")?;
                this.ai_pick_preset(index, window, cx);
                Ok(Outcome::Done)
            }
            "tw_device" => {
                let text = if b.is_empty() {
                    a.to_owned()
                } else {
                    format!("{a}:{b}")
                };
                let input = dialog(this)?.device.clone();
                input.update(cx, |i, cx| i.set_value(text, window, cx));
                Ok(Outcome::Done)
            }
            "tw_connect" => {
                dialog(this)?;
                this.tokenway_connect(window, cx);
                Ok(Outcome::Done)
            }
            "tw_cancel" => {
                dialog(this)?;
                this.tokenway_cancel(window, cx);
                Ok(Outcome::Done)
            }
            "tw_paste" => {
                dialog(this)?;
                this.tokenway_paste_key(window, cx);
                Ok(Outcome::Done)
            }
            "tw_register" => {
                dialog(this)?;
                this.tokenway_register(window, cx);
                Ok(Outcome::Done)
            }
            "tw_close" => {
                dialog(this)?;
                this.close_modal(window, cx);
                Ok(Outcome::Done)
            }
            "tw_disconnect" => {
                let id = this
                    .ai_builtin
                    .rows
                    .iter()
                    .find(|row| row.name == a)
                    .map(|row| row.id)
                    .ok_or("列表里没有这个名字的接口")?;
                this.ask_disconnect_tokenway(id, cx);
                Ok(Outcome::Done)
            }
            "tw_fake" => fake_action(a, b),
            "tw_assert" => assert_what(this, a, b, false, cx),
            "tw_wait" => assert_what(this, a, b, true, cx),
            "tw_card" => {
                let chat =
                    active_tab_chat(this, cx).ok_or_else(|| "AI 面板还没打开过".to_owned())?;
                chat.update(cx, |chat, cx| chat.dev_click_card(a, cx))?;
                Ok(Outcome::Done)
            }
            "tw_balance" => {
                match a {
                    "topup" => tokenway::open_topup()?,
                    _ => return fail("tw_balance:topup"),
                }
                Ok(Outcome::Done)
            }
            _ => fail("不认识的动作"),
        }
    }

    fn fake_action(a: &str, b: &str) -> Result<Outcome, String> {
        use zssh_core::ai::agent::builtin::tokenway::fake::Approve;
        let fake = fake()?;
        match a {
            "approve" => fake.server.set_approve(match b {
                "auto" => Approve::Auto,
                "hold" => Approve::Hold,
                "deny" => Approve::Deny,
                _ => return fail("tw_fake:approve:auto|hold|deny"),
            }),
            "release" => fake.server.release(),
            "balance" => {
                let (usable, total) = b.split_once(':').ok_or("tw_fake:balance:<可用分>:<总分>")?;
                fake.server.set_balance(
                    usable.parse().map_err(|_| "可用分不是数字")?,
                    total.parse().map_err(|_| "总分不是数字")?,
                );
            }
            "topup" => fake.server.set_needs_topup(b == "on"),
            "revoke_all" => fake.server.revoke_all(),
            "status" => {
                fake.server.set_status_override(if b == "none" {
                    None
                } else {
                    Some(b.parse().map_err(|_| "状态码不是数字")?)
                });
            }
            _ => return fail("tw_fake:approve|release|balance|topup|revoke_all|status"),
        }
        Ok(Outcome::Done)
    }

    fn verdict(ok: bool, what: String, wait: bool) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else if wait {
            Ok(Outcome::Pending)
        } else {
            fail(format!("断言没有成立：{what}"))
        }
    }

    fn assert_what(
        this: &MainWindow,
        what: &str,
        value: &str,
        wait: bool,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "modal" => {
                let have = match &this.modal {
                    None => "none",
                    Some(Modal::AiPresets) => "ai_presets",
                    Some(Modal::Tokenway(_)) => "tokenway",
                    Some(Modal::AiEndpoint(_)) => "ai_endpoint",
                    Some(Modal::Confirm(_)) => "confirm",
                    Some(_) => "other",
                };
                verdict(have == value, format!("对话框是 {have}"), wait)
            }
            "phase" => {
                let have = phase_name(&dialog(this)?.phase);
                verdict(have == value, format!("阶段是 {have}"), wait)
            }
            "error_has" => {
                let text = match &dialog(this)?.phase {
                    super::super::ai_presets::Phase::Failed(text) => text.clone(),
                    _ => String::new(),
                };
                verdict(text.contains(value), format!("错误是「{text}」"), wait)
            }
            "note_has" => {
                let text = dialog(this)?.note.clone().unwrap_or_default();
                verdict(text.contains(value), format!("提示是「{text}」"), wait)
            }
            "dialog_field" => {
                let have = dialog(this)?.device.read(cx).value().to_string();
                verdict(have == value, format!("设备名是「{have}」"), wait)
            }
            "preset_first" => {
                let rows = preset_rows(tokenway::tokenway_preset().as_ref());
                let ok = rows
                    .first()
                    .is_some_and(|row| row.title() == "Tokenway" && row.recommended());
                verdict(ok, "第一行不是带推荐的 Tokenway".to_owned(), wait)
            }
            "preset_rows" => {
                let want: usize = value.parse().map_err(|_| "行数不是数字")?;
                let have = preset_rows(tokenway::tokenway_preset().as_ref()).len();
                verdict(have == want, format!("有 {have} 行"), wait)
            }
            "preset_names" => {
                let have: Vec<String> = preset_rows(tokenway::tokenway_preset().as_ref())
                    .iter()
                    .map(|row| row.title().to_owned())
                    .collect();
                verdict(have.join(",") == value, have.join(","), wait)
            }
            "endpoint" => {
                let mut fields = value.split('|');
                let (name, preset, has_key) = (
                    fields.next().unwrap_or(""),
                    fields.next().unwrap_or(""),
                    fields.next().unwrap_or("yes"),
                );
                let db = crate::host_db::open_database().map_err(|e| e.to_string())?;
                let found = db
                    .ai_endpoint_list()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|e| e.name == name);
                let ok = found.as_ref().is_some_and(|e| {
                    e.preset_id.as_deref().unwrap_or("") == preset
                        && e.has_key == (has_key == "yes")
                });
                verdict(ok, format!("接口 {name} 现在是 {found:?}"), wait)
            }
            "key_matches_fake" => {
                let db = crate::host_db::open_database().map_err(|e| e.to_string())?;
                let id = db
                    .ai_endpoint_list()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|e| {
                        zssh_core::ai::agent::builtin::tokenway::endpoint::is_authorized_token(
                            e.preset_id.as_deref(),
                        )
                    })
                    .map(|e| e.id);
                let stored = id.and_then(|id| db.ai_endpoint_api_key(id).ok().flatten());
                let ok = stored.is_some() && stored == fake()?.server.last_key();
                verdict(
                    ok == (value == "yes"),
                    "保险库里的 Key 和假服务器签发的对不上".to_owned(),
                    wait,
                )
            }
            "fake_requests" => {
                let (needle, count) = value
                    .rsplit_once('=')
                    .ok_or("fake_requests:<文字>=<次数>")?;
                let want: usize = count.parse().map_err(|_| "次数不是数字")?;
                let have = fake()?.server.count(needle);
                verdict(have == want, format!("「{needle}」收到 {have} 次"), wait)
            }
            "fake_opened" => {
                let opened = fake()?
                    .opened
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                verdict(
                    opened.iter().any(|url| url.contains(value)),
                    format!("打开过的页面：{opened:?}"),
                    wait,
                )
            }
            "device_sent" => {
                let have = fake()?.server.last_device().unwrap_or_default();
                verdict(
                    have == value,
                    format!("假服务器收到的设备名是「{have}」"),
                    wait,
                )
            }
            "fake_key_revoked" => {
                let key = fake()?.server.last_key().ok_or("还没签发过令牌")?;
                let have = fake()?.server.key_is_revoked(&key) == Some(true);
                verdict(
                    have == (value == "yes"),
                    format!("令牌停用状态 {have}"),
                    wait,
                )
            }
            "panel_balance" => {
                let chat =
                    active_tab_chat(this, cx).ok_or_else(|| "AI 面板还没打开过".to_owned())?;
                let have = chat.read_with(cx, |chat, _| chat.dev_balance_label());
                verdict(
                    have.as_deref().is_some_and(|label| label.contains(value)),
                    format!("标题栏余额是 {have:?}"),
                    wait,
                )
            }
            "panel_balance_none" => {
                let chat =
                    active_tab_chat(this, cx).ok_or_else(|| "AI 面板还没打开过".to_owned())?;
                let have = chat.read_with(cx, |chat, _| chat.dev_balance_label());
                verdict(have.is_none(), format!("标题栏余额是 {have:?}"), wait)
            }
            "panel_cards" => {
                let chat =
                    active_tab_chat(this, cx).ok_or_else(|| "AI 面板还没打开过".to_owned())?;
                let have = chat.read_with(cx, |chat, _| chat.dev_cards().join(","));
                verdict(have == value, format!("提示卡：{have}"), wait)
            }
            "row_balance" => {
                let (name, text) = value.split_once('|').ok_or("row_balance:<名称>|<文字>")?;
                let id = this
                    .ai_builtin
                    .rows
                    .iter()
                    .find(|row| row.name == name)
                    .map(|row| row.id)
                    .ok_or("列表里没有这个名字的接口")?;
                let have = tokenway::balance_view(id).map(|view| view.label);
                verdict(
                    have.as_deref().is_some_and(|label| label.contains(text)),
                    format!("设置页余额是 {have:?}"),
                    wait,
                )
            }
            _ => fail("tw_assert 的项不认识"),
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
