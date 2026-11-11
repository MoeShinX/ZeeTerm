//! 递归修改权限（权限对话框里“递归应用到文件夹里面的所有内容”）。
//!
//! 和 0.2.14 的 `sftp_set_permissions` 一样一层层往下走，但有两处刻意不同，都是为了不伤到树外面的东西：
//! - **符号链接不改也不跟进去**：SFTP 的 `setstat` 按路径处理、会作用到链接的目标，
//!   链接指到树外面时递归改权限就改到了别人的文件；这里跳过它们并在结果里告诉用户跳过了几个。
//! - 一项改失败不中断整棵树：继续改完剩下的，最后写明失败了几项和第一个原因。
//!
//! 遍历逻辑对着 [`ChmodFs`] 写，单测用内存实现，真实服务器用 [`SftpChannel`] 的实现。

use std::sync::atomic::{AtomicBool, Ordering};

use zssh_core::sftp::fs;
use zssh_core::sftp::{FileKind, SftpChannel};
use zssh_core::ssh::BoxFuture;

use crate::error_text::error_text;
use crate::i18n::t;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeKind {
    Dir,
    File,
    Symlink,
}

/// 一次“改权限”要设成什么。低 9 位（rwx）一定设；setuid / setgid / sticky 这三位默认**保留每个条目
/// 自己原有的**（`special = None`），只有用户明确写了四位八进制（如 `4755`、`0755`）才按写的设。
/// 以前一律 `& 0o777`：对 `1777` 的共享目录改任何一位都会丢 sticky，勾“递归”就是整棵树丢。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModeRequest {
    /// rwx 九位（0..=0o777）。
    pub(super) low: u32,
    /// 用户明确指定的特殊位（只含 `0o7000` 里的位）；`None` = 各条目保留自己的。
    pub(super) special: Option<u32>,
}

impl ModeRequest {
    pub(super) fn keep_special(low: u32) -> Self {
        Self {
            low: low & 0o777,
            special: None,
        }
    }

    /// 最终发给服务器的权限位。`current` 是条目现有的 mode（可带文件类型位）；读不到时只能当 0，
    /// 此时和旧行为一样不带特殊位，但调用方应尽量先取到。
    pub(super) fn resolve(self, current: Option<u32>) -> u32 {
        let special = self
            .special
            .map_or_else(|| current.unwrap_or(0) & 0o7000, |bits| bits & 0o7000);
        special | (self.low & 0o777)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Child {
    pub(super) path: String,
    pub(super) kind: NodeKind,
    /// readdir 带回来的现有权限位，递归时据此保留这一项自己的特殊位。
    pub(super) permissions: Option<u32>,
    /// 名字不是合法 UTF-8 的不碰（服务器上的原名不是界面上的名字）。
    pub(super) utf8: bool,
}

pub(super) trait ChmodFs: Send + Sync {
    /// 读条目本身的类型和现有权限位（不跟随链接）。
    fn kind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(NodeKind, Option<u32>), String>>;
    fn children<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Result<Vec<Child>, String>>;
    fn chmod<'a>(&'a self, path: &'a str, mode: u32) -> BoxFuture<'a, Result<(), String>>;
}

impl ChmodFs for SftpChannel {
    fn kind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(NodeKind, Option<u32>), String>> {
        Box::pin(async move {
            let stat = fs::stat(self, path, false)
                .await
                .map_err(|err| error_text(&err))?;
            let kind = match stat.kind {
                FileKind::Dir => NodeKind::Dir,
                FileKind::Symlink { .. } => NodeKind::Symlink,
                _ => NodeKind::File,
            };
            Ok((kind, stat.permissions))
        })
    }

    fn children<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Result<Vec<Child>, String>> {
        Box::pin(async move {
            let entries = self.read_dir(dir).await.map_err(|err| error_text(&err))?;
            Ok(entries
                .into_iter()
                .map(|entry| Child {
                    kind: match entry.kind {
                        FileKind::Dir => NodeKind::Dir,
                        FileKind::Symlink { .. } => NodeKind::Symlink,
                        _ => NodeKind::File,
                    },
                    utf8: entry.name_is_utf8,
                    permissions: entry.permissions,
                    path: entry.path,
                })
                .collect())
        })
    }

    fn chmod<'a>(&'a self, path: &'a str, mode: u32) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.set_permissions(path, mode)
                .await
                .map_err(|err| error_text(&err))
        })
    }
}

/// 递归改权限的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ChmodSummary {
    pub(super) changed: u64,
    pub(super) skipped_links: u64,
    pub(super) skipped_names: u64,
    pub(super) failed: u64,
    /// 第一个失败：路径和原因。
    pub(super) first_failure: Option<(String, String)>,
    pub(super) cancelled: bool,
}

impl ChmodSummary {
    /// 状态栏上的一句话。
    pub(super) fn message(&self, name: &str, mode_text: &str) -> String {
        let mut text = if self.cancelled {
            format!(
                "{}{name} → {mode_text}{}{}{}",
                t(super::SftpMsg::ChmodCancelledPrefix),
                t(super::SftpMsg::ChmodCancelledMiddle),
                self.changed,
                t(super::SftpMsg::ChmodCancelledSuffix)
            )
        } else {
            format!(
                "{}{name} → {mode_text}{}{}{}",
                t(super::SftpMsg::ChmodDonePrefix),
                t(super::SftpMsg::ChmodDoneMiddle),
                self.changed,
                t(super::SftpMsg::ChmodDoneSuffix)
            )
        };
        let mut notes = Vec::new();
        if self.skipped_links > 0 {
            notes.push(format!(
                "{}{}{}",
                t(super::SftpMsg::ChmodSkippedLinksPrefix),
                self.skipped_links,
                t(super::SftpMsg::ChmodSkippedLinksSuffix)
            ));
        }
        if self.skipped_names > 0 {
            notes.push(format!(
                "{}{}{}",
                t(super::SftpMsg::ChmodSkippedNonUtf8Prefix),
                self.skipped_names,
                t(super::SftpMsg::ChmodSkippedNonUtf8Suffix)
            ));
        }
        if let Some((path, reason)) = &self.first_failure {
            notes.push(format!(
                "{}{}{}{path}{}{reason}",
                t(super::SftpMsg::ChmodFailedPrefix),
                self.failed,
                t(super::SftpMsg::ChmodFailedMiddle),
                t(super::SftpMsg::ChmodFailedSuffix)
            ));
        }
        if !notes.is_empty() {
            text.push_str(&format!(
                "{}{}",
                crate::i18n::semicolon(),
                notes.join(crate::i18n::semicolon())
            ));
        }
        text
    }

    pub(super) fn is_clean(&self) -> bool {
        self.failed == 0 && !self.cancelled
    }
}

/// 把 `root`（文件夹）和里面的所有内容改成 `request`（先改文件夹自己，再往下）。
/// 每一项各自保留自己原有的特殊位（除非 `request.special` 明确指定）。
/// `progress` 每改完一项调一次，参数是已改的项数。
pub(super) async fn recursive_chmod(
    fs: &dyn ChmodFs,
    root: &str,
    request: ModeRequest,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64) + Send,
) -> Result<ChmodSummary, String> {
    let mut summary = ChmodSummary::default();
    // 根本身读不了或者改不了就是整个失败（用户明确选的就是它）。
    let (root_kind, root_permissions) = fs.kind(root).await?;
    if root_kind == NodeKind::Symlink {
        return Err(t(super::SftpMsg::ChmodSymlinkRefused).to_owned());
    }
    let mut pending = vec![(root.to_owned(), root_permissions)];
    while let Some((dir, dir_permissions)) = pending.pop() {
        if cancel.load(Ordering::SeqCst) {
            summary.cancelled = true;
            break;
        }
        match fs.chmod(&dir, request.resolve(dir_permissions)).await {
            Ok(()) => {
                summary.changed += 1;
                progress(summary.changed);
            }
            Err(reason) if dir == root => {
                return Err(crate::i18n::colon(&dir, &reason));
            }
            Err(reason) => {
                summary.failed += 1;
                summary.first_failure.get_or_insert((dir.clone(), reason));
                // 目录自己改不了，里面的仍然试一试（只读目录里的文件还是能改）。
            }
        }
        let children = match fs.children(&dir).await {
            Ok(children) => children,
            Err(reason) => {
                summary.failed += 1;
                summary.first_failure.get_or_insert((dir.clone(), reason));
                continue;
            }
        };
        for child in children {
            if cancel.load(Ordering::SeqCst) {
                summary.cancelled = true;
                break;
            }
            if !child.utf8 {
                summary.skipped_names += 1;
                continue;
            }
            match child.kind {
                NodeKind::Dir => pending.push((child.path, child.permissions)),
                NodeKind::Symlink => summary.skipped_links += 1,
                NodeKind::File => match fs
                    .chmod(&child.path, request.resolve(child.permissions))
                    .await
                {
                    Ok(()) => {
                        summary.changed += 1;
                        progress(summary.changed);
                    }
                    Err(reason) => {
                        summary.failed += 1;
                        summary.first_failure.get_or_insert((child.path, reason));
                    }
                },
            }
        }
        if summary.cancelled {
            break;
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use zssh_core::ssh::BoxFuture;

    use super::{Child, ChmodFs, ModeRequest, NodeKind, recursive_chmod};

    /// 内存里的一棵树：路径 → 类型；记下所有 chmod 调用。
    struct MemFs {
        nodes: BTreeMap<String, NodeKind>,
        /// 路径 → 现有 mode（可带文件类型位）；没写的就是读不到。
        modes: BTreeMap<String, u32>,
        non_utf8: BTreeSet<String>,
        refuse: BTreeSet<String>,
        calls: Mutex<Vec<(String, u32)>>,
    }

    impl MemFs {
        fn new(nodes: &[(&str, NodeKind)]) -> Self {
            Self {
                nodes: nodes.iter().map(|(p, k)| ((*p).to_owned(), *k)).collect(),
                modes: BTreeMap::new(),
                non_utf8: BTreeSet::new(),
                refuse: BTreeSet::new(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, u32)> {
            self.calls.lock().expect("锁").clone()
        }
    }

    impl ChmodFs for MemFs {
        fn kind<'a>(
            &'a self,
            path: &'a str,
        ) -> BoxFuture<'a, Result<(NodeKind, Option<u32>), String>> {
            Box::pin(async move {
                self.nodes
                    .get(path)
                    .copied()
                    .map(|kind| (kind, self.modes.get(path).copied()))
                    .ok_or_else(|| "没有这个路径".to_owned())
            })
        }

        fn children<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Result<Vec<Child>, String>> {
            Box::pin(async move {
                let prefix = format!("{}/", dir.trim_end_matches('/'));
                Ok(self
                    .nodes
                    .iter()
                    .filter(|(path, _)| {
                        path.strip_prefix(&prefix)
                            .is_some_and(|rest| !rest.contains('/'))
                    })
                    .map(|(path, kind)| Child {
                        path: path.clone(),
                        kind: *kind,
                        permissions: self.modes.get(path).copied(),
                        utf8: !self.non_utf8.contains(path),
                    })
                    .collect())
            })
        }

        fn chmod<'a>(&'a self, path: &'a str, mode: u32) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.calls.lock().expect("锁").push((path.to_owned(), mode));
                if self.refuse.contains(path) {
                    Err("没有权限".to_owned())
                } else {
                    Ok(())
                }
            })
        }
    }

    fn run(fs: &MemFs, root: &str, mode: u32) -> Result<super::ChmodSummary, String> {
        run_request(fs, root, ModeRequest::keep_special(mode))
    }

    fn run_request(
        fs: &MemFs,
        root: &str,
        request: ModeRequest,
    ) -> Result<super::ChmodSummary, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        runtime.block_on(recursive_chmod(
            fs,
            root,
            request,
            &AtomicBool::new(false),
            |_| {},
        ))
    }

    fn tree() -> MemFs {
        MemFs::new(&[
            ("/srv/site", NodeKind::Dir),
            ("/srv/site/a.txt", NodeKind::File),
            ("/srv/site/link", NodeKind::Symlink),
            ("/srv/site/sub", NodeKind::Dir),
            ("/srv/site/sub/b.txt", NodeKind::File),
            ("/srv/site/sub/deep", NodeKind::Dir),
            ("/srv/site/sub/deep/c.txt", NodeKind::File),
        ])
    }

    #[test]
    fn every_directory_and_file_is_changed_but_links_are_left_alone() {
        let fs = tree();
        let summary = run(&fs, "/srv/site", 0o750).expect("递归改权限");
        assert_eq!(summary.changed, 6, "{summary:?}");
        assert_eq!(summary.skipped_links, 1);
        assert!(summary.is_clean());
        let paths: Vec<String> = fs.calls().into_iter().map(|(p, _)| p).collect();
        assert!(!paths.contains(&"/srv/site/link".to_owned()), "不能碰链接");
        assert!(paths.contains(&"/srv/site/sub/deep/c.txt".to_owned()));
        assert_eq!(paths[0], "/srv/site", "先改文件夹自己");
        assert!(fs.calls().iter().all(|(_, mode)| *mode == 0o750));
    }

    #[test]
    fn the_mode_is_masked_to_permission_bits() {
        let fs = tree();
        run(&fs, "/srv/site", 0o100644).expect("改权限");
        assert!(fs.calls().iter().all(|(_, mode)| *mode == 0o644));
    }

    #[test]
    fn non_utf8_names_are_skipped_not_touched() {
        let mut fs = tree();
        fs.non_utf8.insert("/srv/site/a.txt".to_owned());
        let summary = run(&fs, "/srv/site", 0o700).expect("改权限");
        assert_eq!(summary.skipped_names, 1);
        assert!(fs.calls().iter().all(|(p, _)| p != "/srv/site/a.txt"));
    }

    #[test]
    fn one_failure_does_not_stop_the_rest_and_is_reported() {
        let mut fs = tree();
        fs.refuse.insert("/srv/site/sub/b.txt".to_owned());
        let summary = run(&fs, "/srv/site", 0o755).expect("改权限");
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.changed, 5);
        assert_eq!(
            summary.first_failure,
            Some(("/srv/site/sub/b.txt".to_owned(), "没有权限".to_owned()))
        );
        assert!(!summary.is_clean());
        let text = summary.message("site", "0755");
        assert!(
            text.contains("1 项没改成") && text.contains("b.txt"),
            "{text}"
        );
        assert!(text.contains("跳过 1 个符号链接"));
    }

    #[test]
    fn a_root_that_cannot_be_changed_is_a_failure() {
        let mut fs = tree();
        fs.refuse.insert("/srv/site".to_owned());
        let error = run(&fs, "/srv/site", 0o755).expect_err("根改不了就是失败");
        assert!(error.contains("/srv/site") && error.contains("没有权限"));
        // 根改失败就一项都没往下走。
        assert_eq!(fs.calls().len(), 1);
    }

    #[test]
    fn a_symlink_root_is_refused() {
        let fs = tree();
        let error = run(&fs, "/srv/site/link", 0o755).expect_err("链接不递归");
        assert!(error.contains("符号链接"));
        assert!(fs.calls().is_empty());
    }

    fn call_mode(fs: &MemFs, path: &str) -> u32 {
        fs.calls()
            .into_iter()
            .find(|(p, _)| p == path)
            .map(|(_, mode)| mode)
            .unwrap_or_else(|| panic!("没有对 {path} 调 chmod"))
    }

    #[test]
    fn each_entry_keeps_its_own_special_bits() {
        let mut fs = tree();
        // 共享目录 sticky、子目录 setgid、一个可执行文件 setuid，其余没有特殊位。
        fs.modes.insert("/srv/site".to_owned(), 0o041777);
        fs.modes.insert("/srv/site/sub".to_owned(), 0o042775);
        fs.modes.insert("/srv/site/a.txt".to_owned(), 0o104755);
        fs.modes.insert("/srv/site/sub/b.txt".to_owned(), 0o100644);
        run(&fs, "/srv/site", 0o750).expect("改权限");
        assert_eq!(call_mode(&fs, "/srv/site"), 0o1750, "sticky 不能丢");
        assert_eq!(call_mode(&fs, "/srv/site/sub"), 0o2750, "setgid 不能丢");
        assert_eq!(call_mode(&fs, "/srv/site/a.txt"), 0o4750, "setuid 不能丢");
        assert_eq!(
            call_mode(&fs, "/srv/site/sub/b.txt"),
            0o750,
            "没有特殊位的不能凭空多出来"
        );
    }

    #[test]
    fn explicit_special_bits_replace_the_old_ones_everywhere() {
        let mut fs = tree();
        fs.modes.insert("/srv/site".to_owned(), 0o041777);
        fs.modes.insert("/srv/site/a.txt".to_owned(), 0o104755);
        let request = ModeRequest {
            low: 0o755,
            special: Some(0o2000),
        };
        run_request(&fs, "/srv/site", request).expect("改权限");
        assert!(fs.calls().iter().all(|(_, mode)| *mode == 0o2755));
        // 明确写 0 就是清掉。
        let fs = {
            let mut fs = tree();
            fs.modes.insert("/srv/site".to_owned(), 0o041777);
            fs
        };
        let request = ModeRequest {
            low: 0o755,
            special: Some(0),
        };
        run_request(&fs, "/srv/site", request).expect("改权限");
        assert!(fs.calls().iter().all(|(_, mode)| *mode == 0o755));
    }

    #[test]
    fn resolve_keeps_special_bits_and_drops_file_type_bits() {
        let keep = ModeRequest::keep_special(0o644);
        assert_eq!(keep.resolve(Some(0o041777)), 0o1644);
        assert_eq!(keep.resolve(Some(0o104755)), 0o4644);
        assert_eq!(keep.resolve(Some(0o100600)), 0o644);
        assert_eq!(keep.resolve(None), 0o644);
        // 低位只取 rwx 九位。
        assert_eq!(ModeRequest::keep_special(0o17777).low, 0o777);
    }

    #[test]
    fn cancelling_stops_and_says_how_far_it_got() {
        let fs = tree();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        let cancel = AtomicBool::new(false);
        let summary = runtime
            .block_on(recursive_chmod(
                &fs,
                "/srv/site",
                ModeRequest::keep_special(0o755),
                &cancel,
                |done| {
                    if done == 2 {
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                },
            ))
            .expect("取消不是错误");
        assert!(summary.cancelled);
        assert!(summary.changed < 6);
        assert!(summary.message("site", "0755").contains("已取消改权限"));
    }
}
