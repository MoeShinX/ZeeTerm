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
use crate::i18n::{Msg, t};
use crate::settings_view::card;
use crate::sync::format_stamp;
use crate::typography::{Role, Typo as _};

/// 本文件（「从 Z-SSH 迁移」一块）自己的界面文案。
///
/// 只放应用自己的固定文案；主机名、文件夹、时间、数量这些来自数据或由 `format!` 带进来的
/// 内容不进这张表。带参数的句子拆成固定片段，数字和用户数据留在 `format!` 里。
#[derive(Clone, Copy, Debug)]
enum MigrateViewMsg {
    // ---- 直接导入：查找结果 ----
    CheckingLocal,
    NotFoundLocal,
    LastUsedPrefix,
    FoundYesPrefix,
    FoundYesMiddle,
    // ---- 预览：数字 ----
    AddedPrefix,
    UpdatedMiddle,
    DeletedMiddle,
    HostsUnit,
    ItemsUnit,
    RecordsUnit,
    SkippedMiddle,
    SkippedRecordsSuffix,
    QuickCommandsSuffix,
    SettingsCountMiddle,
    SettingsCountSuffix,
    PreviewConflictsSuffix,
    // ---- 预览 / 结果的标签 ----
    Hosts,
    Groups,
    Credentials,
    Proxies,
    TrustedHostKeys,
    QuickCommands,
    SyncableSettings,
    Conflicts,
    Settings,
    ClearedReferences,
    BackupBeforeImport,
    // ---- 预览：提醒 ----
    DuplicateHostsPrefix,
    DuplicateHostsSuffix,
    ReplaceDeletesPrefix,
    ReplaceDeletesGroups,
    ReplaceDeletesCredentials,
    ReplaceDeletesProxies,
    ReplaceDeletesSuffix,
    NulledReferencesPrefix,
    NulledReferencesSuffix,
    DirectNote,
    FileNote,
    // ---- 结果 ----
    DoneSkippedMiddle,
    KnownKeysAddedSuffix,
    KnownKeysSkippedSuffix,
    SettingsChangedPrefix,
    SettingsChangedMiddle,
    SettingsChangedSuffix,
    DoneConflictsSuffix,
    DoneNulledRefsSuffix,
    BackupSuffix,
    // ---- 画：卡片 / 两条路 ----
    CardTitle,
    CardDescription,
    DirectTitle,
    PickFolder,
    ReadAndPreview,
    FileTitle,
    FileDescription,
    FileLabel,
    PickFile,
    PasswordLabel,
    ExitZsshHint,
    ExitedRetry,
    // ---- 画：预览 / 结果 ----
    PreviewDirectTitle,
    PreviewFileTitle,
    ImportMerge,
    ImportConfirmReplace,
    ImportReplace,
    ImportMode,
    ModeMerge,
    ModeMergeHint,
    ModeReplace,
    ModeReplaceHint,
    Cancel,
    ReplaceArmedWarning,
    BackupNote,
    ImportDone,
    OpenHosts,
}

impl Msg for MigrateViewMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::CheckingLocal => "正在查找本机的 Z-SSH 数据…",
            Self::NotFoundLocal => {
                "本机没有找到 Z-SSH 的数据。如果装在别处、或者自定义过数据文件夹，点「指定文件夹…」选它（里面应该有 data.db）。"
            }
            Self::LastUsedPrefix => "，最后使用 ",
            Self::FoundYesPrefix => "已找到 Z-SSH 的数据（数据库 ",
            Self::FoundYesMiddle => "）。只读取，不会改动它。\n文件夹：",
            Self::AddedPrefix => "新增 ",
            Self::UpdatedMiddle => "，更新 ",
            Self::DeletedMiddle => "，删除 ",
            Self::HostsUnit => " 台：",
            Self::ItemsUnit => " 个：",
            Self::RecordsUnit => " 条：",
            Self::SkippedMiddle => "，跳过 ",
            Self::SkippedRecordsSuffix => " 条可疑记录",
            Self::QuickCommandsSuffix => " 条（随可同步的设置一起导入）",
            Self::SettingsCountMiddle => " 项，其中 ",
            Self::SettingsCountSuffix => " 项和现在不同",
            Self::PreviewConflictsSuffix => " 条（同一条记录两边都改过：取较新的，另一份进冲突页）",
            Self::Hosts => "主机",
            Self::Groups => "分组",
            Self::Credentials => "凭据（含密码和私钥）",
            Self::Proxies => "代理",
            Self::TrustedHostKeys => "已信任的主机指纹",
            Self::QuickCommands => "快捷命令",
            Self::SyncableSettings => "可同步的设置",
            Self::Conflicts => "冲突",
            Self::Settings => "设置",
            Self::ClearedReferences => "清空的引用",
            Self::BackupBeforeImport => "导入前的备份",
            Self::DuplicateHostsPrefix => "其中 ",
            Self::DuplicateHostsSuffix => {
                " 台主机的名称、地址、端口、用户名和 ZeeTerm 里已有的完全一样：合并后两边各留一份（删掉不要的那份即可）。"
            }
            Self::ReplaceDeletesPrefix => {
                "替换会让 ZeeTerm 和这份数据一模一样：ZeeTerm 里有、这里没有的会被删除（主机 "
            }
            Self::ReplaceDeletesGroups => "、分组 ",
            Self::ReplaceDeletesCredentials => "、凭据 ",
            Self::ReplaceDeletesProxies => "、代理 ",
            Self::ReplaceDeletesSuffix => "）。",
            Self::NulledReferencesPrefix => "有 ",
            Self::NulledReferencesSuffix => " 处引用指向不存在的记录，导入时会清空。",
            Self::DirectNote => "端口转发规则和主机备注不会导入（Z-SSH 的备份文件里也没有它们）。",
            Self::FileNote => "端口转发规则不在备份文件里，不会导入。",
            Self::DoneSkippedMiddle => "，已有且相同而跳过 ",
            Self::KnownKeysAddedSuffix => " 条",
            Self::KnownKeysSkippedSuffix => " 条可疑主机密钥记录",
            Self::SettingsChangedPrefix => "改动 ",
            Self::SettingsChangedMiddle => " 项（含快捷命令 ",
            Self::SettingsChangedSuffix => " 条）",
            Self::DoneConflictsSuffix => " 条（两边都改过，保留了较新的，另一份在冲突页）",
            Self::DoneNulledRefsSuffix => " 处（指向的记录不存在）",
            Self::BackupSuffix => "（导错了可以在「本地备份」里恢复）",
            Self::CardTitle => "从 Z-SSH 迁移",
            Self::CardDescription => {
                "把 Z-SSH 里的主机、分组、凭据（含密码和私钥）、代理、已信任的主机指纹、快捷命令和可同步的设置搬到 ZeeTerm。导入前会先预览，并自动备份 ZeeTerm 现在的数据。"
            }
            Self::DirectTitle => "直接导入本机的 Z-SSH 数据（推荐）",
            Self::PickFolder => "指定文件夹…",
            Self::ReadAndPreview => "读取并预览",
            Self::FileTitle => "导入 Z-SSH 导出的备份文件",
            Self::FileDescription => {
                "在 Z-SSH 的「设置 → 同步 → 离线备份文件」里导出的 .zsshbak。格式和 ZeeTerm 的离线备份相同，选好文件、输入当时设的密码就能导入。"
            }
            Self::FileLabel => "文件",
            Self::PickFile => "选择文件…",
            Self::PasswordLabel => "密码",
            Self::ExitZsshHint => "退出 Z-SSH（包括托盘里的图标）后点「重试」。",
            Self::ExitedRetry => "我已退出 Z-SSH，重试",
            Self::PreviewDirectTitle => "预览：本机 Z-SSH 的数据",
            Self::PreviewFileTitle => "预览：备份文件里的数据",
            Self::ImportMerge => "合并导入",
            Self::ImportConfirmReplace => "确认替换导入",
            Self::ImportReplace => "替换导入",
            Self::ImportMode => "导入方式",
            Self::ModeMerge => "合并（推荐）",
            Self::ModeMergeHint => "两边都留。同一条记录两边都改过时取较新的，另一份进冲突页。",
            Self::ModeReplace => "替换",
            Self::ModeReplaceHint => {
                "ZeeTerm 变成和这份数据一模一样，这里没有的主机、凭据会被删掉。"
            }
            Self::Cancel => "取消",
            Self::ReplaceArmedWarning => {
                "替换会删除 ZeeTerm 里这份数据没有的记录，再点一次「确认替换导入」才会执行。"
            }
            Self::BackupNote => {
                "导入前会自动给 ZeeTerm 现在的数据做一份本地备份，导错了可以在下面的「本地备份」里恢复。"
            }
            Self::ImportDone => "导入完成",
            Self::OpenHosts => "打开主机页",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::CheckingLocal => "Looking for Z-SSH data on this computer…",
            Self::NotFoundLocal => {
                "No Z-SSH data was found on this computer. If it is installed elsewhere or you changed its data folder, click \"Choose Folder…\" and select it (it should contain data.db)."
            }
            Self::LastUsedPrefix => ", last used ",
            Self::FoundYesPrefix => "Found Z-SSH data (database ",
            Self::FoundYesMiddle => "). Read-only; nothing is changed.\nFolder: ",
            Self::AddedPrefix => "Added ",
            Self::UpdatedMiddle => ", updated ",
            Self::DeletedMiddle => ", deleted ",
            Self::HostsUnit => " host(s): ",
            Self::ItemsUnit => " item(s): ",
            Self::RecordsUnit => " item(s): ",
            Self::SkippedMiddle => ", skipped ",
            Self::SkippedRecordsSuffix => " suspicious record(s)",
            Self::QuickCommandsSuffix => " item(s) (imported with syncable settings)",
            Self::SettingsCountMiddle => " item(s), ",
            Self::SettingsCountSuffix => " differ from the current settings",
            Self::PreviewConflictsSuffix => {
                " item(s) (the same record changed on both sides: the newer one is kept, the other goes to the conflicts page)"
            }
            Self::Hosts => "Hosts",
            Self::Groups => "Groups",
            Self::Credentials => "Credentials (passwords and private keys)",
            Self::Proxies => "Proxies",
            Self::TrustedHostKeys => "Trusted host keys",
            Self::QuickCommands => "Quick commands",
            Self::SyncableSettings => "Syncable settings",
            Self::Conflicts => "Conflicts",
            Self::Settings => "Settings",
            Self::ClearedReferences => "Cleared references",
            Self::BackupBeforeImport => "Backup before import",
            Self::DuplicateHostsPrefix => "",
            Self::DuplicateHostsSuffix => {
                " host(s) have the same name, address, port, and username as existing ZeeTerm entries: after merging both are kept (delete the one you do not want)."
            }
            Self::ReplaceDeletesPrefix => {
                "Replace makes ZeeTerm identical to this data: anything in ZeeTerm that is not here will be deleted (hosts "
            }
            Self::ReplaceDeletesGroups => ", groups ",
            Self::ReplaceDeletesCredentials => ", credentials ",
            Self::ReplaceDeletesProxies => ", proxies ",
            Self::ReplaceDeletesSuffix => ").",
            Self::NulledReferencesPrefix => "",
            Self::NulledReferencesSuffix => {
                " references point to records that do not exist; they will be cleared on import."
            }
            Self::DirectNote => {
                "Port forwarding rules and host notes are not imported (they are not in Z-SSH backups either)."
            }
            Self::FileNote => {
                "Port forwarding rules are not in the backup file and are not imported."
            }
            Self::DoneSkippedMiddle => ", already present and unchanged, skipped ",
            Self::KnownKeysAddedSuffix => " item(s)",
            Self::KnownKeysSkippedSuffix => " suspicious host key record(s)",
            Self::SettingsChangedPrefix => "Changed ",
            Self::SettingsChangedMiddle => " item(s) (including ",
            Self::SettingsChangedSuffix => " quick commands)",
            Self::DoneConflictsSuffix => {
                " item(s) (changed on both sides; the newer one was kept and the other is on the conflicts page)"
            }
            Self::DoneNulledRefsSuffix => " references (targets do not exist)",
            Self::BackupSuffix => " (if something went wrong, restore it from Local Backups)",
            Self::CardTitle => "Migrate from Z-SSH",
            Self::CardDescription => {
                "Move hosts, groups, credentials (passwords and private keys), proxies, trusted host keys, quick commands, and syncable settings from Z-SSH to ZeeTerm. You will see a preview before importing, and ZeeTerm's current data is backed up automatically."
            }
            Self::DirectTitle => "Import Z-SSH data from this computer (recommended)",
            Self::PickFolder => "Choose Folder…",
            Self::ReadAndPreview => "Read and Preview",
            Self::FileTitle => "Import a backup file exported by Z-SSH",
            Self::FileDescription => {
                "The .zsshbak exported from Z-SSH under \"Settings → Sync → Offline Backup File\". The format is the same as ZeeTerm's offline backups; pick the file and enter the password you set then."
            }
            Self::FileLabel => "File",
            Self::PickFile => "Choose File…",
            Self::PasswordLabel => "Password",
            Self::ExitZsshHint => "Quit Z-SSH (including its tray icon), then click \"Retry\".",
            Self::ExitedRetry => "I Quit Z-SSH, Retry",
            Self::PreviewDirectTitle => "Preview: Z-SSH data on this computer",
            Self::PreviewFileTitle => "Preview: data in the backup file",
            Self::ImportMerge => "Import (Merge)",
            Self::ImportConfirmReplace => "Confirm Replace Import",
            Self::ImportReplace => "Replace Import",
            Self::ImportMode => "Import mode",
            Self::ModeMerge => "Merge (recommended)",
            Self::ModeMergeHint => {
                "Keep both sides. When the same record changed on both sides, the newer one wins and the other goes to the conflicts page."
            }
            Self::ModeReplace => "Replace",
            Self::ModeReplaceHint => {
                "ZeeTerm becomes identical to this data; hosts and credentials that are not here will be deleted."
            }
            Self::Cancel => "Cancel",
            Self::ReplaceArmedWarning => {
                "Replace deletes records in ZeeTerm that are not in this data. Click \"Confirm Replace Import\" again to proceed."
            }
            Self::BackupNote => {
                "ZeeTerm's current data is backed up locally before importing; if something went wrong, restore it from Local Backups below."
            }
            Self::ImportDone => "Import complete",
            Self::OpenHosts => "Open Hosts Page",
        }
    }
}

// ---- 文字 ----

/// 直接导入那一行的说明：找没找到、在哪、什么时候用过。
pub(super) fn found_text(found: &Found, folder_error: Option<&str>) -> String {
    let base = match found {
        Found::Checking => t(MigrateViewMsg::CheckingLocal).to_owned(),
        Found::Yes(source) => found_yes_text(source),
        Found::No => t(MigrateViewMsg::NotFoundLocal).to_owned(),
    };
    match folder_error {
        Some(error) => format!("{base}\n{error}"),
        None => base,
    }
}

fn found_yes_text(source: &ZsshSource) -> String {
    let used = source
        .modified_at
        .map(|at| format!("{}{}", t(MigrateViewMsg::LastUsedPrefix), format_stamp(at)))
        .unwrap_or_default();
    format!(
        "{}{}{}{}{}",
        t(MigrateViewMsg::FoundYesPrefix),
        crate::format::size(source.db_bytes),
        used,
        t(MigrateViewMsg::FoundYesMiddle),
        source.data_dir.display()
    )
}

fn changes(counts: &ChangeCounts, replace: bool) -> String {
    let mut text = format!(
        "{}{}{}{}",
        t(MigrateViewMsg::AddedPrefix),
        counts.inserted,
        t(MigrateViewMsg::UpdatedMiddle),
        counts.updated
    );
    if replace && counts.deleted > 0 {
        text.push_str(&format!(
            "{}{}",
            t(MigrateViewMsg::DeletedMiddle),
            counts.deleted
        ));
    }
    text
}

/// 预览：每一类有多少、导入后新增 / 更新 / 删除多少。
pub(super) fn preview_rows(preview: &ImportPreview) -> Vec<(&'static str, String)> {
    let replace = preview.mode == OfflineImportMode::Replace;
    let source = &preview.source;
    vec![
        (
            t(MigrateViewMsg::Hosts),
            format!(
                "{}{}{}",
                source.hosts,
                t(MigrateViewMsg::HostsUnit),
                changes(&preview.hosts, replace)
            ),
        ),
        (
            t(MigrateViewMsg::Groups),
            format!(
                "{}{}{}",
                source.groups,
                t(MigrateViewMsg::ItemsUnit),
                changes(&preview.groups, replace)
            ),
        ),
        (
            t(MigrateViewMsg::Credentials),
            format!(
                "{}{}{}",
                source.identities,
                t(MigrateViewMsg::RecordsUnit),
                changes(&preview.identities, replace)
            ),
        ),
        (
            t(MigrateViewMsg::Proxies),
            format!(
                "{}{}{}",
                source.proxies,
                t(MigrateViewMsg::ItemsUnit),
                changes(&preview.proxies, replace)
            ),
        ),
        (
            t(MigrateViewMsg::TrustedHostKeys),
            if preview.known_hosts_skipped > 0 {
                format!(
                    "{}{}{}{}{}{}{}",
                    source.known_hosts,
                    t(MigrateViewMsg::RecordsUnit),
                    t(MigrateViewMsg::AddedPrefix),
                    preview.known_hosts_added,
                    t(MigrateViewMsg::SkippedMiddle),
                    preview.known_hosts_skipped,
                    t(MigrateViewMsg::SkippedRecordsSuffix)
                )
            } else {
                format!(
                    "{}{}{}{}",
                    source.known_hosts,
                    t(MigrateViewMsg::RecordsUnit),
                    t(MigrateViewMsg::AddedPrefix),
                    preview.known_hosts_added
                )
            },
        ),
        (
            t(MigrateViewMsg::QuickCommands),
            format!(
                "{}{}",
                source.quick_commands,
                t(MigrateViewMsg::QuickCommandsSuffix)
            ),
        ),
        (
            t(MigrateViewMsg::SyncableSettings),
            format!(
                "{}{}{}{}",
                source.settings,
                t(MigrateViewMsg::SettingsCountMiddle),
                preview.settings_changed,
                t(MigrateViewMsg::SettingsCountSuffix)
            ),
        ),
        (
            t(MigrateViewMsg::Conflicts),
            format!(
                "{}{}",
                preview.conflicts,
                t(MigrateViewMsg::PreviewConflictsSuffix)
            ),
        ),
    ]
}

/// 预览下面要特别提醒的话。
pub(super) fn preview_warnings(preview: &ImportPreview, origin: &Origin) -> Vec<String> {
    let mut lines = Vec::new();
    if preview.duplicate_hosts > 0 {
        lines.push(format!(
            "{}{}{}",
            t(MigrateViewMsg::DuplicateHostsPrefix),
            preview.duplicate_hosts,
            t(MigrateViewMsg::DuplicateHostsSuffix)
        ));
    }
    if preview.mode == OfflineImportMode::Replace {
        lines.push(format!(
            "{}{}{}{}{}{}{}{}{}",
            t(MigrateViewMsg::ReplaceDeletesPrefix),
            preview.hosts.deleted,
            t(MigrateViewMsg::ReplaceDeletesGroups),
            preview.groups.deleted,
            t(MigrateViewMsg::ReplaceDeletesCredentials),
            preview.identities.deleted,
            t(MigrateViewMsg::ReplaceDeletesProxies),
            preview.proxies.deleted,
            t(MigrateViewMsg::ReplaceDeletesSuffix)
        ));
    }
    if preview.nulled_references > 0 {
        lines.push(format!(
            "{}{}{}",
            t(MigrateViewMsg::NulledReferencesPrefix),
            preview.nulled_references,
            t(MigrateViewMsg::NulledReferencesSuffix)
        ));
    }
    lines.push(match origin {
        Origin::Direct => t(MigrateViewMsg::DirectNote).to_owned(),
        Origin::File => t(MigrateViewMsg::FileNote).to_owned(),
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
            "{}{}{}{}{}{}",
            t(MigrateViewMsg::AddedPrefix),
            counts.inserted,
            t(MigrateViewMsg::UpdatedMiddle),
            counts.updated,
            t(MigrateViewMsg::DoneSkippedMiddle),
            skipped
        );
        if replace && counts.deleted > 0 {
            text.push_str(&format!(
                "{}{}",
                t(MigrateViewMsg::DeletedMiddle),
                counts.deleted
            ));
        }
        text
    };
    let mut rows = vec![
        (
            t(MigrateViewMsg::Hosts).to_owned(),
            line(preview.source.hosts, &preview.hosts),
        ),
        (
            t(MigrateViewMsg::Groups).to_owned(),
            line(preview.source.groups, &preview.groups),
        ),
        (
            t(MigrateViewMsg::Credentials).to_owned(),
            line(preview.source.identities, &preview.identities),
        ),
        (
            t(MigrateViewMsg::Proxies).to_owned(),
            line(preview.source.proxies, &preview.proxies),
        ),
        (
            t(MigrateViewMsg::TrustedHostKeys).to_owned(),
            if applied.known_hosts_skipped > 0 {
                format!(
                    "{}{}{}{}{}",
                    t(MigrateViewMsg::AddedPrefix),
                    applied.known_hosts_added,
                    t(MigrateViewMsg::SkippedMiddle),
                    applied.known_hosts_skipped,
                    t(MigrateViewMsg::KnownKeysSkippedSuffix)
                )
            } else {
                format!(
                    "{}{}{}",
                    t(MigrateViewMsg::AddedPrefix),
                    applied.known_hosts_added,
                    t(MigrateViewMsg::KnownKeysAddedSuffix)
                )
            },
        ),
        (
            t(MigrateViewMsg::Settings).to_owned(),
            format!(
                "{}{}{}{}{}",
                t(MigrateViewMsg::SettingsChangedPrefix),
                preview.settings_changed,
                t(MigrateViewMsg::SettingsChangedMiddle),
                preview.source.quick_commands,
                t(MigrateViewMsg::SettingsChangedSuffix)
            ),
        ),
        (
            t(MigrateViewMsg::Conflicts).to_owned(),
            format!(
                "{}{}",
                report.conflicts,
                t(MigrateViewMsg::DoneConflictsSuffix)
            ),
        ),
    ];
    if !report.nulled_references.is_empty() {
        rows.push((
            t(MigrateViewMsg::ClearedReferences).to_owned(),
            format!(
                "{}{}",
                report.nulled_references.len(),
                t(MigrateViewMsg::DoneNulledRefsSuffix)
            ),
        ));
    }
    rows.push((
        t(MigrateViewMsg::BackupBeforeImport).to_owned(),
        format!(
            "{}{}",
            report.safety_backup.name,
            t(MigrateViewMsg::BackupSuffix)
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
            t(MigrateViewMsg::CardTitle),
            Some(t(MigrateViewMsg::CardDescription)),
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
                            .child(t(MigrateViewMsg::DirectTitle)),
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
                        t(MigrateViewMsg::PickFolder),
                        entity,
                        busy,
                        |this, window, cx| this.browse_folder(window, cx),
                    ))
                    .child(
                        act_button(
                            "migrate-direct-read",
                            t(MigrateViewMsg::ReadAndPreview),
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
                            .child(t(MigrateViewMsg::FileTitle)),
                    )
                    .child(muted(t(MigrateViewMsg::FileDescription), tokens)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(label_cell(t(MigrateViewMsg::FileLabel)))
                    .child(Input::new(&self.file_path).flex_1())
                    .child(act_button(
                        "migrate-pick-file",
                        t(MigrateViewMsg::PickFile),
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
                    .child(label_cell(t(MigrateViewMsg::PasswordLabel)))
                    .child(
                        Input::new(&self.file_password)
                            .flex_1()
                            .content_type(InputContentType::Password),
                    )
                    .child(
                        act_button(
                            "migrate-file-read",
                            t(MigrateViewMsg::ReadAndPreview),
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
                .child(muted(t(MigrateViewMsg::ExitZsshHint), tokens))
                .child(
                    h_flex().child(
                        act_button(
                            "migrate-retry",
                            t(MigrateViewMsg::ExitedRetry),
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
            Origin::Direct => t(MigrateViewMsg::PreviewDirectTitle),
            Origin::File => t(MigrateViewMsg::PreviewFileTitle),
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
            OfflineImportMode::Merge => t(MigrateViewMsg::ImportMerge),
            OfflineImportMode::Replace if self.state.replace_armed => {
                t(MigrateViewMsg::ImportConfirmReplace)
            }
            OfflineImportMode::Replace => t(MigrateViewMsg::ImportReplace),
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
                    .child(label_cell(t(MigrateViewMsg::ImportMode)))
                    .child(mode_button(
                        "migrate-mode-merge",
                        t(MigrateViewMsg::ModeMerge),
                        OfflineImportMode::Merge,
                        t(MigrateViewMsg::ModeMergeHint),
                    ))
                    .child(mode_button(
                        "migrate-mode-replace",
                        t(MigrateViewMsg::ModeReplace),
                        OfflineImportMode::Replace,
                        t(MigrateViewMsg::ModeReplaceHint),
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(import_button)
                    .child(act_button(
                        "migrate-cancel",
                        t(MigrateViewMsg::Cancel),
                        entity,
                        busy,
                        |this, _, cx| this.cancel(cx),
                    )),
            )
            .when(
                mode == OfflineImportMode::Replace && self.state.replace_armed,
                |col| {
                    col.child(colored(
                        t(MigrateViewMsg::ReplaceArmedWarning),
                        tokens.danger,
                    ))
                },
            )
            .child(muted(t(MigrateViewMsg::BackupNote), tokens))
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
                    .child(t(MigrateViewMsg::ImportDone)),
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
                        t(MigrateViewMsg::OpenHosts),
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
