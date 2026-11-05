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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Child {
    pub(super) path: String,
    pub(super) kind: NodeKind,
    /// 名字不是合法 UTF-8 的不碰（服务器上的原名不是界面上的名字）。
    pub(super) utf8: bool,
}

pub(super) trait ChmodFs: Send + Sync {
    /// 读条目本身的类型（不跟随链接）。
    fn kind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<NodeKind, String>>;
    fn children<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Result<Vec<Child>, String>>;
    fn chmod<'a>(&'a self, path: &'a str, mode: u32) -> BoxFuture<'a, Result<(), String>>;
}

impl ChmodFs for SftpChannel {
    fn kind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<NodeKind, String>> {
        Box::pin(async move {
            let stat = fs::stat(self, path, false)
                .await
                .map_err(|err| error_text(&err))?;
            Ok(match stat.kind {
                FileKind::Dir => NodeKind::Dir,
                FileKind::Symlink { .. } => NodeKind::Symlink,
                _ => NodeKind::File,
            })
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

/// 把 `root`（文件夹）和里面的所有内容改成 `mode`（先改文件夹自己，再往下）。
/// `progress` 每改完一项调一次，参数是已改的项数。
pub(super) async fn recursive_chmod(
    fs: &dyn ChmodFs,
    root: &str,
    mode: u32,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64) + Send,
) -> Result<ChmodSummary, String> {
    let mode = mode & 0o777;
    let mut summary = ChmodSummary::default();
    // 根本身读不了或者改不了就是整个失败（用户明确选的就是它）。
    if fs.kind(root).await? == NodeKind::Symlink {
        return Err(t(super::SftpMsg::ChmodSymlinkRefused).to_owned());
    }
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        if cancel.load(Ordering::SeqCst) {
            summary.cancelled = true;
            break;
        }
        match fs.chmod(&dir, mode).await {
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
                NodeKind::Dir => pending.push(child.path),
                NodeKind::Symlink => summary.skipped_links += 1,
                NodeKind::File => match fs.chmod(&child.path, mode).await {
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

    use super::{Child, ChmodFs, NodeKind, recursive_chmod};

    /// 内存里的一棵树：路径 → 类型；记下所有 chmod 调用。
    struct MemFs {
        nodes: BTreeMap<String, NodeKind>,
        non_utf8: BTreeSet<String>,
        refuse: BTreeSet<String>,
        calls: Mutex<Vec<(String, u32)>>,
    }

    impl MemFs {
        fn new(nodes: &[(&str, NodeKind)]) -> Self {
            Self {
                nodes: nodes.iter().map(|(p, k)| ((*p).to_owned(), *k)).collect(),
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
        fn kind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<NodeKind, String>> {
            Box::pin(async move {
                self.nodes
                    .get(path)
                    .copied()
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        runtime.block_on(recursive_chmod(
            fs,
            root,
            mode,
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

    #[test]
    fn cancelling_stops_and_says_how_far_it_got() {
        let fs = tree();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        let cancel = AtomicBool::new(false);
        let summary = runtime
            .block_on(recursive_chmod(&fs, "/srv/site", 0o755, &cancel, |done| {
                if done == 2 {
                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }))
            .expect("取消不是错误");
        assert!(summary.cancelled);
        assert!(summary.changed < 6);
        assert!(summary.message("site", "0755").contains("已取消改权限"));
    }
}
