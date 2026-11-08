//! 任务展开（M3-1 第 3 节）：把一个"文件/文件夹"任务摊成要传的文件清单。
//!
//! - 上传：走本地树（[`super::super::local::walk_local_tree`]），
//!   **符号链接和目录联接默认跳过并记下来**；空目录也照搬。
//! - 下载：走远端树（`read_dir` 递归），符号链接默认跳过并记下来；每个目录里落地时
//!   过一次 [`LocalNamePlan`]（Windows 非法名字改名、大小写冲突加序号），改动都写进
//!   `notes`，任务里能看见。

use std::path::{Path, PathBuf};

use crate::error::{reason_keys, Reason};
use crate::sftp::error::SftpError;
use crate::sftp::local::{self, LocalNamePlan};
use crate::sftp::path as remote_path;

/// 展开远端目录树时最多进多少层。
///
/// 为什么要有：服务器（坏实现、恶意服务器、目录环）可以给出没有尽头的目录链，展开不设界就会
/// 一直列下去、计划无限变长。超过它**报错失败**，不能把深处的内容丢掉再报成功。256 层远超正常
/// 目录树（Linux 路径最长 4096 字节，常见的树不过十几层），正常使用碰不到。
pub const MAX_PLAN_DEPTH: usize = 256;

fn tree_too_deep(dir: &str, max_depth: usize) -> SftpError {
    SftpError::RemoteReason(
        Reason::new(
            reason_keys::XFER_TREE_TOO_DEEP,
            format!("文件夹嵌套超过 {max_depth} 层，不展开：{dir}"),
        )
        .with("max", max_depth)
        .with("dir", dir),
    )
}

/// 一个要传的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// 源（上传：本地路径；下载：远端路径）。
    pub source: String,
    /// 目标（上传：远端路径；下载：本地路径）。
    pub target: String,
    /// 源的大小（拿不到就是 `None`，进度里按 0 计）。
    pub size: Option<u64>,
    /// 这个文件的名字被改过（下载时的 Windows 名字处理）。
    pub note: Option<String>,
    /// 下载：展开计划时服务端已经给出的远端权限位（目录列表 / stat 带的），收尾时直接用，
    /// 省得每个文件再问一次；服务端没给、或不是下载计划就是 `None`。
    pub permissions: Option<u32>,
}

/// 一份展开好的传输计划。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferPlan {
    pub files: Vec<PlannedFile>,
    /// 目标端要先建出来的目录（远端路径或本地路径，按方向解释）。
    pub dirs: Vec<String>,
    /// 需要告诉用户的说明（跳过链接、名字改动、空目录等）。
    pub notes: Vec<Reason>,
}

impl TransferPlan {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size.unwrap_or(0)).sum()
    }

    /// 大小未知的文件数（进度可能不准）。
    pub fn unknown_size_files(&self) -> usize {
        self.files.iter().filter(|file| file.size.is_none()).count()
    }
}

/// 上传：本地路径 → 远端路径。
pub fn plan_upload(
    local_source: &Path,
    remote_target: &str,
    recursive: bool,
) -> Result<TransferPlan, SftpError> {
    let mut plan = TransferPlan::default();
    if local_source.is_file() {
        let size = std::fs::metadata(local::long_path(local_source))
            .map(|meta| meta.len())
            .ok();
        plan.files.push(PlannedFile {
            source: local_source.display().to_string(),
            target: remote_target.to_owned(),
            size,
            note: None,
            permissions: None,
        });
        return Ok(plan);
    }
    if !local_source.is_dir() {
        return Err(SftpError::NotFound {
            path: local_source.display().to_string(),
        });
    }
    if !recursive {
        return Err(dir_needs_recursive());
    }

    let tree = local::walk_local_tree(local_source)?;
    // 目标根本身也要建（"拖目录 a 到 /remote" 的落点是 /remote/a，它可能不存在；
    // M3-1 的测试传进的都是已存在的临时目录，没踩到这条）。create_target_dir
    // 对"已存在"是成功的，所以多建一次没有代价。
    plan.dirs.push(remote_target.to_owned());
    for dir in &tree.empty_dirs {
        plan.dirs.push(remote_path::join(remote_target, dir));
        plan.notes.push(
            Reason::new(
                reason_keys::XFER_NOTE_EMPTY_DIR,
                format!("空目录 {dir} 也建出来"),
            )
            .with("dir", dir),
        );
    }
    // 所有文件的父目录都要建。
    for file in &tree.files {
        if let Some(parent) = parent_of_relative(&file.relative) {
            let dir = remote_path::join(remote_target, &parent);
            if !plan.dirs.contains(&dir) {
                plan.dirs.push(dir);
            }
        }
    }
    for skipped in &tree.skipped_links {
        plan.notes.push(skipped_link_note(skipped));
    }
    for file in tree.files {
        plan.files.push(PlannedFile {
            source: file.path.display().to_string(),
            target: remote_path::join(remote_target, &file.relative),
            size: Some(file.size),
            note: None,
            permissions: None,
        });
    }
    plan.notes.push(file_count_note(plan.files.len()));
    Ok(plan)
}

fn skipped_link_note(path: &str) -> Reason {
    Reason::new(
        reason_keys::XFER_NOTE_SKIPPED_LINK,
        format!("跳过符号链接 / 目录联接：{path}"),
    )
    .with("path", path)
}

fn skipped_symlink_note(path: &str) -> Reason {
    Reason::new(
        reason_keys::XFER_NOTE_SKIPPED_SYMLINK,
        format!("跳过符号链接：{path}"),
    )
    .with("path", path)
}

fn file_count_note(count: usize) -> Reason {
    Reason::new(
        reason_keys::XFER_NOTE_FILE_COUNT,
        format!("共 {count} 个文件"),
    )
    .with("count", count)
}

/// 服务器给的条目名拼成 `dir` 下的孩子路径。名字不是单个文件名（含 `/`、NUL、空、`.`、`..`）时
/// 明确报错，不静默跳过（跳过再报"成功"会让用户以为都复制了）。
///
/// 列目录的边界（`fs::entries_from_protocol`）已经会丢掉这类条目；这里是第二道防线，因为
/// "规划写 / 删的目标"一旦被带到目录之外就是越权写，不能只靠上游。
pub(crate) fn child_path(dir: &str, name: &str) -> Result<String, SftpError> {
    remote_path::join_child(dir, name).ok_or_else(|| unsafe_entry_name(dir, name))
}

pub(crate) fn unsafe_entry_name(dir: &str, name: &str) -> SftpError {
    // 名字来自服务器：转义掉控制字符再放进提示，免得把终端控制序列带到界面 / 日志里。
    let shown: String = name
        .chars()
        .take(120)
        .flat_map(char::escape_debug)
        .collect();
    SftpError::InvalidInput(
        Reason::new(
            reason_keys::XFER_UNSAFE_ENTRY_NAME,
            format!("服务器返回的条目名不是单个文件名（{dir} 里的「{shown}」），已拒绝，没有复制任何东西"),
        )
        .with("name", &shown)
        .with("dir", dir),
    )
}

fn dir_needs_recursive() -> SftpError {
    SftpError::InvalidInput(Reason::new(
        reason_keys::XFER_DIR_NEEDS_RECURSIVE,
        "这是个目录：要么勾选递归，要么选一个文件",
    ))
}

/// 下载时一个条目的名字被改过：记一条「远端名 → 本地名（原因）」。
fn rename_note(remote_name: &str, assigned: &local::LocalName) -> Option<Reason> {
    let note = assigned.note.as_ref()?;
    Some(
        Reason::new(
            reason_keys::XFER_NOTE_RENAMED,
            format!("{remote_name} → {}（{note}）", assigned.name),
        )
        .with("remote", remote_name)
        .with("local", &assigned.name)
        .with("fixes", local::NameFix::codes(&assigned.fixes)),
    )
}

/// 下载：远端路径 → 本地路径。
pub async fn plan_download(
    lister: &dyn super::backend::RemoteLister,
    remote_source: &str,
    local_target: &Path,
    recursive: bool,
) -> Result<TransferPlan, SftpError> {
    plan_download_within(
        lister,
        remote_source,
        local_target,
        recursive,
        MAX_PLAN_DEPTH,
    )
    .await
}

/// 同 [`plan_download`]，层数上限可调（测试用小值造「太深」）。
pub(crate) async fn plan_download_within(
    lister: &dyn super::backend::RemoteLister,
    remote_source: &str,
    local_target: &Path,
    recursive: bool,
    max_depth: usize,
) -> Result<TransferPlan, SftpError> {
    let mut plan = TransferPlan::default();
    let mut attrs = lister.stat_remote(remote_source).await?;
    if attrs.is_symlink() {
        // 用户直接选中的符号链接：下载它指向的文件 / 目录（和 `scp`、系统文件管理器一样）。
        // 大小按目标算——`lstat` 的大小是链接文本的长度，用它当复制上限会把文件截短。
        // 目录里遇到的链接仍然跳过（不跟随，免得绕圈或带出目录外的东西）。
        attrs = lister.stat_follow(remote_source).await?;
        if attrs.is_symlink() {
            return Err(SftpError::InvalidInput(skipped_symlink_note(remote_source)));
        }
    }
    if !attrs.is_dir() {
        plan.files.push(PlannedFile {
            source: remote_source.to_owned(),
            target: local_target.display().to_string(),
            size: attrs.size,
            note: None,
            permissions: attrs.permissions,
        });
        return Ok(plan);
    }
    if !recursive {
        return Err(dir_needs_recursive());
    }

    // 逐层走远端树：每层先拿到名字，再给这一层分配本地名字。
    let mut pending: Vec<(String, PathBuf, usize)> =
        vec![(remote_source.to_owned(), local_target.to_path_buf(), 0)];
    plan.dirs.push(local_target.display().to_string());
    while let Some((dir, local_dir, depth)) = pending.pop() {
        if depth > max_depth {
            return Err(tree_too_deep(&dir, max_depth));
        }
        let entries = lister.list_dir(&dir).await?;
        let mut names = Vec::new();
        let mut sub_dirs = Vec::new();
        for entry in &entries {
            if entry.kind.is_symlink() {
                plan.notes.push(skipped_symlink_note(&entry.path));
                continue;
            }
            // 本地落盘的名字有 `LocalNamePlan` 清洗，但源路径是服务器给的 `entry.path`：
            // 名字不是单个文件名时它可能指到所选目录之外，别去读。
            child_path(&dir, &entry.name)?;
            if !entry.name_is_utf8 {
                // 名字不是 UTF-8：本地落盘会有损，明确记下来（下载下来内容是对的，
                // 名字只能用有损版本）。
                let lossy = local::local_name(&entry.name).name;
                plan.notes.push(
                    Reason::new(
                        reason_keys::XFER_NOTE_NON_UTF8,
                        format!(
                            "{} 的名字不是 UTF-8，本地名字用有损版本 {lossy}",
                            entry.path
                        ),
                    )
                    .with("path", &entry.path)
                    .with("local", lossy),
                );
            }
            if entry.kind.is_dir() {
                sub_dirs.push((entry.path.clone(), entry.name.clone()));
            } else {
                names.push((
                    entry.path.clone(),
                    entry.name.clone(),
                    entry.size,
                    entry.permissions,
                ));
            }
        }
        let existing = local_dir_entries(&local_dir);
        let remote_names: Vec<(String, bool)> = names
            .iter()
            .map(|(_, name, _, _)| (name.clone(), false))
            .chain(sub_dirs.iter().map(|(_, name)| (name.clone(), true)))
            .collect();
        let name_plan = LocalNamePlan::plan_with_kinds(&remote_names, &existing);
        let mut local_for = std::collections::HashMap::new();
        for (remote_name, assigned) in &name_plan.assignments {
            local_for.insert(remote_name.clone(), assigned.clone());
        }

        for (remote_file, remote_name, size, permissions) in names {
            let assigned = local_for
                .get(&remote_name)
                .cloned()
                .unwrap_or_else(|| local::LocalName::plain(remote_name.clone()));
            plan.notes.extend(rename_note(&remote_name, &assigned));
            let local_file = local_dir.join(&assigned.name);
            local::ensure_inside(local_target, &local_file)?;
            plan.files.push(PlannedFile {
                source: remote_file,
                target: local_file.display().to_string(),
                size,
                note: assigned.note.clone(),
                permissions,
            });
        }
        for (remote_dir, remote_name) in sub_dirs {
            let assigned = local_for
                .get(&remote_name)
                .cloned()
                .unwrap_or_else(|| local::LocalName::plain(remote_name.clone()));
            plan.notes.extend(rename_note(&remote_name, &assigned));
            let local_child = local_dir.join(&assigned.name);
            local::ensure_inside(local_target, &local_child)?;
            plan.dirs.push(local_child.display().to_string());
            pending.push((remote_dir, local_child, depth + 1));
        }
    }
    plan.notes.push(file_count_note(plan.files.len()));
    Ok(plan)
}

/// 远端 → 远端（复制粘贴的中转）：源树用 `lister` 读（可能是另一台主机），目标全是远端路径。
/// 符号链接跳过并记下来（和上传 / 下载一致）。
pub async fn plan_remote_copy(
    lister: &dyn super::backend::RemoteLister,
    source: &str,
    target: &str,
    recursive: bool,
) -> Result<TransferPlan, SftpError> {
    plan_remote_copy_within(lister, source, target, recursive, MAX_PLAN_DEPTH).await
}

/// 同 [`plan_remote_copy`]，层数上限可调（测试用小值造「太深」）。
pub(crate) async fn plan_remote_copy_within(
    lister: &dyn super::backend::RemoteLister,
    source: &str,
    target: &str,
    recursive: bool,
    max_depth: usize,
) -> Result<TransferPlan, SftpError> {
    let mut plan = TransferPlan::default();
    let attrs = lister.stat_remote(source).await?;
    if attrs.is_symlink() {
        return Err(SftpError::InvalidInput(Reason::new(
            reason_keys::XFER_SYMLINK_NO_RELAY,
            "符号链接不能用中转复制（会复制成链接指向的内容，和 cp -a 的行为不一致）",
        )));
    }
    if !attrs.is_dir() {
        plan.files.push(PlannedFile {
            source: source.to_owned(),
            target: target.to_owned(),
            size: attrs.size,
            note: None,
            permissions: None,
        });
        return Ok(plan);
    }
    if !recursive {
        return Err(dir_needs_recursive());
    }
    plan.dirs.push(target.to_owned());
    let mut pending = vec![(source.to_owned(), target.to_owned(), 0usize)];
    while let Some((dir, target_dir, depth)) = pending.pop() {
        if depth > max_depth {
            return Err(tree_too_deep(&dir, max_depth));
        }
        for entry in lister.list_dir(&dir).await? {
            if entry.kind.is_symlink() {
                plan.notes.push(skipped_symlink_note(&entry.path));
                continue;
            }
            let child = child_path(&target_dir, &entry.name)?;
            if entry.kind.is_dir() {
                plan.dirs.push(child.clone());
                pending.push((entry.path.clone(), child, depth + 1));
            } else {
                plan.files.push(PlannedFile {
                    source: entry.path.clone(),
                    target: child,
                    size: entry.size,
                    note: None,
                    permissions: None,
                });
            }
        }
    }
    plan.notes.push(file_count_note(plan.files.len()));
    Ok(plan)
}

/// 本地 → 本地（复制粘贴）：目录结构照搬，符号链接 / 联接跳过并记下来，空目录也复制。
pub fn plan_local_copy(
    source: &Path,
    target: &Path,
    recursive: bool,
) -> Result<TransferPlan, SftpError> {
    let mut plan = TransferPlan::default();
    if source.is_file() {
        let size = std::fs::metadata(local::long_path(source))
            .map(|meta| meta.len())
            .ok();
        plan.files.push(PlannedFile {
            source: source.display().to_string(),
            target: target.display().to_string(),
            size,
            note: None,
            permissions: None,
        });
        return Ok(plan);
    }
    if !source.is_dir() {
        return Err(SftpError::NotFound {
            path: source.display().to_string(),
        });
    }
    if !recursive {
        return Err(dir_needs_recursive());
    }
    let tree = local::walk_local_tree(source)?;
    plan.dirs.push(target.display().to_string());
    let join = |relative: &str| -> PathBuf {
        let mut path = target.to_path_buf();
        for part in relative.split('/').filter(|part| !part.is_empty()) {
            path.push(part);
        }
        path
    };
    for dir in &tree.empty_dirs {
        plan.dirs.push(join(dir).display().to_string());
    }
    for file in &tree.files {
        if let Some(parent) = parent_of_relative(&file.relative) {
            let dir = join(&parent).display().to_string();
            if !plan.dirs.contains(&dir) {
                plan.dirs.push(dir);
            }
        }
    }
    for skipped in &tree.skipped_links {
        plan.notes.push(skipped_link_note(skipped));
    }
    for file in tree.files {
        plan.files.push(PlannedFile {
            source: file.path.display().to_string(),
            target: join(&file.relative).display().to_string(),
            size: Some(file.size),
            note: None,
            permissions: None,
        });
    }
    plan.notes.push(file_count_note(plan.files.len()));
    Ok(plan)
}

/// 本地目录里已经有的名字（大小写冲突要避开它们）。
/// 本地目录里已有的条目：`(名字, 是不是目录)`。符号链接按它指向的东西算；读不出类型的按文件算
/// （和同名的远端文件走冲突对话框，和同名的远端目录则换名字，不会往里面写）。
fn local_dir_entries(dir: &Path) -> Vec<(String, bool)> {
    match std::fs::read_dir(local::long_path(dir)) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let is_dir = std::fs::metadata(entry.path())
                    .map(|meta| meta.is_dir())
                    .unwrap_or(false);
                (entry.file_name().to_string_lossy().into_owned(), is_dir)
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `a/b/c.txt` → `Some("a/b")`；`c.txt` → `None`。
fn parent_of_relative(relative: &str) -> Option<String> {
    relative
        .rfind('/')
        .map(|index| relative[..index].to_owned())
        .filter(|parent| !parent.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_plan_keeps_relative_structure_and_notes_empty_dirs() {
        let root = crate::test_util::TestDir::new("plan-up");
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("a.txt"), b"aaa").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), b"bbbb").unwrap();
        std::fs::write(root.join("sub").join("deep").join("c.txt"), b"cc").unwrap();

        let plan = plan_upload(&root, "/tmp/dest", true).expect("展开上传计划");
        let mut targets: Vec<&str> = plan.files.iter().map(|f| f.target.as_str()).collect();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                "/tmp/dest/a.txt",
                "/tmp/dest/sub/b.txt",
                "/tmp/dest/sub/deep/c.txt"
            ]
        );
        assert_eq!(plan.total_bytes(), 3 + 4 + 2);
        assert!(plan.dirs.contains(&"/tmp/dest/sub".to_owned()));
        assert!(plan.dirs.contains(&"/tmp/dest/sub/deep".to_owned()));
        assert!(
            plan.dirs.contains(&"/tmp/dest/empty".to_owned()),
            "空目录也要建：{:?}",
            plan.dirs
        );
        assert!(plan.notes.iter().any(|note| note.contains("空目录")));
    }

    #[test]
    fn upload_plan_refuses_a_directory_without_recursive() {
        let root = crate::test_util::TestDir::new("plan-up2");
        let err = plan_upload(&root, "/tmp/dest", false).expect_err("目录要递归");
        assert!(err.to_string().contains("目录"), "{err}");
    }

    #[test]
    fn parent_of_relative_handles_top_level_files() {
        assert_eq!(parent_of_relative("a.txt"), None);
        assert_eq!(parent_of_relative("sub/a.txt").as_deref(), Some("sub"));
        assert_eq!(
            parent_of_relative("sub/deep/a.txt").as_deref(),
            Some("sub/deep")
        );
    }

    /// 顶层符号链接：跟随到目标，大小按目标算；链接到目录时按目录展开（要递归）。
    struct Linked {
        /// 链接指向的是目录还是 1024 字节的普通文件。
        to_dir: bool,
    }

    impl super::super::backend::RemoteLister for Linked {
        fn list_dir<'a>(
            &'a self,
            path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<Vec<crate::sftp::fs::DirEntry>, SftpError>> {
            assert!(self.to_dir, "指向文件的链接不能当目录展开");
            let path = path.to_owned();
            Box::pin(async move {
                Ok(vec![crate::sftp::fs::DirEntry {
                    name: "app.conf".to_owned(),
                    path: format!("{path}/app.conf"),
                    kind: crate::sftp::fs::FileKind::File,
                    link_target: None,
                    size: Some(5),
                    mtime: None,
                    permissions: None,
                    owner: None,
                    name_is_utf8: true,
                }])
            })
        }

        fn stat_remote<'a>(
            &'a self,
            _path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>>
        {
            // lstat：链接本身，大小是链接文本的长度。
            Box::pin(async {
                Ok(russh_sftp::protocol::FileAttributes {
                    size: Some(11),
                    permissions: Some(0o120777),
                    ..Default::default()
                })
            })
        }

        fn stat_follow<'a>(
            &'a self,
            _path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>>
        {
            let to_dir = self.to_dir;
            Box::pin(async move {
                Ok(russh_sftp::protocol::FileAttributes {
                    size: Some(if to_dir { 4096 } else { 1024 }),
                    permissions: Some(if to_dir { 0o040755 } else { 0o100644 }),
                    ..Default::default()
                })
            })
        }
    }

    /// 展开下载计划时服务端给的权限位（目录列表 / stat 带的）随计划走，收尾时不必每个文件再问一次。
    #[tokio::test]
    async fn download_plans_carry_the_permissions_the_server_already_listed() {
        use crate::sftp::transfer::memory::MemoryEndpoints;

        let remote = MemoryEndpoints::new("/remote");
        remote.put_remote("/remote/dir/run.sh", b"#!/bin/sh\n".to_vec());
        remote.put_remote("/remote/dir/key.pem", b"k".to_vec());
        remote.set_remote_permissions("/remote/dir/run.sh", 0o100755);
        remote.set_remote_permissions("/remote/dir/key.pem", 0o100600);
        let root = crate::test_util::TestDir::new("plan-permissions");

        // 递归：每个文件带自己目录列表里的权限。
        let plan = plan_download(&remote, "/remote/dir", &root.join("out"), true)
            .await
            .unwrap();
        let mut seen: Vec<(String, Option<u32>)> = plan
            .files
            .iter()
            .map(|file| (file.source.clone(), file.permissions))
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("/remote/dir/key.pem".to_owned(), Some(0o100600)),
                ("/remote/dir/run.sh".to_owned(), Some(0o100755)),
            ]
        );

        // 单个文件：stat 带的权限。
        let single = plan_download(&remote, "/remote/dir/run.sh", &root.join("run.sh"), false)
            .await
            .unwrap();
        assert_eq!(single.files[0].permissions, Some(0o100755));

        // 上传 / 本地复制的计划没有「远端权限」。
        std::fs::write(root.join("a.txt"), b"x").unwrap();
        let local = plan_local_copy(&root.join("a.txt"), &root.join("b.txt"), false).unwrap();
        assert!(local.files.iter().all(|file| file.permissions.is_none()));
    }

    #[tokio::test]
    async fn a_top_level_symlink_downloads_its_target_with_the_target_size() {
        let root = crate::test_util::TestDir::new("plan-symlink-file");
        let target = root.join("default.conf");
        for recursive in [false, true] {
            let plan = plan_download(&Linked { to_dir: false }, "/link", &target, recursive)
                .await
                .unwrap();
            assert_eq!(plan.files.len(), 1);
            assert_eq!(plan.files[0].source, "/link");
            // 不是链接文本的 11 字节，而是目标文件的 1024 字节。
            assert_eq!(plan.files[0].size, Some(1024));
        }
    }

    #[tokio::test]
    async fn a_top_level_symlink_to_a_directory_is_expanded_like_the_directory() {
        let root = crate::test_util::TestDir::new("plan-symlink-dir");
        let target = root.join("current");
        let err = plan_download(&Linked { to_dir: true }, "/srv/current", &target, false)
            .await
            .expect_err("指向目录的链接和目录一样要递归");
        assert!(!matches!(err, SftpError::Cancelled));
        let plan = plan_download(&Linked { to_dir: true }, "/srv/current", &target, true)
            .await
            .unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].source, "/srv/current/app.conf");
    }

    /// 一个目录，里面的条目名由测试指定（模拟异常 / 恶意服务器给出的名字）。
    struct Named(Vec<&'static str>);

    impl super::super::backend::RemoteLister for Named {
        fn list_dir<'a>(
            &'a self,
            path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<Vec<crate::sftp::fs::DirEntry>, SftpError>> {
            let path = path.to_owned();
            let names = self.0.clone();
            Box::pin(async move {
                Ok(names
                    .into_iter()
                    .map(|name| crate::sftp::fs::DirEntry {
                        name: name.to_owned(),
                        path: remote_path::join(&path, name),
                        kind: crate::sftp::fs::FileKind::File,
                        link_target: None,
                        size: Some(1),
                        mtime: None,
                        permissions: None,
                        owner: None,
                        name_is_utf8: true,
                    })
                    .collect())
            })
        }

        fn stat_remote<'a>(
            &'a self,
            _path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>>
        {
            Box::pin(async {
                Ok(russh_sftp::protocol::FileAttributes {
                    permissions: Some(0o040755),
                    ..Default::default()
                })
            })
        }
    }

    /// W03：源服务器给的名字带绝对路径 / `..` / `/` / NUL 时，复制计划要明确报错，
    /// 目标里不能出现目录之外的路径；正常名字照常落在所选目录下。
    #[tokio::test]
    async fn remote_copy_refuses_entry_names_that_could_leave_the_target_dir() {
        for bad in [
            "/outside/chosen.txt",
            "../x",
            "a/b",
            "nul\0byte",
            "",
            ".",
            "..",
        ] {
            let err = plan_remote_copy(
                &Named(vec!["fine.txt", bad]),
                "/src",
                "/chosen-destination",
                true,
            )
            .await
            .expect_err(&format!("{bad:?} 应该被拒绝"));
            match err {
                SftpError::InvalidInput(reason) => {
                    assert_eq!(reason.key(), reason_keys::XFER_UNSAFE_ENTRY_NAME, "{bad:?}");
                }
                other => panic!("{bad:?}: {other}"),
            }
        }
        let plan = plan_remote_copy(
            &Named(vec!["fine.txt", ".hidden", "back\\slash"]),
            "/src",
            "/chosen-destination",
            true,
        )
        .await
        .expect("正常名字");
        assert_eq!(plan.files.len(), 3);
        for file in &plan.files {
            assert!(
                file.target.starts_with("/chosen-destination/"),
                "{}",
                file.target
            );
            assert_eq!(remote_path::parent(&file.target), "/chosen-destination");
        }
    }

    /// 下载计划也一样：源路径是服务器给的，名字不是单个文件名就不去读。
    #[tokio::test]
    async fn recursive_download_refuses_entry_names_that_escape_the_source_dir() {
        let root = crate::test_util::TestDir::new("plan-down-escape");
        let err = plan_download(
            &Named(vec!["/etc/shadow"]),
            "/srv/data",
            &root.join("out"),
            true,
        )
        .await
        .expect_err("绝对路径名字");
        assert!(matches!(err, SftpError::InvalidInput(_)), "{err}");
    }

    /// F02：递归下载时远端子项的名字再怪，也只能落在目标目录里面。
    #[tokio::test]
    async fn recursive_download_keeps_hostile_names_inside_the_target() {
        use crate::sftp::transfer::memory::MemoryEndpoints;
        let remote = MemoryEndpoints::new("/remote");
        for name in [
            "C:evil.txt",
            "CON",
            "a:b",
            "back\\slash.txt",
            "..\\..\\up.txt",
            "trail. ",
            "sub:dir/inner.txt",
            "D:/x.txt",
        ] {
            remote.put_remote(&format!("/remote/tree/{name}"), b"x".to_vec());
        }
        let root = crate::test_util::TestDir::new("plan-down-evil");
        let target = root.join("out");
        let plan = plan_download(&remote, "/remote/tree", &target, true)
            .await
            .expect("展开下载计划");
        assert!(!plan.files.is_empty());
        for file in &plan.files {
            local::ensure_inside(&target, Path::new(&file.target))
                .unwrap_or_else(|err| panic!("{} 跑出了目标目录：{err}", file.target));
        }
        for dir in plan.dirs.iter().skip(1) {
            local::ensure_inside(&target, Path::new(dir))
                .unwrap_or_else(|err| panic!("{dir} 跑出了目标目录：{err}"));
        }
    }

    /// 一棵只有一条链的远端树：`/r/d1/d2/.../d{levels}`，每层一个目录，最深一层里一个文件。
    struct Chain {
        levels: usize,
    }

    impl Chain {
        fn dir_at(&self, path: &str) -> Option<usize> {
            if path == "/r" {
                return Some(0);
            }
            let rest = path.strip_prefix("/r/")?;
            let depth = rest.split('/').count();
            rest.split('/')
                .enumerate()
                .all(|(index, part)| part == format!("d{}", index + 1))
                .then_some(depth)
        }
    }

    impl super::super::backend::RemoteLister for Chain {
        fn list_dir<'a>(
            &'a self,
            path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<Vec<crate::sftp::fs::DirEntry>, SftpError>> {
            Box::pin(async move {
                let depth = self.dir_at(path).unwrap_or(self.levels);
                let (name, kind) = if depth < self.levels {
                    (format!("d{}", depth + 1), crate::sftp::fs::FileKind::Dir)
                } else {
                    ("leaf.txt".to_owned(), crate::sftp::fs::FileKind::File)
                };
                Ok(vec![crate::sftp::fs::DirEntry {
                    path: remote_path::join(path, &name),
                    name,
                    kind,
                    link_target: None,
                    size: Some(1),
                    mtime: None,
                    permissions: None,
                    owner: None,
                    name_is_utf8: true,
                }])
            })
        }

        fn stat_remote<'a>(
            &'a self,
            _path: &'a str,
        ) -> crate::ssh::BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>>
        {
            Box::pin(async {
                Ok(russh_sftp::protocol::FileAttributes {
                    permissions: Some(0o040755),
                    ..Default::default()
                })
            })
        }
    }

    fn tree_too_deep_key(err: &SftpError) -> bool {
        matches!(err, SftpError::RemoteReason(reason) if reason.key() == reason_keys::XFER_TREE_TOO_DEEP)
    }

    /// 深度超限必须报错，不能把深处丢掉再报成功；刚好在上限上的树照常展开。
    #[tokio::test]
    async fn plans_refuse_trees_deeper_than_the_limit() {
        let root = crate::test_util::TestDir::new("plan-deep");
        let out = root.join("out");

        let deep = Chain { levels: 5 };
        let err = plan_download_within(&deep, "/r", &out, true, 3)
            .await
            .expect_err("下载计划太深要报错");
        assert!(tree_too_deep_key(&err), "{err:?}");
        let err = plan_remote_copy_within(&deep, "/r", "/dst", true, 3)
            .await
            .expect_err("复制计划太深要报错");
        assert!(tree_too_deep_key(&err), "{err:?}");

        // 5 层的链：最深的目录在第 5 层，上限 5 刚好放行，列到文件为止。
        let plan = plan_download_within(&deep, "/r", &out, true, 5)
            .await
            .expect("正好在上限上要放行");
        assert_eq!(plan.files.len(), 1);
        let plan = plan_remote_copy_within(&deep, "/r", "/dst", true, 5)
            .await
            .expect("正好在上限上要放行");
        assert_eq!(plan.files.len(), 1);
    }

    /// 默认上限下，比它深的树同样失败（常量真的接上了）。
    #[tokio::test]
    async fn the_default_depth_limit_is_enforced() {
        let root = crate::test_util::TestDir::new("plan-deep-default");
        let deep = Chain {
            levels: MAX_PLAN_DEPTH + 5,
        };
        let err = plan_remote_copy(&deep, "/r", "/dst", true)
            .await
            .expect_err("太深要报错");
        assert!(tree_too_deep_key(&err), "{err:?}");
        let err = plan_download(&deep, "/r", &root.join("out"), true)
            .await
            .expect_err("太深要报错");
        assert!(tree_too_deep_key(&err), "{err:?}");
    }
}
