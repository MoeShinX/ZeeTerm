//! 「从 Z-SSH 迁移」一块的画法：一张卡，里面是两条路（直接导入本机 / 选备份文件）和下面的结果区
//! （预览 → 导入 → 结果摘要）。文字都由下面几个纯函数生成，测试直接断言它们。

use ::gpui::{AnyElement, Context, ElementId, Entity, PathPromptOptions, SharedString, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputContentType};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::sync::apply::ChangeCounts;
use zssh_core::sync::import_preview::ImportPreview;
use zssh_core::sync::offline::{OfflineImportMode, OfflineImportReport};
use zssh_core::sync::zssh_import::ZsshSource;

use super::{Busy, Done, Failure, FailureKind, Found, Loaded, MigrateDemo, Origin, Stage};
use crate::app_theme::Tokens;
use crate::settings_view::card;
use crate::sync::format_stamp;
use crate::typography::{Role, Typo as _};

// ---- 文字 ----

/// 直接导入那一行的说明：找没找到、在哪、什么时候用过。
pub(super) fn found_text(found: &Found, folder_error: Option<&str>) -> String {
    let base = match found {
        Found::Checking => "正在查找本机的 Z-SSH 数据…".to_owned(),
        Found::Yes(source) => found_yes_text(source),
        Found::No => "本机没有找到 Z-SSH 的数据。如果装在别处、或者自定义过数据文件夹，\
                      点「指定文件夹…」选它（里面应该有 data.db）。"
            .to_owned(),
    };
    match folder_error {
        Some(error) => format!("{base}\n{error}"),
        None => base,
    }
}

fn found_yes_text(source: &ZsshSource) -> String {
    let used = source
        .modified_at
        .map(|at| format!("，最后使用 {}", format_stamp(at)))
        .unwrap_or_default();
    format!(
        "已找到 Z-SSH 的数据（数据库 {}{used}）。只读取，不会改动它。
文件夹：{}",
        crate::format::size(source.db_bytes),
        source.data_dir.display()
    )
}

fn changes(counts: &ChangeCounts, replace: bool) -> String {
    let mut text = format!("新增 {}，更新 {}", counts.inserted, counts.updated);
    if replace && counts.deleted > 0 {
        text.push_str(&format!("，删除 {}", counts.deleted));
    }
    text
}

/// 预览：每一类有多少、导入后新增 / 更新 / 删除多少。
pub(super) fn preview_rows(preview: &ImportPreview) -> Vec<(&'static str, String)> {
    let replace = preview.mode == OfflineImportMode::Replace;
    let source = &preview.source;
    vec![
        (
            "主机",
            format!("{} 台：{}", source.hosts, changes(&preview.hosts, replace)),
        ),
        (
            "分组",
            format!(
                "{} 个：{}",
                source.groups,
                changes(&preview.groups, replace)
            ),
        ),
        (
            "凭据（含密码和私钥）",
            format!(
                "{} 条：{}",
                source.identities,
                changes(&preview.identities, replace)
            ),
        ),
        (
            "代理",
            format!(
                "{} 个：{}",
                source.proxies,
                changes(&preview.proxies, replace)
            ),
        ),
        (
            "已信任的主机指纹",
            if preview.known_hosts_skipped > 0 {
                format!(
                    "{} 条：新增 {}，跳过 {} 条可疑记录",
                    source.known_hosts, preview.known_hosts_added, preview.known_hosts_skipped
                )
            } else {
                format!(
                    "{} 条：新增 {}",
                    source.known_hosts, preview.known_hosts_added
                )
            },
        ),
        (
            "快捷命令",
            format!("{} 条（随可同步的设置一起导入）", source.quick_commands),
        ),
        (
            "可同步的设置",
            format!(
                "{} 项，其中 {} 项和现在不同",
                source.settings, preview.settings_changed
            ),
        ),
        (
            "冲突",
            format!(
                "{} 条（同一条记录两边都改过：取较新的，另一份进冲突页）",
                preview.conflicts
            ),
        ),
    ]
}

/// 预览下面要特别提醒的话。
pub(super) fn preview_warnings(preview: &ImportPreview, origin: &Origin) -> Vec<String> {
    let mut lines = Vec::new();
    if preview.duplicate_hosts > 0 {
        lines.push(format!(
            "其中 {} 台主机的名称、地址、端口、用户名和 ZeeTerm 里已有的完全一样：\
             合并后两边各留一份（删掉不要的那份即可）。",
            preview.duplicate_hosts
        ));
    }
    if preview.mode == OfflineImportMode::Replace {
        lines.push(format!(
            "替换会让 ZeeTerm 和这份数据一模一样：ZeeTerm 里有、这里没有的会被删除\
             （主机 {}、分组 {}、凭据 {}、代理 {}）。",
            preview.hosts.deleted,
            preview.groups.deleted,
            preview.identities.deleted,
            preview.proxies.deleted
        ));
    }
    if preview.nulled_references > 0 {
        lines.push(format!(
            "有 {} 处引用指向不存在的记录，导入时会清空。",
            preview.nulled_references
        ));
    }
    lines.push(match origin {
        Origin::Direct => {
            "端口转发规则和主机备注不会导入（Z-SSH 的备份文件里也没有它们）。".to_owned()
        }
        Origin::File => "端口转发规则不在备份文件里，不会导入。".to_owned(),
    });
    lines
}

/// 导入结果：导入了多少、跳过（已存在且一样）多少。
pub(super) fn done_rows(
    preview: &ImportPreview,
    report: &OfflineImportReport,
) -> Vec<(String, String)> {
    let replace = preview.mode == OfflineImportMode::Replace;
    // 数字取自预览（逐条比较内容得出的新增 / 更新 / 跳过）：引擎写库时会把已有的记录整条重写一遍，
    // 它自己的"更新"计数不代表内容变了。
    let applied = &report.applied;
    let line = |total: usize, counts: &ChangeCounts| {
        let skipped = total.saturating_sub(counts.inserted + counts.updated);
        let mut text = format!(
            "新增 {}，更新 {}，已有且相同而跳过 {}",
            counts.inserted, counts.updated, skipped
        );
        if replace && counts.deleted > 0 {
            text.push_str(&format!("，删除 {}", counts.deleted));
        }
        text
    };
    let mut rows = vec![
        (
            "主机".to_owned(),
            line(preview.source.hosts, &preview.hosts),
        ),
        (
            "分组".to_owned(),
            line(preview.source.groups, &preview.groups),
        ),
        (
            "凭据".to_owned(),
            line(preview.source.identities, &preview.identities),
        ),
        (
            "代理".to_owned(),
            line(preview.source.proxies, &preview.proxies),
        ),
        (
            "已信任的主机指纹".to_owned(),
            if applied.known_hosts_skipped > 0 {
                format!(
                    "新增 {} 条，跳过 {} 条可疑主机密钥记录",
                    applied.known_hosts_added, applied.known_hosts_skipped
                )
            } else {
                format!("新增 {} 条", applied.known_hosts_added)
            },
        ),
        (
            "设置".to_owned(),
            format!(
                "改动 {} 项（含快捷命令 {} 条）",
                preview.settings_changed, preview.source.quick_commands
            ),
        ),
        (
            "冲突".to_owned(),
            format!(
                "{} 条（两边都改过，保留了较新的，另一份在冲突页）",
                report.conflicts
            ),
        ),
    ];
    if !report.nulled_references.is_empty() {
        rows.push((
            "清空的引用".to_owned(),
            format!("{} 处（指向的记录不存在）", report.nulled_references.len()),
        ));
    }
    rows.push((
        "导入前的备份".to_owned(),
        format!(
            "{}（导错了可以在「本地备份」里恢复）",
            report.safety_backup.name
        ),
    ));
    rows
}

// ---- 画 ----

fn muted(text: impl Into<SharedString>, tokens: &Tokens) -> AnyElement {
    div()
        .typo(Role::Caption, tokens)
        .child(text.into())
        .into_any_element()
}

fn colored(text: impl Into<SharedString>, color: ::gpui::Hsla) -> AnyElement {
    div()
        .text_sm()
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

fn label_cell(label: &'static str) -> impl IntoElement {
    div().w(px(96.0)).flex_none().text_sm().child(label)
}

fn act_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    entity: &Entity<MigrateDemo>,
    disabled: bool,
    run: impl Fn(&mut MigrateDemo, &mut Window, &mut Context<MigrateDemo>) + 'static,
) -> Button {
    let entity = entity.clone();
    Button::new(id)
        .label(label)
        .compact()
        .disabled(disabled)
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| run(this, window, cx));
        })
}

fn kv_row(label: impl Into<SharedString>, text: impl Into<SharedString>) -> AnyElement {
    h_flex()
        .w_full()
        .gap_3()
        .items_start()
        .child(div().w(px(168.0)).flex_none().text_sm().child(label.into()))
        .child(div().min_w_0().flex_1().text_sm().child(text.into()))
        .into_any_element()
}

impl Render for MigrateDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        let entity = cx.entity();
        let busy = self.state.busy.is_some();
        let mut rows = vec![
            self.direct_row(&entity, &tokens, busy),
            self.file_row(&entity, &tokens, busy),
        ];
        if let Some(result) = self.result_block(&entity, &tokens, busy) {
            rows.push(result);
        }
        card(
            "migrate-zssh",
            "从 Z-SSH 迁移",
            Some(
                "把 Z-SSH 里的主机、分组、凭据（含密码和私钥）、代理、已信任的主机指纹、快捷命令和\
                 可同步的设置搬到 ZeeTerm。导入前会先预览，并自动备份 ZeeTerm 现在的数据。",
            ),
            &tokens,
            rows,
        )
    }
}

impl MigrateDemo {
    /// 路径一：直接导入本机的 Z-SSH 数据。
    fn direct_row(&self, entity: &Entity<Self>, tokens: &Tokens, busy: bool) -> AnyElement {
        let found = matches!(self.state.found, Found::Yes(_));
        h_flex()
            .id("migrate-direct")
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
                            .child("直接导入本机的 Z-SSH 数据（推荐）"),
                    )
                    .child(muted(
                        found_text(&self.state.found, self.state.folder_error.as_deref()),
                        tokens,
                    )),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(act_button(
                        "migrate-pick-folder",
                        "指定文件夹…",
                        entity,
                        busy,
                        |this, window, cx| this.browse_folder(window, cx),
                    ))
                    .child(
                        act_button(
                            "migrate-direct-read",
                            "读取并预览",
                            entity,
                            busy || !found,
                            |this, _, cx| this.start_direct(cx),
                        )
                        .primary(),
                    ),
            )
            .into_any_element()
    }

    /// 路径二：Z-SSH 导出的备份文件。
    fn file_row(&self, entity: &Entity<Self>, tokens: &Tokens, busy: bool) -> AnyElement {
        v_flex()
            .id("migrate-file")
            .w_full()
            .gap_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .typo(Role::ItemName, tokens)
                            .child("导入 Z-SSH 导出的备份文件"),
                    )
                    .child(muted(
                        "在 Z-SSH 的「设置 → 同步 → 离线备份文件」里导出的 .zsshbak。\
                         格式和 ZeeTerm 的离线备份相同，选好文件、输入当时设的密码就能导入。",
                        tokens,
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(label_cell("文件"))
                    .child(Input::new(&self.file_path).flex_1())
                    .child(act_button(
                        "migrate-pick-file",
                        "选择文件…",
                        entity,
                        busy,
                        |this, window, cx| this.browse_file(window, cx),
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(label_cell("密码"))
                    .child(
                        Input::new(&self.file_password)
                            .flex_1()
                            .content_type(InputContentType::Password),
                    )
                    .child(
                        act_button(
                            "migrate-file-read",
                            "读取并预览",
                            entity,
                            busy,
                            |this, _, cx| this.start_file(cx),
                        )
                        .primary(),
                    ),
            )
            .into_any_element()
    }

    /// 结果区：正在做什么 / 失败原因 / 预览 / 导入结果。什么都没有就不占位。
    fn result_block(
        &self,
        entity: &Entity<Self>,
        tokens: &Tokens,
        busy: bool,
    ) -> Option<AnyElement> {
        let working = self.state.busy.filter(|b| *b != Busy::Detecting);
        let body: AnyElement = match &self.state.stage {
            Stage::Idle => {
                let working = working?;
                muted(working.label(), tokens)
            }
            Stage::Failed(failure) => self.failure_block(failure, entity, tokens, busy),
            Stage::Preview(loaded) => self.preview_block(loaded, entity, tokens, busy),
            Stage::Done(done) => self.done_block(done, entity, tokens),
        };
        Some(
            v_flex()
                .id("migrate-result")
                .w_full()
                .gap_2()
                .child(body)
                .when_some(
                    working.filter(|_| !matches!(self.state.stage, Stage::Idle)),
                    |col, working| col.child(muted(working.label(), tokens)),
                )
                .into_any_element(),
        )
    }

    fn failure_block(
        &self,
        failure: &Failure,
        entity: &Entity<Self>,
        tokens: &Tokens,
        busy: bool,
    ) -> AnyElement {
        match failure.kind {
            FailureKind::InUse => v_flex()
                .id("migrate-in-use")
                .gap_2()
                .child(colored(failure.message.clone(), tokens.caution))
                .child(muted(
                    "退出 Z-SSH（包括托盘里的图标）后点「重试」。",
                    tokens,
                ))
                .child(
                    h_flex().child(
                        act_button(
                            "migrate-retry",
                            "我已退出 Z-SSH，重试",
                            entity,
                            busy,
                            |this, _, cx| this.start_direct(cx),
                        )
                        .primary(),
                    ),
                )
                .into_any_element(),
            FailureKind::Other => v_flex()
                .id("migrate-failed")
                .gap_2()
                .child(colored(failure.message.clone(), tokens.danger))
                .into_any_element(),
        }
    }

    fn preview_block(
        &self,
        loaded: &Loaded,
        entity: &Entity<Self>,
        tokens: &Tokens,
        busy: bool,
    ) -> AnyElement {
        let mode = self.state.mode;
        let title = match loaded.origin {
            Origin::Direct => "预览：本机 Z-SSH 的数据",
            Origin::File => "预览：备份文件里的数据",
        };
        let mode_button = |id: &'static str,
                           label: &'static str,
                           target: OfflineImportMode,
                           hint: &'static str| {
            let entity = entity.clone();
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child({
                    let button = Button::new(id)
                        .label(label)
                        .compact()
                        .toggled(mode == target)
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.set_mode(target, cx));
                        });
                    // 选中的那个用实心色（合并是主色、替换是红色），一眼看出现在是哪种方式。
                    match (mode == target, target) {
                        (true, OfflineImportMode::Merge) => button.primary(),
                        (true, OfflineImportMode::Replace) => button.danger(),
                        _ => button,
                    }
                })
                .child(muted(hint, tokens))
        };
        let import_label = match mode {
            OfflineImportMode::Merge => "合并导入",
            OfflineImportMode::Replace if self.state.replace_armed => "确认替换导入",
            OfflineImportMode::Replace => "替换导入",
        };
        let import_button = {
            let button = act_button(
                "migrate-import",
                import_label,
                entity,
                busy,
                |this, _, cx| this.start_import(cx),
            );
            if mode == OfflineImportMode::Replace {
                button.danger()
            } else {
                button.primary()
            }
        };
        v_flex()
            .id("migrate-preview")
            .w_full()
            .gap_2()
            .child(div().typo(Role::ItemName, tokens).child(title))
            .children(
                preview_rows(&loaded.preview)
                    .into_iter()
                    .map(|(label, text)| kv_row(label, text)),
            )
            .children(
                preview_warnings(&loaded.preview, &loaded.origin)
                    .into_iter()
                    .map(|text| colored(text, tokens.caution)),
            )
            .children(loaded.notes.iter().map(|text| muted(text.clone(), tokens)))
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .items_start()
                    .child(label_cell("导入方式"))
                    .child(mode_button(
                        "migrate-mode-merge",
                        "合并（推荐）",
                        OfflineImportMode::Merge,
                        "两边都留。同一条记录两边都改过时取较新的，另一份进冲突页。",
                    ))
                    .child(mode_button(
                        "migrate-mode-replace",
                        "替换",
                        OfflineImportMode::Replace,
                        "ZeeTerm 变成和这份数据一模一样，这里没有的主机、凭据会被删掉。",
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(import_button)
                    .child(act_button(
                        "migrate-cancel",
                        "取消",
                        entity,
                        busy,
                        |this, _, cx| this.cancel(cx),
                    )),
            )
            .when(mode == OfflineImportMode::Replace && self.state.replace_armed, |col| {
                col.child(colored(
                    "替换会删除 ZeeTerm 里这份数据没有的记录，再点一次「确认替换导入」才会执行。",
                    tokens.danger,
                ))
            })
            .child(muted(
                "导入前会自动给 ZeeTerm 现在的数据做一份本地备份，导错了可以在下面的「本地备份」里恢复。",
                tokens,
            ))
            .into_any_element()
    }

    fn done_block(&self, done: &Done, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        v_flex()
            .id("migrate-done")
            .w_full()
            .gap_2()
            .child(
                div()
                    .typo(Role::ItemName, tokens)
                    .text_color(tokens.dot_connected)
                    .child("导入完成"),
            )
            .children(
                done_rows(&done.preview, &done.report)
                    .into_iter()
                    .map(|(label, text)| kv_row(label, text)),
            )
            .child(
                h_flex().child(
                    act_button(
                        "migrate-open-hosts",
                        "打开主机页",
                        entity,
                        false,
                        |this, window, cx| this.open_hosts(window, cx),
                    )
                    .primary(),
                ),
            )
            .into_any_element()
    }

    /// 「选择文件…」：系统对话框选好了就填进路径框。
    pub(super) fn browse_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                let _ = this.update_in(cx, |this, window, cx| {
                    let text = path.to_string_lossy().into_owned();
                    this.file_path
                        .update(cx, |state, cx| state.set_value(text, window, cx));
                });
            }
        })
        .detach();
    }

    /// 「指定文件夹…」：选 Z-SSH 的数据文件夹。
    pub(super) fn browse_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, _window, cx| this.use_folder(path, cx));
            }
        })
        .detach();
    }
}
