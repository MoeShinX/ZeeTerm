//! 右键菜单的内容和禁用规则（0.2.14 `state/sftpMenu.ts`）。
//!
//! 菜单长什么样由 [`build_menu`] 按“哪一栏、点在文件 / 文件夹 / 空白处、选了几项、
//! 服务器上有什么工具”算出来；界面（`view` 里的 `PopupMenu`）只负责画，所以每种情形都
//! 可以直接单元测试。不支持的项不隐藏，置灰并写明原因（悬停不到的地方就把原因写在项的右边）。

use zssh_core::sftp::archive::{ArchiveCaps, ArchiveFormat, extract_folder_name};
use zssh_core::sftp::transfer::TransferState;

use super::model::{Entry, EntryKind, TargetKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Local,
    Remote,
}

/// 内置编辑器能打开的文件大小上限（和后端 `MAX_EDIT_BYTES` 一致：5 MB）。
pub(super) const EDIT_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// 菜单项的动作名（`SftpDemo::run_menu_action` 按它分派）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuAction {
    Open,
    EditBuiltin,
    Transfer,
    DownloadTo,
    Copy,
    Paste,
    CopyPath,
    Rename,
    Delete,
    Mkdir,
    Create,
    Permissions,
    Refresh,
    Properties,
    CompressTarGz,
    CompressZip,
    ExtractFolder,
    ExtractHere,
    TransferRetry,
    TransferPause,
    TransferResume,
    TransferCancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MenuNode {
    Item {
        action: MenuAction,
        label: String,
        shortcut: Option<&'static str>,
        /// `Some` = 置灰，值是原因。
        reason: Option<String>,
    },
    Submenu {
        label: &'static str,
        reason: Option<String>,
        children: Vec<MenuNode>,
    },
    Separator,
}

impl MenuNode {
    #[cfg(test)]
    pub(super) fn disabled(&self) -> bool {
        match self {
            Self::Item { reason, .. } | Self::Submenu { reason, .. } => reason.is_some(),
            Self::Separator => false,
        }
    }

    #[cfg(test)]
    fn label(&self) -> &str {
        match self {
            Self::Item { label, .. } => label,
            Self::Submenu { label, .. } => label,
            Self::Separator => "",
        }
    }
}

/// 哪些功能已经接上了（没接的先置灰写明原因）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MenuFeatures {
    pub(super) builtin_editor: bool,
    pub(super) clipboard: bool,
    pub(super) properties: bool,
}

/// 远端压缩 / 解压的能力：第一次右键时还在探测服务器上有没有 tar / zip / unzip。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArchiveMenuState {
    Pending,
    Ready(ArchiveCaps),
}

pub(super) const ARCHIVE_PENDING_REASON: &str = "正在检测服务器上的压缩工具…";
pub(super) const NOT_UTF8_REASON: &str = "文件名编码不是 UTF-8，暂不支持操作";
const FEATURE_PENDING: &str = "这个功能还在做";

#[derive(Debug, Clone)]
pub(super) struct MenuContext {
    pub(super) side: Side,
    /// `true`：点在空白处（`entries` 是空的）。
    pub(super) blank: bool,
    pub(super) entries: Vec<Entry>,
    /// 这一栏当前目录；本地的空字符串 = “此电脑”（盘符列表）。
    pub(super) current_dir: String,
    /// 剪贴板里有几项、来自哪一边；没有就是 `None`。
    pub(super) clipboard: Option<(Side, usize)>,
    pub(super) features: MenuFeatures,
    pub(super) archive: Option<ArchiveMenuState>,
}

fn item(
    action: MenuAction,
    label: &str,
    reason: Option<String>,
    shortcut: Option<&'static str>,
) -> MenuNode {
    MenuNode::Item {
        action,
        label: label.to_owned(),
        shortcut,
        reason,
    }
}

/// 去掉开头、结尾和连续的分隔线（某一组全被省略时不留空线）。
pub(super) fn tidy_separators(nodes: Vec<MenuNode>) -> Vec<MenuNode> {
    let mut out: Vec<MenuNode> = Vec::new();
    for node in nodes {
        if node == MenuNode::Separator && out.last().is_none_or(|last| *last == MenuNode::Separator)
        {
            continue;
        }
        out.push(node);
    }
    while out.last() == Some(&MenuNode::Separator) {
        out.pop();
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    One,
    Some,
}

/// 为什么不能对这些条目做某类操作；`None` = 可以。
fn selection_reason(entries: &[Entry], need: Need, forbid_drives: bool) -> Option<String> {
    if entries.is_empty() {
        return Some("先选中一个条目".to_owned());
    }
    if need == Need::One && entries.len() != 1 {
        return Some("只能对单个条目操作".to_owned());
    }
    if entries.iter().any(|e| !e.name_is_utf8) {
        return Some(NOT_UTF8_REASON.to_owned());
    }
    if forbid_drives && entries.iter().any(Entry::is_drive) {
        return Some("盘符不能这样操作".to_owned());
    }
    None
}

/// 编辑类动作（内置编辑器）为什么不可用；`None` = 可以。
fn edit_reason(entries: &[Entry]) -> Option<String> {
    if entries.is_empty() {
        return Some("先选中一个文件".to_owned());
    }
    if entries.len() != 1 {
        return Some("编辑只能对单个文件".to_owned());
    }
    let entry = &entries[0];
    if !entry.name_is_utf8 {
        return Some(NOT_UTF8_REASON.to_owned());
    }
    if entry.is_enterable() || entry.target_kind == Some(TargetKind::Dir) {
        return Some(if entry.is_drive() {
            "盘符不能编辑".to_owned()
        } else {
            "文件夹不能用编辑器打开".to_owned()
        });
    }
    if entry.kind == EntryKind::Other {
        return Some("这种特殊文件不能编辑".to_owned());
    }
    if entry.size.is_some_and(|size| size > EDIT_MAX_BYTES) {
        return Some(format!(
            "文件超过 {} MB，请下载后再看",
            EDIT_MAX_BYTES / 1024 / 1024
        ));
    }
    None
}

/// 粘贴为什么不可用；`None` = 可以。
pub(super) fn paste_reason(context: &MenuContext) -> Option<String> {
    if !context.features.clipboard {
        return Some(FEATURE_PENDING.to_owned());
    }
    match context.clipboard {
        None | Some((_, 0)) => Some("剪贴板是空的".to_owned()),
        Some((side, _)) if side != context.side => Some("远端和本地之间请用上传 / 下载".to_owned()),
        _ if context.current_dir.is_empty() => Some(dir_missing(context.side)),
        _ => None,
    }
}

fn dir_missing(side: Side) -> String {
    match side {
        Side::Local => "先进入一个文件夹",
        Side::Remote => "先进入一个目录",
    }
    .to_owned()
}

/// 当前目录能不能新建东西（远端没进任何目录、本地在“此电脑”时不行）。
fn dir_reason(context: &MenuContext) -> Option<String> {
    context
        .current_dir
        .is_empty()
        .then(|| dir_missing(context.side))
}

/// 解压这种格式服务器上要有什么命令；缺了返回原因。
pub(super) fn extract_tool_reason(format: ArchiveFormat, caps: &ArchiveCaps) -> Option<String> {
    if format.is_zip() {
        (!caps.unzip).then(|| "服务器上没有 unzip 命令".to_owned())
    } else {
        (!caps.tar).then(|| "服务器上没有 tar 命令".to_owned())
    }
}

/// 压缩成这种格式服务器上要有什么命令。
pub(super) fn compress_tool_reason(zip: bool, caps: &ArchiveCaps) -> Option<String> {
    if zip {
        (!caps.zip).then(|| "服务器上没有 zip 命令".to_owned())
    } else {
        (!caps.tar).then(|| "服务器上没有 tar 命令".to_owned())
    }
}

/// 是不是能解压的条目：文件（或指向文件的链接）、名字合法、后缀认得。
pub(super) fn is_extractable(entry: &Entry) -> bool {
    entry.name_is_utf8 && entry.is_file_like() && ArchiveFormat::detect(&entry.name).is_some()
}

/// 压缩 / 解压那一组（只有远端、点在条目上才有）：压缩包上先给「解压到「同名文件夹」」
/// 「解压到当前目录」，然后是「压缩为」子菜单。服务器上缺命令的项置灰并写明原因；
/// 登录 shell 不是 POSIX 整组不显示。
fn archive_nodes(context: &MenuContext) -> Vec<MenuNode> {
    let (Side::Remote, Some(state)) = (context.side, context.archive) else {
        return Vec::new();
    };
    let caps = match state {
        ArchiveMenuState::Pending => None,
        ArchiveMenuState::Ready(caps) if !caps.posix => return Vec::new(),
        ArchiveMenuState::Ready(caps) => Some(caps),
    };
    let pending = || Some(ARCHIVE_PENDING_REASON.to_owned());
    let mut nodes = Vec::new();

    if context.entries.len() == 1 && is_extractable(&context.entries[0]) {
        let name = &context.entries[0].name;
        if let Some(format) = ArchiveFormat::detect(name) {
            let reason = match &caps {
                Some(caps) => extract_tool_reason(format, caps),
                None => pending(),
            };
            if let Some(folder) = extract_folder_name(name) {
                nodes.push(item(
                    MenuAction::ExtractFolder,
                    &format!("解压到「{folder}」"),
                    reason.clone(),
                    None,
                ));
            }
            nodes.push(item(
                MenuAction::ExtractHere,
                "解压到当前目录",
                reason,
                None,
            ));
        }
    }

    let selection = selection_reason(&context.entries, Need::Some, true);
    let submenu_reason =
        selection.or_else(|| caps.is_none().then(|| ARCHIVE_PENDING_REASON.to_owned()));
    let child = |action, label: &str, zip: bool| {
        let reason = match &caps {
            Some(caps) => compress_tool_reason(zip, caps),
            None => submenu_reason.clone(),
        };
        item(action, label, reason, None)
    };
    nodes.push(MenuNode::Submenu {
        label: "压缩为",
        reason: submenu_reason.clone(),
        children: vec![
            child(MenuAction::CompressTarGz, ".tar.gz", false),
            child(MenuAction::CompressZip, ".zip", true),
        ],
    });
    nodes
}

/// 按上下文算出完整菜单。
pub(super) fn build_menu(context: &MenuContext) -> Vec<MenuNode> {
    let entries = &context.entries;
    let is_remote = context.side == Side::Remote;
    let new_dir_reason = dir_reason(context);

    let new_menu = MenuNode::Submenu {
        label: "新建",
        reason: new_dir_reason.clone(),
        children: vec![
            item(MenuAction::Mkdir, "文件夹", new_dir_reason.clone(), None),
            item(MenuAction::Create, "文件", new_dir_reason.clone(), None),
        ],
    };
    let paste = item(
        MenuAction::Paste,
        "粘贴",
        paste_reason(context),
        Some("Ctrl+V"),
    );
    let refresh = item(MenuAction::Refresh, "刷新", None, Some("F5"));
    let properties = item(
        MenuAction::Properties,
        "属性",
        if !context.features.properties {
            Some(FEATURE_PENDING.to_owned())
        } else if context.blank {
            new_dir_reason.clone()
        } else {
            selection_reason(entries, Need::Some, true)
        },
        Some("Alt+Enter"),
    );

    if context.blank {
        return tidy_separators(vec![
            paste,
            MenuNode::Separator,
            new_menu,
            MenuNode::Separator,
            refresh,
            properties,
        ]);
    }

    let builtin = if context.features.builtin_editor {
        edit_reason(entries)
    } else {
        Some(FEATURE_PENDING.to_owned())
    };
    let transfer_reason = selection_reason(entries, Need::Some, true);
    let copy_reason = if context.features.clipboard {
        selection_reason(entries, Need::Some, true)
    } else {
        Some(FEATURE_PENDING.to_owned())
    };
    let open_reason = selection_reason(entries, Need::One, false);

    let mut nodes = vec![
        item(MenuAction::Open, "打开", open_reason, None),
        item(MenuAction::EditBuiltin, "用内置编辑器编辑", builtin, None),
        MenuNode::Separator,
        item(
            MenuAction::Transfer,
            if is_remote {
                "下载到本地"
            } else {
                "上传到远端"
            },
            transfer_reason.clone(),
            None,
        ),
    ];
    if is_remote {
        nodes.push(item(
            MenuAction::DownloadTo,
            "下载到…",
            transfer_reason,
            None,
        ));
    }
    nodes.push(MenuNode::Separator);
    nodes.extend(archive_nodes(context));
    nodes.push(MenuNode::Separator);
    nodes.extend([
        item(MenuAction::Copy, "复制", copy_reason, Some("Ctrl+C")),
        paste,
        item(
            MenuAction::CopyPath,
            "复制路径",
            entries.is_empty().then(|| "先选中一个条目".to_owned()),
            None,
        ),
        MenuNode::Separator,
        item(
            MenuAction::Rename,
            "重命名",
            selection_reason(entries, Need::One, true),
            Some("F2"),
        ),
        item(
            MenuAction::Delete,
            "删除",
            selection_reason(entries, Need::Some, true),
            Some("Del"),
        ),
        MenuNode::Separator,
        new_menu,
    ]);
    if is_remote {
        nodes.push(item(
            MenuAction::Permissions,
            "修改权限",
            selection_reason(entries, Need::One, false),
            None,
        ));
    }
    nodes.push(MenuNode::Separator);
    nodes.push(refresh);
    nodes.push(properties);
    tidy_separators(nodes)
}

// ---------------------------------------------------------------------------
// 传输队列里一行的菜单
// ---------------------------------------------------------------------------

/// 能重试：失败、已取消、已跳过（跳过的会重新问一次同名文件怎么处理）。
pub(super) fn transfer_can_retry(state: &TransferState) -> bool {
    matches!(
        state,
        TransferState::Failed(_) | TransferState::Cancelled | TransferState::Skipped
    )
}

/// 能暂停：排队、准备、传输中（等同名文件的回答、等重连时暂停没有意义）。
pub(super) fn transfer_can_pause(state: &TransferState) -> bool {
    matches!(
        state,
        TransferState::Running | TransferState::Queued | TransferState::Planning
    )
}

/// 没结束（能取消）。
pub(super) fn transfer_is_live(state: &TransferState) -> bool {
    !state.is_terminal()
}

/// 传输队列里一行的右键菜单：和这一行右边的按钮出现的条件一致。
/// 没有可做的操作（已完成）返回空，界面就不弹菜单。
pub(super) fn build_transfer_menu(state: &TransferState) -> Vec<MenuNode> {
    let mut nodes = Vec::new();
    let mut add = |action, label: &str| nodes.push(item(action, label, None, None));
    if transfer_can_retry(state) {
        add(MenuAction::TransferRetry, "重试");
    }
    if transfer_can_pause(state) {
        add(MenuAction::TransferPause, "暂停");
    }
    if *state == TransferState::Paused {
        add(MenuAction::TransferResume, "继续");
    }
    if transfer_is_live(state) {
        add(MenuAction::TransferCancel, "取消");
    }
    nodes
}

#[cfg(test)]
mod tests {
    use zssh_core::sftp::archive::ArchiveCaps;
    use zssh_core::sftp::transfer::TransferState;

    use super::super::model::{Entry, EntryKind, TargetKind};
    use super::{
        ARCHIVE_PENDING_REASON, ArchiveMenuState, EDIT_MAX_BYTES, MenuAction, MenuContext,
        MenuFeatures, MenuNode, NOT_UTF8_REASON, Side, build_menu, build_transfer_menu,
        paste_reason, tidy_separators,
    };

    const FEATURES: MenuFeatures = MenuFeatures {
        builtin_editor: true,
        clipboard: false,
        properties: true,
    };

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.to_owned(),
            path: format!("/srv/{name}"),
            kind,
            target_kind: None,
            link_target: None,
            size: Some(10),
            mtime: None,
            permissions: None,
            owner: None,
            hidden: false,
            name_is_utf8: true,
        }
    }

    fn context(side: Side, blank: bool, entries: Vec<Entry>) -> MenuContext {
        MenuContext {
            side,
            blank,
            entries,
            current_dir: "/srv".to_owned(),
            clipboard: None,
            features: FEATURES,
            archive: None,
        }
    }

    fn find(nodes: &[MenuNode], action: MenuAction) -> Option<&MenuNode> {
        nodes.iter().find_map(|node| match node {
            MenuNode::Item { action: a, .. } if *a == action => Some(node),
            MenuNode::Submenu { children, .. } => find(children, action),
            _ => None,
        })
    }

    fn reason(nodes: &[MenuNode], action: MenuAction) -> Option<String> {
        match find(nodes, action).expect("菜单里应该有这一项") {
            MenuNode::Item { reason, .. } => reason.clone(),
            _ => unreachable!(),
        }
    }

    fn labels(nodes: &[MenuNode]) -> Vec<String> {
        nodes
            .iter()
            .filter(|n| **n != MenuNode::Separator)
            .map(|n| n.label().to_owned())
            .collect()
    }

    #[test]
    fn a_blank_menu_has_paste_new_refresh_and_properties() {
        let nodes = build_menu(&context(Side::Remote, true, vec![]));
        assert_eq!(labels(&nodes), ["粘贴", "新建", "刷新", "属性"]);
        // 粘贴这一步还没接上：置灰并写明；属性可用（当前目录自己）。
        assert_eq!(
            reason(&nodes, MenuAction::Paste).as_deref(),
            Some("这个功能还在做")
        );
        assert_eq!(reason(&nodes, MenuAction::Properties), None);
        assert_eq!(reason(&nodes, MenuAction::Mkdir), None);
    }

    #[test]
    fn without_a_directory_nothing_can_be_created() {
        let mut ctx = context(Side::Local, true, vec![]);
        ctx.current_dir = String::new();
        let nodes = build_menu(&ctx);
        assert_eq!(
            reason(&nodes, MenuAction::Mkdir).as_deref(),
            Some("先进入一个文件夹")
        );
        assert_eq!(
            reason(&nodes, MenuAction::Properties).as_deref(),
            Some("先进入一个文件夹")
        );
        let mut ctx = context(Side::Remote, true, vec![]);
        ctx.current_dir = String::new();
        assert_eq!(
            reason(&build_menu(&ctx), MenuAction::Create).as_deref(),
            Some("先进入一个目录")
        );
    }

    #[test]
    fn a_remote_file_menu_matches_the_card() {
        let nodes = build_menu(&context(
            Side::Remote,
            false,
            vec![entry("a.txt", EntryKind::File)],
        ));
        assert_eq!(
            labels(&nodes),
            [
                "打开",
                "用内置编辑器编辑",
                "下载到本地",
                "下载到…",
                "复制",
                "粘贴",
                "复制路径",
                "重命名",
                "删除",
                "新建",
                "修改权限",
                "刷新",
                "属性"
            ]
        );
        assert_eq!(reason(&nodes, MenuAction::EditBuiltin), None);
        assert_eq!(reason(&nodes, MenuAction::Rename), None);
        assert_eq!(reason(&nodes, MenuAction::Permissions), None);
        // 复制 / 粘贴还没接上。
        assert!(reason(&nodes, MenuAction::Copy).is_some());
        assert!(reason(&nodes, MenuAction::Paste).is_some());
    }

    #[test]
    fn a_local_menu_uploads_and_has_no_download_to_or_permissions() {
        let nodes = build_menu(&context(
            Side::Local,
            false,
            vec![entry("a.txt", EntryKind::File)],
        ));
        let names = labels(&nodes);
        assert!(names.contains(&"上传到远端".to_owned()));
        assert!(!names.contains(&"下载到…".to_owned()));
        assert!(!names.contains(&"修改权限".to_owned()));
    }

    #[test]
    fn folders_cannot_be_opened_in_the_editor_but_can_be_renamed() {
        let nodes = build_menu(&context(
            Side::Remote,
            false,
            vec![entry("dir", EntryKind::Dir)],
        ));
        assert_eq!(
            reason(&nodes, MenuAction::EditBuiltin).as_deref(),
            Some("文件夹不能用编辑器打开")
        );
        assert_eq!(reason(&nodes, MenuAction::Rename), None);
        assert_eq!(reason(&nodes, MenuAction::Transfer), None);
    }

    #[test]
    fn big_files_and_special_files_refuse_the_builtin_editor() {
        let mut big = entry("big.log", EntryKind::File);
        big.size = Some(EDIT_MAX_BYTES + 1);
        let nodes = build_menu(&context(Side::Remote, false, vec![big]));
        assert_eq!(
            reason(&nodes, MenuAction::EditBuiltin).as_deref(),
            Some("文件超过 5 MB，请下载后再看")
        );
        let nodes = build_menu(&context(
            Side::Remote,
            false,
            vec![entry("sock", EntryKind::Other)],
        ));
        assert_eq!(
            reason(&nodes, MenuAction::EditBuiltin).as_deref(),
            Some("这种特殊文件不能编辑")
        );
        // 指向目录的链接也算文件夹。
        let mut link = entry("l", EntryKind::Symlink);
        link.target_kind = Some(TargetKind::Dir);
        let nodes = build_menu(&context(Side::Remote, false, vec![link]));
        assert_eq!(
            reason(&nodes, MenuAction::EditBuiltin).as_deref(),
            Some("文件夹不能用编辑器打开")
        );
    }

    #[test]
    fn local_files_have_no_builtin_editor_yet() {
        let mut ctx = context(Side::Local, false, vec![entry("a", EntryKind::File)]);
        ctx.features.builtin_editor = false;
        assert_eq!(
            reason(&build_menu(&ctx), MenuAction::EditBuiltin).as_deref(),
            Some("这个功能还在做")
        );
    }

    #[test]
    fn a_multi_selection_disables_the_single_item_actions() {
        let nodes = build_menu(&context(
            Side::Remote,
            false,
            vec![entry("a", EntryKind::File), entry("b", EntryKind::File)],
        ));
        for action in [
            MenuAction::Open,
            MenuAction::EditBuiltin,
            MenuAction::Rename,
            MenuAction::Permissions,
        ] {
            assert!(reason(&nodes, action).is_some(), "{action:?}");
        }
        for action in [
            MenuAction::Transfer,
            MenuAction::DownloadTo,
            MenuAction::Delete,
            MenuAction::CopyPath,
            MenuAction::Properties,
        ] {
            assert_eq!(reason(&nodes, action), None, "{action:?}");
        }
    }

    #[test]
    fn non_utf8_names_cannot_be_operated_on() {
        let mut bad = entry("\u{fffd}x", EntryKind::File);
        bad.name_is_utf8 = false;
        let nodes = build_menu(&context(Side::Remote, false, vec![bad]));
        for action in [
            MenuAction::Open,
            MenuAction::Transfer,
            MenuAction::Rename,
            MenuAction::Delete,
            MenuAction::Permissions,
            MenuAction::Properties,
        ] {
            assert_eq!(
                reason(&nodes, action).as_deref(),
                Some(NOT_UTF8_REASON),
                "{action:?}"
            );
        }
        // 复制路径不改名字，仍可用。
        assert_eq!(reason(&nodes, MenuAction::CopyPath), None);
    }

    #[test]
    fn drives_cannot_be_deleted_or_renamed() {
        let nodes = build_menu(&context(
            Side::Local,
            false,
            vec![entry("C:", EntryKind::Drive)],
        ));
        assert_eq!(
            reason(&nodes, MenuAction::Delete).as_deref(),
            Some("盘符不能这样操作")
        );
        assert_eq!(
            reason(&nodes, MenuAction::Rename).as_deref(),
            Some("盘符不能这样操作")
        );
        assert_eq!(reason(&nodes, MenuAction::Open), None);
    }

    fn caps(tar: bool, zip: bool, unzip: bool) -> ArchiveMenuState {
        ArchiveMenuState::Ready(ArchiveCaps {
            posix: true,
            tar,
            zip,
            unzip,
        })
    }

    #[test]
    fn archives_get_the_extract_items_and_everything_the_compress_submenu() {
        let mut ctx = context(
            Side::Remote,
            false,
            vec![entry("site.tar.gz", EntryKind::File)],
        );
        ctx.archive = Some(caps(true, true, true));
        let nodes = build_menu(&ctx);
        match find(&nodes, MenuAction::ExtractFolder) {
            Some(MenuNode::Item { label, reason, .. }) => {
                assert_eq!(label, "解压到「site」");
                assert!(reason.is_none());
            }
            other => panic!("缺少解压项：{other:?}"),
        }
        assert_eq!(reason(&nodes, MenuAction::ExtractHere), None);
        assert_eq!(reason(&nodes, MenuAction::CompressTarGz), None);
        assert_eq!(reason(&nodes, MenuAction::CompressZip), None);
    }

    #[test]
    fn plain_files_have_no_extract_items() {
        let mut ctx = context(Side::Remote, false, vec![entry("a.txt", EntryKind::File)]);
        ctx.archive = Some(caps(true, true, true));
        let nodes = build_menu(&ctx);
        assert!(find(&nodes, MenuAction::ExtractHere).is_none());
        assert!(find(&nodes, MenuAction::CompressZip).is_some());
    }

    #[test]
    fn missing_server_tools_grey_out_only_what_needs_them() {
        let mut ctx = context(Side::Remote, false, vec![entry("a.zip", EntryKind::File)]);
        ctx.archive = Some(caps(true, false, false));
        let nodes = build_menu(&ctx);
        assert_eq!(
            reason(&nodes, MenuAction::ExtractHere).as_deref(),
            Some("服务器上没有 unzip 命令")
        );
        assert_eq!(
            reason(&nodes, MenuAction::CompressZip).as_deref(),
            Some("服务器上没有 zip 命令")
        );
        assert_eq!(reason(&nodes, MenuAction::CompressTarGz), None);
    }

    #[test]
    fn while_probing_the_server_the_archive_items_wait() {
        let mut ctx = context(Side::Remote, false, vec![entry("a.tar", EntryKind::File)]);
        ctx.archive = Some(ArchiveMenuState::Pending);
        let nodes = build_menu(&ctx);
        for action in [
            MenuAction::ExtractHere,
            MenuAction::CompressTarGz,
            MenuAction::CompressZip,
        ] {
            assert_eq!(
                reason(&nodes, action).as_deref(),
                Some(ARCHIVE_PENDING_REASON),
                "{action:?}"
            );
        }
    }

    #[test]
    fn a_non_posix_shell_or_a_local_pane_has_no_archive_group() {
        let mut ctx = context(Side::Remote, false, vec![entry("a.zip", EntryKind::File)]);
        ctx.archive = Some(ArchiveMenuState::Ready(ArchiveCaps {
            posix: false,
            tar: true,
            zip: true,
            unzip: true,
        }));
        assert!(find(&build_menu(&ctx), MenuAction::CompressZip).is_none());
        let mut ctx = context(Side::Local, false, vec![entry("a.zip", EntryKind::File)]);
        ctx.archive = Some(caps(true, true, true));
        assert!(find(&build_menu(&ctx), MenuAction::CompressZip).is_none());
        // 没探测过（探测失败）：不显示。
        let ctx = context(Side::Remote, false, vec![entry("a.zip", EntryKind::File)]);
        assert!(find(&build_menu(&ctx), MenuAction::CompressZip).is_none());
    }

    #[test]
    fn compressing_drives_or_nothing_is_refused() {
        let mut ctx = context(Side::Remote, false, vec![]);
        ctx.archive = Some(caps(true, true, true));
        let nodes = build_menu(&ctx);
        let compress = nodes
            .iter()
            .find(|n| {
                matches!(
                    n,
                    MenuNode::Submenu {
                        label: "压缩为",
                        ..
                    }
                )
            })
            .expect("压缩为");
        assert!(compress.disabled());
    }

    #[test]
    fn paste_rules_when_the_clipboard_is_connected() {
        let mut ctx = context(Side::Remote, true, vec![]);
        ctx.features.clipboard = true;
        assert_eq!(paste_reason(&ctx).as_deref(), Some("剪贴板是空的"));
        ctx.clipboard = Some((Side::Local, 2));
        assert_eq!(
            paste_reason(&ctx).as_deref(),
            Some("远端和本地之间请用上传 / 下载")
        );
        ctx.clipboard = Some((Side::Remote, 2));
        assert_eq!(paste_reason(&ctx), None);
        ctx.current_dir.clear();
        assert_eq!(paste_reason(&ctx).as_deref(), Some("先进入一个目录"));
    }

    #[test]
    fn separators_are_tidied() {
        let sep = MenuNode::Separator;
        let a = MenuNode::Item {
            action: MenuAction::Refresh,
            label: "a".into(),
            shortcut: None,
            reason: None,
        };
        let tidy = tidy_separators(vec![
            sep.clone(),
            a.clone(),
            sep.clone(),
            sep.clone(),
            a.clone(),
            sep.clone(),
        ]);
        assert_eq!(tidy, vec![a.clone(), sep, a]);
    }

    #[test]
    fn the_transfer_menu_follows_the_state() {
        let cases: Vec<(TransferState, Vec<&str>)> = vec![
            (TransferState::Queued, vec!["暂停", "取消"]),
            (TransferState::Planning, vec!["暂停", "取消"]),
            (TransferState::Running, vec!["暂停", "取消"]),
            (TransferState::Paused, vec!["继续", "取消"]),
            (TransferState::WaitingReconnect, vec!["取消"]),
            (TransferState::WaitingConflict, vec!["取消"]),
            (TransferState::Failed("x".into()), vec!["重试"]),
            (TransferState::Cancelled, vec!["重试"]),
            (TransferState::Skipped, vec!["重试"]),
            (TransferState::Done, vec![]),
        ];
        for (state, want) in cases {
            let nodes = build_transfer_menu(&state);
            let have: Vec<&str> = nodes.iter().map(MenuNode::label).collect();
            assert_eq!(have, want, "{state:?}");
        }
    }
}
