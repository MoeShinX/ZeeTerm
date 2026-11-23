//! 从 OpenSSH 配置文件（`~/.ssh/config`）导入主机的对话框（V2-U59）。
//!
//! 解析、去重、跳板机和落库都在核心库（`zeeterm_core::ssh_config`）；这里只负责：找到文件、画预览、
//! 记勾选、点「导入」时调核心库（一个事务）。配置文件和它 `Include` 的文件只读不写。

use std::path::{Path, PathBuf};

use ::gpui::{AnyElement, PathPromptOptions, SharedString, px};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeeterm_core::ssh_config::{
    self, ImportContext, ImportPlan, ImportRow, JumpTarget, ParsedConfig, ReadError, RealFs,
    RowPlan,
};

use super::MainWindow;
use super::modal::Modal;
use super::page_logic::flatten_groups;
use super::ssh_import_text::{
    SshImportMsg, default_user_note, host_note_text, imported_toast, notice_text, plan_note_text,
    skipped_text, summary, unsupported_parts,
};
use super::widgets::{BadgeKind, ButtonLook, Choice, badge, error_note, note, select, text_button};
use crate::app_theme::Tokens;
use crate::i18n::{colon, t};
use crate::typography::{Role, Typo as _};

/// 对话框宽度：一行要放得下「别名 + user@host:port + 已存在」。
pub(super) const DIALOG_WIDTH: f32 = 580.0;
/// 列表区最高多少，超出在里面滚动。
const LIST_MAX_HEIGHT: f32 = 380.0;

/// 读到的内容：配置文件在不在、读不读得了、有什么主机。
pub(super) enum ImportState {
    /// 文件不存在（或根本没有家目录）。
    Missing,
    TooLarge,
    /// 读不了（权限、被占用……），带系统给的说明。
    Unreadable(String),
    Ready(Box<Preview>),
}

/// 预览：解析结果、预览行和每行的勾选。
pub(super) struct Preview {
    pub parsed: ParsedConfig,
    pub rows: Vec<ImportRow>,
    pub selected: Vec<bool>,
}

pub struct SshImportDialog {
    pub(super) path: PathBuf,
    /// 用户自己选的文件（不是默认路径）：「没找到」时的说法不同。
    pub(super) picked: bool,
    /// 显示路径时把这个目录写成 `~`。
    pub(super) home: PathBuf,
    pub(super) state: ImportState,
    pub(super) group: Option<i64>,
    pub(super) error: Option<String>,
}

impl SshImportDialog {
    fn selected_count(&self) -> usize {
        match &self.state {
            ImportState::Ready(preview) => preview.selected.iter().filter(|on| **on).count(),
            _ => 0,
        }
    }
}

/// 配置文件在哪：开发钩子 `ZEETERM_SSH_CONFIG_PATH`（只在调试构建里读，截图和手动验证用，这时相对路径和 `~`
/// 都以那个文件所在的目录为准，不会去碰真实的 `~/.ssh`）优先，否则是 `~/.ssh/config`。
fn default_location() -> Option<(PathBuf, ssh_config::ParseOptions)> {
    if let Some(raw) = crate::dev_hooks::var("ZEETERM_SSH_CONFIG_PATH")
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
    {
        let path = PathBuf::from(raw);
        let options = ssh_config::sandboxed_config_location(&path);
        return Some((path, options));
    }
    ssh_config::default_config_location(std::env::home_dir())
}

/// 真实的用户目录：「来源」缩写成 `~` 和用户选的文件里 `~` 的展开都只认它（开发钩子的沙箱不影响这里）。
pub(super) fn real_home() -> PathBuf {
    std::env::home_dir()
        .filter(|home| !home.as_os_str().is_empty())
        .unwrap_or_default()
}

/// 配置里没写 `User` 时用的名字：OpenSSH 用本机用户名。调试钩子 `ZEETERM_SSH_CONFIG_USER` 可以换掉它
/// （截图时不让本机用户名出现在画面里）。
fn local_user_name() -> String {
    crate::dev_hooks::var("ZEETERM_SSH_CONFIG_USER")
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default()
}

/// 路径里的 `home` 写成 `~`（用 `/` 分隔），不在 `home` 下面就原样。
pub(super) fn display_path(path: &Path, home: &Path) -> String {
    // 家目录拿不到（空）时不缩写，否则任何路径都会被写成 `~/…`。
    let rest = (!home.as_os_str().is_empty())
        .then(|| path.strip_prefix(home).ok())
        .flatten();
    match rest {
        Some(rest) if !rest.as_os_str().is_empty() => {
            let parts: Vec<String> = rest
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            format!("~/{}", parts.join("/"))
        }
        _ => path.display().to_string(),
    }
}

/// `用户名@地址:端口`（IPv6 地址加方括号）。
pub(super) fn detail_text(row: &ImportRow) -> String {
    let address = if row.address.contains(':') {
        format!("[{}]", row.address)
    } else {
        row.address.clone()
    };
    format!("{}@{address}:{}", row.username, row.port)
}

/// 私钥文件名（路径最后一段）。
pub(super) fn key_file_name(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

impl MainWindow {
    // ---- 打开 / 读文件 ----

    /// 入口：直接读默认路径（`~/.ssh/config`）。文件不存在就在对话框里提示并给「选择文件…」。
    pub(crate) fn open_ssh_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 已经开着别的对话框（比如属性）时不叠上去。
        if self.modal.is_some() {
            return;
        }
        match default_location() {
            Some((path, options)) => self.load_ssh_config(path, options, false, cx),
            None => {
                self.modal = Some(Modal::SshImport(Box::new(SshImportDialog {
                    path: PathBuf::new(),
                    picked: false,
                    home: PathBuf::new(),
                    state: ImportState::Missing,
                    group: None,
                    error: None,
                })));
            }
        }
        let _ = window;
        cx.notify();
    }

    /// 读 `path` 并解析，换成预览（或「没找到 / 读不了」）。分组选择保留。
    pub(super) fn load_ssh_config(
        &mut self,
        path: PathBuf,
        options: ssh_config::ParseOptions,
        picked: bool,
        cx: &mut Context<Self>,
    ) {
        let group = match &self.modal {
            Some(Modal::SshImport(dialog)) => dialog.group,
            _ => None,
        };
        let state = match ssh_config::parse_config(&RealFs, &path, &options) {
            Ok(parsed) => {
                let user = local_user_name();
                let context = ImportContext {
                    hosts: self.store.hosts(),
                    identities: self.store.identities(),
                    default_user: &user,
                };
                let rows = ssh_config::build_rows(&parsed, &context);
                let selected = ssh_config::default_selection(&rows);
                ImportState::Ready(Box::new(Preview {
                    parsed,
                    rows,
                    selected,
                }))
            }
            Err(ReadError::NotFound) => ImportState::Missing,
            Err(ReadError::TooLarge) => ImportState::TooLarge,
            Err(ReadError::Unreadable(error)) => ImportState::Unreadable(error),
        };
        self.modal = Some(Modal::SshImport(Box::new(SshImportDialog {
            path,
            picked,
            home: real_home(),
            state,
            group,
            error: None,
        })));
        cx.notify();
    }

    /// 「选择文件…」：系统文件对话框选一个配置文件。选到的文件里的相对路径以它所在的目录为基准。
    pub(super) fn ssh_import_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.ssh_import_load_file(path, false, cx);
                });
            }
        })
        .detach();
    }

    /// 读用户选的文件（`ssh_import_browse` 和调试脚本共用）。
    pub(super) fn ssh_import_load_file(
        &mut self,
        path: PathBuf,
        sandbox: bool,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.modal, Some(Modal::SshImport(_))) {
            return;
        }
        // 用户选的文件：`~` 展开到真实家目录，相对路径以文件所在目录为基准；调试脚本可以要求沙箱（`~` 也留在文件所在目录）。
        let options = if sandbox {
            ssh_config::sandboxed_config_location(&path)
        } else {
            ssh_config::picked_config_location(&path, Some(real_home()))
        };
        self.load_ssh_config(path, options, true, cx);
    }

    // ---- 勾选 ----

    pub(super) fn ssh_import_toggle(&mut self, index: usize, value: bool, cx: &mut Context<Self>) {
        if let Some(Modal::SshImport(dialog)) = &mut self.modal
            && let ImportState::Ready(preview) = &mut dialog.state
            && let Some(slot) = preview.selected.get_mut(index)
        {
            *slot = value;
            dialog.error = None;
        }
        cx.notify();
    }

    pub(super) fn ssh_import_select_all(&mut self, value: bool, cx: &mut Context<Self>) {
        if let Some(Modal::SshImport(dialog)) = &mut self.modal
            && let ImportState::Ready(preview) = &mut dialog.state
        {
            preview.selected.iter_mut().for_each(|slot| *slot = value);
            dialog.error = None;
        }
        cx.notify();
    }

    pub(super) fn ssh_import_set_group(&mut self, group: Option<i64>, cx: &mut Context<Self>) {
        if let Some(Modal::SshImport(dialog)) = &mut self.modal {
            dialog.group = group;
        }
        cx.notify();
    }

    // ---- 导入 ----

    /// 按当前勾选算出整份计划（预览显示和落库用同一份算法）。
    fn ssh_import_plan(&self, preview: &Preview) -> ImportPlan {
        let user = local_user_name();
        let context = ImportContext {
            hosts: self.store.hosts(),
            identities: self.store.identities(),
            default_user: &user,
        };
        ssh_config::plan_import(&preview.rows, &preview.selected, &context)
    }

    /// 「导入」：一个事务建好所有勾选的主机（和要新建的私钥凭据）；成功后关对话框、提示、刷新侧栏。
    pub(crate) fn ssh_import_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::SshImport(dialog)) = &self.modal else {
            return;
        };
        let ImportState::Ready(preview) = &dialog.state else {
            return;
        };
        if dialog.selected_count() == 0 {
            return;
        }
        let plan = self.ssh_import_plan(preview);
        let group = dialog.group;
        let result = self.store.db().and_then(|db| {
            db.import_ssh_hosts(&preview.rows, &plan, group)
                .map_err(|err| crate::error_text::error_text(&err))
        });
        match result {
            Ok(outcome) => {
                self.modal = None;
                self.store.reload();
                self.focus_content(window, cx);
                self.notice(&imported_toast(outcome.hosts_created), window, cx);
            }
            Err(message) => {
                // 事务已经整体回滚；留在对话框里，把原因写在按钮上面。
                self.store.reload();
                if let Some(Modal::SshImport(dialog)) = &mut self.modal {
                    dialog.error = Some(message);
                }
            }
        }
        cx.notify();
    }

    // ---- 画 ----

    pub(super) fn ssh_import_body(
        &self,
        dialog: &SshImportDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        match &dialog.state {
            ImportState::Ready(preview) => {
                self.ssh_import_preview(dialog, preview, tokens, &entity)
            }
            state => self.ssh_import_problem(dialog, state, tokens, &entity),
        }
    }

    /// 没找到 / 读不了：一句话 + 「选择文件…」。
    fn ssh_import_problem(
        &self,
        dialog: &SshImportDialog,
        state: &ImportState,
        tokens: &Tokens,
        entity: &Entity<Self>,
    ) -> AnyElement {
        let shown = display_path(&dialog.path, &dialog.home);
        let (headline, detail): (String, Option<String>) = match state {
            ImportState::Missing if dialog.picked => (
                t(SshImportMsg::NotFoundPicked).to_owned(),
                Some(shown.clone()),
            ),
            ImportState::Missing => (t(SshImportMsg::NotFoundDefault).to_owned(), None),
            ImportState::TooLarge => (t(SshImportMsg::TooLargePrefix).to_owned(), Some(shown)),
            ImportState::Unreadable(error) => (
                t(SshImportMsg::UnreadablePrefix).to_owned(),
                Some(format!("{shown}{}{error}", crate::i18n::comma())),
            ),
            ImportState::Ready(_) => (String::new(), None),
        };
        let (cancel, browse) = (entity.clone(), entity.clone());
        v_flex()
            .gap_4()
            .child(
                div()
                    .typo(Role::GroupTitle, tokens)
                    .child(t(SshImportMsg::Title)),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .typo(Role::Body, tokens)
                            .child(SharedString::from(headline)),
                    )
                    .children(detail.map(|text| {
                        div()
                            .typo(Role::Caption, tokens)
                            .font_family(crate::fonts::mono_text_family())
                            .child(SharedString::from(text))
                    }))
                    .child(note(t(SshImportMsg::NotFoundHint), tokens)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "ssh-import-cancel",
                        t(SshImportMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "ssh-import-browse",
                        t(SshImportMsg::ChooseFile),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            browse.update(cx, |this, cx| this.ssh_import_browse(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn ssh_import_preview(
        &self,
        dialog: &SshImportDialog,
        preview: &Preview,
        tokens: &Tokens,
        entity: &Entity<Self>,
    ) -> AnyElement {
        let plan = self.ssh_import_plan(preview);
        let total = preview.rows.len();
        let chosen = dialog.selected_count();

        let source = colon(
            t(SshImportMsg::Source),
            &display_path(&dialog.path, &dialog.home),
        );
        let controls = {
            let (all, none, browse) = (entity.clone(), entity.clone(), entity.clone());
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .typo(Role::FieldLabel, tokens)
                        .child(SharedString::from(summary(total, chosen))),
                )
                .child(
                    text_button(
                        "ssh-import-all",
                        t(SshImportMsg::SelectAll),
                        ButtonLook::Ghost,
                        move |_, _, cx| {
                            all.update(cx, |this, cx| this.ssh_import_select_all(true, cx))
                        },
                    )
                    .disabled(total == 0),
                )
                .child(
                    text_button(
                        "ssh-import-none",
                        t(SshImportMsg::SelectNone),
                        ButtonLook::Ghost,
                        move |_, _, cx| {
                            none.update(cx, |this, cx| this.ssh_import_select_all(false, cx))
                        },
                    )
                    .disabled(total == 0),
                )
                .child(text_button(
                    "ssh-import-other-file",
                    t(SshImportMsg::ChooseFile),
                    ButtonLook::Ghost,
                    move |_, window, cx| {
                        browse.update(cx, |this, cx| this.ssh_import_browse(window, cx));
                    },
                ))
        };

        let mut list_items: Vec<AnyElement> = Vec::new();
        if total == 0 {
            list_items.push(
                v_flex()
                    .w_full()
                    .py_6()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .typo(Role::Body, tokens)
                            .text_color(tokens.text_muted)
                            .child(t(SshImportMsg::EmptyTitle)),
                    )
                    .child(note(t(SshImportMsg::EmptyHint), tokens))
                    .into_any_element(),
            );
        }
        for (index, row) in preview.rows.iter().enumerate() {
            let selected = preview.selected.get(index).copied().unwrap_or(false);
            list_items.push(self.ssh_import_row(
                index,
                row,
                &plan.rows[index],
                &preview.rows,
                selected,
                tokens,
                entity,
            ));
        }
        if !preview.parsed.skipped.is_empty() {
            list_items.push(
                v_flex()
                    .w_full()
                    .gap_1()
                    .pt_1()
                    .child(
                        div()
                            .typo(Role::FieldLabel, tokens)
                            .child(t(SshImportMsg::SkippedTitle)),
                    )
                    .children(preview.parsed.skipped.iter().map(|entry| {
                        error_note(SharedString::from(skipped_text(entry)), tokens.danger)
                    }))
                    .into_any_element(),
            );
        }
        if !preview.parsed.notices.is_empty() {
            list_items.push(
                v_flex()
                    .w_full()
                    .gap_1()
                    .pt_1()
                    .child(
                        div()
                            .typo(Role::FieldLabel, tokens)
                            .child(t(SshImportMsg::NoticesTitle)),
                    )
                    .children(
                        preview
                            .parsed
                            .notices
                            .iter()
                            .map(|notice| note(SharedString::from(notice_text(notice)), tokens)),
                    )
                    .into_any_element(),
            );
        }

        let group_row = (!self.store.groups().is_empty()).then(|| {
            let groups = self.store.groups();
            let mut choices = vec![Choice::new(
                t(SshImportMsg::NoGroup),
                dialog.group.is_none(),
            )];
            let mut targets: Vec<Option<i64>> = vec![None];
            for (group, depth) in flatten_groups(groups) {
                choices.push(
                    Choice::new(group.name.clone(), dialog.group == Some(group.id)).depth(depth),
                );
                targets.push(Some(group.id));
            }
            let current = dialog
                .group
                .and_then(|id| groups.iter().find(|group| group.id == id))
                .map_or_else(
                    || t(SshImportMsg::NoGroup).to_owned(),
                    |group| group.name.clone(),
                );
            let pick_entity = entity.clone();
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_none()
                        .typo(Role::FieldLabel, tokens)
                        .child(t(SshImportMsg::GroupLabel)),
                )
                .child(div().flex_1().min_w_0().child(select(
                    "ssh-import-group",
                    current,
                    choices,
                    move |index: usize, _window: &mut Window, cx: &mut App| {
                        let Some(target) = targets.get(index).copied() else {
                            return;
                        };
                        pick_entity.update(cx, |this, cx| this.ssh_import_set_group(target, cx));
                    },
                )))
        });

        let (cancel, confirm) = (entity.clone(), entity.clone());
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(t(SshImportMsg::Title)),
                    )
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .font_family(crate::fonts::mono_text_family())
                            .truncate()
                            .child(SharedString::from(source)),
                    ),
            )
            .child(controls)
            .child(
                v_flex()
                    .id("ssh-import-list")
                    .w_full()
                    .gap_2()
                    .max_h(px(LIST_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .children(list_items),
            )
            .children(group_row)
            .children(dialog.error.as_ref().map(|error| {
                error_note(
                    SharedString::from(format!("{}{error}", t(SshImportMsg::ImportFailedPrefix))),
                    tokens.danger,
                )
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "ssh-import-cancel",
                        t(SshImportMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(
                        text_button(
                            "ssh-import-confirm",
                            t(SshImportMsg::Import),
                            ButtonLook::Primary,
                            move |_, window, cx| {
                                confirm.update(cx, |this, cx| this.ssh_import_confirm(window, cx));
                            },
                        )
                        .disabled(chosen == 0),
                    ),
            )
            .into_any_element()
    }

    /// 一台主机一行：勾选框 + 别名 + `用户名@地址:端口`（+「已存在」），下面一行认证方式和跳板机，
    /// 再下面是小字提示和「没有导入的设置」。
    #[allow(clippy::too_many_arguments)]
    fn ssh_import_row(
        &self,
        index: usize,
        row: &ImportRow,
        plan: &RowPlan,
        rows: &[ImportRow],
        selected: bool,
        tokens: &Tokens,
        entity: &Entity<Self>,
    ) -> AnyElement {
        let auth = match &row.key_path {
            Some(path) => format!("{}{}", t(SshImportMsg::AuthKey), key_file_name(path)),
            None => t(SshImportMsg::AuthPassword).to_owned(),
        };
        let jump = match &plan.jump {
            JumpTarget::None => None,
            JumpTarget::Row(target) => rows.get(*target).map(|other| other.alias.clone()),
            JumpTarget::Existing { name, .. } => Some(name.clone()),
        };
        let mut facts = vec![auth];
        if let Some(name) = jump {
            facts.push(format!("{}{name}", t(SshImportMsg::Via)));
        }
        let mut lines: Vec<String> = Vec::new();
        if row.user_defaulted {
            lines.push(default_user_note(&row.username));
        }
        lines.extend(row.notes.iter().map(host_note_text));
        lines.extend(plan.notes.iter().map(plan_note_text));

        let toggle = entity.clone();
        v_flex()
            .id(("ssh-import-row", index))
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .rounded(px(crate::app_theme::CARD_RADIUS))
            .border_1()
            .border_color(tokens.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Checkbox::new(SharedString::from(format!("ssh-import-check-{index}")))
                            .checked(selected)
                            .label(SharedString::from(row.alias.clone()))
                            .on_click(move |value, _, cx| {
                                let value = *value;
                                toggle.update(cx, |this, cx| {
                                    this.ssh_import_toggle(index, value, cx)
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .font_family(crate::fonts::mono_text_family())
                            .child(SharedString::from(detail_text(row))),
                    )
                    .when(row.exists, |line| {
                        line.child(badge(t(SshImportMsg::Exists), BadgeKind::Outline, tokens))
                    }),
            )
            .child(
                div()
                    .pl(px(26.0))
                    .typo(Role::Caption, tokens)
                    .child(SharedString::from(facts.join(" · "))),
            )
            .children(lines.into_iter().map(|line| {
                div()
                    .pl(px(26.0))
                    .typo(Role::Caption, tokens)
                    .text_color(tokens.text_faint)
                    .child(SharedString::from(line))
            }))
            .when(!row.unsupported.is_empty(), |row_el| {
                row_el.child(
                    h_flex()
                        .pl(px(26.0))
                        .flex_wrap()
                        .gap_x(px(if crate::i18n::is_en() { 4.0 } else { 0.0 }))
                        .typo(Role::Caption, tokens)
                        .text_color(tokens.text_faint)
                        .children(
                            unsupported_parts(&row.unsupported)
                                .into_iter()
                                .map(|part| div().child(SharedString::from(part))),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zeeterm_core::ssh_config::{ImportRow, ProxyJump};

    use super::{detail_text, display_path, key_file_name};

    fn row(address: &str) -> ImportRow {
        ImportRow {
            alias: "web".into(),
            address: address.into(),
            port: 22,
            username: "deploy".into(),
            user_defaulted: false,
            key_path: None,
            proxy_jump: ProxyJump::Unset,
            exists: false,
            unsupported: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_detail_line_is_user_at_address_colon_port_with_ipv6_in_brackets() {
        assert_eq!(
            detail_text(&row("web.example.com")),
            "deploy@web.example.com:22"
        );
        assert_eq!(detail_text(&row("2001:db8::1")), "deploy@[2001:db8::1]:22");
    }

    #[test]
    fn paths_under_home_are_shown_with_a_tilde() {
        let home = Path::new("/h/tester");
        assert_eq!(
            display_path(&home.join(".ssh").join("config"), home),
            "~/.ssh/config"
        );
        assert_eq!(display_path(Path::new("/elsewhere/config"), home), {
            Path::new("/elsewhere/config").display().to_string()
        });
    }

    #[test]
    fn a_file_outside_the_home_is_shown_with_its_full_path_never_as_tilde_config() {
        let home = Path::new("/h/tester");
        let elsewhere = Path::new("/backup/ssh-copy/config");
        assert_eq!(
            display_path(elsewhere, home),
            elsewhere.display().to_string()
        );
        // 开发钩子的沙箱目录同样不在真实家目录下。
        assert_ne!(display_path(elsewhere, home), "~/config");
        // 家目录拿不到（空）时一律显示完整路径。
        assert_eq!(
            display_path(Path::new("/x/config"), Path::new("")),
            Path::new("/x/config").display().to_string()
        );
        // 家目录本身不缩写成 `~/`。
        assert_eq!(display_path(home, home), home.display().to_string());
    }

    #[test]
    fn the_key_name_is_the_last_path_segment_in_either_separator() {
        assert_eq!(key_file_name(r"C:\keys\id_ed25519"), "id_ed25519");
        assert_eq!(key_file_name("/keys/id_rsa"), "id_rsa");
        assert_eq!(key_file_name("plain"), "plain");
    }
}
