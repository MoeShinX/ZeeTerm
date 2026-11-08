//! 联网用例：真实服务器上的打开 / 编辑 / 保存 / 冲突 / 草稿恢复。
//!
//! 只在 `ZSSH_LIVE_FULL=1` 且设了测试服务器环境变量（`ZSSH_TEST_HOST` / `USER` / `PORT` / `KEY_PATH`）时跑，
//! 否则打印「跳过」直接返回（见 `live_support`）。服务器上只在 `/tmp/zssh-live-g09-<用例名>-<进程号>/`
//! 里操作，用例结束（包括 panic）整个目录删掉。
//!
//! 这里验的是「保存成功对应正确的文件版本和远端摘要」：每次保存之后读回远端字节，比 SHA-256，
//! 而不是只看返回值。

use std::sync::Arc;

use zssh_core::ai::files::{FsEntry, FsStat, RemoteFs, RenameFailure};
use zssh_core::sftp::SftpChannel;
use zssh_core::sftp::edit::recovery::sha256_hex;
use zssh_core::sftp::edit::{EditBackend, OverwriteError, OwnerMode};
use zssh_core::sftp::error::ReplaceState;
use zssh_core::ssh::BoxFuture;

use super::contract::{encode, open_bytes, version_of};
use super::drafts::{EditDraft, draft_file_in, now_unix, write_draft};
use super::recovery::{Action, Outcome, act_with, scan};
use super::remote::{SaveOutcome, fetch_bytes, save_remote};
use crate::live_support::LiveSftp;
use crate::ssh_backend::runtime;

/// 借用真实通道，但让「改名」一定失败：临时文件已经写到服务器上，提交那一步断掉。
struct FailRename<'a>(zssh_core::ai::files::SftpRemoteFs<&'a SftpChannel>);

impl RemoteFs for FailRename<'_> {
    fn server_key(&self) -> Option<String> {
        self.0.server_key()
    }
    fn home<'a>(&'a self) -> BoxFuture<'a, Result<String, String>> {
        self.0.home()
    }
    fn stat<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<FsStat>, String>> {
        self.0.stat(path)
    }
    fn realpath<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String, String>> {
        self.0.realpath(path)
    }
    fn list<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<FsEntry>, String>> {
        self.0.list(path)
    }
    fn read_range<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        len: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        self.0.read_range(path, offset, len)
    }
    fn read_all<'a>(&'a self, path: &'a str, limit: u64) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        self.0.read_all(path, limit)
    }
    fn write_file<'a>(
        &'a self,
        path: &'a str,
        data: &'a [u8],
        mode: Option<u32>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.0.write_file(path, data, mode)
    }
    fn rename_over<'a>(
        &'a self,
        _from: &'a str,
        _to: &'a str,
    ) -> BoxFuture<'a, Result<(), RenameFailure>> {
        Box::pin(async {
            Err(RenameFailure {
                state: ReplaceState::Untouched,
                message: "测试注入：提交前断线".to_owned(),
            })
        })
    }
    fn remove_file<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), String>> {
        self.0.remove_file(path)
    }
    fn create_exclusive<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<(), zssh_core::ai::files::CreateExclusiveError>> {
        self.0.create_exclusive(path)
    }
    fn set_owner<'a>(
        &'a self,
        path: &'a str,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.0.set_owner(path, uid, gid)
    }
}

fn digest(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// 打开 → 编辑 → 保存：BOM + CRLF 原样往返、权限保持、远端摘要正确、目录里不留临时文件。
#[test]
fn live_open_edit_save_keeps_bytes_permissions_and_leaves_no_temp_files() {
    let Some(live) = LiveSftp::connect("g09-save") else {
        return;
    };
    let runtime = runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let path = live.path("win.conf");
    runtime.block_on(async {
        let original = b"\xEF\xBB\xBFkey=1\r\nname=\xE4\xB8\xAD\xE6\x96\x87\r\n";
        channel.write_all(&path, original).await.expect("建文件");
        channel.set_permissions(&path, 0o640).await.expect("设权限");

        let bytes = fetch_bytes(&*channel, &path).await.expect("读回");
        assert_eq!(digest(&bytes), digest(original));
        let opened = open_bytes(&bytes).expect("能打开");
        assert_eq!(
            opened.text, "key=1\nname=中文\n",
            "编辑器里只有 LF，没有 BOM"
        );

        // 编辑：改一个值、加一行。
        let edited = opened.text.replace("key=1", "key=2") + "added=yes\n";
        let to_write = encode(&edited, &opened.format);
        let outcome = save_remote(
            &*channel,
            &path,
            &to_write,
            Some(opened.version),
            OwnerMode::Require,
        )
        .await;
        match outcome {
            SaveOutcome::Saved { version, size, .. } => {
                assert_eq!(version, version_of(&to_write));
                assert_eq!(size, to_write.len());
            }
            other => panic!("应该保存成功：{other:?}"),
        }
        let expected = b"\xEF\xBB\xBFkey=2\r\nname=\xE4\xB8\xAD\xE6\x96\x87\r\nadded=yes\r\n";
        let after = fetch_bytes(&*channel, &path).await.expect("读回");
        assert_eq!(after, expected, "BOM 和 CRLF 逐字节还原");
        assert_eq!(digest(&after), digest(expected), "远端摘要正确");
        let stat = zssh_core::sftp::fs::stat(&channel, &path, true)
            .await
            .expect("stat");
        assert_eq!(stat.permissions.map(|mode| mode & 0o7777), Some(0o640));
        let names: Vec<String> = channel
            .read_dir(&live.dir)
            .await
            .expect("列目录")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, vec!["win.conf".to_owned()], "不留临时文件");
    });
}

/// 保存时远端被别处改过：拒绝，远端内容不动；选「覆盖」才写；两个编辑器同时保存只有一个写成。
#[test]
fn live_conflict_overwrite_and_concurrent_saves() {
    let Some(live) = LiveSftp::connect("g09-conflict") else {
        return;
    };
    let runtime = runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let path = live.path("shared.txt");
    runtime.block_on(async {
        channel.write_all(&path, b"base\n").await.expect("建文件");
        let base = version_of(b"base\n");

        // 1) 别处改了文件：保存冲突，远端不动。
        channel
            .write_all(&path, b"edited elsewhere\n")
            .await
            .expect("别处改动");
        let outcome =
            save_remote(&*channel, &path, b"mine\n", Some(base), OwnerMode::Require).await;
        assert!(matches!(outcome, SaveOutcome::Conflict(_)), "{outcome:?}");
        assert_eq!(
            fetch_bytes(&*channel, &path).await.expect("读回"),
            b"edited elsewhere\n"
        );
        // 2) 用户选「覆盖」：不比版本，写成。
        let outcome = save_remote(&*channel, &path, b"mine\n", None, OwnerMode::Require).await;
        assert!(matches!(outcome, SaveOutcome::Saved { .. }), "{outcome:?}");
        assert_eq!(
            fetch_bytes(&*channel, &path).await.expect("读回"),
            b"mine\n"
        );

        // 3) 两个「编辑器」拿着同一个旧版本同时保存：只有一个写成，远端是写成的那份。
        let version = version_of(b"mine\n");
        let (a, b) = (Arc::clone(&channel), Arc::clone(&channel));
        let (pa, pb) = (path.clone(), path.clone());
        let one = tokio::spawn(async move {
            save_remote(&*a, &pa, b"tab one\n", Some(version), OwnerMode::Require).await
        });
        let two = tokio::spawn(async move {
            save_remote(&*b, &pb, b"tab two\n", Some(version), OwnerMode::Require).await
        });
        let (one, two) = (one.await.expect("任务一"), two.await.expect("任务二"));
        let saved = [&one, &two]
            .iter()
            .filter(|outcome| matches!(outcome, SaveOutcome::Saved { .. }))
            .count();
        let conflicts = [&one, &two]
            .iter()
            .filter(|outcome| matches!(outcome, SaveOutcome::Conflict(_)))
            .count();
        assert_eq!((saved, conflicts), (1, 1), "{one:?} / {two:?}");
        let winner: &[u8] = if matches!(one, SaveOutcome::Saved { .. }) {
            b"tab one\n"
        } else {
            b"tab two\n"
        };
        assert_eq!(fetch_bytes(&*channel, &path).await.expect("读回"), winner);
    });
}

/// 保存中断（临时文件已写好、提交那一步失败）：原文件逐字节完好，临时文件被清掉。
#[test]
fn live_interrupted_save_leaves_the_original_intact() {
    let Some(live) = LiveSftp::connect("g09-interrupt") else {
        return;
    };
    let runtime = runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let path = live.path("conf.txt");
    runtime.block_on(async {
        let original = "第一行\nsecond line\n".as_bytes();
        channel.write_all(&path, original).await.expect("建文件");
        channel.set_permissions(&path, 0o600).await.expect("设权限");
        let failing = FailRename(zssh_core::ai::files::SftpRemoteFs::borrowed(&channel));
        let result = zssh_core::sftp::edit::upload_via_fs(
            &failing,
            &path,
            b"half written\n",
            "g09inject001",
            OwnerMode::Require,
        )
        .await;
        assert!(
            matches!(result, Err(OverwriteError::Failed(_))),
            "{result:?}"
        );
        assert_eq!(
            fetch_bytes(&*channel, &path).await.expect("读回"),
            original,
            "原文件必须完好"
        );
        let names: Vec<String> = channel
            .read_dir(&live.dir)
            .await
            .expect("列目录")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, vec!["conf.txt".to_owned()], "不留临时文件");
        let stat = zssh_core::sftp::fs::stat(&channel, &path, true)
            .await
            .expect("stat");
        assert_eq!(stat.permissions.map(|mode| mode & 0o7777), Some(0o600));
    });
}

/// 异常退出后的恢复链（真实服务器）：草稿 → 扫描 → 重新上传；远端变过则报冲突、草稿留着，选覆盖才写。
#[test]
fn live_draft_recovery_reupload_checks_conflicts() {
    let Some(live) = LiveSftp::connect("g09-recover") else {
        return;
    };
    let runtime = runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let data = std::env::temp_dir().join(format!("zssh-g09-recover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let clean = live.path("clean.txt");
    let moved = live.path("moved.txt");
    runtime.block_on(async {
        channel.write_all(&clean, b"v1\n").await.expect("建文件");
        channel.write_all(&moved, b"v1\n").await.expect("建文件");
        // 两份草稿（编辑器里没保存的内容，进程被结束时留在数据目录里的）。
        for (file, text) in [(&clean, "clean draft\n"), (&moved, "stale draft\n")] {
            let draft = EditDraft {
                path: file.clone(),
                host_id: Some(9),
                base_version: version_of(b"v1\n").0,
                text: text.to_owned(),
                saved_at_unix: now_unix(),
                revision: 3,
                server: None,
            };
            write_draft(&draft_file_in(&data, 9, file), &draft).expect("写草稿");
        }
        // 第二份：服务器上的文件在这期间被别人改了。
        channel
            .write_all(&moved, b"v2 by someone else\n")
            .await
            .expect("别处改动");

        let items = scan(&data);
        assert_eq!(items.len(), 2);
        let by_path = |wanted: &str| {
            items
                .iter()
                .find(|item| item.path.as_deref() == Some(wanted))
                .expect("扫得到")
        };
        // 远端没变：重新上传成功，草稿删掉，远端摘要正确。
        let item = by_path(&clean);
        let outcome = act_with(&*channel, item, Action::Reupload).await;
        assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
        let after = fetch_bytes(&*channel, &clean).await.expect("读回");
        assert_eq!(digest(&after), digest(b"clean draft\n"));
        let file = &item.file;
        assert!(!file.exists(), "传上去之后草稿删掉");
        // 远端变过：不传、草稿留着；覆盖才传。
        let item = by_path(&moved);
        let outcome = act_with(&*channel, item, Action::Reupload).await;
        assert!(matches!(outcome, Outcome::Conflict(_)), "{outcome:?}");
        assert_eq!(
            fetch_bytes(&*channel, &moved).await.expect("读回"),
            b"v2 by someone else\n"
        );
        let file = &item.file;
        assert!(file.exists(), "冲突时草稿必须留着");
        let outcome = act_with(&*channel, item, Action::ReuploadOverwrite).await;
        assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
        assert_eq!(
            fetch_bytes(&*channel, &moved).await.expect("读回"),
            b"stale draft\n"
        );
    });
    let _ = std::fs::remove_dir_all(&data);
}
