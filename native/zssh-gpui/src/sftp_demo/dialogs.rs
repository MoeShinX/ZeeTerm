//! 文件栏的对话框：新建 / 重命名、删除确认、修改权限、属性、下载到…、解压遇到同名文件、
//! 远端改名遇到同名文件时问是否覆盖。这里是状态和纯规则（名字校验、文案）；画出来在 `view`。
//!
//! 对照 0.2.14 的 `SftpDialogs.tsx`、`SftpPropertiesDialog.tsx`、`SftpExtractConflictDialog.tsx`。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ::gpui::Subscription;
use gpui_kit::component::input::InputState;
use gpui_kit::*;
use zssh_core::sftp::archive::ExtractPlan;
use zssh_core::sftp::local::check_child_name;

use super::menu::Side;
use super::model::{Entry, EntryKind};
use super::move_to::MoveAsk;
use super::props::{CalcState, PermMatrix, PropTarget};

/// 同一时间只有一个对话框，盖在整个文件视图上。
pub(super) enum Dialog {
    Name(NameDialog),
    Delete(DeleteDialog),
    Permissions(PermDialog),
    Properties(PropDialog),
    DownloadTo(DownloadDialog),
    ExtractConflict(ExtractAsk),
    OverwriteRename(OverwriteAsk),
    /// 「移动到…」：选目标文件夹（V2-U23）。
    MoveTo(MoveDialog),
    /// 移动时目标里有同名项，问替换 / 跳过。
    MoveConflict(MoveConflict),
    /// 移动结束后有跳过 / 失败的项，列出来。
    MoveSummary(MoveReport),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameKind {
    Mkdir,
    Create,
    Rename,
}

pub(super) struct NameDialog {
    pub(super) kind: NameKind,
    pub(super) side: Side,
    /// 新建在哪个目录下；改名时是原条目所在的目录。
    pub(super) dir: String,
    /// 改名的原条目。
    pub(super) from: Option<Entry>,
    pub(super) input: Entity<InputState>,
    /// 这个目录里已有的其它名字（不分大小写比较，重名时提示）。
    pub(super) existing: Vec<String>,
    pub(super) _subscriptions: Vec<Subscription>,
}

pub(super) struct DeleteDialog {
    pub(super) side: Side,
    pub(super) entries: Vec<Entry>,
}

pub(super) struct PermDialog {
    pub(super) entry: Entry,
    pub(super) matrix: PermMatrix,
    pub(super) octal: Entity<InputState>,
    pub(super) recursive: bool,
    pub(super) _subscriptions: Vec<Subscription>,
}

pub(super) struct PropDialog {
    pub(super) side: Side,
    pub(super) targets: Vec<PropTarget>,
    /// 和 `targets` 一一对应：只有文件夹会被算。
    pub(super) calcs: Vec<CalcState>,
    /// 还在跑的计算的取消开关（和 `calcs` 一一对应）。
    pub(super) cancels: Vec<Option<Arc<AtomicBool>>>,
}

pub(super) struct DownloadDialog {
    pub(super) entries: Vec<Entry>,
    pub(super) input: Entity<InputState>,
    pub(super) error: Option<String>,
    /// 点了“下载”，后台正在确认这个本地目录读得出来（慢盘上不能卡界面）；回来才开始下载。
    pub(super) checking: bool,
    /// 最近一次后台确认的序号（旧的结果不算数）。
    pub(super) check_request: u64,
    pub(super) _subscriptions: Vec<Subscription>,
}

/// 「移动到…」里目录浏览列表的状态。
pub(super) struct BrowseState {
    /// 列表显示的是哪个文件夹里的子文件夹（还没读回来之前是上一个）。
    pub(super) dir: String,
    pub(super) folders: Vec<Entry>,
    pub(super) loading: bool,
    /// 读不出来的原因（保留旧列表）。
    pub(super) error: Option<String>,
    /// 单击选中的子文件夹（路径）。
    pub(super) selected: Option<String>,
    /// 最新一次读取的序号：旧请求的结果不算数。
    pub(super) request: u64,
    /// 读回来之后要选中的子文件夹（刚新建的那个）。
    pub(super) pending_select: Option<String>,
}

pub(super) struct MoveDialog {
    pub(super) side: Side,
    pub(super) entries: Vec<Entry>,
    /// 目标文件夹的路径输入框（和列表双向同步）。
    pub(super) input: Entity<InputState>,
    pub(super) browse: BrowseState,
    /// 点了“移动”，正在确认目标文件夹存在（列一遍它的子文件夹），回来才开始移动。
    pub(super) confirming: bool,
    /// 目标文件夹用不了等原因，写在按钮上面。
    pub(super) error: Option<String>,
    /// 「新建文件夹」展开了：名字输入框。
    pub(super) new_folder: Option<Entity<InputState>>,
    pub(super) new_folder_error: Option<String>,
    /// 新建文件夹的请求在路上，别重复发。
    pub(super) creating: bool,
    /// 目录浏览列表的滚动（新建文件夹后要滚到它）。
    pub(super) scroll: ::gpui::ScrollHandle,
    pub(super) _subscriptions: Vec<Subscription>,
}

/// 移动时目标里有同名项：等用户在 替换 / 跳过 / 取消移动 里选。
pub(super) struct MoveConflict {
    pub(super) ask: MoveAsk,
    /// 勾了“对其余同名项都这样做”。
    pub(super) apply_rest: bool,
}

/// 移动结束后有跳过 / 失败的项：汇总 + 明细。
pub(super) struct MoveReport {
    pub(super) summary: String,
    pub(super) lines: Vec<String>,
}

/// 一次解压的目标：压缩包、解到哪个文件夹。
#[derive(Debug, Clone)]
pub(super) struct ExtractTarget {
    pub(super) archive: String,
    pub(super) archive_name: String,
    pub(super) dest: String,
    pub(super) dest_name: String,
    /// 解到新文件夹时，完成后选中它。
    pub(super) select_dest: bool,
}

/// 解压前检查完、发现目标里有同名文件，等用户选覆盖 / 跳过 / 取消。
#[derive(Debug, Clone)]
pub(super) struct ExtractAsk {
    pub(super) target: ExtractTarget,
    pub(super) plan: ExtractPlan,
}

/// 远端改名撞上同名文件：先不覆盖地试过了，服务器说已存在，问用户要不要覆盖。
#[derive(Debug, Clone)]
pub(super) struct OverwriteAsk {
    pub(super) from: String,
    pub(super) to: String,
    pub(super) name: String,
}

// ---------------------------------------------------------------------------
// 名字校验
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NameCheck {
    Ok,
    Bad(String),
}

/// 新建 / 重命名的名字校验（0.2.14 `validateName`，再加 ZeeTerm 一直有的 `.` / `..` 和路径分隔符规则；
/// 本地名字另外过 Windows 规则：保留名、非法字符、尾部点等）。
pub(super) fn validate_name(name: &str, side: Side) -> NameCheck {
    if name.trim().is_empty() {
        return NameCheck::Bad("名字不能为空".to_owned());
    }
    if name.contains('/') {
        return NameCheck::Bad("名字里不能有 /".to_owned());
    }
    if matches!(name.trim(), "." | "..") {
        return NameCheck::Bad(format!("名字不能是 {}", name.trim()));
    }
    match side {
        Side::Remote => {
            if name.contains('\\') {
                return NameCheck::Bad("名字里不能有 \\".to_owned());
            }
            NameCheck::Ok
        }
        Side::Local => {
            if name.contains('\\') {
                return NameCheck::Bad("名字里不能有 \\".to_owned());
            }
            for bad in [':', '*', '?', '"', '<', '>', '|'] {
                if name.contains(bad) {
                    return NameCheck::Bad(format!("名字里不能有 {bad}"));
                }
            }
            match check_child_name(name.trim()) {
                Ok(_) => NameCheck::Ok,
                Err(err) => NameCheck::Bad(err.to_string()),
            }
        }
    }
}

/// 名字已经被这个目录里的别的条目用了吗（不分大小写）。
pub(super) fn name_taken(name: &str, existing: &[String]) -> bool {
    let wanted = name.trim().to_lowercase();
    existing.iter().any(|other| other.to_lowercase() == wanted)
}

/// “下载到…”的目录检查（只看写法，目录存不存在要去问系统）。
pub(super) fn validate_local_dir(raw: &str) -> NameCheck {
    let value = raw.trim();
    if value.is_empty() {
        return NameCheck::Bad("请输入一个本地目录".to_owned());
    }
    // 盘符后面的冒号是合法的，其余位置不许有 Windows 不允许的字符。
    let rest = match value.as_bytes() {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => &value[2..],
        _ => value,
    };
    if rest.contains(['<', '>', '"', '|', '?', '*']) {
        return NameCheck::Bad("路径里有 Windows 不允许的字符".to_owned());
    }
    NameCheck::Ok
}

// ---------------------------------------------------------------------------
// 删除确认
// ---------------------------------------------------------------------------

/// 删除确认框里最多列几个名字（再多就写“等 N 项”）。
pub(super) const DELETE_NAMES_SHOWN: usize = 8;

pub(super) fn delete_title(side: Side, count: usize) -> String {
    let place = match side {
        Side::Remote => "远端",
        Side::Local => "本地",
    };
    format!("删除{place}的 {count} 项？")
}

/// 说明文字：有文件夹要写明会连同里面的全部内容一起删（目录递归删除不能恢复）；
/// 只有符号链接时写明只删链接本身。
pub(super) fn delete_description(entries: &[Entry]) -> String {
    let has_folder = entries
        .iter()
        .any(|e| matches!(e.kind, EntryKind::Dir | EntryKind::Drive));
    let only_links = !entries.is_empty() && entries.iter().all(|e| e.kind == EntryKind::Symlink);
    if has_folder {
        "选中的里有文件夹，将递归删除其中所有内容，不能恢复。".to_owned()
    } else if only_links {
        "选中的都是符号链接：只删链接本身，不动它指向的内容。".to_owned()
    } else {
        "选中的都是文件，删除后无法恢复。".to_owned()
    }
}

/// 列出来的名字和“…等另外 N 项”那一行。
pub(super) fn delete_names(entries: &[Entry]) -> (Vec<String>, Option<String>) {
    let shown: Vec<String> = entries
        .iter()
        .take(DELETE_NAMES_SHOWN)
        .map(|e| e.name.clone())
        .collect();
    let more = (entries.len() > DELETE_NAMES_SHOWN)
        .then(|| format!("…等另外 {} 项", entries.len() - DELETE_NAMES_SHOWN));
    (shown, more)
}

/// 删除进行中 / 结束时状态栏里怎么称呼这次删除的对象。
pub(super) fn delete_subject(entries: &[Entry]) -> String {
    match entries {
        [only] => only.name.clone(),
        _ => format!("{} 项", entries.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::menu::Side;
    use super::super::model::{Entry, EntryKind};
    use super::{
        DELETE_NAMES_SHOWN, NameCheck, delete_description, delete_names, delete_subject,
        delete_title, name_taken, validate_local_dir, validate_name,
    };

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.to_owned(),
            path: format!("/x/{name}"),
            kind,
            target_kind: None,
            link_target: None,
            size: None,
            mtime: None,
            permissions: None,
            owner: None,
            hidden: false,
            name_is_utf8: true,
        }
    }

    fn bad(check: NameCheck) -> String {
        match check {
            NameCheck::Bad(reason) => reason,
            NameCheck::Ok => panic!("本来应该被拒绝"),
        }
    }

    #[test]
    fn names_follow_the_front_end_rules() {
        assert_eq!(bad(validate_name("  ", Side::Remote)), "名字不能为空");
        assert_eq!(bad(validate_name("a/b", Side::Remote)), "名字里不能有 /");
        assert_eq!(validate_name("报告 final.txt", Side::Remote), NameCheck::Ok);
        // 远端的冒号、星号都是合法字符。
        assert_eq!(validate_name("a:b*c", Side::Remote), NameCheck::Ok);
        assert_eq!(bad(validate_name("..", Side::Remote)), "名字不能是 ..");
        assert_eq!(bad(validate_name("a\\b", Side::Remote)), "名字里不能有 \\");
    }

    #[test]
    fn local_names_also_follow_windows_rules() {
        assert_eq!(validate_name("notes.txt", Side::Local), NameCheck::Ok);
        for ch in [':', '*', '?', '"', '<', '>', '|'] {
            let reason = bad(validate_name(&format!("a{ch}b"), Side::Local));
            assert_eq!(reason, format!("名字里不能有 {ch}"));
        }
        // 保留名、尾部点由核心的本地名字检查拒绝。
        assert!(matches!(
            validate_name("CON", Side::Local),
            NameCheck::Bad(_)
        ));
        assert!(matches!(
            validate_name("name.", Side::Local),
            NameCheck::Bad(_)
        ));
    }

    #[test]
    fn taken_names_ignore_case() {
        let existing = vec!["Readme.md".to_owned(), "src".to_owned()];
        assert!(name_taken(" readme.MD ", &existing));
        assert!(!name_taken("docs", &existing));
        assert!(!name_taken("x", &[]));
    }

    #[test]
    fn download_directories_are_checked_for_writing_only() {
        assert_eq!(bad(validate_local_dir("")), "请输入一个本地目录");
        assert_eq!(
            bad(validate_local_dir("D:\\a<b")),
            "路径里有 Windows 不允许的字符"
        );
        assert_eq!(validate_local_dir("D:\\下载 目录"), NameCheck::Ok);
        assert_eq!(validate_local_dir("C:"), NameCheck::Ok);
        assert_eq!(validate_local_dir("\\\\srv\\share\\x"), NameCheck::Ok);
        // 盘符以外的冒号不行（? * 等也不行）。
        assert!(matches!(validate_local_dir("D:\\a?"), NameCheck::Bad(_)));
    }

    #[test]
    fn delete_text_names_what_will_happen() {
        assert_eq!(delete_title(Side::Remote, 3), "删除远端的 3 项？");
        assert_eq!(delete_title(Side::Local, 1), "删除本地的 1 项？");
        let files = [entry("a", EntryKind::File)];
        assert_eq!(
            delete_description(&files),
            "选中的都是文件，删除后无法恢复。"
        );
        let mixed = [entry("a", EntryKind::File), entry("d", EntryKind::Dir)];
        let text = delete_description(&mixed);
        assert!(text.contains("递归删除其中所有内容") && text.contains("不能恢复"));
        let links = [entry("l", EntryKind::Symlink)];
        assert!(delete_description(&links).contains("只删链接本身"));
    }

    #[test]
    fn long_delete_lists_are_cut_with_a_count() {
        let many: Vec<Entry> = (0..11)
            .map(|n| entry(&format!("f{n}"), EntryKind::File))
            .collect();
        let (shown, more) = delete_names(&many);
        assert_eq!(shown.len(), DELETE_NAMES_SHOWN);
        assert_eq!(more.as_deref(), Some("…等另外 3 项"));
        let (shown, more) = delete_names(&many[..2]);
        assert_eq!(shown.len(), 2);
        assert!(more.is_none());
        assert_eq!(delete_subject(&many[..1]), "f0");
        assert_eq!(delete_subject(&many), "11 项");
    }
}
