//! 备份的画法（G11）：按 0.2.14 设置页的样子分成两张卡——本地备份（保留份数 + 列表 + 恢复）、
//! 离线备份文件（导出 / 导入，合并或替换）。设置页里嵌的是这两张卡；独立窗口（调试入口）
//! 只是再套一层标题和滚动容器，并多一个「新建备份」按钮（0.2.14 没有手动新建，只会自动留）。
//!
//! 逻辑都在 `backup.rs`（F5 修好的那一套：恢复走引擎、替换要确认、覆盖要确认、和同步互斥），
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

use super::{BackupDemo, BackupMsg, write_keep};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::settings_view::card;
use crate::sync::format_stamp;
use crate::typography::{Role, Typo as _};

/// 备份卡片（`view.rs`）自己的界面文案：标题、说明、按钮、标签和状态行。
///
/// 只放应用自己的固定文案；备份文件名、路径和解不开的备份原因照原样显示，带参数的句子
/// 保留 `format!`，这里只提供固定片段。id（`backup-local`、`offline-export`……）不进这张表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackupViewMsg {
    // ---- 列表行 ----
    AppVersionPrefix,
    VersionSeparator,
    RecordCountPrefix,
    HostCountSeparator,
    UnknownVersion,
    UnreadableBackup,
    // ---- 本地备份卡 ----
    LocalCardTitle,
    LocalCardDescription,
    KeepTitle,
    KeepDescription,
    KeepUnit,
    NoBackups,
    Loading,
    Restore,
    ConfirmRestore,
    Cancel,
    RestoreWillPrefix,
    // ---- 离线备份卡 ----
    OfflineCardTitle,
    OfflineCardDescription,
    FileLabel,
    ChooseExportPath,
    ChooseImportFile,
    PasswordLabel,
    PasswordConfirmLabel,
    OfflineHint,
    ExportCredentialsNote,
    ImportModeLabel,
    MergeChoice,
    ReplaceChoice,
    BusyCannotChangeChoice,
    MergeDescription,
    ReplaceDescription,
    ImportMerge,
    ImportReplaceConfirm,
    ImportReplace,
    Export,
    ConfirmOverwrite,
    // ---- 同步账号设置提示 ----
    NoPrimarySync,
    AccountsPromptTitle,
    AccountsPromptPrefix,
    AccountsPromptMiddle,
    AccountsPromptSuffix,
    KeepLocal,
    ReplaceWithBackup,
    // ---- 独立窗口标题 / 按钮 ----
    WindowTitle,
    CreateButton,
}

impl Msg for BackupViewMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::AppVersionPrefix => "应用版本 ",
            Self::VersionSeparator => " · ",
            Self::RecordCountPrefix => " 条记录（主机 ",
            Self::HostCountSeparator => " 台）· ",
            Self::UnknownVersion => "未知",
            Self::UnreadableBackup if !cfg!(windows) => {
                "这份备份读不了（可能是另一个用户或另一台电脑保存的，或本机的资料库密钥不可用）"
            }
            Self::UnreadableBackup => "这份备份读不了（可能是换了 Windows 用户保存的）",
            Self::LocalCardTitle => "本地备份",
            Self::LocalCardDescription => {
                "用 Windows 账户加密保存在本机。每次从云端合并之前、每次恢复之前、版本升级后第一次启动时\
                 （库里有数据才备份）会自动留一份。恢复之前会先自动备份当前状态。备份里也带着同步账号设置\
                 （存储连接、登录信息、云端备份目标），恢复后不用重新填。"
            }
            Self::KeepTitle => "最多保留份数",
            Self::KeepDescription => {
                "超过的最早的备份会被清理。调小之后，多出来的旧备份在下一次备份时才删，改这个设置本身不会删任何东西。"
            }
            Self::KeepUnit => "份",
            Self::NoBackups => "还没有备份。第一次从云端合并之前会自动做一份。",
            Self::Loading => "读取中…",
            Self::Restore => "恢复",
            Self::ConfirmRestore => "确认恢复",
            Self::Cancel => "取消",
            Self::RestoreWillPrefix => "将恢复：",
            Self::OfflineCardTitle => "离线备份文件",
            Self::OfflineCardDescription => {
                "把主机、分组、凭据（含密码和私钥）、代理、可同步的设置和已信任的主机指纹导出成一个用密码\
                 加密的文件，可以拷到别的电脑导入。端口转发规则不在文件里。"
            }
            Self::FileLabel => "文件",
            Self::ChooseExportPath => "选择保存位置…",
            Self::ChooseImportFile => "选择文件…",
            Self::PasswordLabel => "密码",
            Self::PasswordConfirmLabel => "再输一次（导出时用）",
            Self::OfflineHint => {
                "导出：选保存位置，再设一个密码（至少 8 个字符，忘了密码文件就打不开）。\
                 导入：导入之前会自动给当前状态做一份本地备份，导错了可以在上面的列表恢复。密码不会保存。"
            }
            Self::ExportCredentialsNote => {
                "备份文件里包含同步账号（S3 / WebDAV / OneDrive 等）的登录信息，请妥善保管"
            }
            Self::ImportModeLabel => "导入方式",
            Self::MergeChoice => "合并",
            Self::ReplaceChoice => "替换",
            Self::BusyCannotChangeChoice => "正在处理，稍后再选",
            Self::MergeDescription => "两边都留。同一条记录两边都改过时取较新的，另一份进冲突页。",
            Self::ReplaceDescription => "本机变成和文件一模一样，文件里没有的主机、凭据会被删掉。",
            Self::ImportMerge => "合并导入",
            Self::ImportReplaceConfirm => "确认替换导入",
            Self::ImportReplace => "替换导入",
            Self::Export => "导出",
            Self::ConfirmOverwrite => "确认覆盖",
            Self::NoPrimarySync => "没有主同步",
            Self::AccountsPromptTitle => "用备份里的同步设置替换本机现在的？",
            Self::AccountsPromptPrefix => "备份里带着同步账号设置（主同步：",
            Self::AccountsPromptMiddle => "，云端备份目标 ",
            Self::AccountsPromptSuffix => {
                " 个），本机已经配置了同步。替换会换掉本机现在的存储连接、登录信息、记住的主密码和云端备份目标；不替换就保留本机现在的。"
            }
            Self::KeepLocal => "保留本机现在的",
            Self::ReplaceWithBackup => "替换为备份里的",
            Self::WindowTitle => "备份与恢复",
            Self::CreateButton => "新建备份",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::AppVersionPrefix => "App version ",
            Self::VersionSeparator => " · ",
            Self::RecordCountPrefix => " records (hosts: ",
            Self::HostCountSeparator => ") · ",
            Self::UnknownVersion => "unknown",
            Self::UnreadableBackup if !cfg!(windows) => {
                "This backup cannot be read (it may have been saved by another user or on another computer, or this computer's data key is unavailable)"
            }
            Self::UnreadableBackup => {
                "This backup cannot be read (it may have been saved under a different Windows user)"
            }
            Self::LocalCardTitle => "Local Backups",
            Self::LocalCardDescription => {
                "Encrypted with your Windows account and stored on this computer. A backup is kept automatically \
                 before every merge from the cloud, before every restore, and on the first start after an upgrade \
                 (only when the database has data). The current state is backed up automatically before a restore. \
                 Backups also carry the sync account settings (storage connection, sign-in info, cloud backup \
                 targets), so you do not need to fill them in again after restoring."
            }
            Self::KeepTitle => "Backups to Keep",
            Self::KeepDescription => {
                "The oldest backups beyond this limit are cleaned up. If you lower it, the extra old backups are \
                 removed at the next backup; changing this setting deletes nothing by itself."
            }
            Self::KeepUnit => "files",
            Self::NoBackups => {
                "No backups yet. One is made automatically before the first merge from the cloud."
            }
            Self::Loading => "Reading…",
            Self::Restore => "Restore",
            Self::ConfirmRestore => "Confirm Restore",
            Self::Cancel => "Cancel",
            Self::RestoreWillPrefix => "Will restore: ",
            Self::OfflineCardTitle => "Offline Backup File",
            Self::OfflineCardDescription => {
                "Exports hosts, groups, credentials (including passwords and private keys), proxies, syncable \
                 settings and trusted host fingerprints into a password-encrypted file that you can copy to another \
                 computer and import there. Port forwarding rules are not included."
            }
            Self::FileLabel => "File",
            Self::ChooseExportPath => "Choose Save Location…",
            Self::ChooseImportFile => "Choose File…",
            Self::PasswordLabel => "Password",
            Self::PasswordConfirmLabel => "Enter it again (for export)",
            Self::OfflineHint => {
                "Export: choose a save location and set a password (at least 8 characters; if you forget it the \
                 file cannot be opened). Import: the current state is backed up locally first, so you can restore \
                 it from the list above if you import the wrong file. The password is never saved."
            }
            Self::ExportCredentialsNote => {
                "The backup file contains sign-in information for sync accounts (S3 / WebDAV / OneDrive etc.); keep it safe"
            }
            Self::ImportModeLabel => "Import Mode",
            Self::MergeChoice => "Merge",
            Self::ReplaceChoice => "Replace",
            Self::BusyCannotChangeChoice => "Working, choose again in a moment",
            Self::MergeDescription => {
                "Keep both sides. When the same record changed on both sides the newer one wins and the other goes to the conflicts page."
            }
            Self::ReplaceDescription => {
                "This computer becomes identical to the file; hosts and credentials that the file does not contain are deleted."
            }
            Self::ImportMerge => "Merge Import",
            Self::ImportReplaceConfirm => "Confirm Replace Import",
            Self::ImportReplace => "Replace Import",
            Self::Export => "Export",
            Self::ConfirmOverwrite => "Confirm Overwrite",
            Self::NoPrimarySync => "No primary sync",
            Self::AccountsPromptTitle => {
                "Replace this computer's sync settings with the ones from the backup?"
            }
            Self::AccountsPromptPrefix => {
                "The backup carries sync account settings (primary sync: "
            }
            Self::AccountsPromptMiddle => ", cloud backup targets ",
            Self::AccountsPromptSuffix => {
                "), and this computer already has sync configured. Replacing swaps this computer's storage connection, sign-in info, remembered master password and cloud backup targets; keeping keeps this computer's current ones."
            }
            Self::KeepLocal => "Keep This Computer's",
            Self::ReplaceWithBackup => "Use the Backup's",
            Self::WindowTitle => "Backup & Restore",
            Self::CreateButton => "New Backup",
        }
    }
}

/// 导出界面的提醒：文件里带着同步账号的登录信息。
pub(super) fn export_credentials_note() -> &'static str {
    t(BackupViewMsg::ExportCredentialsNote)
}

/// 离线备份的默认文件名（`zeeterm-日期.zsshbak`）：扩展名和 Z-SSH 0.2.14 一样，两边的备份文件互相能导入；
/// 文件名改成产品名，免得和 Z-SSH 导出的混在一起分不清。
pub(super) fn default_file_name() -> String {
    let stamp = format_stamp(super::now_secs());
    let date: String = stamp.chars().take(10).filter(|c| *c != '-').collect();
    format!("zeeterm-{date}.zsshbak")
}

/// 列表里一行的两行字：时间；应用版本 + 记录数（解不开的说明原因）。
pub(super) fn row_texts(detail: &BackupDetail) -> (String, String) {
    let title = match detail.created_at {
        Some(at) => format_stamp(at),
        None => detail.info.stamp.clone(),
    };
    let sub = if detail.readable {
        format!(
            "{}{}{}{}{}{}{}{}",
            t(BackupViewMsg::AppVersionPrefix),
            detail
                .app_version
                .as_deref()
                .unwrap_or(t(BackupViewMsg::UnknownVersion)),
            t(BackupViewMsg::VersionSeparator),
            detail.record_count.unwrap_or(0),
            t(BackupViewMsg::RecordCountPrefix),
            detail.host_count.unwrap_or(0),
            t(BackupViewMsg::HostCountSeparator),
            crate::format::size(detail.info.size)
        )
    } else {
        t(BackupViewMsg::UnreadableBackup).to_owned()
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

/// 字段上方的小标签：表单是「上标签、下输入框」的单栏（V2-U17）。
fn label_above(label: &'static str, tokens: &Tokens) -> impl IntoElement {
    div().typo(Role::FieldLabel, tokens).child(label)
}

/// 标签在上、一个整行宽控件在下。
fn labelled(label: &'static str, control: impl IntoElement, tokens: &Tokens) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1()
        .child(label_above(label, tokens))
        .child(control)
        .into_any_element()
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
                    .child(div().text_lg().child(t(BackupViewMsg::WindowTitle)))
                    .child(act_button(
                        "backup-create",
                        t(BackupViewMsg::CreateButton),
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
        let mut rows: Vec<AnyElement> = vec![self.keep_row(tokens, busy)];
        if self.backups.is_empty() {
            rows.push(
                div()
                    .id("backups-empty")
                    .child(muted(
                        if busy {
                            t(BackupViewMsg::Loading)
                        } else {
                            t(BackupViewMsg::NoBackups)
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
        if let Some(prompt) = self.accounts_prompt(super::AccountsOrigin::Local, entity, tokens) {
            rows.push(prompt);
        }
        card(
            "backup-local",
            t(BackupViewMsg::LocalCardTitle),
            Some(t(BackupViewMsg::LocalCardDescription)),
            tokens,
            rows,
        )
    }

    fn keep_row(&self, tokens: &Tokens, busy: bool) -> AnyElement {
        // 份数是一个 1–200 的整数：输入框，回车或点到别处时保存；不合法的不保存，就地写范围。
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
                    .child(
                        div()
                            .typo(Role::ItemName, tokens)
                            .child(t(BackupViewMsg::KeepTitle)),
                    )
                    .child(muted(t(BackupViewMsg::KeepDescription), tokens))
                    .when_some(self.keep_error.clone(), |col, message| {
                        col.child(
                            div()
                                .id("backup-keep-error")
                                .child(colored(message, tokens.danger)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(88.0))
                            .child(Input::new(&self.keep_input).w_full().disabled(busy)),
                    )
                    .child(muted(t(BackupViewMsg::KeepUnit), tokens)),
            )
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
                                t(BackupViewMsg::ConfirmRestore),
                                entity,
                                busy,
                                |this, _, cx| this.confirm_restore(cx),
                            )
                            .danger(),
                        )
                        .child(act_button(
                            ("backup-cancel", index),
                            t(BackupViewMsg::Cancel),
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
                    t(BackupViewMsg::Restore),
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
                col.child(muted(
                    format!("{}{name}", t(BackupViewMsg::RestoreWillPrefix)),
                    tokens,
                ))
                .child(colored(t(BackupMsg::ConfirmWarning), tokens.danger))
            })
            .into_any_element()
    }

    /// 输入框回车 / 失焦：合法就保存（和现在的值一样不重复写），不合法只显示提示、不保存。
    pub(super) fn commit_keep_input(&mut self, cx: &mut Context<Self>) {
        let text = self.keep_input.read(cx).value().to_string();
        match super::parse_keep(&text) {
            Ok(keep) => {
                self.keep_error = None;
                if keep != self.keep {
                    self.set_keep(keep, cx);
                }
            }
            Err(message) => self.keep_error = Some(message),
        }
        cx.notify();
    }

    /// 把已经读到 / 存好的份数显示到输入框里（输入框里的数字已经是这个值就不动，免得打断输入）。
    pub(super) fn show_keep(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = self.keep.to_string();
        let shown = self.keep_input.read(cx).value().trim().to_owned();
        if shown != want {
            self.keep_input
                .update(cx, |input, cx| input.set_value(want, window, cx));
        }
        self.keep_error = None;
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
            OfflineImportMode::Merge => t(BackupViewMsg::ImportMerge),
            OfflineImportMode::Replace if self.replace_confirm.is_armed_for(&path) => {
                t(BackupViewMsg::ImportReplaceConfirm)
            }
            OfflineImportMode::Replace => t(BackupViewMsg::ImportReplace),
        };
        let mut rows: Vec<AnyElement> = vec![
            labelled(
                t(BackupViewMsg::FileLabel),
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(Input::new(&self.package_path).flex_1())
                    .child(act_button(
                        "offline-browse-export",
                        t(BackupViewMsg::ChooseExportPath),
                        entity,
                        busy,
                        |this, window, cx| this.browse(true, window, cx),
                    ))
                    .child(act_button(
                        "offline-browse-import",
                        t(BackupViewMsg::ChooseImportFile),
                        entity,
                        busy,
                        |this, window, cx| this.browse(false, window, cx),
                    ))
                    .into_any_element(),
                tokens,
            ),
            labelled(
                t(BackupViewMsg::PasswordLabel),
                Input::new(&self.password)
                    .w_full()
                    .content_type(InputContentType::Password),
                tokens,
            ),
            labelled(
                t(BackupViewMsg::PasswordConfirmLabel),
                Input::new(&self.password_confirm)
                    .w_full()
                    .content_type(InputContentType::Password),
                tokens,
            ),
            muted(t(BackupViewMsg::OfflineHint), tokens),
            div()
                .id("offline-credentials-note")
                .child(colored(export_credentials_note(), tokens.caution))
                .into_any_element(),
            v_flex()
                .w_full()
                .gap_1()
                .child(label_above(t(BackupViewMsg::ImportModeLabel), tokens))
                .child(crate::main_window::widgets::segmented_bar(
                    "offline-import-mode",
                    vec![
                        (
                            t(BackupViewMsg::MergeChoice).to_owned(),
                            mode == OfflineImportMode::Merge,
                            busy.then_some(t(BackupViewMsg::BusyCannotChangeChoice)),
                        ),
                        (
                            t(BackupViewMsg::ReplaceChoice).to_owned(),
                            mode == OfflineImportMode::Replace,
                            busy.then_some(t(BackupViewMsg::BusyCannotChangeChoice)),
                        ),
                    ],
                    tokens,
                    {
                        let target = entity.clone();
                        move |index, _, cx| {
                            let picked = if index == 0 {
                                OfflineImportMode::Merge
                            } else {
                                OfflineImportMode::Replace
                            };
                            target.update(cx, |this, cx| this.set_import_mode(picked, cx));
                        }
                    },
                ))
                .child(muted(
                    match mode {
                        OfflineImportMode::Merge => t(BackupViewMsg::MergeDescription),
                        OfflineImportMode::Replace => t(BackupViewMsg::ReplaceDescription),
                    },
                    tokens,
                ))
                .into_any_element(),
            h_flex()
                .gap_2()
                .items_center()
                .child(act_button(
                    "offline-export",
                    t(BackupViewMsg::Export),
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
                        t(BackupViewMsg::ConfirmOverwrite),
                        entity,
                        busy,
                        |this, _, cx| this.confirm_overwrite(cx),
                    ))
                    .child(act_button(
                        "offline-overwrite-cancel",
                        t(BackupViewMsg::Cancel),
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
        if let Some(prompt) = self.accounts_prompt(super::AccountsOrigin::Offline, entity, tokens) {
            rows.push(prompt);
        }
        card(
            "backup-offline",
            t(BackupViewMsg::OfflineCardTitle),
            Some(t(BackupViewMsg::OfflineCardDescription)),
            tokens,
            rows,
        )
    }

    /// 恢复 / 导入时备份里带着同步账号设置、本机又已经配置了同步：问要不要替换，默认不替换。
    /// 只在触发这次恢复 / 导入的那张卡里画。
    fn accounts_prompt(
        &self,
        origin: super::AccountsOrigin,
        entity: &Entity<Self>,
        tokens: &Tokens,
    ) -> Option<AnyElement> {
        let pending = self
            .pending_accounts
            .as_ref()
            .filter(|p| p.origin == origin)?;
        let what = match pending.accounts.storage.as_ref() {
            Some(config) => match config.provider() {
                "webdav" => "WebDAV",
                "s3" => "S3",
                "github" => "GitHub Gist",
                _ => "OneDrive",
            },
            None => t(BackupViewMsg::NoPrimarySync),
        };
        let targets = pending.accounts.backup_targets.len();
        Some(
            div()
                .id("backup-accounts-prompt")
                .child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(tokens.dot_connecting.opacity(0.5))
                        .bg(tokens.dot_connecting.opacity(0.1))
                        .text_xs()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(tokens.caution)
                                .child(t(BackupViewMsg::AccountsPromptTitle)),
                        )
                        .child(format!(
                            "{}{what}{}{targets}{}",
                            t(BackupViewMsg::AccountsPromptPrefix),
                            t(BackupViewMsg::AccountsPromptMiddle),
                            t(BackupViewMsg::AccountsPromptSuffix)
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(act_button(
                                    "backup-accounts-keep",
                                    t(BackupViewMsg::KeepLocal),
                                    entity,
                                    false,
                                    |this, _, cx| this.keep_accounts(cx),
                                ))
                                .child(
                                    act_button(
                                        "backup-accounts-replace",
                                        t(BackupViewMsg::ReplaceWithBackup),
                                        entity,
                                        false,
                                        |this, _, cx| this.replace_accounts(cx),
                                    )
                                    .danger(),
                                ),
                        ),
                )
                .into_any_element(),
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

    use super::{default_file_name, row_texts};

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
    fn the_default_file_name_is_dated_and_has_the_old_extension() {
        let name = default_file_name();
        assert!(
            name.starts_with("zeeterm-") && name.ends_with(".zsshbak"),
            "{name}"
        );
        assert_eq!(name.len(), "zeeterm-20260101.zsshbak".len(), "{name}");
    }
}

#[cfg(test)]
mod export_note_tests {
    #[test]
    fn the_export_note_warns_that_the_file_holds_sync_logins() {
        let note = super::export_credentials_note();
        assert!(
            note.contains("同步账号") && note.contains("登录信息") && note.contains("妥善保管"),
            "{note}"
        );
    }
}
