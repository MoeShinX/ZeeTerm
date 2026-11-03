//! 备份的画法（G11）：按 0.2.14 设置页的样子分成两张卡——本地备份（保留份数 + 列表 + 恢复）、
//! 离线备份文件（导出 / 导入，合并或替换）。设置页里嵌的是这两张卡；独立窗口（调试入口）
//! 只是再套一层标题和滚动容器，并多一个「新建备份」按钮（0.2.14 没有手动新建，只会自动留）。
//!
//! 逻辑都在 `backup_demo.rs`（F5 修好的那一套：恢复走引擎、替换要确认、覆盖要确认、和同步互斥），
//! 这里只负责画和接线。

use ::gpui::{
    AnyElement, App, Context, ElementId, Entity, PathPromptOptions, SharedString, Window, px,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputContentType};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::sync::backup::BackupDetail;
use zssh_core::sync::offline::OfflineImportMode;

use super::{BackupDemo, CONFIRM_WARNING, format_size, write_keep};
use crate::app_theme::Tokens;
use crate::settings_view::card;
use crate::sync_demo::format_stamp;
use crate::typography::{Role, Typo as _};

/// 保留份数的可选值（核心接受 1～200，同 0.2.14）。
pub(super) const KEEP_CHOICES: [usize; 6] = [5, 10, 20, 50, 100, 200];

/// 离线备份的默认文件名（`zeeterm-日期.zsshbak`）：扩展名和 Z-SSH 0.2.14 一样，两边的备份文件互相能导入；
/// 文件名改成产品名，免得和 Z-SSH 导出的混在一起分不清。
pub(super) fn default_file_name() -> String {
    let stamp = format_stamp(super::now_secs());
    let date: String = stamp.chars().take(10).filter(|c| *c != '-').collect();
    format!("zeeterm-{date}.zsshbak")
}

/// 保留份数下拉里要列出的值：固定几档，当前值不在里面就加进去排好。
pub(super) fn keep_choices(current: usize) -> Vec<usize> {
    let mut choices = KEEP_CHOICES.to_vec();
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_unstable();
    }
    choices
}

/// 列表里一行的两行字：时间；应用版本 + 记录数（解不开的说明原因）。
pub(super) fn row_texts(detail: &BackupDetail) -> (String, String) {
    let title = match detail.created_at {
        Some(at) => format_stamp(at),
        None => detail.info.stamp.clone(),
    };
    let sub = if detail.readable {
        format!(
            "应用版本 {} · {} 条记录（主机 {} 台）· {}",
            detail.app_version.as_deref().unwrap_or("未知"),
            detail.record_count.unwrap_or(0),
            detail.host_count.unwrap_or(0),
            format_size(detail.info.size)
        )
    } else {
        "这份备份读不了（可能是换了 Windows 用户保存的）".to_owned()
    };
    (title, sub)
}

fn muted(text: impl Into<SharedString>, tokens: &Tokens) -> AnyElement {
    div()
        .typo(Role::Caption, tokens)
        .child(text.into())
        .into_any_element()
}

fn colored(text: impl Into<SharedString>, color: ::gpui::Hsla) -> AnyElement {
    div()
        .text_xs()
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
    entity: &Entity<BackupDemo>,
    disabled: bool,
    run: impl Fn(&mut BackupDemo, &mut Window, &mut Context<BackupDemo>) + 'static,
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

impl BackupDemo {
    /// 根：嵌在设置页里只画卡片；独立窗口再加标题和滚动。
    pub(super) fn render_root(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = Tokens::current(cx);
        let entity = cx.entity();
        let cards = vec![
            self.local_card(&entity, &tokens),
            self.offline_card(&entity, &tokens, cx),
        ];
        if self.embedded {
            return v_flex()
                .id("backup-section")
                .w_full()
                .gap_3()
                .children(cards)
                .into_any_element();
        }
        let busy_text = self.busy.map(super::Busy::label).unwrap_or_default();
        v_flex()
            .id("backup-settings-scroll")
            .size_full()
            .p_4()
            .gap_3()
            .track_focus(&self.focus)
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(tokens.background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().child("备份与恢复"))
                    .child(act_button(
                        "backup-create",
                        "新建备份",
                        &entity,
                        self.busy.is_some(),
                        |this, _, cx| this.start_create(cx),
                    ))
                    .when(!busy_text.is_empty(), |header| {
                        header.child(muted(busy_text, &tokens))
                    }),
            )
            .children(cards)
            .into_any_element()
    }

    // ---- 本地备份 ----

    #[inline(never)]
    fn local_card(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let busy = self.busy.is_some();
        let mut rows: Vec<AnyElement> = vec![self.keep_row(entity, tokens, busy)];
        if self.backups.is_empty() {
            rows.push(
                div()
                    .id("backups-empty")
                    .child(muted(
                        if busy {
                            "读取中…"
                        } else {
                            "还没有备份。第一次从云端合并之前会自动做一份。"
                        },
                        tokens,
                    ))
                    .into_any_element(),
            );
        }
        for (index, detail) in self.backups.iter().enumerate() {
            rows.push(self.backup_row(index, detail, entity, tokens, busy));
        }
        // 本卡的状态文字放在最下面：恢复 / 创建的结果，失败原因。
        if !self.status.is_empty() {
            rows.push(
                div()
                    .id("backup-status")
                    .text_sm()
                    .child(self.status.clone())
                    .into_any_element(),
            );
        }
        card(
            "backup-local",
            "本地备份",
            Some(
                "用 Windows 账户加密保存在本机。每次从云端合并之前、每次恢复之前、版本升级后第一次启动时\
                 （库里有数据才备份）会自动留一份。恢复之前会先自动备份当前状态。",
            ),
            tokens,
            rows,
        )
    }

    fn keep_row(&self, entity: &Entity<Self>, tokens: &Tokens, busy: bool) -> AnyElement {
        let mut choices = h_flex().gap_1();
        for count in keep_choices(self.keep) {
            let target = entity.clone();
            choices = choices.child(
                Button::new(("backup-keep", count))
                    .label(format!("{count} 份"))
                    .compact()
                    .toggled(self.keep == count)
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        target.update(cx, |this, cx| this.set_keep(count, cx));
                    }),
            );
        }
        h_flex()
            .w_full()
            .min_h(px(28.0))
            .gap_4()
            .items_center()
            .justify_between()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().typo(Role::ItemName, tokens).child("最多保留份数"))
                    .child(muted(
                        "超过的最早的备份会被清理。调小之后，多出来的旧备份在下一次备份时才删，改这个设置\
                         本身不会删任何东西。",
                        tokens,
                    )),
            )
            .child(choices)
            .into_any_element()
    }

    fn backup_row(
        &self,
        index: usize,
        detail: &BackupDetail,
        entity: &Entity<Self>,
        tokens: &Tokens,
        busy: bool,
    ) -> AnyElement {
        let (title, sub) = row_texts(detail);
        let mut right = v_flex().flex_none().items_end().gap_1();
        let armed = self
            .confirm
            .armed_for(&detail.info.path)
            .map(|armed| armed.name.clone());
        if detail.readable {
            if armed.is_some() {
                right = right.child(
                    h_flex()
                        .gap_1()
                        .child(
                            act_button(
                                ("backup-confirm", index),
                                "确认恢复",
                                entity,
                                busy,
                                |this, _, cx| this.confirm_restore(cx),
                            )
                            .danger(),
                        )
                        .child(act_button(
                            ("backup-cancel", index),
                            "取消",
                            entity,
                            busy,
                            |this, _, cx| this.cancel_restore(cx),
                        )),
                );
            } else {
                let path = detail.info.path.clone();
                let name = detail.info.name.clone();
                right = right.child(act_button(
                    ("backup-restore", index),
                    "恢复",
                    entity,
                    busy,
                    move |this, _, cx| this.arm_restore(path.clone(), name.clone(), cx),
                ));
            }
        }
        v_flex()
            .id(("backup-row", index))
            .gap_1()
            .py_2()
            .border_t_1()
            .border_color(tokens.border)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(div().text_sm().child(title))
                            .child(muted(sub, tokens)),
                    )
                    .child(right),
            )
            .when_some(armed, |col, name| {
                col.child(muted(format!("将恢复：{name}"), tokens))
                    .child(colored(CONFIRM_WARNING, tokens.danger))
            })
            .into_any_element()
    }

    /// 改保留份数：直接写设置；失败把原因显示出来，界面上的值不变。
    pub(super) fn set_keep(&mut self, keep: usize, cx: &mut Context<Self>) {
        match write_keep(&crate::settings::data_dir(), keep) {
            Ok(saved) => self.keep = saved,
            Err(message) => self.status = message,
        }
        cx.notify();
    }

    // ---- 离线备份文件 ----

    #[inline(never)]
    fn offline_card(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let busy = self.busy.is_some();
        let path = std::path::PathBuf::from(self.package_path.read(cx).value().trim().to_string());
        let overwrite_armed = self.overwrite.is_armed_for(&path);
        let mode = self.import_mode;
        let import_label = match mode {
            OfflineImportMode::Merge => "合并导入",
            OfflineImportMode::Replace if self.replace_confirm.is_armed_for(&path) => {
                "确认替换导入"
            }
            OfflineImportMode::Replace => "替换导入",
        };
        let mode_button = |id: &'static str,
                           label: &'static str,
                           target: OfflineImportMode,
                           hint: &'static str| {
            let entity = entity.clone();
            v_flex()
                .gap_0p5()
                .child(
                    Button::new(id)
                        .label(label)
                        .compact()
                        .toggled(mode == target)
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.set_import_mode(target, cx));
                        }),
                )
                .child(muted(hint, tokens))
        };
        let mut rows: Vec<AnyElement> = vec![
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(label_cell("文件"))
                .child(Input::new(&self.package_path).flex_1())
                .child(act_button(
                    "offline-browse-export",
                    "选择保存位置…",
                    entity,
                    busy,
                    |this, window, cx| this.browse(true, window, cx),
                ))
                .child(act_button(
                    "offline-browse-import",
                    "选择文件…",
                    entity,
                    busy,
                    |this, window, cx| this.browse(false, window, cx),
                ))
                .into_any_element(),
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(label_cell("密码"))
                .child(
                    Input::new(&self.password)
                        .flex_1()
                        .content_type(InputContentType::Password),
                )
                .into_any_element(),
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(label_cell("再输一次（导出）"))
                .child(
                    Input::new(&self.password_confirm)
                        .flex_1()
                        .content_type(InputContentType::Password),
                )
                .into_any_element(),
            muted(
                "导出：选保存位置，再设一个密码（至少 8 个字符，忘了密码文件就打不开）。\
                 导入：导入之前会自动给当前状态做一份本地备份，导错了可以在上面的列表恢复。密码不会保存。",
                tokens,
            ),
            h_flex()
                .w_full()
                .gap_6()
                .items_start()
                .child(label_cell("导入方式"))
                .child(mode_button(
                    "offline-import-merge",
                    "合并",
                    OfflineImportMode::Merge,
                    "两边都留。同一条记录两边都改过时取较新的，另一份进冲突页。",
                ))
                .child(mode_button(
                    "offline-import-replace",
                    "替换",
                    OfflineImportMode::Replace,
                    "本机变成和文件一模一样，文件里没有的主机、凭据会被删掉。",
                ))
                .into_any_element(),
            h_flex()
                .gap_2()
                .items_center()
                .child(act_button(
                    "offline-export",
                    "导出",
                    entity,
                    busy,
                    |this, _, cx| this.start_export(cx),
                ))
                .child({
                    let button = act_button(
                        "offline-import",
                        import_label,
                        entity,
                        busy,
                        |this, _, cx| this.start_import(cx),
                    );
                    if mode == OfflineImportMode::Replace {
                        button.danger()
                    } else {
                        button
                    }
                })
                .when(overwrite_armed, |row| {
                    row.child(act_button(
                        "offline-overwrite",
                        "确认覆盖",
                        entity,
                        busy,
                        |this, _, cx| this.confirm_overwrite(cx),
                    ))
                    .child(act_button(
                        "offline-overwrite-cancel",
                        "取消",
                        entity,
                        busy,
                        |this, _, cx| this.cancel_overwrite(cx),
                    ))
                })
                .into_any_element(),
        ];
        if self.embedded && !self.status.is_empty() {
            // 离线包的结果也在这张卡里再说一遍，免得用户在上面的卡里找。
            rows.push(
                div()
                    .id("offline-status")
                    .text_sm()
                    .child(self.status.clone())
                    .into_any_element(),
            );
        }
        card(
            "backup-offline",
            "离线备份文件",
            Some(
                "把主机、分组、凭据（含密码和私钥）、代理、可同步的设置和已信任的主机指纹导出成一个用密码\
                 加密的文件，可以拷到别的电脑导入。端口转发规则不在文件里。",
            ),
            tokens,
            rows,
        )
    }

    /// 「选择保存位置…」/「选择文件…」：系统对话框选好了就填进路径框。
    pub(super) fn browse(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let dir = crate::settings::data_dir();
        if save {
            let receiver = cx.prompt_for_new_path(&dir, Some(&default_file_name()));
            cx.spawn_in(window, async move |this, cx| {
                if let Ok(Ok(Some(path))) = receiver.await {
                    let _ = this.update_in(cx, |this, window, cx| this.fill_path(path, window, cx));
                }
            })
            .detach();
        } else {
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
                    let _ = this.update_in(cx, |this, window, cx| this.fill_path(path, window, cx));
                }
            })
            .detach();
        }
    }

    /// 路径框换成选好的文件；原来置为覆盖 / 替换确认的状态不再对应这份文件，一起撤掉。
    fn fill_path(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.overwrite.disarm();
        self.replace_confirm.disarm();
        let text = path.to_string_lossy().into_owned();
        self.package_path
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }
}

#[cfg(test)]
mod tests {
    use zssh_core::sync::backup::{BackupDetail, BackupInfo};

    use super::{default_file_name, keep_choices, row_texts};

    fn detail(readable: bool) -> BackupDetail {
        BackupDetail {
            info: BackupInfo {
                path: "C:\\data\\backups\\a.bin".to_owned(),
                name: "a.bin".to_owned(),
                size: 1536,
                stamp: "20260101-010203".to_owned(),
                sequence: 1,
            },
            readable,
            created_at: readable.then_some(1_700_000_000),
            app_version: readable.then(|| "0.4.0".to_owned()),
            record_count: readable.then_some(9),
            host_count: readable.then_some(3),
        }
    }

    #[test]
    fn rows_read_like_the_old_front_end() {
        let (title, sub) = row_texts(&detail(true));
        assert_eq!(title.len(), 16, "{title}");
        assert!(
            sub.starts_with("应用版本 0.4.0 · 9 条记录（主机 3 台）"),
            "{sub}"
        );
        assert!(sub.ends_with("1.5 KB"), "{sub}");
    }

    #[test]
    fn an_unreadable_backup_says_why_and_shows_the_file_stamp() {
        let (title, sub) = row_texts(&detail(false));
        assert_eq!(title, "20260101-010203");
        assert_eq!(sub, "这份备份读不了（可能是换了 Windows 用户保存的）");
    }

    #[test]
    fn keep_choices_include_the_current_value_in_order() {
        assert_eq!(keep_choices(20), vec![5, 10, 20, 50, 100, 200]);
        assert_eq!(keep_choices(7), vec![5, 7, 10, 20, 50, 100, 200]);
        assert_eq!(keep_choices(500), vec![5, 10, 20, 50, 100, 200, 500]);
    }

    #[test]
    fn the_default_file_name_is_dated_and_has_the_old_extension() {
        let name = default_file_name();
        assert!(
            name.starts_with("zeeterm-") && name.ends_with(".zsshbak"),
            "{name}"
        );
        assert_eq!(name.len(), "zeeterm-20260101.zsshbak".len(), "{name}");
    }
}
