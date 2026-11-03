//! 设置 → AI → "内置助手"一节（V2-A4）：接口列表、设为默认、删除、模型资料库的显示与更新。
//!
//! 添加 / 编辑在 [`super::ai_endpoint_dialog`]。这里是列表那一块：每个接口一行（名称、类型、Base URL、
//! Key 末 4 位、默认模型、是不是默认接口），右上"添加接口"；下面是"模型资料：……"一行、
//! "立即更新"和"自动更新（每 7 天）"开关。接口的存取、能力识别、资料库更新都是 core 的，这里只拼装和画。
//!
//! 保存 / 删除 / 改默认之后调 `agent_setup::builtin::reload()`，AI 面板据版本号变了就重读。

use ::gpui::{SharedString, Window};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::agent::builtin::config::EndpointKind;
use zssh_core::store::Database;

use super::MainWindow;
use super::ai_endpoint_dialog::kind_label;
use super::modal::{Confirm, ConfirmAction, Modal};
use super::widgets::{BadgeKind, ButtonLook, badge, note, text_button};
use crate::agent_setup::builtin;
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 列表里的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: i64,
    pub name: String,
    pub kind: EndpointKind,
    pub base_url: String,
    /// "…ab12"：Key 的末 4 位；没有 Key 是 `None`。
    pub key_tail: Option<String>,
    /// 明确设成不需要 Key（Ollama 之类）。
    pub no_key: bool,
    /// 来自哪个预设（`tokenway` = 浏览器授权的令牌，`tokenway_key` = 自己粘贴的 Key）。
    pub preset_id: Option<String>,
    pub default_model: Option<String>,
    pub is_default: bool,
}

/// Key 末 4 位的显示：太短的 Key 不露字符（只显示圆点），避免把整个 Key 摆出来。
pub fn key_tail(key: &str) -> Option<String> {
    let key = key.trim();
    let count = key.chars().count();
    if count == 0 {
        return None;
    }
    if count < 12 {
        return Some("••••".to_owned());
    }
    let tail: String = key.chars().skip(count - 4).collect();
    Some(format!("••••{tail}"))
}

/// 设置页这一块的状态。
#[derive(Default)]
pub struct AiBuiltinUi {
    pub rows: Vec<Row>,
    /// 读过的 `agent_setup::builtin::version()`：变了就重读。
    seen_version: Option<u64>,
    pub catalog_line: String,
    pub auto_update: bool,
    /// 正在"立即更新"。
    pub catalog_busy: bool,
    /// 上一次操作的提示（文字，是不是出错）。
    pub note: Option<(String, bool)>,
}

impl AiBuiltinUi {
    /// 从设置库重读接口行（保存 / 删除 / 改默认之后，或进程级清单变了）。
    pub fn reload_rows(&mut self) {
        let Ok(db) = crate::hosts_demo::open_database() else {
            self.rows.clear();
            return;
        };
        self.rows = read_rows(&db);
        self.seen_version = Some(builtin::version());
        self.catalog_line = builtin::describe_catalog(&builtin::catalog());
        self.auto_update = db.ai_models_auto_update().unwrap_or(true);
    }

    /// 进程级的版本号变了就重读；返回有没有变化。
    pub fn refresh_if_changed(&mut self) -> bool {
        let version = builtin::version();
        if self.seen_version == Some(version) {
            return false;
        }
        self.reload_rows();
        true
    }
}

pub fn read_rows(db: &Database) -> Vec<Row> {
    let endpoints = db.ai_endpoint_list().unwrap_or_default();
    let default = db.ai_builtin_default().ok().flatten();
    endpoints
        .into_iter()
        .map(|endpoint| {
            let tail = endpoint
                .has_key
                .then(|| db.ai_endpoint_api_key(endpoint.id).ok().flatten())
                .flatten()
                .and_then(|key| key_tail(&key));
            let no_key = endpoint.key_not_needed();
            Row {
                is_default: default
                    .as_ref()
                    .is_some_and(|choice| choice.endpoint_id == endpoint.id),
                id: endpoint.id,
                name: endpoint.name,
                kind: endpoint.kind,
                base_url: endpoint.base_url,
                key_tail: tail,
                no_key,
                preset_id: endpoint.preset_id,
                default_model: endpoint.default_model,
            }
        })
        .collect()
}

/// 行上的补充说明："地址 · Key …ab12 · 默认模型 xxx"。
pub fn row_detail(row: &Row) -> String {
    let mut parts = vec![row.base_url.clone()];
    parts.push(match &row.key_tail {
        Some(tail) => format!("Key {tail}"),
        None if row.no_key => "不需要 Key".to_owned(),
        None => "未设置 Key".to_owned(),
    });
    if let Some(model) = &row.default_model {
        parts.push(format!("默认模型 {model}"));
    }
    parts.join(" · ")
}

impl MainWindow {
    /// 设置页 AI 分类里最上面的一块。
    pub(super) fn ai_builtin_block(
        &mut self,
        tokens: &Tokens,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> ::gpui::AnyElement {
        let _ = window;
        if self.ai_builtin.refresh_if_changed() {
            cx.notify();
        }
        let entity = cx.entity();
        let ui = &self.ai_builtin;
        let mut rows: Vec<::gpui::AnyElement> = Vec::new();

        // 头部：一句说明 + 添加接口。
        let add = entity.clone();
        rows.push(
            h_flex()
                .w_full()
                .gap_4()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .typo(Role::ItemName, tokens)
                                .child("ZeePilot"),
                        )
                        .child(div().typo(Role::Caption, tokens).child(
                            "填一个接口地址和 API Key 就能用，不用装别的工具。支持 OpenAI 兼容和 Anthropic 两种接口；配好后，AI 面板默认用它。",
                        )),
                )
                .child(
                    Button::new("ai-endpoint-add")
                        .label("添加接口")
                        .compact()
                        .primary()
                        .on_click(move |_, window, cx| {
                            add.update(cx, |this, cx| {
                                this.open_ai_presets(window, cx)
                            });
                        }),
                )
                .into_any_element(),
        );

        if ui.rows.is_empty() {
            rows.push(
                note(
                    "还没有接口。点右上的「添加接口」，填上名称、地址和 Key；填好后可以点「测试」确认能用。",
                    tokens,
                )
                .into_any_element(),
            );
        }
        for row in &ui.rows {
            rows.push(self.ai_endpoint_row(row, &entity, tokens));
        }

        // 模型资料库。
        let refresh = entity.clone();
        let busy = ui.catalog_busy;
        rows.push(
            h_flex()
                .w_full()
                .gap_4()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().typo(Role::ItemName, tokens).child("模型资料库"))
                        .child(
                            div()
                                .id("ai-catalog-line")
                                .typo(Role::Caption, tokens)
                                .child(SharedString::from(ui.catalog_line.clone())),
                        )
                        .child(div().typo(Role::Caption, tokens).child(
                            "来自 models.dev，用来识别模型的上下文窗口、思考、工具调用等能力；识别不对可以在接口的「高级」里手动改。",
                        ))
                        .children(ui.note.as_ref().map(|(text, error)| {
                            div()
                                .text_xs()
                                .text_color(if *error {
                                    tokens.danger
                                } else {
                                    tokens.dot_connected
                                })
                                .child(SharedString::from(text.clone()))
                        })),
                )
                .child(
                    text_button(
                        "ai-catalog-refresh",
                        if busy { "更新中…" } else { "立即更新" },
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            refresh.update(cx, |this, cx| this.ai_catalog_refresh(window, cx));
                        },
                    )
                    .disabled(busy),
                )
                .into_any_element(),
        );
        let toggle = entity.clone();
        rows.push(
            h_flex()
                .w_full()
                .gap_4()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().typo(Role::ItemName, tokens).child("自动更新（每 7 天）"))
                        .child(div().typo(Role::Caption, tokens).child(
                            "启动后在后台联网更新模型资料库（走「软件网络」的代理设置）；关掉就只用内置快照和已有的缓存。只存在这台电脑上。",
                        )),
                )
                .child(
                    Switch::new("ai-catalog-auto")
                        .checked(ui.auto_update)
                        .on_click(move |_, window, cx| {
                            toggle.update(cx, |this, cx| this.ai_set_auto_update(window, cx));
                        }),
                )
                .into_any_element(),
        );

        crate::settings_view::card(
            "ai-builtin",
            "ZeePilot",
            Some("ZeeTerm 自带的 AI 助手：填一个接口地址和 API Key 就能用。"),
            tokens,
            rows,
        )
    }

    fn ai_endpoint_row(
        &self,
        row: &Row,
        entity: &gpui_kit::Entity<Self>,
        tokens: &Tokens,
    ) -> ::gpui::AnyElement {
        let id = row.id;
        let (edit, delete, set_default) = (entity.clone(), entity.clone(), entity.clone());
        // Tokenway（浏览器授权的令牌 / 自己粘贴的 Key）：删除按钮叫「断开」，授权的令牌还显示余额。
        let tokenway = super::ai_presets::is_tokenway_row(row.preset_id.as_deref());
        let authorized = row.preset_id.as_deref()
            == Some(zssh_core::ai::agent::builtin::tokenway::endpoint::PRESET_AUTHORIZED);
        let balance = authorized
            .then(|| super::ai_presets::balance_text(id))
            .flatten();
        h_flex()
            .id(("ai-endpoint-row", id as usize))
            .w_full()
            .gap_4()
            .items_center()
            .justify_between()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .typo(Role::ItemName, tokens)
                                    .child(SharedString::from(row.name.clone())),
                            )
                            .child(badge(kind_label(row.kind), BadgeKind::Outline, tokens))
                            .when(row.is_default, |d| {
                                d.child(badge("默认接口", BadgeKind::Neutral, tokens))
                            })
                            .when(authorized, |d| {
                                d.child(badge("已授权", BadgeKind::Good, tokens))
                            })
                            .when_some(balance, |d, text| {
                                d.child(
                                    div()
                                        .id(("ai-endpoint-balance", id as usize))
                                        .text_xs()
                                        .text_color(tokens.text_muted)
                                        .child(SharedString::from(text)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .truncate()
                            .child(SharedString::from(row_detail(row))),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .when(!row.is_default, |d| {
                        d.child(text_button(
                            format!("ai-endpoint-default-{id}"),
                            "设为默认",
                            ButtonLook::Ghost,
                            move |_, window, cx| {
                                set_default.update(cx, |this, cx| {
                                    this.ai_set_default_endpoint(id, window, cx)
                                });
                            },
                        ))
                    })
                    .child(text_button(
                        format!("ai-endpoint-edit-{id}"),
                        "编辑",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            edit.update(cx, |this, cx| {
                                this.open_ai_endpoint_dialog(Some(id), window, cx)
                            });
                        },
                    ))
                    .child(text_button(
                        format!("ai-endpoint-delete-{id}"),
                        if tokenway { "断开" } else { "删除" },
                        ButtonLook::Ghost,
                        move |_, _window, cx| {
                            delete.update(cx, |this, cx| {
                                if tokenway {
                                    this.ask_disconnect_tokenway(id, cx)
                                } else {
                                    this.ask_delete_ai_endpoint(id, cx)
                                }
                            });
                        },
                    )),
            )
            .into_any_element()
    }

    /// 设为默认接口 + 默认模型（模型用接口自己的默认模型）。
    pub(crate) fn ai_set_default_endpoint(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let done = crate::hosts_demo::open_database()
            .map_err(|err| err.to_string())
            .and_then(|db| {
                let model = db
                    .ai_endpoint_get(id)
                    .map_err(|e| e.to_string())?
                    .default_model;
                db.set_ai_builtin_default(Some((id, model.as_deref())))
                    .map_err(|err| err.to_string())
            });
        match done {
            Ok(()) => {
                builtin::reload();
                self.ai_builtin.reload_rows();
                self.ai_builtin.note = None;
            }
            Err(err) => self.ai_builtin.note = Some((format!("设为默认失败：{err}"), true)),
        }
        let _ = window;
        cx.notify();
    }

    pub(crate) fn ask_delete_ai_endpoint(&mut self, id: i64, cx: &mut gpui_kit::Context<Self>) {
        let Some(row) = self.ai_builtin.rows.iter().find(|row| row.id == id) else {
            return;
        };
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("删除接口「{}」？", row.name),
            message: "接口的配置和它的 API Key 会一起删掉，不能恢复。正在用它的 AI 对话不受影响，下次要重新选接口。"
                .to_owned(),
            confirm_label: Some("删除".to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteAiEndpoint(id),
        }));
        cx.notify();
    }

    pub(super) fn do_delete_ai_endpoint(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let uid = crate::hosts_demo::open_database()
            .ok()
            .and_then(|db| db.ai_endpoint_get(id).ok())
            .map(|endpoint| endpoint.uid);
        let done = crate::hosts_demo::open_database()
            .map_err(|err| err.to_string())
            .and_then(|db| db.ai_endpoint_delete(id).map_err(|err| err.to_string()));
        match done {
            Ok(()) => {
                if let Some(uid) = uid {
                    builtin::remote_forget(&uid);
                }
                builtin::reload();
                self.ai_builtin.reload_rows();
                self.ai_builtin.note = None;
            }
            Err(err) => self.notice(&format!("删除接口失败：{err}"), window, cx),
        }
    }

    /// 自动更新开关。
    pub(crate) fn ai_set_auto_update(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let next = !self.ai_builtin.auto_update;
        let saved = crate::hosts_demo::open_database()
            .map_err(|err| err.to_string())
            .and_then(|db| {
                db.set_ai_models_auto_update(next)
                    .map_err(|err| err.to_string())
            });
        match saved {
            Ok(()) => {
                self.ai_builtin.auto_update = next;
                self.ai_builtin.note = None;
            }
            Err(err) => self.ai_builtin.note = Some((format!("保存失败：{err}"), true)),
        }
        let _ = window;
        cx.notify();
    }

    /// "立即更新"：不看缓存新不新鲜，马上拉一次 models.dev。
    pub(crate) fn ai_catalog_refresh(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        if self.ai_builtin.catalog_busy {
            return;
        }
        let Ok(runtime) = crate::ssh_backend::runtime() else {
            self.ai_builtin.note = Some(("后台运行时没起来，不能更新".to_owned(), true));
            cx.notify();
            return;
        };
        self.ai_builtin.catalog_busy = true;
        self.ai_builtin.note = None;
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let result = if crate::agent_setup::builtin_fake::transport().is_some() {
                // 调试版的假接口：不联网，当作更新成功。
                Ok(false)
            } else {
                builtin::refresh_catalog(true).await
            };
            let _ = tx.send(result);
        });
        self.await_result(rx, window, cx, |this, result, _window, cx| {
            this.ai_builtin.catalog_busy = false;
            match result {
                Ok(updated) => {
                    if updated {
                        builtin::reload_catalog();
                    }
                    this.ai_builtin.reload_rows();
                    this.ai_builtin.note = Some((
                        if updated {
                            "已更新".to_owned()
                        } else {
                            "没有需要更新的内容".to_owned()
                        },
                        false,
                    ));
                }
                Err(reason) => {
                    this.ai_builtin.note =
                        Some((format!("更新没成功，继续用现有的资料：{reason}"), true));
                }
            }
            cx.notify();
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：上面的 `gpui_kit::*` 里有个同名的 `test` 宏，会把 `#[test]` 抢走。
    use super::{Row, key_tail, read_rows, row_detail};
    use zssh_core::ai::agent::builtin::config::EndpointKind;
    use zssh_core::store::Database;

    #[test]
    fn the_key_tail_shows_four_characters_and_never_the_whole_short_key() {
        assert_eq!(key_tail(""), None);
        assert_eq!(key_tail("  "), None);
        assert_eq!(key_tail("sk-test-abcdef1234"), Some("••••1234".to_owned()));
        // 太短的 Key 不露任何字符。
        assert_eq!(key_tail("short"), Some("••••".to_owned()));
        // 中文等多字节字符按字符数取末 4 位，不会切坏。
        assert_eq!(
            key_tail("密钥密钥密钥密钥密钥密钥甲乙丙丁"),
            Some("••••甲乙丙丁".to_owned())
        );
    }

    #[test]
    fn the_row_detail_lists_address_key_and_default_model() {
        let mut row = Row {
            id: 1,
            name: "主力".into(),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.example.com/v1".into(),
            key_tail: Some("••••1234".into()),
            no_key: false,
            preset_id: None,
            default_model: Some("gpt-5.5".into()),
            is_default: true,
        };
        assert_eq!(
            row_detail(&row),
            "https://api.example.com/v1 · Key ••••1234 · 默认模型 gpt-5.5"
        );
        row.key_tail = None;
        row.default_model = None;
        assert_eq!(row_detail(&row), "https://api.example.com/v1 · 未设置 Key");
    }

    #[test]
    fn rows_come_from_the_database_with_the_default_marked_and_the_key_masked() {
        use std::collections::BTreeMap;
        use zssh_core::ai::agent::builtin::config::{AiEndpointDraft, OpenAiOptionsConfig};
        let db = Database::open_in_memory().expect("内存库");
        let draft = |name: &str, key: Option<&str>| AiEndpointDraft {
            id: None,
            name: name.to_owned(),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.example.com/v1".to_owned(),
            api_key: key.map(str::to_owned),
            default_model: Some("m".to_owned()),
            models: Vec::new(),
            overrides: BTreeMap::new(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: None,
        };
        let a = db
            .ai_endpoint_save(&draft("甲", Some("sk-test-abcdef1234")))
            .unwrap();
        let b = db.ai_endpoint_save(&draft("乙", None)).unwrap();
        db.set_ai_builtin_default(Some((b.id, None))).unwrap();
        let rows = read_rows(&db);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, a.id);
        assert_eq!(rows[0].key_tail.as_deref(), Some("••••1234"));
        assert!(!rows[0].is_default);
        assert!(rows[1].is_default, "乙是默认接口");
        assert_eq!(rows[1].key_tail, None, "没填 Key 的不显示末 4 位");
        let text = format!("{rows:?}");
        assert!(
            !text.contains("abcdef"),
            "整个 Key 不能出现在行数据里：{text}"
        );
    }
}
