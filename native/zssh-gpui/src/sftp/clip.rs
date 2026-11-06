//! 复制 / 粘贴的规则（0.2.14 `state/sftpPaste.ts` + `stores/sftpClipboard.ts` + `SftpTabView` 里的粘贴部分）。
//!
//! 这里没有界面也没有网络：应用内剪贴板（所有 SFTP 视图共用一份，不碰系统剪贴板）、粘贴结果的
//! 文案、"改为经本机中转重试"该重试哪些条目、粘贴前的自检、往终端插路径的引号规则。
//! 每条规则都有单测。

use std::path::Path;
use std::sync::{Mutex, MutexGuard, Weak};

use zssh_core::sftp::path as remote_path;
use zssh_core::sftp::server_copy::is_same_or_inside;
use zssh_core::sftp::transfer::backend::SftpChannelProvider;
use zssh_core::sftp::transfer::copy_path;

use super::menu::Side;
use super::model::{Entry, EntryKind, TargetKind};
use crate::i18n::t;

/// 剪贴板里的一个条目：只有路径和是不是文件夹（粘贴时服务器上再看真实情况）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClipItem {
    pub(super) path: String,
    pub(super) is_dir: bool,
}

/// 复制的源在哪个视图里（远端才有意义）。视图关了（通道提供者没人持有了）就复制不了。
#[derive(Clone)]
pub(super) struct ClipSource {
    /// 视图编号（`SftpDemo::view_id`）：同一个视图里粘贴不用登记对端。
    pub(super) view: u64,
    /// 源所在服务器的身份（用户@主机:端口）：和目标相同就是"同一台主机"。
    pub(super) server_key: Option<String>,
    /// 源的传输数据面：跨主机复制时登记给目标这边的引擎。
    pub(super) provider: Option<Weak<SftpChannelProvider>>,
}

impl std::fmt::Debug for ClipSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipSource")
            .field("view", &self.view)
            .field("server_key", &self.server_key)
            .finish_non_exhaustive()
    }
}

/// 应用内剪贴板的内容。
#[derive(Debug, Clone)]
pub(super) struct ClipContent {
    pub(super) side: Side,
    pub(super) items: Vec<ClipItem>,
    pub(super) source: ClipSource,
}

static CLIPBOARD: Mutex<Option<ClipContent>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<ClipContent>> {
    CLIPBOARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn set(content: ClipContent) {
    *lock() = Some(content);
}

pub(super) fn get() -> Option<ClipContent> {
    lock().clone()
}

/// 剪贴板里有几项、来自哪一边（菜单置灰规则用）。
pub(super) fn summary() -> Option<(Side, usize)> {
    lock().as_ref().map(|clip| (clip.side, clip.items.len()))
}

#[cfg(test)]
pub(super) fn clear() {
    *lock() = None;
}

/// 选中的条目 → 剪贴板里的条目。名字不是 UTF-8 的（不可操作）和盘符不放进去。
pub(super) fn items_from(entries: &[Entry]) -> Vec<ClipItem> {
    entries
        .iter()
        .filter(|entry| entry.name_is_utf8 && entry.kind != EntryKind::Drive)
        .map(|entry| ClipItem {
            path: entry.path.clone(),
            is_dir: entry.kind == EntryKind::Dir
                || (entry.kind == EntryKind::Symlink && entry.target_kind == Some(TargetKind::Dir)),
        })
        .collect()
}

/// 路径最后一段（远端 `/`、本地 `\` 都认；末尾的分隔符忽略）。
pub(super) fn name_of(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// 粘贴前的检查：文件夹不能粘贴到它自己或它的子文件夹里。返回要给用户看的原因。
pub(super) fn refuse_into_self(remote: bool, item: &ClipItem, target_dir: &str) -> Option<String> {
    if !item.is_dir {
        return None;
    }
    let inside = if remote {
        is_same_or_inside(&item.path, target_dir)
    } else {
        zssh_core::pathnorm::is_within(Path::new(&item.path), Path::new(target_dir))
    };
    inside.then(|| {
        format!(
            "{}{}{}",
            t(super::SftpMsg::ClipIntoSelfPrefix),
            name_of(&item.path),
            t(super::SftpMsg::ClipIntoSelfSuffix)
        )
    })
}

/// 同一个目录里已经有的名字时，第一个没被占用的「名字 - 副本」「名字 - 副本 2」…
pub(super) fn first_free_copy_path(dest: &str, mut taken: impl FnMut(&str) -> bool) -> String {
    let mut index = 1;
    loop {
        let candidate = copy_path(dest, index);
        if !taken(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

/// 远端目标路径：目标文件夹 + 源的名字。
pub(super) fn remote_dest(target_dir: &str, item: &ClipItem) -> String {
    remote_path::join(target_dir, &name_of(&item.path))
}

// ---------------------------------------------------------------------------
// 结果
// ---------------------------------------------------------------------------

/// 一个条目的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemState {
    Ok,
    /// 已进传输队列（中转复制、本地复制）。
    Queued,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ItemResult {
    pub(super) name: String,
    /// 源路径（"改为经本机中转重试"要用）。
    pub(super) path: String,
    pub(super) is_dir: bool,
    pub(super) state: ItemState,
    pub(super) message: Option<String>,
    /// 这次失败可以改经本机中转重试（`cp` 跑了但失败）。
    pub(super) can_relay: bool,
}

impl ItemResult {
    pub(super) fn new(item: &ClipItem, state: ItemState, message: Option<String>) -> Self {
        Self {
            name: name_of(&item.path),
            path: item.path.clone(),
            is_dir: item.is_dir,
            state,
            message,
            can_relay: false,
        }
    }

    pub(super) fn failed_relayable(item: &ClipItem, message: impl Into<String>) -> Self {
        Self {
            can_relay: true,
            ..Self::new(item, ItemState::Failed, Some(message.into()))
        }
    }
}

/// 用了哪种办法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    CopyData,
    ServerCp,
    Relay,
    Local,
}

impl Method {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::CopyData => t(super::SftpMsg::ClipMethodCopyData),
            Self::ServerCp => t(super::SftpMsg::ClipMethodServerCp),
            Self::Relay => t(super::SftpMsg::ClipMethodRelay),
            Self::Local => t(super::SftpMsg::ClipMethodLocal),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Outcome {
    pub(super) method: Method,
    pub(super) items: Vec<ItemResult>,
    pub(super) elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoticeKind {
    Success,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PasteNotice {
    pub(super) kind: NoticeKind,
    pub(super) text: String,
}

/// 服务器上复制中……（第 N 个文件 · 已 N 秒）。本地复制没有"服务器"，只说在准备。
pub(super) fn copying_text(local: bool, elapsed_seconds: u64, done_files: Option<u64>) -> String {
    if local {
        return format!(
            "{}{elapsed_seconds}{}",
            t(super::SftpMsg::ClipPreparingPrefix),
            t(super::SftpMsg::ClipPreparingSuffix)
        );
    }
    let progress = match done_files {
        Some(done) => format!(
            "{}{}{}",
            t(super::SftpMsg::ClipFileIndexPrefix),
            done + 1,
            t(super::SftpMsg::ClipFileIndexSuffix)
        ),
        None => String::new(),
    };
    format!(
        "{}{progress}{}{elapsed_seconds}{}{}",
        t(super::SftpMsg::ClipCopyingPrefix),
        t(super::SftpMsg::ArchiveElapsedPrefix),
        t(super::SftpMsg::ArchiveElapsedSuffix),
        t(super::SftpMsg::ParenClose)
    )
}

/// 粘贴结束后给用户看的一句话；失败的条目逐个写出原因（最多三个）。
pub(super) fn notice(outcome: &Outcome) -> PasteNotice {
    let count = |state: ItemState| outcome.items.iter().filter(|i| i.state == state).count();
    let (ok, queued, skipped, cancelled) = (
        count(ItemState::Ok),
        count(ItemState::Queued),
        count(ItemState::Skipped),
        count(ItemState::Cancelled),
    );
    let failed: Vec<&ItemResult> = outcome
        .items
        .iter()
        .filter(|item| item.state == ItemState::Failed)
        .collect();
    let seconds = (outcome.elapsed_ms as f64 / 1000.0).round().max(1.0) as u64;
    let label = outcome.method.label();

    let mut parts: Vec<String> = Vec::new();
    if ok > 0 {
        let took = if outcome.elapsed_ms < 1000 {
            t(super::SftpMsg::ClipLessThanOne).to_owned()
        } else {
            seconds.to_string()
        };
        parts.push(format!(
            "{}{ok}{}{label}{}{took}{}",
            t(super::SftpMsg::ClipCopiedPrefix),
            t(super::SftpMsg::ClipCopiedMiddle),
            t(super::SftpMsg::ClipCopiedTookMiddle),
            t(super::SftpMsg::ClipCopiedTookSuffix)
        ));
    }
    if queued > 0 {
        parts.push(format!(
            "{queued}{}{label}{}",
            t(super::SftpMsg::ClipQueuedMiddle),
            t(super::SftpMsg::ParenClose)
        ));
    }
    if skipped > 0 {
        parts.push(format!(
            "{}{skipped}{}",
            t(super::SftpMsg::ClipSkippedPrefix),
            t(super::SftpMsg::ClipSkippedSuffix)
        ));
    }
    if cancelled > 0 {
        parts.push(format!(
            "{}{cancelled}{}",
            t(super::SftpMsg::ClipCancelledPrefix),
            t(super::SftpMsg::ClipSkippedSuffix)
        ));
    }
    if !failed.is_empty() {
        let shown: Vec<String> = failed
            .iter()
            .take(3)
            .map(|item| {
                format!(
                    "{}{}{}{}",
                    t(super::SftpMsg::ClipFailedItemOpen),
                    item.name,
                    t(super::SftpMsg::ClipFailedItemClose),
                    item.message.as_deref().unwrap_or("")
                )
            })
            .collect();
        parts.push(format!(
            "{}{}{}{}",
            failed.len(),
            t(super::SftpMsg::ClipFailedMiddle),
            shown.join(t(super::SftpMsg::ClipSeparator)),
            if failed.len() > 3 {
                t(super::SftpMsg::ClipFailedEllipsis)
            } else {
                ""
            }
        ));
    }
    if parts.is_empty() {
        parts.push(t(super::SftpMsg::ClipNothingToPaste).to_owned());
    }
    PasteNotice {
        kind: if !failed.is_empty() {
            NoticeKind::Error
        } else if ok + queued > 0 {
            NoticeKind::Success
        } else {
            NoticeKind::Info
        },
        text: parts.join(t(super::SftpMsg::ClipSeparator)),
    }
}

/// `cp` 失败之后点了"改为经本机中转重试"：只把可以中转的失败条目挑出来。没有就是 `None`。
pub(super) fn relay_retry_items(outcome: &Outcome) -> Option<Vec<ClipItem>> {
    let items: Vec<ClipItem> = outcome
        .items
        .iter()
        .filter(|item| item.state == ItemState::Failed && item.can_relay && !item.path.is_empty())
        .map(|item| ClipItem {
            path: item.path.clone(),
            is_dir: item.is_dir,
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.to_owned(),
            path: format!("/srv/{name}"),
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

    fn item(path: &str, is_dir: bool) -> ClipItem {
        ClipItem {
            path: path.to_owned(),
            is_dir,
        }
    }

    #[test]
    fn clipboard_items_skip_drives_and_non_utf8_and_follow_dir_links() {
        let mut link = entry("lnk", EntryKind::Symlink);
        link.target_kind = Some(TargetKind::Dir);
        let mut broken = entry("bad", EntryKind::File);
        broken.name_is_utf8 = false;
        let items = items_from(&[
            entry("a.txt", EntryKind::File),
            entry("d", EntryKind::Dir),
            link,
            broken,
            entry("C:", EntryKind::Drive),
        ]);
        assert_eq!(
            items,
            vec![
                item("/srv/a.txt", false),
                item("/srv/d", true),
                item("/srv/lnk", true),
            ]
        );
        assert!(items_from(&[entry("C:", EntryKind::Drive)]).is_empty());
    }

    #[test]
    fn folder_cannot_go_into_itself_or_below() {
        let dir = item("/srv/data", true);
        assert!(refuse_into_self(true, &dir, "/srv/data").is_some());
        assert!(refuse_into_self(true, &dir, "/srv/data/sub").is_some());
        assert!(refuse_into_self(true, &dir, "/srv/data2").is_none());
        assert!(refuse_into_self(true, &dir, "/srv").is_none());
        // 文件没有这个限制。
        assert!(refuse_into_self(true, &item("/srv/a", false), "/srv/a").is_none());
        let root = if cfg!(windows) {
            r"C:\work\data"
        } else {
            "/work/data"
        };
        let local = item(root, true);
        assert!(
            refuse_into_self(
                false,
                &local,
                &Path::new(root).join("sub").display().to_string()
            )
            .is_some()
        );
        assert!(
            refuse_into_self(
                false,
                &local,
                if cfg!(windows) {
                    r"C:\work\data2"
                } else {
                    "/work/data2"
                }
            )
            .is_none()
        );
        let message = refuse_into_self(true, &dir, "/srv/data").unwrap_or_default();
        assert!(message.contains("「data」"), "{message}");
    }

    #[test]
    fn copy_names_count_up_until_free() {
        let taken = ["/d/a.txt - 副本"];
        // 扩展名前面插「 - 副本」（核心的 copy_path 规则）。
        let first = first_free_copy_path("/d/a.txt", |c| taken.contains(&c));
        assert_eq!(first, "/d/a - 副本.txt");
        let busy = ["/d/a - 副本.txt", "/d/a - 副本 2.txt"];
        assert_eq!(
            first_free_copy_path("/d/a.txt", |c| busy.contains(&c)),
            "/d/a - 副本 3.txt"
        );
    }

    #[test]
    fn name_and_dest() {
        assert_eq!(name_of("/a/b/c.txt"), "c.txt");
        assert_eq!(name_of("/a/b/"), "b");
        assert_eq!(name_of(r"C:\w\x y.txt"), "x y.txt");
        assert_eq!(remote_dest("/t", &item("/a/b.txt", false)), "/t/b.txt");
        assert_eq!(remote_dest("/", &item("/a/b", true)), "/b");
    }

    fn outcome(method: Method, results: Vec<(ItemState, Option<&str>, bool)>, ms: u64) -> Outcome {
        Outcome {
            method,
            items: results
                .into_iter()
                .enumerate()
                .map(|(i, (state, message, relay))| ItemResult {
                    name: format!("f{i}"),
                    path: format!("/p/f{i}"),
                    is_dir: i == 0,
                    state,
                    message: message.map(str::to_owned),
                    can_relay: relay,
                })
                .collect(),
            elapsed_ms: ms,
        }
    }

    #[test]
    fn notice_wording_matches_0_2_14() {
        let done = notice(&outcome(
            Method::CopyData,
            vec![(ItemState::Ok, None, false), (ItemState::Ok, None, false)],
            400,
        ));
        assert_eq!(done.kind, NoticeKind::Success);
        assert_eq!(
            done.text,
            "已复制 2 项（服务器端复制（copy-data），用时 不到 1 秒）"
        );
        let slow = notice(&outcome(
            Method::ServerCp,
            vec![(ItemState::Ok, None, false)],
            4_400,
        ));
        assert!(slow.text.contains("用时 4 秒"), "{}", slow.text);
        let queued = notice(&outcome(
            Method::Relay,
            vec![
                (ItemState::Queued, None, false),
                (ItemState::Skipped, None, false),
            ],
            10,
        ));
        assert_eq!(queued.kind, NoticeKind::Success);
        assert_eq!(queued.text, "1 项已加入传输队列（经本机中转）；跳过 1 项");
        let nothing = notice(&outcome(
            Method::Local,
            vec![(ItemState::Skipped, None, false)],
            1,
        ));
        assert_eq!(nothing.kind, NoticeKind::Info);
        let cancelled = notice(&outcome(
            Method::Relay,
            vec![(ItemState::Cancelled, None, false)],
            1,
        ));
        assert_eq!(cancelled.text, "已取消 1 项");
        assert_eq!(cancelled.kind, NoticeKind::Info);
    }

    #[test]
    fn notice_lists_at_most_three_failures() {
        let failed = notice(&outcome(
            Method::ServerCp,
            (0..5)
                .map(|_| (ItemState::Failed, Some("没有权限"), true))
                .collect(),
            10,
        ));
        assert_eq!(failed.kind, NoticeKind::Error);
        assert!(
            failed
                .text
                .starts_with("5 项失败：「f0」没有权限；「f1」没有权限；「f2」没有权限")
        );
        assert!(failed.text.ends_with("……"), "{}", failed.text);
        assert!(!failed.text.contains("f3"));
        assert_eq!(
            notice(&Outcome {
                method: Method::Local,
                items: Vec::new(),
                elapsed_ms: 0
            })
            .text,
            "没有可粘贴的内容"
        );
    }

    #[test]
    fn relay_retry_only_picks_relayable_failures() {
        let result = outcome(
            Method::ServerCp,
            vec![
                (ItemState::Failed, Some("cp 失败"), true),
                (ItemState::Failed, Some("别的原因"), false),
                (ItemState::Ok, None, false),
                (ItemState::Failed, Some("cp 失败"), true),
            ],
            10,
        );
        let items = relay_retry_items(&result).unwrap_or_default();
        assert_eq!(items, vec![item("/p/f0", true), item("/p/f3", false)]);
        assert!(
            relay_retry_items(&outcome(
                Method::Relay,
                vec![(ItemState::Ok, None, false)],
                1
            ))
            .is_none()
        );
    }

    #[test]
    fn copying_text_counts_files_from_one() {
        assert_eq!(copying_text(false, 3, None), "服务器上复制中…（已 3 秒）");
        assert_eq!(
            copying_text(false, 3, Some(0)),
            "服务器上复制中…（第 1 个文件 · 已 3 秒）"
        );
        assert_eq!(copying_text(true, 2, Some(4)), "正在准备复制…（已 2 秒）");
    }

    #[test]
    fn shared_clipboard_round_trip() {
        clear();
        assert!(summary().is_none());
        set(ClipContent {
            side: Side::Remote,
            items: vec![item("/a", false), item("/b", true)],
            source: ClipSource {
                view: 7,
                server_key: Some("u@h:22".to_owned()),
                provider: None,
            },
        });
        assert_eq!(
            summary().map(|(side, n)| (side == Side::Remote, n)),
            Some((true, 2))
        );
        let got = get().expect("clip");
        assert_eq!(got.source.view, 7);
        clear();
    }
}
