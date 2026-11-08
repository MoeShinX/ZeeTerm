//! 传输引擎的单测（M3-1 第 3、4 节）：队列与并发、冲突策略、断线重试与退避、
//! 暂停与续传的位置、路径改名。
//!
//! 这些用例**不需要服务器**：数据面用内存替身（[`memory::MemoryEndpoint`]），
//! 通过 `TestProvider` 把它接给引擎（生产上是 SFTP 通道）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::endpoint::FileEndpoint;
use super::endpoint::LocalEndpoint;
use super::memory::{MemoryEndpoint, MemoryEndpoints};
use super::*;

fn local_options() -> TransferOptions {
    TransferOptions {
        concurrency: 2,
        pipeline: true,
        in_flight: 4,
        chunk_len: Some(4096),
        preserve_mtime: false,
        preserve_permissions: false,
        max_retries: 2,
        progress_interval: Duration::from_millis(10),
        tail_window: 1024,
    }
}

fn temp_root(tag: &str) -> crate::test_util::TestDir {
    crate::test_util::TestDir::new(&format!("engine-{tag}"))
}

/// 收事件，直到任务进终态（或超时）。
async fn drain_until_finished(
    receiver: &mut mpsc::UnboundedReceiver<TransferEvent>,
    id: TransferId,
    limit: Duration,
) -> ProgressReport {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "等不到任务 {id} 的终态");
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Ok(Some(TransferEvent::Finished(report))) if report.id == id => return report,
            Ok(Some(_)) => {}
            Ok(None) => panic!("事件通道关了"),
            Err(_) => panic!("等不到任务 {id} 的终态"),
        }
    }
}

#[test]
fn backoff_grows_and_is_capped() {
    assert_eq!(backoff_delay(1), Duration::from_millis(400));
    assert_eq!(backoff_delay(2), Duration::from_millis(800));
    assert!(backoff_delay(5) <= Duration::from_secs(5));
    assert_eq!(backoff_delay(20), Duration::from_secs(5), "有上限");
}

#[cfg(windows)]
#[test]
fn renamed_path_and_with_file_name_know_both_kinds_of_separators() {
    // Windows 本地路径：目录名里有点、文件没扩展名，不能改在目录名上。
    assert_eq!(renamed_path(r"C:\my.dir\file", 2), r"C:\my.dir\file (2)");
    assert_eq!(
        renamed_path(r"C:\my.dir\photo.jpg", 2),
        r"C:\my.dir\photo (2).jpg"
    );
    // 把最后一段换成用户给的名字。
    assert_eq!(with_file_name("/a/b.txt", "c.txt"), "/a/c.txt");
    assert_eq!(with_file_name(r"C:\a\b.txt", "c.txt"), r"C:\a\c.txt");
    assert_eq!(with_file_name("b.txt", "c.txt"), "c.txt");
}

/// Linux：分隔符只有 `/`，`\` 是文件名里的普通字符。
#[cfg(unix)]
#[test]
fn renamed_path_and_with_file_name_only_split_on_slashes() {
    assert_eq!(renamed_path("/a/b.txt", 2), "/a/b (2).txt");
    assert_eq!(renamed_path("/my.dir/file", 2), "/my.dir/file (2)");
    // `a\b.txt` 是一个名字，扩展名在最后；`my.dir\file` 里的点也是名字的一部分，序号照常加在
    // 最后一个点前面（Windows 上反斜杠是目录，结果不一样）。
    assert_eq!(renamed_path("/d/a\\b.txt", 2), "/d/a\\b (2).txt");
    assert_eq!(renamed_path("/d/my.dir\\file", 2), "/d/my (2).dir\\file");
    assert_eq!(renamed_path(r"C:\my.dir\file", 2), r"C:\my (2).dir\file");

    assert_eq!(with_file_name("/a/b.txt", "c.txt"), "/a/c.txt");
    assert_eq!(with_file_name("b.txt", "c.txt"), "c.txt");
    // 最后一段整个被换掉，里面的反斜杠不是目录。
    assert_eq!(with_file_name("/a/x\\y.txt", "c.txt"), "/a/c.txt");
    assert_eq!(with_file_name("/a/b.txt", "p\\q.txt"), "/a/p\\q.txt");
    assert_eq!(with_file_name(r"C:\a\b.txt", "c.txt"), "c.txt");
}

#[cfg(unix)]
#[test]
fn copy_names_on_linux_treat_backslash_as_part_of_the_name() {
    assert_eq!(copy_path("/a/report.txt", 1), "/a/report - 副本.txt");
    assert_eq!(copy_path("/a/report.txt", 2), "/a/report - 副本 2.txt");
    assert_eq!(copy_path("/a/noext", 1), "/a/noext - 副本");
    assert_eq!(copy_path("/a/.bashrc", 1), "/a/.bashrc - 副本");
    assert_eq!(copy_path("/a/my.dir/file", 1), "/a/my.dir/file - 副本");
    assert_eq!(copy_path("/a/a\\b.txt", 1), "/a/a\\b - 副本.txt");
    assert_eq!(copy_path("/a/a\\b.txt", 3), "/a/a\\b - 副本 3.txt");
    assert_eq!(copy_path("/a/my.dir\\file", 1), "/a/my - 副本.dir\\file");
}

/// Linux：冲突里改的新名字带反斜杠是合法名字，要用上，不能当成路径分隔符退回自动名字。
#[cfg(unix)]
#[tokio::test]
async fn conflict_rename_accepts_a_backslash_name_on_linux() {
    struct RenameTo(&'static str);
    impl ConflictResolver for RenameTo {
        fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            let name = self.0;
            Box::pin(async move {
                ConflictDecision {
                    action: ConflictAction::Rename,
                    apply_to_all: false,
                    rename_to: Some(name.to_owned()),
                }
            })
        }
    }

    let root = temp_root("rename-backslash");
    let source = root.join("data.bin");
    std::fs::write(&source, vec![8u8; 2048]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/sub/data.bin", vec![1u8; 100]);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(RenameTo("a\\b.bin")),
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/sub/data.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/sub/a\\b.bin"),
        vec![8u8; 2048]
    );
    assert_eq!(
        endpoints.remote_contents("/remote/sub/data.bin"),
        vec![1u8; 100],
        "原来的文件不能被动"
    );
}

#[test]
fn renamed_path_keeps_the_extension() {
    assert_eq!(renamed_path("/a/photo.jpg", 2), "/a/photo (2).jpg");
    assert_eq!(renamed_path("/a/noext", 2), "/a/noext (2)");
    assert_eq!(renamed_path("/a/.bashrc", 2), "/a/.bashrc (2)");
}

#[test]
fn control_flag_transitions() {
    let flag = ControlFlag::new();
    assert_eq!(flag.state(), file::ControlState::Running);
    flag.pause();
    assert_eq!(flag.state(), file::ControlState::Paused);
    flag.resume();
    assert_eq!(flag.state(), file::ControlState::Running);
    flag.cancel();
    assert_eq!(flag.state(), file::ControlState::Cancelled);
}

/// 队列：两个任务都排进去，都能跑到终态；并发上限生效（同时传的文件数不超过设置）。
#[tokio::test]
async fn queue_runs_jobs_and_respects_concurrency() {
    let root = temp_root("queue");
    let source_dir = root.join("src");
    std::fs::create_dir_all(&source_dir).unwrap();
    for index in 0..6 {
        std::fs::write(
            source_dir.join(format!("file-{index}.bin")),
            vec![index as u8; 64 * 1024],
        )
        .unwrap();
    }
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(OverwriteAll),
        TransferOptions {
            concurrency: 2,
            ..local_options()
        },
    );

    // 用本地目录当"远端"，让引擎真的跑一遍（本地→本地；远端分支的线测试在
    // sftp_transfer_live.rs 里跑真服务器）。
    let mut ids = Vec::new();
    for index in 0..6 {
        ids.push(
            engine.submit(TransferJob {
                direction: Direction::Upload,
                source: source_dir
                    .join(format!("file-{index}.bin"))
                    .display()
                    .to_string(),
                target: format!("/remote/file-{index}.bin"),
                recursive: false,
                peer: None,
            }),
        );
    }

    let mut finished = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while finished < ids.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "任务没跑完");
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(TransferEvent::Finished(report))) => {
                assert_eq!(report.state, TransferState::Done, "{report:?}");
                assert_eq!(report.done_bytes, 64 * 1024, "{report:?}");
                assert_eq!(report.total_files, 1);
                finished += 1;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => panic!("任务没跑完"),
        }
    }
    for index in 0..6 {
        let content = endpoints.remote_contents(&format!("/remote/file-{index}.bin"));
        assert_eq!(content.len(), 64 * 1024, "第 {index} 个文件要传完");
        assert_eq!(content[0], index as u8);
    }
}

/// 目标已存在 + "跳过"策略：任务进 Skipped，目标文件保持原样。
#[tokio::test]
async fn skip_policy_leaves_the_target_alone() {
    let root = temp_root("skip");
    let source = root.join("source.txt");
    std::fs::write(&source, b"new contents").unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/target.txt", b"old".to_vec());
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(SkipAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/target.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Skipped, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/target.txt"),
        b"old",
        "跳过的目标不该被动"
    );
    assert!(
        report.notes.iter().any(|note| note.contains("跳过")),
        "{:?}",
        report.notes
    );
}

/// 覆盖策略：目标被新内容替换。
#[tokio::test]
async fn overwrite_policy_replaces_the_target() {
    let root = temp_root("overwrite");
    let source = root.join("source.txt");
    std::fs::write(&source, b"new contents").unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote(
        "/remote/target.txt",
        b"much older and longer content".to_vec(),
    );
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/target.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/target.txt"),
        b"new contents"
    );
}

/// 冲突回调："目标更大时不能续传"由引擎判断（can_resume 传进回调）。
#[tokio::test]
async fn conflict_callback_knows_when_resume_is_possible() {
    struct RecordDecisions(Arc<Mutex<Vec<ConflictInfo>>>);
    impl ConflictResolver for RecordDecisions {
        fn decide<'a>(&'a self, info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            Box::pin(async move {
                self.0.lock().unwrap().push(info);
                ConflictDecision {
                    action: ConflictAction::Overwrite,
                    apply_to_all: true,
                    rename_to: None,
                }
            })
        }
    }

    let root = temp_root("conflict-info");
    let source = root.join("source.bin");
    std::fs::write(&source, vec![1u8; 4096]).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 远端已经有一个更小的同名文件 → 可以续传。
    endpoints.put_remote("/remote/target.bin", vec![2u8; 1024]);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(RecordDecisions(Arc::clone(&seen))),
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/target.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    let infos = seen.lock().unwrap();
    assert_eq!(infos.len(), 1, "只问一次");
    assert_eq!(infos[0].source_size, Some(4096));
    assert_eq!(infos[0].target_size, Some(1024));
    assert!(infos[0].can_resume, "目标比源小 → 可以续传");
    assert_eq!(infos[0].direction, Direction::Upload);
}

/// 断线重试：用内存端点注入"写到一半断线"，看引擎重试之后能不能接着传完。
///
/// 这里是 `copy_file` + 尾部对账的组合（引擎的重试循环在真链路上由
/// `sftp_transfer_live.rs` 的断线续传用例覆盖）。
#[tokio::test]
async fn disconnect_then_resume_completes_the_file() {
    let source_bytes: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_be_bytes()).collect();
    let source = MemoryEndpoint::new("source", source_bytes.clone());
    let target = MemoryEndpoint::new("target", Vec::new());
    // 写到 150KB 就断一次（批是 8×8192=64KB，这样前两批是确认写好的前缀）。
    target.fail_write_after(150_000);

    let control = ControlFlag::new();
    let first = copy_file(
        source.as_endpoint(),
        target.as_endpoint(),
        source_bytes.len() as u64,
        0,
        CopyTuning::from_limits(8192, true),
        &control,
        |_, _| {},
    )
    .await;
    let err = first.expect_err("第一次要断");
    assert!(err.is_disconnect(), "{err}");
    // 断线收尾会把临时文件截回**已确认的前缀**（整批整批地确认）。
    let partial = target.partial_len().await.unwrap();
    assert!(
        partial > 0 && partial < source_bytes.len() as u64,
        "已确认的前缀应该在中间：{partial}"
    );
    assert_eq!(partial % 8192, 0, "前缀落在批边界上：{partial}");

    // 重连之后：先对账，再接着传（模拟引擎的重试路径）。
    let source_ep = source.as_endpoint();
    let target_ep = target.as_endpoint();
    let resume_len = target_ep.partial_len().await.unwrap();
    assert!(
        tails_match(&source_ep, &target_ep, resume_len, 1024)
            .await
            .unwrap(),
        "断线前写的字节应当和源一致"
    );
    let outcome = copy_file(
        Arc::clone(&source_ep),
        Arc::clone(&target_ep),
        source_bytes.len() as u64,
        resume_len,
        CopyTuning::from_limits(8192, true),
        &control,
        |_, _| {},
    )
    .await
    .expect("续传要成功");
    assert_eq!(
        outcome,
        CopyOutcome::Completed {
            bytes: source_bytes.len() as u64 - resume_len
        }
    );
    target_ep
        .commit(source_bytes.len() as u64, true)
        .await
        .unwrap();
    assert_eq!(target.contents(), source_bytes, "续传之后内容要完全一致");

    // 尾部对不上时（目标被改过）必须从头传：这里换个目标验证。
    let target2 = MemoryEndpoint::new("target2", vec![0u8; 100_000]);
    let source2 = MemoryEndpoint::new("source2", vec![7u8; 200_000]);
    assert!(
        !tails_match(
            &source2.as_endpoint(),
            &target2.as_endpoint(),
            100_000,
            1024
        )
        .await
        .unwrap(),
        "内容不一样 → 不能续"
    );
}

/// 重启任务（临时文件比源还长）：临时文件被丢掉，从头传。
#[tokio::test]
async fn partial_longer_than_source_is_discarded() {
    let source = MemoryEndpoint::new("source", vec![3u8; 1000]);
    let target = MemoryEndpoint::new("target", Vec::new());
    // 手工造一个比源长的临时文件。
    {
        let writer = target.open_write(0).await.unwrap();
        writer.write_at(0, vec![9u8; 5000]).await.unwrap();
        writer.finish().await.unwrap();
    }
    assert_eq!(target.partial_len().await.unwrap(), 5000);

    let source_ep = source.as_endpoint();
    let target_ep = target.as_endpoint();
    let source_size = source_ep.size().await.unwrap().unwrap();
    let resume_len = target_ep.partial_len().await.unwrap();
    assert!(resume_len > source_size, "前提：临时文件更长");
    // 引擎的处理：丢掉临时文件，从头传。
    target_ep.remove_partial().await.unwrap();
    let control = ControlFlag::new();
    let outcome = copy_file(
        Arc::clone(&source_ep),
        Arc::clone(&target_ep),
        source_size,
        0,
        CopyTuning::from_limits(512, false),
        &control,
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome, CopyOutcome::Completed { bytes: 1000 });
    assert_eq!(target_ep.partial_len().await.unwrap(), 1000);
}

/// 本地磁盘满：明确报错，临时文件留着（以便续传）。
#[tokio::test]
async fn local_disk_full_reports_clearly_and_keeps_the_partial() {
    let source = MemoryEndpoint::new("source", vec![1u8; 100_000]);
    let target = MemoryEndpoint::new("target", Vec::new());
    target.no_space_after(30_000);

    let control = ControlFlag::new();
    let err = copy_file(
        source.as_endpoint(),
        target.as_endpoint(),
        100_000,
        0,
        CopyTuning::from_limits(4096, true),
        &control,
        |_, _| {},
    )
    .await
    .expect_err("磁盘满要报错");
    assert!(matches!(err, SftpError::LocalNoSpace { .. }), "{err}");
    assert!(err.to_string().contains("磁盘空间不足"), "{err}");
    // 写是成批发的：一批里失败时，后面的块可能已经落盘 —— 收尾会把临时文件截回
    // **已确认的前缀**（这一批整批失败，所以是 0）。续传的不变量由此保住：
    // 临时文件长度 = 连续写好的前缀，下次从它接着传，不会在中间留洞。
    assert_eq!(
        target.partial_len().await.unwrap(),
        0,
        "失败之后临时文件要截回已确认的前缀"
    );
}

/// 续传位置的计算：暂停时临时文件长度就是下次的起点。
#[tokio::test]
async fn pause_records_the_resume_offset() {
    let source = MemoryEndpoint::new("source", vec![5u8; 200_000]);
    let target = MemoryEndpoint::new("target", Vec::new());
    let control = ControlFlag::new();
    let flag = control.clone();
    let outcome = copy_file(
        source.as_endpoint(),
        target.as_endpoint(),
        200_000,
        0,
        CopyTuning::from_limits(1024, false),
        &control,
        move |done, _| {
            if done >= 5000 {
                flag.pause();
            }
        },
    )
    .await
    .unwrap();
    let CopyOutcome::Paused { resume_len, .. } = outcome else {
        panic!("应当暂停：{outcome:?}");
    };
    assert_eq!(target.partial_len().await.unwrap(), resume_len);
    assert!(resume_len >= 5000, "{resume_len}");
}

/// 空文件也要能传（不能因为 size=0 就跳过或死循环）。
#[tokio::test]
async fn empty_files_are_transferred() {
    let root = temp_root("empty-file");
    let source = root.join("empty.txt");
    std::fs::write(&source, b"").unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/copy.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert!(
        endpoints.remote_exists("/remote/copy.txt"),
        "空文件也要建出来"
    );
    assert!(endpoints.remote_contents("/remote/copy.txt").is_empty());
}

/// 下载方向：远端（内存）→ 本地目录，内容一致；名字按 Windows 规则处理。
#[cfg(windows)]
#[tokio::test]
async fn download_writes_local_files_with_clean_names() {
    let root = temp_root("download");
    let local_dir = root.join("downloads");
    std::fs::create_dir_all(&local_dir).unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/dir/a.txt", b"aaa".to_vec());
    endpoints.put_remote("/remote/dir/weird:name?.txt", b"bbb".to_vec());
    endpoints.put_remote("/remote/dir/empty-dir-marker", Vec::new());

    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Download,
        source: "/remote/dir".to_owned(),
        target: local_dir.display().to_string(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(std::fs::read(local_dir.join("a.txt")).unwrap(), b"aaa");
    // 非法字符被换掉，说明写在 notes 里。
    assert_eq!(
        std::fs::read(local_dir.join("weird_name_.txt")).unwrap(),
        b"bbb"
    );
    assert!(
        report.notes.iter().any(|note| note.contains("非法字符")),
        "{:?}",
        report.notes
    );
}

/// 下载方向（Linux）：Windows 才有的限制不适用，这些名字原样落盘；只有控制字符被换掉并注明；
/// 只差大小写的两个文件是两个文件。
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn download_keeps_posix_file_names_as_they_are() {
    let root = temp_root("download-posix");
    let local_dir = root.join("downloads");
    std::fs::create_dir_all(&local_dir).unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    for (name, body) in [
        ("a.txt", &b"aaa"[..]),
        ("weird:name?.txt", b"bbb"),
        ("CON.txt", b"ccc"),
        ("trailing. ", b"ddd"),
        ("back\\slash.log", b"eee"),
        ("File", b"upper"),
        ("file", b"lower"),
        ("bell\u{7}.txt", b"fff"),
    ] {
        endpoints.put_remote(&format!("/remote/dir/{name}"), body.to_vec());
    }

    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Download,
        source: "/remote/dir".to_owned(),
        target: local_dir.display().to_string(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    for (name, body) in [
        ("a.txt", &b"aaa"[..]),
        ("weird:name?.txt", b"bbb"),
        ("CON.txt", b"ccc"),
        ("trailing. ", b"ddd"),
        ("back\\slash.log", b"eee"),
        ("File", b"upper"),
        ("file", b"lower"),
        ("bell_.txt", b"fff"),
    ] {
        assert_eq!(
            std::fs::read(local_dir.join(name)).unwrap(),
            body,
            "{name:?}"
        );
    }
    let mut on_disk: Vec<String> = std::fs::read_dir(&local_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    assert_eq!(on_disk.len(), 8, "不该多出带序号的改名文件：{on_disk:?}");
    // 说明里只有控制字符那一条；没有保留名 / 冲突之类的。
    let renamed: Vec<_> = report
        .notes
        .iter()
        .filter(|note| note.contains("非法字符"))
        .collect();
    assert_eq!(renamed.len(), 1, "{:?}", report.notes);
    assert!(
        !report
            .notes
            .iter()
            .any(|note| note.contains("保留名") || note.contains("只差大小写")),
        "{:?}",
        report.notes
    );
}

/// 下载方向（macOS）：卷不分大小写，远端同时有 `File` 和 `file` 时两个都要落下来——
/// 后一个改成带序号的本地名字，不能互相覆盖。POSIX 里合法的名字（`:`、`?`、`\`）照旧保留，
/// 只有控制字符换成 `_` 并注明。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn download_keeps_both_files_whose_names_differ_only_by_case_on_macos() {
    let root = temp_root("download-macos-case");
    let local_dir = root.join("downloads");
    std::fs::create_dir_all(&local_dir).unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    for (name, body) in [
        ("File", &b"upper"[..]),
        ("file", b"lower"),
        ("weird:name?.txt", b"bbb"),
        ("bell\u{7}.txt", b"fff"),
    ] {
        endpoints.put_remote(&format!("/remote/dir/{name}"), body.to_vec());
    }

    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Download,
        source: "/remote/dir".to_owned(),
        target: local_dir.display().to_string(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");

    // 两个文件都在，内容各自正确：谁拿到原名取决于列目录顺序，所以不假设具体是谁。
    let mut contents: Vec<Vec<u8>> = std::fs::read_dir(&local_dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            std::fs::read(entry.path()).unwrap()
        })
        .collect();
    contents.sort();
    assert_eq!(
        contents,
        vec![
            b"bbb".to_vec(),
            b"fff".to_vec(),
            b"lower".to_vec(),
            b"upper".to_vec()
        ],
        "两个只差大小写的文件都要落下来，不能互相覆盖"
    );
    let names: Vec<String> = std::fs::read_dir(&local_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|name| name == "File" || name == "file"),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| name == "File (2)" || name == "file (2)"),
        "后一个要带序号改名：{names:?}"
    );
    // POSIX 的非法字符规则不变：`:` 和 `?` 原样，控制字符换成 `_` 并注明。
    assert!(
        names.iter().any(|name| name == "weird:name?.txt"),
        "{names:?}"
    );
    assert!(names.iter().any(|name| name == "bell_.txt"), "{names:?}");
    assert!(
        report.notes.iter().any(|note| note.contains("只差大小写")),
        "{:?}",
        report.notes
    );
    assert!(
        report.notes.iter().any(|note| note.contains("非法字符")),
        "{:?}",
        report.notes
    );
}

/// 本地端点：目标目录不存在时自动建父目录。
#[tokio::test]
async fn local_endpoint_creates_parent_directories() {
    let root = temp_root("parents");
    let endpoint = LocalEndpoint::new(root.join("a").join("b").join("c.txt"));
    let writer = endpoint.open_write(0).await.expect("打开写（父目录要建）");
    writer.write_at(0, b"hi".to_vec()).await.unwrap();
    writer.finish().await.unwrap();
    endpoint.commit(2, true).await.unwrap();
    assert_eq!(
        std::fs::read(root.join("a").join("b").join("c.txt")).unwrap(),
        b"hi"
    );
}

/// 重试（M3-2 第 1 节）：失败的任务同一个任务号重跑，上次留下的 `.zssh-part`
/// 接着用（尾部对账把着关）；不能重试的（没这个任务 / 已完成）返回 false。
#[tokio::test]
async fn retry_reruns_a_failed_job_and_resumes_from_the_part_file() {
    let root = temp_root("retry");
    let source = root.join("retry.bin");
    std::fs::write(&source, vec![7u8; 16 * 1024]).unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 写到 4KB 就"断线"（只断一次），max_retries = 0：第一次任务直接失败。
    endpoints.fail_remote_write_after(4096);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(OverwriteAll),
        TransferOptions {
            max_retries: 0,
            in_flight: 1,
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/retry.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert!(
        matches!(report.state, TransferState::Failed(_)),
        "{report:?}"
    );
    let partial_left = endpoints.remote_partial_len("/remote/retry.bin");
    assert!(partial_left > 0, "失败后临时文件要留着：{partial_left}");

    // 重试：断线只断一次，这次能传完；内容一致。
    assert!(engine.retry(id), "失败的任务要能重试");
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/retry.bin"),
        vec![7u8; 16 * 1024]
    );
    assert_eq!(
        endpoints.remote_partial_len("/remote/retry.bin"),
        0,
        "传完要收尾"
    );

    // 已完成的任务不能重试；不存在的任务号也不行。
    assert!(!engine.retry(id), "完成的任务不能重试");
    assert!(!engine.retry(9999), "不存在的任务不能重试");

    // 清除已完成：清掉一个；报告列表随之空掉。
    assert_eq!(engine.reports().len(), 1);
    assert_eq!(engine.clear_finished(), 1);
    assert!(engine.reports().is_empty());
    assert_eq!(engine.clear_finished(), 0, "再清一次是空的");
}

/// 冲突信息要把两头的修改时间带给界面（M3-2 第 4 节的冲突对话框要显示）。
#[tokio::test]
async fn conflict_info_carries_both_mtimes() {
    struct Capture(Arc<Mutex<Vec<ConflictInfo>>>);
    impl ConflictResolver for Capture {
        fn decide<'a>(&'a self, info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            let sink = Arc::clone(&self.0);
            Box::pin(async move {
                sink.lock().unwrap().push(info);
                ConflictDecision {
                    action: ConflictAction::Overwrite,
                    apply_to_all: false,
                    rename_to: None,
                }
            })
        }
    }
    let root = temp_root("conflict-mtime");
    let source = root.join("mtime.bin");
    std::fs::write(&source, vec![1u8; 8192]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/mtime.bin", vec![2u8; 1024]);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (engine, mut events) = TransferEngine::new(
        endpoints,
        Arc::new(Capture(Arc::clone(&seen))),
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/mtime.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    let infos = seen.lock().unwrap();
    let info = infos.first().expect("要问过一次冲突");
    // 源是本地文件（真实 mtime），目标是内存远端（固定值）；两头都要带到。
    assert!(info.source_mtime.is_some(), "{info:?}");
    assert_eq!(info.target_mtime, Some(1_700_000_000), "{info:?}");
    assert_eq!(info.target_size, Some(1024), "{info:?}");
    assert!(info.can_resume, "目标比源小，要能续传：{info:?}");
}

/// 上传目录时**目标根本身也要建**（M3-2 端到端实测踩到的坑："拖目录 a 到
/// /remote" 的落点是 /remote/a，之前只建了它里面的子目录，mkdir 嵌套路径报
/// "文件不存在"——M3-1 的测试传进的都是已存在的临时目录，没踩到）。
#[tokio::test]
async fn upload_creates_the_target_root_itself() {
    let root = temp_root("target-root");
    let source = root.join("资料");
    std::fs::create_dir_all(source.join("子目录")).unwrap();
    std::fs::write(source.join("子目录").join("数据.bin"), vec![1u8; 128]).unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/资料".to_owned(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/资料/子目录/数据.bin"),
        vec![1u8; 128],
        "目标根没建出来，文件落不下去"
    );
}

/// 冲突问过之后暂停、再续传：**不重新问**，接着断点传完（M3-2 实测踩到的：
/// 重新问一遍没人答，120 秒超时按跳过，传一半的文件被丢掉）。
#[tokio::test]
async fn pause_after_conflict_resume_does_not_ask_again() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct GatedResolver {
        asked: AtomicUsize,
        /// 第一次问：先叫醒测试（让它按暂停），再等测试放行。
        asked_once: Notify,
        release: Notify,
    }
    impl ConflictResolver for GatedResolver {
        fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            Box::pin(async {
                let count = self.asked.fetch_add(1, AtomicOrdering::SeqCst);
                if count == 0 {
                    self.asked_once.notify_waiters();
                    self.release.notified().await;
                }
                ConflictDecision {
                    action: ConflictAction::Overwrite,
                    apply_to_all: false,
                    rename_to: None,
                }
            })
        }
    }

    let root = temp_root("pause-conflict");
    let source = root.join("big.bin");
    std::fs::write(&source, vec![3u8; 64 * 1024]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 目标已经有同名文件 → 冲突；用户选覆盖，传之前就把任务按了暂停。
    endpoints.put_remote("/remote/big.bin", vec![9u8; 1024]);

    let resolver = Arc::new(GatedResolver::default());
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        TransferOptions {
            in_flight: 1,
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/big.bin".to_owned(),
        recursive: false,
        peer: None,
    });

    // 等第一次冲突提问 → 按暂停 → 放行提问。复制在第一批边界停住。
    tokio::time::timeout(Duration::from_secs(30), resolver.asked_once.notified())
        .await
        .expect("没等到第一次冲突提问");
    assert!(engine.pause(id), "暂停要成功");
    resolver.release.notify_waiters();

    // 等任务停在 Paused（不是终态，Progress 事件里看）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let state = engine.state(id).expect("任务要在");
        if state == TransferState::Paused {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "没停在暂停状态：{state:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        resolver.asked.load(AtomicOrdering::SeqCst),
        1,
        "暂停前只问一次"
    );
    // 清掉排队的事件。
    while events.try_recv().is_ok() {}

    // 续传：不再问，接着传完，内容一致。
    assert!(engine.resume(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        resolver.asked.load(AtomicOrdering::SeqCst),
        1,
        "续传不能重新问冲突"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/big.bin"),
        vec![3u8; 64 * 1024],
        "覆盖之后要是源内容"
    );
}

/// 暂停中被取消：这个任务**自己的**临时文件要清掉（以前这条路径直接返回"已取消"，
/// 临时文件一直留在服务器上）；别人的 / 用户自己的 `.zssh-part` 不能碰。
#[tokio::test]
async fn cancel_while_paused_removes_the_part_file() {
    let root = temp_root("cancel-paused");
    let source = root.join("big.bin");
    std::fs::write(&source, vec![5u8; 64 * 1024]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 服务器上已经有一个旧写法的 `big.bin.zssh-part`：不是这个任务的，不能当缓存、也不能删。
    endpoints.put_remote("/remote/big.bin.zssh-part", vec![1u8; 100]);

    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/big.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    // 这个任务上次传到一半留下的临时文件：名字里带着它的任务标识。任务号是进程内唯一的、
    // 不从 1 开始，所以要提交之后才知道名字；单线程运行时里任务此刻还没开始跑，来得及放。
    endpoints.put_remote(
        &format!("/remote/big.bin.{}{id}.zssh-part", engine.nonce),
        vec![2u8; 100],
    );
    // 单线程运行时：任务还没开始跑，先按暂停；它在检查点停住。
    assert!(engine.pause(id));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while engine.state(id) != Some(TransferState::Paused) {
        assert!(tokio::time::Instant::now() < deadline, "没停在暂停");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let paused_parts = endpoints.remote_part_files("/remote/big.bin");
    assert_eq!(
        paused_parts.len(),
        2,
        "暂停时这个任务自己的临时文件要留着（可以续传），旁边还有别人的那个：{paused_parts:?}"
    );

    assert!(engine.cancel(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Cancelled, "{report:?}");
    assert_eq!(
        endpoints.remote_part_files("/remote/big.bin"),
        vec!["/remote/big.bin.zssh-part".to_owned()],
        "取消之后只该留下别人的那个"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/big.bin.zssh-part"),
        vec![1u8; 100],
        "别人的临时文件被动了"
    );
    assert!(
        !endpoints.remote_exists("/remote/big.bin"),
        "不该有最终文件"
    );
}

/// 冲突里选"重命名"并自己改了名字：对话框传回来的只是文件名，要接在原目标所在的
/// 目录后面（M3-2 走查发现：之前把它当完整路径用，会写到相对路径上）。原文件不动。
#[tokio::test]
async fn conflict_rename_with_a_custom_name_stays_in_the_target_directory() {
    struct RenameTo(&'static str);
    impl ConflictResolver for RenameTo {
        fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            let name = self.0;
            Box::pin(async move {
                ConflictDecision {
                    action: ConflictAction::Rename,
                    apply_to_all: false,
                    rename_to: Some(name.to_owned()),
                }
            })
        }
    }

    let root = temp_root("rename-custom");
    let source = root.join("data.bin");
    std::fs::write(&source, vec![8u8; 4096]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/sub/data.bin", vec![1u8; 100]);

    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(RenameTo("我改的名字.bin")),
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/sub/data.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/sub/我改的名字.bin"),
        vec![8u8; 4096],
        "新名字要落在原目标所在的目录里"
    );
    assert!(
        !endpoints.remote_exists("/remote/我改的名字.bin"),
        "不能写到上一级 / 相对路径上"
    );
    assert!(
        !endpoints.remote_exists("我改的名字.bin"),
        "不能写到相对路径上"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/sub/data.bin"),
        vec![1u8; 100],
        "原来的文件不能被动"
    );
}

/// F04：同一个最终目标已经有任务在写，第二个任务不能去写（也不能动对方的临时文件），
/// 对方写完释放之后再来就能传。
#[tokio::test]
async fn a_second_job_for_the_same_local_target_is_refused_not_mixed_in() {
    let root = temp_root("same-target");
    let local_dir = root.join("downloads");
    std::fs::create_dir_all(&local_dir).unwrap();
    let target = local_dir.join("same.bin");

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/a.bin", vec![b'A'; 3000]);
    endpoints.put_remote("/remote/b.bin", vec![b'B'; 5000]);
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());

    // 模拟"另一个任务正在写这个目标"。
    let holder = lock::TargetLocks::local()
        .try_acquire_local(&target)
        .expect("第一个任务拿到目标");
    let id = engine.submit(TransferJob {
        direction: Direction::Download,
        source: "/remote/b.bin".to_owned(),
        target: target.display().to_string(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    let TransferState::Failed(message) = &report.state else {
        panic!("应该因为目标被占用而失败：{report:?}");
    };
    assert!(message.contains("正在写"), "{message}");
    assert!(!target.exists(), "被拒绝的任务什么都不能写");
    let leftovers: Vec<_> = std::fs::read_dir(&local_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "被拒绝的任务留下了东西：{leftovers:?}"
    );

    // 对方做完，再来一次就行。
    drop(holder);
    let retry = engine.submit(TransferJob {
        direction: Direction::Download,
        source: "/remote/b.bin".to_owned(),
        target: target.display().to_string(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, retry, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(std::fs::read(&target).unwrap(), vec![b'B'; 5000]);
}

/// F04：两个任务同时往同一个目标写，结果要么是其中一个的完整内容，不会混在一起；
/// 用户目录里原本就有的 `<目标>.zssh-part` 也不会被动。
#[tokio::test]
async fn racing_jobs_never_mix_contents_and_leave_a_users_part_file_alone() {
    let root = temp_root("race");
    let local_dir = root.join("downloads");
    std::fs::create_dir_all(&local_dir).unwrap();
    let target = local_dir.join("same.bin");
    let users = local_dir.join("same.bin.zssh-part");
    std::fs::write(&users, b"MINE").unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/a.bin", vec![b'A'; 40_000]);
    endpoints.put_remote("/remote/b.bin", vec![b'B'; 50_000]);
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let ids: Vec<_> = ["/remote/a.bin", "/remote/b.bin"]
        .into_iter()
        .map(|source| {
            engine.submit(TransferJob {
                direction: Direction::Download,
                source: source.to_owned(),
                target: target.display().to_string(),
                recursive: false,
                peer: None,
            })
        })
        .collect();
    // 两个任务的结束事件可能以任意顺序到：一起收。
    let mut pending: std::collections::HashSet<_> = ids.into_iter().collect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !pending.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(TransferEvent::Finished(report))) => {
                pending.remove(&report.id);
            }
            Ok(Some(_)) => {}
            _ => panic!("等不到任务结束：{pending:?}"),
        }
    }
    let content = std::fs::read(&target).expect("至少有一个任务成功");
    let all_a = content == vec![b'A'; 40_000];
    let all_b = content == vec![b'B'; 50_000];
    assert!(
        all_a || all_b,
        "目标内容是两个任务混在一起的：{} 字节",
        content.len()
    );
    assert_eq!(std::fs::read(&users).unwrap(), b"MINE");
}

/// R-6b：远端（内存替身）不覆盖收尾遇到已存在的目标：失败、原文件不变、临时文件还在；覆盖照常成功。
#[tokio::test]
async fn remote_commit_without_overwrite_keeps_the_existing_file() {
    let endpoints = MemoryEndpoints::new("/remote");
    endpoints.put_remote("/remote/f.txt", b"OLD".to_vec());
    let target = MemoryEndpoint::with_fs(&endpoints.remote, "/remote/f.txt");
    let writer = target.open_write(0).await.unwrap();
    writer.write_at(0, b"NEW!".to_vec()).await.unwrap();
    writer.finish().await.unwrap();

    let err = target
        .commit(4, false)
        .await
        .expect_err("目标已存在必须失败");
    assert!(matches!(err, SftpError::AlreadyExists { .. }), "{err:?}");
    assert_eq!(endpoints.remote_contents("/remote/f.txt"), b"OLD");
    assert_eq!(
        endpoints.remote_partial_len("/remote/f.txt"),
        4,
        "临时文件要留着"
    );

    target.commit(4, true).await.unwrap();
    assert_eq!(endpoints.remote_contents("/remote/f.txt"), b"NEW!");
}

// ---- R01：远端临时文件认主 + 远端目标锁在所有引擎之间共享 ----

/// 两个真实的传输引擎同时上传到同一个远端路径：第二个被拒，内容不混。
/// 用冲突提问把第一个引擎卡在"已经拿了目标、还没开始写"的位置，保证两个任务真的重叠。
#[tokio::test]
async fn two_engines_uploading_to_one_remote_path_do_not_mix_contents() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct HoldFirst {
        asked: AtomicUsize,
        asked_once: Notify,
        release: Notify,
    }
    impl ConflictResolver for HoldFirst {
        fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            Box::pin(async {
                if self.asked.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    self.asked_once.notify_one();
                    self.release.notified().await;
                }
                ConflictDecision {
                    action: ConflictAction::Overwrite,
                    apply_to_all: false,
                    rename_to: None,
                }
            })
        }
    }

    let root = temp_root("two-engines");
    let source_a = root.join("a.bin");
    let source_b = root.join("b.bin");
    std::fs::write(&source_a, vec![b'A'; 40_000]).unwrap();
    std::fs::write(&source_b, vec![b'B'; 50_000]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/same.bin", vec![b'0'; 100]);

    let resolver = Arc::new(HoldFirst::default());
    let (engine_a, mut events_a) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        local_options(),
    );
    let (engine_b, mut events_b) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());

    let id_a = engine_a.submit(TransferJob {
        direction: Direction::Upload,
        source: source_a.display().to_string(),
        target: "/remote/same.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(30), resolver.asked_once.notified())
        .await
        .expect("第一个引擎应该走到冲突提问（这时它已经拿了目标）");

    let id_b = engine_b.submit(TransferJob {
        direction: Direction::Upload,
        source: source_b.display().to_string(),
        target: "/remote/same.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report_b = drain_until_finished(&mut events_b, id_b, Duration::from_secs(20)).await;
    let TransferState::Failed(message) = &report_b.state else {
        panic!("第二个引擎应该因为目标被占用而失败：{report_b:?}");
    };
    assert!(message.contains("正在写"), "{message}");
    assert_eq!(
        endpoints.remote_contents("/remote/same.bin"),
        vec![b'0'; 100],
        "被拒绝的引擎什么都不能写"
    );

    resolver.release.notify_one();
    let report_a = drain_until_finished(&mut events_a, id_a, Duration::from_secs(20)).await;
    assert_eq!(report_a.state, TransferState::Done, "{report_a:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/same.bin"),
        vec![b'A'; 40_000],
        "内容必须是第一个任务的完整内容"
    );
}

/// 服务器上用户自己的 `x.zssh-part` 不能被当成缓存截断：上传 `x` 之后它原样还在。
#[tokio::test]
async fn a_users_own_zssh_part_file_on_the_server_is_never_touched() {
    let root = temp_root("remote-user-part");
    let source = root.join("x.bin");
    std::fs::write(&source, vec![b'X'; 20_000]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote(
        "/remote/x.bin.zssh-part",
        b"USER DATA THAT MATTERS".to_vec(),
    );

    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/x.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/x.bin"),
        vec![b'X'; 20_000]
    );
    assert_eq!(
        endpoints.remote_contents("/remote/x.bin.zssh-part"),
        b"USER DATA THAT MATTERS",
        "用户自己的 .zssh-part 文件被动了"
    );
}

/// M3-5 0a：两个引擎各有自己的数据面对象，只要它们连的是**同一个服务器身份**，就共用同一把远端锁；
/// 连的是不同服务器，同名路径互不影响。
#[tokio::test]
async fn engines_on_separate_endpoints_share_the_lock_when_the_server_identity_matches() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct HoldFirst {
        asked: AtomicUsize,
        asked_once: Notify,
        release: Notify,
    }
    impl ConflictResolver for HoldFirst {
        fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            Box::pin(async {
                if self.asked.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    self.asked_once.notify_one();
                    self.release.notified().await;
                }
                ConflictDecision {
                    action: ConflictAction::Overwrite,
                    apply_to_all: false,
                    rename_to: None,
                }
            })
        }
    }

    let root = temp_root("identity-lock");
    let source = root.join("a.bin");
    std::fs::write(&source, vec![b'A'; 30_000]).unwrap();
    let make = |identity: &str| {
        let endpoints = Arc::new(MemoryEndpoints::new("/remote").with_identity(identity));
        endpoints.put_remote("/remote/same.bin", vec![b'0'; 10]);
        endpoints
    };

    let first = make("alice@files.example:22");
    let resolver = Arc::new(HoldFirst::default());
    let (engine_a, mut events_a) = TransferEngine::new(
        first.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        local_options(),
    );
    let id_a = engine_a.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/same.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(30), resolver.asked_once.notified())
        .await
        .expect("第一个引擎应该走到冲突提问");

    // 同一个服务器身份（另一个数据面对象，比如另一个标签）：被拒。
    let same = make("alice@files.example:22");
    let (engine_b, mut events_b) =
        TransferEngine::new(same.clone(), Arc::new(OverwriteAll), local_options());
    let id_b = engine_b.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/same.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report_b = drain_until_finished(&mut events_b, id_b, Duration::from_secs(20)).await;
    assert!(
        matches!(&report_b.state, TransferState::Failed(message) if message.contains("正在写")),
        "同一个服务器身份应该被拒：{report_b:?}"
    );
    assert_eq!(same.remote_contents("/remote/same.bin"), vec![b'0'; 10]);

    // 不同的服务器：同名路径照常可以写。
    let other = make("alice@another.example:22");
    let (engine_c, mut events_c) =
        TransferEngine::new(other.clone(), Arc::new(OverwriteAll), local_options());
    let id_c = engine_c.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/same.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report_c = drain_until_finished(&mut events_c, id_c, Duration::from_secs(20)).await;
    assert_eq!(report_c.state, TransferState::Done, "{report_c:?}");

    resolver.release.notify_one();
    let report_a = drain_until_finished(&mut events_a, id_a, Duration::from_secs(20)).await;
    assert_eq!(report_a.state, TransferState::Done, "{report_a:?}");
}

// ---------- 复制粘贴的两个方向（M3-5 第 3 节） ----------

#[cfg(windows)]
#[test]
fn copy_names_put_the_suffix_before_the_extension() {
    assert_eq!(copy_path("/a/report.txt", 1), "/a/report - 副本.txt");
    assert_eq!(copy_path("/a/report.txt", 2), "/a/report - 副本 2.txt");
    assert_eq!(copy_path("/a/noext", 1), "/a/noext - 副本");
    assert_eq!(copy_path("/a/.bashrc", 1), "/a/.bashrc - 副本");
    assert_eq!(copy_path(r"C:\my.dir\file", 1), r"C:\my.dir\file - 副本");
    assert_eq!(copy_path(r"C:\d\p.jpg", 1), r"C:\d\p - 副本.jpg");
}

#[test]
fn copy_names_with_posix_slashes_are_the_same_everywhere() {
    assert_eq!(copy_path("/a/report.txt", 1), "/a/report - 副本.txt");
    assert_eq!(copy_path("/a/my.dir/file", 2), "/a/my.dir/file - 副本 2");
    assert_eq!(renamed_path("/a/b.txt", 2), "/a/b (2).txt");
    assert_eq!(with_file_name("/a/b.txt", "c.txt"), "/a/c.txt");
}

/// 远端到远端：整棵目录（含子目录）经引擎复制到新位置，内容一致，源不动。
#[tokio::test]
async fn remote_copy_copies_a_whole_tree_and_leaves_the_source_alone() {
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/app/a.txt", b"alpha".to_vec());
    endpoints.put_remote("/remote/app/sub/b.bin", vec![7u8; 10_000]);
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::RemoteCopy,
        source: "/remote/app".to_owned(),
        target: "/remote/app-copy".to_owned(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(10)).await;
    assert_eq!(report.state, TransferState::Done, "{:?}", report.notes);
    assert_eq!(
        endpoints.remote_contents("/remote/app-copy/a.txt"),
        b"alpha"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/app-copy/sub/b.bin"),
        vec![7u8; 10_000]
    );
    assert_eq!(endpoints.remote_contents("/remote/app/a.txt"), b"alpha");
}

/// 远端到远端的单个文件：目标已存在、选"跳过"→ 目标不变，任务算跳过。
#[tokio::test]
async fn remote_copy_skip_keeps_the_existing_target() {
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/x.txt", b"new".to_vec());
    endpoints.put_remote("/remote/dst/x.txt", b"old".to_vec());
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(SkipAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::RemoteCopy,
        source: "/remote/x.txt".to_owned(),
        target: "/remote/dst/x.txt".to_owned(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(10)).await;
    assert_eq!(report.state, TransferState::Skipped);
    assert_eq!(endpoints.remote_contents("/remote/dst/x.txt"), b"old");
}

/// 一个源不存在的复制任务要失败并说清楚，不能悄悄成功。
#[tokio::test]
async fn remote_copy_of_a_missing_source_fails() {
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::RemoteCopy,
        source: "/remote/nope".to_owned(),
        target: "/remote/dst".to_owned(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(10)).await;
    assert!(
        matches!(report.state, TransferState::Failed(_)),
        "{:?}",
        report.state
    );
}

/// 本地到本地：目录树（含空目录）复制，源不动。
#[tokio::test]
async fn local_copy_copies_a_tree_including_empty_directories() {
    let root = temp_root("local-copy");
    let source = root.join("proj");
    std::fs::create_dir_all(source.join("sub").join("empty")).unwrap();
    std::fs::write(source.join("a.txt"), b"alpha").unwrap();
    std::fs::write(source.join("sub").join("b.txt"), b"bravo").unwrap();
    let target = root.join("proj-copy");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) =
        TransferEngine::new(endpoints, Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::LocalCopy,
        source: source.display().to_string(),
        target: target.display().to_string(),
        recursive: true,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(10)).await;
    assert_eq!(report.state, TransferState::Done, "{:?}", report.notes);
    assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(
        std::fs::read(target.join("sub").join("b.txt")).unwrap(),
        b"bravo"
    );
    assert!(target.join("sub").join("empty").is_dir(), "空目录也要复制");
    assert!(source.join("a.txt").exists(), "源不动");
}

/// 本地复制和下载写同一个本地路径时互斥（同一个最终目标只让一个任务写）。
#[tokio::test]
async fn local_copy_takes_the_local_target_lock() {
    let root = temp_root("local-copy-lock");
    let source = root.join("a.txt");
    std::fs::write(&source, b"x").unwrap();
    let target = root.join("b.txt");
    let guard = lock::TargetLocks::local()
        .try_acquire_local(&target)
        .expect("先占住本地目标");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) =
        TransferEngine::new(endpoints, Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::LocalCopy,
        source: source.display().to_string(),
        target: target.display().to_string(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(10)).await;
    assert!(
        matches!(report.state, TransferState::Failed(_)),
        "目标被别人占着时要失败而不是覆盖：{:?}",
        report.state
    );
    drop(guard);
}

/// 新文件（目标原来不存在）传到一半暂停、再继续：要从临时文件接着传，不能从 0 重来
/// （R-9 F14：以前"目标不存在"会把续传标记改成 false，暂停前传好的部分全部作废）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pausing_a_new_file_resumes_from_its_own_part_file() {
    let root = temp_root("pause-new-file");
    let source = root.join("big.bin");
    let size: usize = 16 * 1024 * 1024;
    let contents: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &contents).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(OverwriteAll),
        TransferOptions {
            concurrency: 1,
            in_flight: 1,
            chunk_len: Some(1024),
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/big.bin".to_owned(),
        recursive: false,
        peer: None,
    });

    // 等临时文件里有了一部分内容就暂停。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while endpoints.remote_partial_len("/remote/big.bin") == 0 {
        assert!(tokio::time::Instant::now() < deadline, "临时文件一直没写");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(engine.pause(id), "暂停要成功（文件还没传完）");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while engine.state(id) != Some(TransferState::Paused) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "没停在暂停状态：{:?}",
            engine.state(id)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let paused_at = endpoints.remote_partial_len("/remote/big.bin");
    assert!(
        paused_at > 0 && (paused_at as usize) < size,
        "暂停时临时文件要有一部分：{paused_at}"
    );

    assert!(engine.resume(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(60)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/big.bin"), contents);
    // 进度只算新传的字节：续传时总数正好是文件大小；从 0 重传会多出暂停前那一段。
    assert_eq!(
        report.done_bytes, size as u64,
        "续传不能从 0 重新传（暂停时已有 {paused_at} 字节）"
    );
}

// ---- C05：开始时目标不存在、传输期间冒出了同名文件 ----

/// 按固定动作回答冲突，并记下问了几次、问的时候目标多大。
struct CountingResolver {
    action: ConflictAction,
    asked: Mutex<Vec<ConflictInfo>>,
}

impl CountingResolver {
    fn new(action: ConflictAction) -> Arc<Self> {
        Arc::new(Self {
            action,
            asked: Mutex::new(Vec::new()),
        })
    }
    fn asked(&self) -> Vec<ConflictInfo> {
        self.asked.lock().unwrap().clone()
    }
}

impl ConflictResolver for CountingResolver {
    fn decide<'a>(&'a self, info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        self.asked.lock().unwrap().push(info);
        let action = self.action;
        Box::pin(async move {
            ConflictDecision {
                action,
                apply_to_all: false,
                rename_to: None,
            }
        })
    }
}

async fn upload_while_a_file_appears(
    tag: &str,
    resolver: Arc<CountingResolver>,
) -> (
    Arc<MemoryEndpoints>,
    ProgressReport,
    crate::test_util::TestDir,
) {
    let root = temp_root(tag);
    let source = root.join("report.bin");
    std::fs::write(&source, vec![7u8; 10_000]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 开始时目标不存在（不会问）；收尾之前别的程序建了同名文件。
    endpoints.appear_on_commit("/remote/report.bin", b"SOMEONE ELSES FILE".to_vec());
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        resolver as Arc<dyn ConflictResolver>,
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/report.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    (endpoints, report, root)
}

/// 没经用户同意不能覆盖：提交时发现目标冒出来了，按冲突重新问；选跳过，那个文件原样保留，
/// 自己的临时文件清掉。
#[tokio::test]
async fn a_target_created_during_the_transfer_is_not_overwritten_silently() {
    let resolver = CountingResolver::new(ConflictAction::Skip);
    let (endpoints, report, _root) =
        upload_while_a_file_appears("c05-skip", Arc::clone(&resolver)).await;
    assert_eq!(report.state, TransferState::Skipped, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/report.bin"),
        b"SOMEONE ELSES FILE",
        "别人建的文件被盖掉了"
    );
    let asked = resolver.asked();
    assert_eq!(asked.len(), 1, "提交时要按冲突问一次：{asked:?}");
    assert_eq!(asked[0].target_size, Some(18));
    assert!(
        endpoints.remote_part_files("/remote/report.bin").is_empty(),
        "跳过之后临时文件要清掉"
    );
}

/// 重新问的时候用户选了覆盖：这才替换，而且接着已经传完的临时文件提交（不从头再传）。
#[tokio::test]
async fn a_target_created_during_the_transfer_is_replaced_only_after_the_user_agrees() {
    let resolver = CountingResolver::new(ConflictAction::Overwrite);
    let (endpoints, report, _root) =
        upload_while_a_file_appears("c05-overwrite", Arc::clone(&resolver)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(resolver.asked().len(), 1);
    assert_eq!(
        endpoints.remote_contents("/remote/report.bin"),
        vec![7u8; 10_000]
    );
    assert!(endpoints.remote_part_files("/remote/report.bin").is_empty());
}

/// 冲突里选了改名，然后暂停、续传：要接着往新名字传，不能回到原目标上把它盖掉
/// （以前续传只记得"问过"，不记得选的是什么）。
#[tokio::test]
async fn pause_after_choosing_rename_keeps_writing_to_the_new_name() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct GatedRename {
        asked: AtomicUsize,
        asked_once: Notify,
        release: Notify,
    }
    impl ConflictResolver for GatedRename {
        fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
            Box::pin(async {
                let count = self.asked.fetch_add(1, AtomicOrdering::SeqCst);
                if count == 0 {
                    self.asked_once.notify_waiters();
                    self.release.notified().await;
                }
                ConflictDecision {
                    action: ConflictAction::Rename,
                    apply_to_all: false,
                    rename_to: Some("new name.bin".to_owned()),
                }
            })
        }
    }

    let root = temp_root("c05-pause-rename");
    let source = root.join("big.bin");
    std::fs::write(&source, vec![3u8; 64 * 1024]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/big.bin", vec![9u8; 1024]);
    let resolver = Arc::new(GatedRename::default());
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        TransferOptions {
            in_flight: 1,
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/big.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(30), resolver.asked_once.notified())
        .await
        .expect("没等到冲突提问");
    assert!(engine.pause(id), "暂停要成功");
    resolver.release.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while engine.state(id) != Some(TransferState::Paused) {
        assert!(tokio::time::Instant::now() < deadline, "没停在暂停状态");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    while events.try_recv().is_ok() {}

    assert!(engine.resume(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        resolver.asked.load(AtomicOrdering::SeqCst),
        1,
        "续传不重新问"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/big.bin"),
        vec![9u8; 1024],
        "原来的文件不能被动"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/new name.bin"),
        vec![3u8; 64 * 1024]
    );
}

/// 可以随时改答案的冲突回答者：记下被问过几次。
struct ScriptedResolver {
    action: Mutex<ConflictAction>,
    asked: std::sync::atomic::AtomicUsize,
}

impl ScriptedResolver {
    fn new(action: ConflictAction) -> Arc<Self> {
        Arc::new(Self {
            action: Mutex::new(action),
            asked: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn answer_with(&self, action: ConflictAction) {
        *self.action.lock().unwrap() = action;
    }

    fn times_asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ConflictResolver for ScriptedResolver {
    fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async move {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ConflictDecision {
                action: *self.action.lock().unwrap(),
                apply_to_all: false,
                rename_to: None,
            }
        })
    }
}

/// 提问时停住、等测试放行的冲突回答者。
#[derive(Default)]
struct GatedResolver {
    asked: tokio::sync::Notify,
    release: tokio::sync::Notify,
    action: Mutex<Option<ConflictAction>>,
}

impl ConflictResolver for GatedResolver {
    fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async move {
            self.asked.notify_waiters();
            self.release.notified().await;
            ConflictDecision {
                action: self.action.lock().unwrap().unwrap_or(ConflictAction::Skip),
                apply_to_all: false,
                rename_to: None,
            }
        })
    }
}

/// SFTP-6：「已跳过」的任务也能重试，重试时重新问一次同名文件怎么处理，这次选覆盖就传过去。
#[tokio::test]
async fn a_skipped_job_can_be_retried_and_asks_about_the_conflict_again() {
    let root = temp_root("retry-skipped");
    let source = root.join("again.txt");
    std::fs::write(&source, b"new contents").unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/again.txt", b"old".to_vec());
    let resolver = ScriptedResolver::new(ConflictAction::Skip);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/again.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Skipped, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/again.txt"), b"old");
    assert_eq!(resolver.times_asked(), 1);

    resolver.answer_with(ConflictAction::Overwrite);
    assert!(engine.retry(id), "已跳过的任务要能重试");
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        resolver.times_asked(),
        2,
        "重试要重新问，不能沿用上次跳过的选择"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/again.txt"),
        b"new contents"
    );
    // 做完的任务照旧不能重试。
    assert!(!engine.retry(id));
}

/// 等用户回答同名文件期间：任务状态是「等待处理同名文件」（不是一直「准备中」），
/// 回答之后回到传输中并正常结束；状态要发事件，界面才看得到。
#[tokio::test]
async fn waiting_for_the_conflict_answer_is_a_visible_state() {
    let root = temp_root("waiting-conflict");
    let source = root.join("w.txt");
    std::fs::write(&source, b"fresh").unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/w.txt", b"stale data".to_vec());
    let resolver = Arc::new(GatedResolver::default());
    *resolver.action.lock().unwrap() = Some(ConflictAction::Overwrite);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        local_options(),
    );
    let asked = resolver.asked.notified();
    tokio::pin!(asked);
    asked.as_mut().enable();
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/w.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(20), asked)
        .await
        .expect("没等到冲突提问");

    // 提问期间：快照和事件里都是「等待处理同名文件」，最后一条事件不能还是「准备中」。
    assert_eq!(engine.state(id), Some(TransferState::WaitingConflict));
    let mut states = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let TransferEvent::Progress(report) = event {
            states.push(report.state);
        }
    }
    assert_eq!(
        states.last(),
        Some(&TransferState::WaitingConflict),
        "最后一条事件要说明在等回答：{states:?}"
    );

    resolver.release.notify_waiters();
    let report = drain_until_finished(&mut events, id, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/w.txt"), b"fresh");
}

/// SFTP-6 排查：一个任务停在「等同名文件的回答」时，目标的写锁一直被它占着（回答最长等
/// 2 分钟）。这时对同一个目标再传一次会立刻失败（另一个任务正在写）——用户看到的
/// 「有时是失败」就是这样来的，根因是前面那条提问没人回答。
#[tokio::test]
async fn a_second_upload_fails_while_the_first_still_waits_for_the_conflict_answer() {
    let root = temp_root("busy-while-asking");
    let source = root.join("busy.txt");
    std::fs::write(&source, b"data").unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/busy.txt", b"old".to_vec());
    let resolver = Arc::new(GatedResolver::default());
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        local_options(),
    );
    let asked = resolver.asked.notified();
    tokio::pin!(asked);
    asked.as_mut().enable();
    let first = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/busy.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(20), asked)
        .await
        .expect("没等到冲突提问");

    let second = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/busy.txt".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, second, Duration::from_secs(20)).await;
    let TransferState::Failed(message) = &report.state else {
        panic!("第二次上传应该因为目标被占用而失败：{report:?}");
    };
    assert!(message.contains("正在写"), "{message}");

    resolver.release.notify_waiters();
    let report = drain_until_finished(&mut events, first, Duration::from_secs(20)).await;
    assert_eq!(report.state, TransferState::Skipped, "{report:?}");
}

// ---------------------------------------------------------------------------
// 冲突批次（SFTP-7）：同一次提交里的任务共享"全部应用"
// ---------------------------------------------------------------------------

type DecideFn = Box<dyn Fn(usize, &ConflictInfo) -> ConflictDecision + Send + Sync>;

/// 记下每次提问、按固定脚本回答的 resolver。
struct Scripted {
    asked: Mutex<Vec<ConflictInfo>>,
    decide: DecideFn,
}

impl Scripted {
    fn new(
        decide: impl Fn(usize, &ConflictInfo) -> ConflictDecision + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            decide: Box::new(decide),
        })
    }

    fn count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl ConflictResolver for Scripted {
    fn decide<'a>(&'a self, info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async move {
            let index = {
                let mut asked = self.asked.lock().unwrap();
                asked.push(info.clone());
                asked.len() - 1
            };
            (self.decide)(index, &info)
        })
    }
}

fn answer(action: ConflictAction, apply_to_all: bool, rename_to: Option<&str>) -> ConflictDecision {
    ConflictDecision {
        action,
        apply_to_all,
        rename_to: rename_to.map(str::to_owned),
    }
}

type BatchFixture = (
    Arc<TransferEngine>,
    mpsc::UnboundedReceiver<TransferEvent>,
    Arc<MemoryEndpoints>,
    Vec<TransferJob>,
    crate::test_util::TestDir,
);

/// 一个串行（并发 1）的引擎；本地有 `count` 个源文件，远端已有同名的旧文件。
fn batch_fixture(tag: &str, count: usize, resolver: Arc<Scripted>) -> BatchFixture {
    let root = temp_root(tag);
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let mut jobs = Vec::new();
    for index in 0..count {
        let source = root.join(format!("f{index}.bin"));
        std::fs::write(&source, vec![b'N'; 2048]).unwrap();
        endpoints.put_remote(&format!("/remote/f{index}.bin"), vec![b'O'; 100]);
        jobs.push(TransferJob {
            direction: Direction::Upload,
            source: source.display().to_string(),
            target: format!("/remote/f{index}.bin"),
            recursive: false,
            peer: None,
        });
    }
    let (engine, events) = TransferEngine::new(
        endpoints.clone(),
        resolver as Arc<dyn ConflictResolver>,
        TransferOptions {
            concurrency: 1,
            ..local_options()
        },
    );
    (engine, events, endpoints, jobs, root)
}

#[tokio::test]
async fn apply_to_all_in_a_batch_covers_the_other_jobs_of_that_batch() {
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Overwrite, true, None));
    let (engine, mut events, endpoints, jobs, _root) =
        batch_fixture("batch-overwrite", 3, Arc::clone(&resolver));
    let batch = engine.new_batch();
    let ids: Vec<_> = jobs
        .into_iter()
        .map(|job| engine.submit_in_batch(job, &batch))
        .collect();
    for id in ids {
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Done, "{report:?}");
    }
    assert_eq!(resolver.count(), 1, "勾了全部应用之后同批次不再问");
    assert_eq!(
        resolver.asked.lock().unwrap()[0].batch,
        Some(batch.id()),
        "提问要带批次号，界面靠它认出同批次的提问"
    );
    for index in 0..3 {
        assert_eq!(
            endpoints.remote_contents(&format!("/remote/f{index}.bin")),
            vec![b'N'; 2048],
            "第 {index} 个文件应该被覆盖"
        );
    }
}

#[tokio::test]
async fn batches_and_unbatched_jobs_do_not_share_apply_to_all() {
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Overwrite, true, None));
    let (engine, mut events, _endpoints, jobs, _root) =
        batch_fixture("batch-isolated", 6, Arc::clone(&resolver));
    let mut jobs = jobs.into_iter();
    let batch_a = engine.new_batch();
    let batch_b = engine.new_batch();
    assert_ne!(batch_a.id(), batch_b.id());
    let mut ids = Vec::new();
    for _ in 0..2 {
        ids.push(engine.submit_in_batch(jobs.next().unwrap(), &batch_a));
    }
    for _ in 0..2 {
        ids.push(engine.submit_in_batch(jobs.next().unwrap(), &batch_b));
    }
    // 不属于任何批次的任务：勾了全部应用也只管它自己，所以各问各的。
    for _ in 0..2 {
        ids.push(engine.submit(jobs.next().unwrap()));
    }
    for id in ids {
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Done, "{report:?}");
    }
    // A 批问 1 次、B 批问 1 次、两个单独任务各问 1 次。
    assert_eq!(resolver.count(), 4);
    let asked = resolver.asked.lock().unwrap();
    assert_eq!(asked.iter().filter(|info| info.batch.is_none()).count(), 2);
    assert!(asked.iter().any(|info| info.batch == Some(batch_a.id())));
    assert!(asked.iter().any(|info| info.batch == Some(batch_b.id())));
}

#[tokio::test]
async fn rename_for_the_whole_batch_names_only_the_first_file_by_hand() {
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, true, Some("mine.bin")));
    let (engine, mut events, endpoints, jobs, _root) =
        batch_fixture("batch-rename", 3, Arc::clone(&resolver));
    let batch = engine.new_batch();
    let ids: Vec<_> = jobs
        .into_iter()
        .map(|job| engine.submit_in_batch(job, &batch))
        .collect();
    for id in ids {
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Done, "{report:?}");
    }
    assert_eq!(resolver.count(), 1);
    assert!(
        endpoints.remote_exists("/remote/mine.bin"),
        "用户给的名字只用在被问的那个文件上"
    );
    let auto_named = (0..3)
        .filter(|index| endpoints.remote_exists(&format!("/remote/f{index} (2).bin")))
        .count();
    assert_eq!(auto_named, 2, "其余文件各自用自动名字，不能都叫 mine.bin");
    for index in 0..3 {
        assert_eq!(
            endpoints.remote_contents(&format!("/remote/f{index}.bin")),
            vec![b'O'; 100],
            "改名时原文件不动"
        );
    }
}

#[tokio::test]
async fn a_retried_job_asks_again_and_leaves_the_batch() {
    // 第一次回答「跳过 + 全部应用」，重试那次回答「覆盖 + 全部应用」。
    let resolver = Scripted::new(|index, _| {
        if index == 0 {
            answer(ConflictAction::Skip, true, None)
        } else {
            answer(ConflictAction::Overwrite, true, None)
        }
    });
    let (engine, mut events, endpoints, jobs, _root) =
        batch_fixture("batch-retry", 3, Arc::clone(&resolver));
    let batch = engine.new_batch();
    let ids: Vec<_> = jobs
        .into_iter()
        .map(|job| engine.submit_in_batch(job, &batch))
        .collect();
    for id in &ids {
        let report = drain_until_finished(&mut events, *id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Skipped, "{report:?}");
    }
    assert_eq!(resolver.count(), 1, "批次里的跳过替另外两个回答了");

    // 重试第二个：批次里旧的「跳过」不能再替它回答，要重新问。
    assert!(engine.retry(ids[1]));
    let report = drain_until_finished(&mut events, ids[1], Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(resolver.count(), 2, "重试的任务重新问了一次");
    assert_eq!(
        resolver.asked.lock().unwrap()[1].batch,
        None,
        "重试后脱离批次"
    );
    assert_eq!(
        endpoints.remote_contents("/remote/f1.bin"),
        vec![b'N'; 2048]
    );
    // 它勾的全部应用只管它自己：第三个任务重试时还是要自己问。
    assert!(engine.retry(ids[2]));
    let report = drain_until_finished(&mut events, ids[2], Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(resolver.count(), 3);
}

/// 复制粘贴的任务在批次里选了"重命名"：其余文件的自动名字是「名字 - 副本」，不是「名字 (2)」。
#[tokio::test]
async fn copy_jobs_in_a_batch_get_copy_style_names_when_renaming_for_all() {
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, true, None));
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    for index in 0..2 {
        endpoints.put_remote(&format!("/src/c{index}.txt"), vec![b'S'; 300]);
        endpoints.put_remote(&format!("/dst/c{index}.txt"), vec![b'D'; 50]);
    }
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
        TransferOptions {
            concurrency: 1,
            ..local_options()
        },
    );
    let batch = engine.new_batch();
    let ids: Vec<_> = (0..2)
        .map(|index| {
            engine.submit_in_batch(
                TransferJob {
                    direction: Direction::RemoteCopy,
                    source: format!("/src/c{index}.txt"),
                    target: format!("/dst/c{index}.txt"),
                    recursive: false,
                    peer: None,
                },
                &batch,
            )
        })
        .collect();
    for id in ids {
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Done, "{report:?}");
    }
    assert_eq!(resolver.count(), 1);
    for index in 0..2 {
        assert_eq!(
            endpoints.remote_contents(&format!("/dst/c{index} - 副本.txt")),
            vec![b'S'; 300]
        );
        assert_eq!(
            endpoints.remote_contents(&format!("/dst/c{index}.txt")),
            vec![b'D'; 50]
        );
    }
}

// ---------------------------------------------------------------------------
// 自动改名往后找没被占用的名字（SFTP-8）
// ---------------------------------------------------------------------------

/// 一个上传任务：本地新建 `name`（内容全是 `fill`），目标是远端同名文件。
fn upload_job(root: &crate::test_util::TestDir, name: &str, fill: u8) -> TransferJob {
    let source = root.join(name);
    std::fs::write(&source, vec![fill; 512]).unwrap();
    TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: format!("/remote/{name}"),
        recursive: false,
        peer: None,
    }
}

fn engine_with(
    endpoints: &Arc<MemoryEndpoints>,
    resolver: Arc<Scripted>,
    concurrency: usize,
) -> (Arc<TransferEngine>, mpsc::UnboundedReceiver<TransferEvent>) {
    TransferEngine::new(
        endpoints.clone(),
        resolver as Arc<dyn ConflictResolver>,
        TransferOptions {
            concurrency,
            ..local_options()
        },
    )
}

async fn run_one(
    engine: &Arc<TransferEngine>,
    events: &mut mpsc::UnboundedReceiver<TransferEvent>,
    job: TransferJob,
) -> ProgressReport {
    let id = engine.submit(job);
    drain_until_finished(events, id, Duration::from_secs(30)).await
}

#[tokio::test]
async fn auto_rename_skips_names_that_are_already_taken() {
    let root = temp_root("auto-rename-taken");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    for name in ["f.bin", "f (2).bin", "f (3).bin"] {
        endpoints.put_remote(&format!("/remote/{name}"), vec![b'O'; 100]);
    }
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, None));
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "f.bin", b'N')).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/f (4).bin"),
        vec![b'N'; 512]
    );
    for name in ["f.bin", "f (2).bin", "f (3).bin"] {
        assert_eq!(
            endpoints.remote_contents(&format!("/remote/{name}")),
            vec![b'O'; 100],
            "{name} 原来的内容不能动"
        );
    }
}

#[tokio::test]
async fn copy_jobs_skip_taken_copy_names() {
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/src/c.txt", vec![b'S'; 300]);
    for name in ["c.txt", "c - 副本.txt", "c - 副本 2.txt"] {
        endpoints.put_remote(&format!("/dst/{name}"), vec![b'D'; 50]);
    }
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, None));
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(
        &engine,
        &mut events,
        TransferJob {
            direction: Direction::RemoteCopy,
            source: "/src/c.txt".to_owned(),
            target: "/dst/c.txt".to_owned(),
            recursive: false,
            peer: None,
        },
    )
    .await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/dst/c - 副本 3.txt"),
        vec![b'S'; 300]
    );
}

#[tokio::test]
async fn downloads_skip_taken_local_names() {
    let root = temp_root("auto-rename-local");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/d.bin", vec![b'R'; 400]);
    let target = root.join("d.bin");
    std::fs::write(&target, b"old").unwrap();
    std::fs::write(root.join("d (2).bin"), b"old2").unwrap();
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, None));
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(
        &engine,
        &mut events,
        TransferJob {
            direction: Direction::Download,
            source: "/remote/d.bin".to_owned(),
            target: target.display().to_string(),
            recursive: false,
            peer: None,
        },
    )
    .await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        std::fs::read(root.join("d (3).bin")).unwrap(),
        vec![b'R'; 400]
    );
    assert_eq!(std::fs::read(root.join("d (2).bin")).unwrap(), b"old2");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

#[tokio::test]
async fn a_hand_typed_name_that_is_taken_fails_with_a_clear_message() {
    let root = temp_root("manual-name-taken");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/h.bin", vec![b'O'; 100]);
    endpoints.put_remote("/remote/mine.bin", vec![b'M'; 100]);
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, Some("mine.bin")));
    let (engine, mut events) = engine_with(&endpoints, Arc::clone(&resolver), 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "h.bin", b'N')).await;
    match &report.state {
        TransferState::Failed(text) => assert!(
            text.contains("新名字 mine.bin 也已存在"),
            "错误信息要点明是新名字被占：{text}"
        ),
        other => panic!("手动输入的名字被占用应该失败，实际 {other:?}"),
    }
    assert_eq!(resolver.count(), 1);
    assert_eq!(
        endpoints.remote_contents("/remote/mine.bin"),
        vec![b'M'; 100]
    );
    assert_eq!(endpoints.remote_contents("/remote/h.bin"), vec![b'O'; 100]);
    assert!(
        !endpoints.remote_exists("/remote/h (2).bin"),
        "不能悄悄换成自动名字"
    );
}

#[tokio::test]
async fn a_hand_typed_name_equal_to_the_conflicting_target_is_rejected() {
    let root = temp_root("manual-name-same");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/s.bin", vec![b'O'; 100]);
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, Some("s.bin")));
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "s.bin", b'N')).await;
    assert!(
        matches!(&report.state, TransferState::Failed(text) if text.contains("新名字 s.bin 也已存在")),
        "{report:?}"
    );
    assert_eq!(endpoints.remote_contents("/remote/s.bin"), vec![b'O'; 100]);
}

/// 对话框预填的建议名没被用户改过、而它在回答之前被别人占了：不算手动名字，接着往后找空名字。
#[tokio::test]
async fn an_untouched_suggested_name_taken_meanwhile_falls_through_to_the_next_free_one() {
    let root = temp_root("suggested-taken-meanwhile");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/g.bin", vec![b'O'; 100]);
    let taken_later = Arc::clone(&endpoints);
    let resolver = Scripted::new(move |_, info| {
        assert_eq!(info.suggested_target, "/remote/g (2).bin");
        // 提问到回答之间，建议名被别的任务 / 程序占了。
        taken_later.put_remote("/remote/g (2).bin", vec![b'T'; 100]);
        answer(ConflictAction::Rename, false, Some("g (2).bin"))
    });
    let (engine, mut events) = engine_with(&endpoints, Arc::clone(&resolver), 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "g.bin", b'N')).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/g (3).bin"),
        vec![b'N'; 512]
    );
    assert_eq!(
        endpoints.remote_contents("/remote/g (2).bin"),
        vec![b'T'; 100],
        "后来占上的那个文件不能动"
    );
    assert_eq!(endpoints.remote_contents("/remote/g.bin"), vec![b'O'; 100]);
}

/// 用户改过名字、而改后的名字在回答之前被占了：仍是手动名，按「新名字也已存在」失败（SFTP-8）。
#[tokio::test]
async fn an_edited_name_taken_meanwhile_still_fails_as_a_hand_typed_one() {
    let root = temp_root("edited-taken-meanwhile");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/e.bin", vec![b'O'; 100]);
    let taken_later = Arc::clone(&endpoints);
    let resolver = Scripted::new(move |_, _| {
        taken_later.put_remote("/remote/mine.bin", vec![b'T'; 100]);
        answer(ConflictAction::Rename, false, Some("mine.bin"))
    });
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "e.bin", b'N')).await;
    assert!(
        matches!(&report.state, TransferState::Failed(text) if text.contains("新名字 mine.bin 也已存在")),
        "{report:?}"
    );
    assert!(
        !endpoints.remote_exists("/remote/e (2).bin"),
        "不能悄悄换成自动名字"
    );
}

#[test]
fn only_a_name_equal_to_the_suggestion_is_dropped() {
    let decide = |typed: Option<&str>| answer(ConflictAction::Rename, false, typed);
    let suggested = "/remote/sub/a (2).bin";
    assert_eq!(
        decide(Some("a (2).bin")).without_suggested_name(suggested),
        decide(None)
    );
    assert_eq!(
        decide(Some(" a (2).bin ")).without_suggested_name(suggested),
        decide(None)
    );
    assert_eq!(
        decide(Some("a (3).bin")).without_suggested_name(suggested),
        decide(Some("a (3).bin"))
    );
    assert_eq!(decide(None).without_suggested_name(suggested), decide(None));
    // Windows 路径的建议名同样只比文件名
    assert_eq!(
        decide(Some("d (2).bin")).without_suggested_name(r"C:\dl\d (2).bin"),
        decide(None)
    );
}

#[tokio::test]
async fn a_free_hand_typed_name_is_used_as_is() {
    let root = temp_root("manual-name-free");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/k.bin", vec![b'O'; 100]);
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, Some("fresh.bin")));
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "k.bin", b'N')).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(
        endpoints.remote_contents("/remote/fresh.bin"),
        vec![b'N'; 512]
    );
}

#[tokio::test]
async fn the_suggested_name_in_the_prompt_skips_taken_names() {
    let root = temp_root("suggest-free");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    for name in ["a.bin", "a (2).bin"] {
        endpoints.put_remote(&format!("/remote/{name}"), vec![b'O'; 100]);
    }
    endpoints.put_remote("/src/c.txt", vec![b'S'; 300]);
    for name in ["c.txt", "c - 副本.txt"] {
        endpoints.put_remote(&format!("/dst/{name}"), vec![b'D'; 50]);
    }
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Skip, false, None));
    let (engine, mut events) = engine_with(&endpoints, Arc::clone(&resolver), 1);
    run_one(&engine, &mut events, upload_job(&root, "a.bin", b'N')).await;
    run_one(
        &engine,
        &mut events,
        TransferJob {
            direction: Direction::RemoteCopy,
            source: "/src/c.txt".to_owned(),
            target: "/dst/c.txt".to_owned(),
            recursive: false,
            peer: None,
        },
    )
    .await;
    let asked = resolver.asked.lock().unwrap();
    assert_eq!(asked[0].suggested_target, "/remote/a (3).bin");
    assert_eq!(asked[1].suggested_target, "/dst/c - 副本 2.txt");
}

#[tokio::test]
async fn running_out_of_names_fails_clearly_and_the_prompt_falls_back() {
    let root = temp_root("rename-exhausted");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/z.bin", vec![b'O'; 10]);
    // 从 (2) 起 1000 个名字全占上。
    for index in 2..=1001 {
        endpoints.put_remote(&format!("/remote/z ({index}).bin"), vec![b'O'; 10]);
    }
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, None));
    let (engine, mut events) = engine_with(&endpoints, Arc::clone(&resolver), 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "z.bin", b'N')).await;
    match &report.state {
        TransferState::Failed(text) => assert!(
            text.contains("自动改名") && text.contains("都已被占用"),
            "错误信息要说清是名字用光了：{text}"
        ),
        other => panic!("名字用光应该失败，实际 {other:?}"),
    }
    assert!(
        !endpoints.remote_exists("/remote/z (1002).bin"),
        "不能超过上限继续试"
    );
    assert_eq!(
        resolver.asked.lock().unwrap()[0].suggested_target,
        "/remote/z (2).bin",
        "找不到空名字时建议名退回固定的第一个"
    );
}

#[tokio::test]
async fn auto_rename_leaves_alone_a_name_another_task_is_about_to_write() {
    // 另一个任务已经占了「f (2).bin」这个名字的目标锁，但文件还没写出来：
    // 只看「现在有没有这个文件」会误选它，上锁之后就能避开。
    let root = temp_root("rename-locked-name");
    let identity = "tester@rename-lock:22";
    let endpoints = Arc::new(MemoryEndpoints::new("/remote").with_identity(identity));
    endpoints.put_remote("/remote/f.bin", vec![b'O'; 100]);
    let held = lock::TargetLocks::remote()
        .try_acquire_remote(identity, "/remote/f (2).bin")
        .unwrap();
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, None));
    let (engine, mut events) = engine_with(&endpoints, resolver, 1);
    let report = run_one(&engine, &mut events, upload_job(&root, "f.bin", b'N')).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert!(!endpoints.remote_exists("/remote/f (2).bin"));
    assert_eq!(
        endpoints.remote_contents("/remote/f (3).bin"),
        vec![b'N'; 512]
    );
    drop(held);
}

#[tokio::test]
async fn concurrent_auto_renames_never_end_up_on_the_same_name() {
    // 四个任务（各自的源内容不同）同时往同一个已存在的目标上传、都选自动改名。
    // 目标锁是 try 式的：撞上原目标锁的任务会直接报「另一个任务正在写」，这是已有行为；
    // 没撞上的必须各拿一个不同的新名字，谁也不能盖掉谁、不能盖掉原文件。
    let root = temp_root("rename-concurrent");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/p.bin", vec![b'O'; 100]);
    let resolver = Scripted::new(|_, _| answer(ConflictAction::Rename, false, None));
    let (engine, mut events) = engine_with(&endpoints, resolver, 4);
    let mut ids = Vec::new();
    for (index, fill) in b"1234".iter().copied().enumerate() {
        let dir = root.join(format!("src{index}"));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("p.bin");
        std::fs::write(&source, vec![fill; 512]).unwrap();
        ids.push((
            fill,
            engine.submit(TransferJob {
                direction: Direction::Upload,
                source: source.display().to_string(),
                target: "/remote/p.bin".to_owned(),
                recursive: false,
                peer: None,
            }),
        ));
    }
    // 任务的结束事件是交错到达的：一边收一边按任务号归位，不能按提交顺序一个个等。
    let mut finals: HashMap<TransferId, TransferState> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while finals.len() < ids.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(TransferEvent::Finished(report))) => {
                finals.insert(report.id, report.state);
            }
            Ok(Some(_)) => {}
            other => panic!("等不到全部任务的终态：{other:?}"),
        }
    }
    let mut done = Vec::new();
    for (fill, id) in ids {
        match &finals[&id] {
            TransferState::Done => done.push(fill),
            TransferState::Failed(text) => assert!(
                text.contains("另一个传输任务正在写"),
                "只允许因为目标锁被占而失败：{text}"
            ),
            other => panic!("意外的终态 {other:?}"),
        }
    }
    assert!(!done.is_empty(), "至少要有一个任务成功");
    assert_eq!(endpoints.remote_contents("/remote/p.bin"), vec![b'O'; 100]);
    // 每个成功的任务都在自己独有的名字上，内容是自己的源。
    let mut landed: Vec<u8> = Vec::new();
    for index in 2..=5 {
        let path = format!("/remote/p ({index}).bin");
        if endpoints.remote_exists(&path) {
            let contents = endpoints.remote_contents(&path);
            assert_eq!(contents.len(), 512);
            landed.push(contents[0]);
        }
    }
    landed.sort_unstable();
    done.sort_unstable();
    assert_eq!(landed, done, "成功的任务和落地的文件一一对应");
}

// ---- C7b：断点续传不能接在一个带洞的临时文件后面 ----

/// 全是非零字节的样本：空洞（零）一眼就看出来。
fn nonzero_pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 + 1).collect()
}

/// 上传 `data`，第二批里的第二块写失败而同批其他块已经落盘，且失败后连接已断
/// （截断 / 删除临时文件都做不到）。返回引擎、事件、任务号、远端、源文件。
async fn upload_that_leaves_a_hole(
    tag: &str,
    data: &[u8],
) -> (
    Arc<TransferEngine>,
    mpsc::UnboundedReceiver<TransferEvent>,
    TransferId,
    Arc<MemoryEndpoints>,
    crate::test_util::TestDir,
) {
    let root = temp_root(tag);
    let source = root.join("hole.bin");
    std::fs::write(&source, data).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 块 4096、一批 4 块：第二批是 16384..32768，写 20480 那块失败，其余 3 块照常落盘。
    endpoints.fail_remote_write_at_offset(20_480);
    endpoints.set_remote_cleanup_fails(true);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(OverwriteAll),
        TransferOptions {
            max_retries: 0,
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/hole.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert!(
        matches!(report.state, TransferState::Failed(_)),
        "{report:?}"
    );
    (engine, events, id, endpoints, root)
}

/// 乱序写完一部分后断线、截断也失败：临时文件比连续前缀长，中间有洞。
/// 重试必须把洞补上，最终文件和源逐字节一致（以前只比对末尾 4096 字节，洞发现不了）。
#[tokio::test]
async fn resume_after_out_of_order_writes_and_failed_truncate_has_no_hole() {
    let data = nonzero_pattern(10 * 4096);
    let (engine, mut events, id, endpoints, _root) =
        upload_that_leaves_a_hole("hole-resume", &data).await;

    // 前提成立：临时文件长度 32768 > 已确认前缀 16384，20480..24576 是零。
    assert_eq!(endpoints.remote_partial_len("/remote/hole.bin"), 8 * 4096);
    let part = endpoints.remote_part_files("/remote/hole.bin");
    assert_eq!(part.len(), 1, "{part:?}");
    let partial = endpoints.remote_contents(&part[0]);
    assert!(partial[20_480..24_576].iter().all(|byte| *byte == 0));
    assert!(
        partial[24_576..28_672] == data[24_576..28_672],
        "末块是好的：只比末尾的对账发现不了洞"
    );
    let state = engine.job(id).unwrap();
    assert_eq!(state.confirmed_prefix("/remote/hole.bin"), Some(16_384));

    // 重连之后重试：清理这次能做了，先截回 16384 再续。
    endpoints.set_remote_cleanup_fails(false);
    assert!(engine.retry(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/hole.bin"), data);
}

/// 重试时清理仍然做不到（连接还没恢复）：截不动就从头传，同样不能留洞。
#[tokio::test]
async fn resume_with_failing_truncate_restarts_from_scratch() {
    let data = nonzero_pattern(10 * 4096);
    let (engine, mut events, id, endpoints, _root) =
        upload_that_leaves_a_hole("hole-restart", &data).await;

    // 清理依旧失败：续传点（16384）之后的洞截不掉。
    assert!(engine.retry(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert!(
        report.notes.iter().any(|note| note.contains("从头传")),
        "要告诉用户为什么从头传：{:?}",
        report.notes
    );
    assert_eq!(endpoints.remote_contents("/remote/hole.bin"), data);
}

/// 没有已确认前缀的记录（老任务 / 记录丢了）：不信临时文件的长度，从头传。
#[tokio::test]
async fn resume_without_a_confirmed_prefix_record_restarts_from_scratch() {
    let data = nonzero_pattern(10 * 4096);
    let (engine, mut events, id, endpoints, _root) =
        upload_that_leaves_a_hole("hole-norecord", &data).await;

    endpoints.set_remote_cleanup_fails(false);
    engine.job(id).unwrap().confirmed.lock().unwrap().clear();
    assert!(engine.retry(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("没有已确认的续传位置")),
        "{:?}",
        report.notes
    );
    assert_eq!(endpoints.remote_contents("/remote/hole.bin"), data);
}

#[tokio::test]
async fn paused_wait_cannot_lose_resume_or_cancel_before_await() {
    for cancel in [false, true] {
        let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
        let (engine, _events) =
            TransferEngine::new(endpoints, Arc::new(OverwriteAll), local_options());
        let state = Arc::new(JobState {
            id: 1,
            job: TransferJob {
                direction: Direction::RemoteCopy,
                source: "/remote/source".to_owned(),
                target: "/remote/target".to_owned(),
                recursive: false,
                peer: None,
            },
            control: ControlFlag::new(),
            batch: Mutex::new(None),
            resume_signal: Notify::new(),
            progress: Mutex::new(JobProgress::new()),
            state: Mutex::new(TransferState::Paused),
            run_lock: tokio::sync::Mutex::new(()),
            confirmed: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
        });
        engine
            .jobs
            .lock()
            .unwrap()
            .insert(state.id, Arc::clone(&state));
        state.control.pause();
        let mut observed_paused = 0;
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            state.wait_while_paused(|| {
                // 正好在读到 Paused 之后、首次 await 之前广播；不依赖调度运气。
                observed_paused += 1;
                if cancel {
                    assert!(engine.cancel(state.id));
                } else {
                    assert!(engine.resume(state.id));
                }
            }),
        )
        .await
        .expect("resume / cancel 广播不能丢失");
        assert_eq!(observed_paused, 1);
        assert_eq!(
            outcome,
            if cancel {
                file::ControlState::Cancelled
            } else {
                file::ControlState::Running
            }
        );
    }
}

#[tokio::test]
async fn cancel_during_finish_does_not_commit_or_replace_target() {
    for total in [0, 8] {
        let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
        endpoints.put_remote("/remote/source", vec![7; total]);
        endpoints.put_remote("/remote/target", b"original".to_vec());
        let gate = endpoints.remote.block_next_finish();
        let (engine, mut events) =
            TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
        let id = engine.submit(TransferJob {
            direction: Direction::RemoteCopy,
            source: "/remote/source".to_owned(),
            target: "/remote/target".to_owned(),
            recursive: false,
            peer: None,
        });
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("必须进入 finish 后再取消");
        assert_eq!(endpoints.remote_part_files("/remote/target").len(), 1);
        assert!(engine.cancel(id));
        gate.release.notify_one();
        let report = drain_until_finished(&mut events, id, Duration::from_secs(2)).await;
        assert_eq!(report.state, TransferState::Cancelled);
        assert_eq!(report.done_files, 0);
        assert_eq!(endpoints.remote_contents("/remote/target"), b"original");
        assert!(endpoints.remote_part_files("/remote/target").is_empty());
    }
}

#[test]
fn resume_point_is_the_smaller_of_actual_length_and_confirmed_prefix() {
    assert_eq!(
        resume_point(32_768, Some(16_384)),
        16_384,
        "长出来的部分不要"
    );
    assert_eq!(resume_point(8_192, Some(16_384)), 8_192, "临时文件被截短了");
    assert_eq!(resume_point(32_768, None), 0, "没有记录：不可续传");
    assert_eq!(resume_point(0, Some(4_096)), 0);
}

/// 下载到 Unix 本地的新文件权限不比远端宽（真实磁盘上的文件）。
#[cfg(unix)]
mod download_permissions {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::sftp::local::{download_mode, process_umask};

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// 下载 `/remote/<name>`（权限 `remote_mode`）到临时目录，返回目录。
    async fn download(
        tag: &str,
        remote_mode: u32,
        preserve_permissions: bool,
        prepare: impl FnOnce(&std::path::Path),
        action: ConflictAction,
    ) -> (crate::test_util::TestDir, std::path::PathBuf) {
        let root = temp_root(tag);
        let local_dir = root.join("out");
        std::fs::create_dir_all(&local_dir).unwrap();
        prepare(&local_dir);
        let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
        endpoints.put_remote("/remote/key.pem", vec![9u8; 5000]);
        endpoints.set_remote_permissions("/remote/key.pem", remote_mode);
        let options = TransferOptions {
            preserve_permissions,
            ..local_options()
        };
        let resolver = ScriptedResolver::new(action);
        let (engine, mut events) = TransferEngine::new(endpoints.clone(), resolver, options);
        let id = engine.submit(TransferJob {
            direction: Direction::Download,
            source: "/remote/key.pem".to_owned(),
            target: local_dir.join("key.pem").display().to_string(),
            recursive: false,
            peer: None,
        });
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Done, "{report:?}");
        (root, local_dir)
    }

    #[tokio::test]
    async fn a_private_remote_file_stays_private_locally() {
        // 不管用户开没开「保留权限」，0600 的私钥都不能变成 0664。
        for preserve in [false, true] {
            let (_root, dir) = download(
                "perm-private",
                0o100600,
                preserve,
                |_| {},
                ConflictAction::Overwrite,
            )
            .await;
            assert_eq!(mode_of(&dir.join("key.pem")), 0o600, "preserve={preserve}");
            assert_eq!(std::fs::read(dir.join("key.pem")).unwrap(), vec![9u8; 5000]);
            // 没有留下临时文件。
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn other_modes_follow_the_remote_bits_and_the_umask() {
        for remote in [0o100644, 0o100664, 0o100755, 0o100400, 0o100777] {
            let (_root, dir) = download(
                "perm-follow",
                remote,
                false,
                |_| {},
                ConflictAction::Overwrite,
            )
            .await;
            let expected = download_mode(Some(remote), process_umask());
            assert_eq!(mode_of(&dir.join("key.pem")), expected, "{remote:o}");
            // 永远不比远端的权限位多出任何一位（执行位是远端有的才会有）。
            assert_eq!(expected & !(remote & 0o777), 0, "{remote:o}");
        }
    }

    #[tokio::test]
    async fn overwriting_an_existing_wide_file_gives_the_new_file_the_narrow_mode() {
        let (_root, dir) = download(
            "perm-overwrite",
            0o100600,
            false,
            |dir| {
                let old = dir.join("key.pem");
                std::fs::write(&old, b"old").unwrap();
                std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o666)).unwrap();
            },
            ConflictAction::Overwrite,
        )
        .await;
        assert_eq!(mode_of(&dir.join("key.pem")), 0o600);
        assert_eq!(std::fs::read(dir.join("key.pem")).unwrap().len(), 5000);
    }

    #[tokio::test]
    async fn a_renamed_download_is_also_narrow() {
        let (_root, dir) = download(
            "perm-rename",
            0o100600,
            false,
            |dir| std::fs::write(dir.join("key.pem"), b"mine").unwrap(),
            ConflictAction::Rename,
        )
        .await;
        assert_eq!(std::fs::read(dir.join("key.pem")).unwrap(), b"mine");
        assert_eq!(mode_of(&dir.join("key (2).pem")), 0o600);
    }

    /// 本地复制一个文件，源文件先设成 `source_mode`，返回复制出来的文件。
    async fn local_copy_of(
        tag: &str,
        source_mode: u32,
    ) -> (crate::test_util::TestDir, std::path::PathBuf) {
        let root = temp_root(tag);
        let from = root.join("from.sh");
        std::fs::write(&from, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&from, std::fs::Permissions::from_mode(source_mode)).unwrap();
        let to = root.join("to.sh");
        let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
        let (engine, mut events) = TransferEngine::new(
            endpoints,
            ScriptedResolver::new(ConflictAction::Overwrite),
            local_options(),
        );
        let id = engine.submit(TransferJob {
            direction: Direction::LocalCopy,
            source: from.display().to_string(),
            target: to.display().to_string(),
            recursive: false,
            peer: None,
        });
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        assert_eq!(report.state, TransferState::Done, "{report:?}");
        (root, to)
    }

    /// 本地复制和下载走同一套规则：执行位保留，不比源宽、不比 umask 宽（以前一律是默认的 0644）。
    #[tokio::test]
    async fn a_local_copy_keeps_the_source_permissions() {
        for source in [0o755, 0o700, 0o600, 0o644, 0o4755] {
            let (_root, to) = local_copy_of("perm-localcopy", source).await;
            assert_eq!(
                mode_of(&to),
                download_mode(Some(source), process_umask()),
                "{source:o}"
            );
        }
        // 与 umask 无关的几个确定值（测试进程的 umask 一般是 022）：脚本仍然可以运行，私密文件仍然私密。
        let umask = process_umask();
        let (_root, exec) = local_copy_of("perm-localcopy-exec", 0o755).await;
        assert_eq!(mode_of(&exec) & 0o100, 0o100, "属主执行位必须保住");
        assert_eq!(mode_of(&exec), 0o755 & !umask);
        let (_root, private) = local_copy_of("perm-localcopy-private", 0o600).await;
        assert_eq!(mode_of(&private), 0o600);
    }

    /// 临时文件名可预测：别人预先放一个同名软链接，下载不能写到链接指向的文件里。
    #[tokio::test]
    async fn a_symlinked_partial_file_is_replaced_not_followed() {
        let root = temp_root("perm-part-symlink");
        let victim = root.join("victim.txt");
        std::fs::write(&victim, b"keep me").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o640)).unwrap();
        let target = root.join("key.pem");
        let endpoint = LocalEndpoint::with_owner(&target, "t4");
        let part = endpoint.part_path();
        std::os::unix::fs::symlink(&victim, &part).unwrap();

        let writer = endpoint.open_write(0).await.unwrap();
        writer.write_at(0, b"remote data".to_vec()).await.unwrap();
        writer.finish().await.unwrap();

        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"keep me",
            "链接目标不能被写"
        );
        assert_eq!(mode_of(&victim), 0o640);
        let meta = std::fs::symlink_metadata(&part).unwrap();
        assert!(meta.file_type().is_file(), "临时文件应是新建的普通文件");
        assert_eq!(std::fs::read(&part).unwrap(), b"remote data");
        assert_eq!(mode_of(&part), 0o600);
    }

    /// 续传不能打开软链接（也就不会往里续写）。
    #[tokio::test]
    async fn resuming_into_a_symlinked_partial_file_is_refused() {
        let root = temp_root("perm-part-symlink-resume");
        let victim = root.join("victim.txt");
        std::fs::write(&victim, b"keep me").unwrap();
        let endpoint = LocalEndpoint::with_owner(root.join("key.pem"), "t5");
        std::os::unix::fs::symlink(&victim, endpoint.part_path()).unwrap();

        assert!(endpoint.open_write(4).await.is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
    }

    /// 收尾改权限用的是句柄：临时文件被换成软链接时不能去改链接目标的权限。
    #[tokio::test]
    async fn preparing_the_commit_never_chmods_a_symlink_target() {
        let root = temp_root("perm-part-symlink-chmod");
        let victim = root.join("victim.txt");
        std::fs::write(&victim, b"keep me").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o664)).unwrap();
        let endpoint = LocalEndpoint::with_owner(root.join("key.pem"), "t6");
        std::os::unix::fs::symlink(&victim, endpoint.part_path()).unwrap();

        assert!(endpoint.prepare_commit(Some(0o100600)).await.is_err());
        assert_eq!(mode_of(&victim), 0o664);
    }

    /// 续传：上一次（旧版本）留下的临时文件是 0664，收尾时也要收窄。
    #[tokio::test]
    async fn a_resumed_partial_file_is_narrowed_before_it_is_committed() {
        let root = temp_root("perm-resume");
        let target = root.join("key.pem");
        let endpoint = LocalEndpoint::with_owner(&target, "t1");
        let part = root.join(format!("key.pem.t1{}", endpoint::PART_SUFFIX));
        std::fs::write(&part, b"partial").unwrap();
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o664)).unwrap();
        endpoint.prepare_commit(Some(0o100600)).await.unwrap();
        assert_eq!(mode_of(&part), 0o600);
        endpoint.commit(7, false).await.unwrap();
        assert_eq!(mode_of(&target), 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"partial");
    }

    /// 传输中的临时文件只有自己能访问。
    #[tokio::test]
    async fn the_partial_file_is_private_while_it_is_being_written() {
        let root = temp_root("perm-part");
        let target = root.join("key.pem");
        let endpoint = LocalEndpoint::with_owner(&target, "t2");
        let _writer = endpoint.open_write(0).await.unwrap();
        let part = root.join(format!("key.pem.t2{}", endpoint::PART_SUFFIX));
        assert_eq!(mode_of(&part), 0o600);
    }

    /// 远端没给权限信息：用默认权限（`0666 & !umask`）。
    #[tokio::test]
    async fn unknown_remote_permissions_fall_back_to_the_default() {
        let root = temp_root("perm-unknown");
        let target = root.join("a.bin");
        let endpoint = LocalEndpoint::with_owner(&target, "t3");
        drop(endpoint.open_write(0).await.unwrap());
        endpoint.prepare_commit(None).await.unwrap();
        endpoint.commit(0, false).await.unwrap();
        assert_eq!(mode_of(&target), download_mode(None, process_umask()));
    }
}

/// W05：任务号 / 批次号在进程内唯一。界面在断线重建引擎后还留着旧引擎的行，新引擎若又从 1 发号，
/// 进度会落到旧行上，旧行的暂停 / 取消 / 重试也会命中新任务。
#[tokio::test]
async fn ids_are_unique_across_engines_so_rebuilt_engines_never_collide_with_old_rows() {
    let root = temp_root("unique-ids");
    let source = root.join("a.bin");
    std::fs::write(&source, vec![1u8; 1024]).unwrap();
    let make = || {
        TransferEngine::new(
            Arc::new(MemoryEndpoints::new("/remote")),
            Arc::new(OverwriteAll),
            local_options(),
        )
    };
    let (first, mut first_events) = make();
    let (second, mut second_events) = make();
    let job = |target: &str| TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: target.to_owned(),
        recursive: false,
        peer: None,
    };
    let a = first.submit(job("/remote/a.bin"));
    let b = second.submit(job("/remote/b.bin"));
    assert_ne!(a, b, "两个引擎发出的任务号不能相同");
    assert_ne!(first.new_batch().id(), second.new_batch().id());
    // 同一个引擎里连续的任务号也各不相同。
    assert_ne!(first.submit(job("/remote/c.bin")), a);

    // 一个引擎不认另一个引擎的任务号：旧行的操作不会命中新任务。
    assert_eq!(first.state(b), None);
    assert_eq!(second.state(a), None);
    assert!(!second.cancel(a));
    assert!(!first.retry(b));

    // 各自的进度事件只带自己的号。
    let report_a = drain_until_finished(&mut first_events, a, Duration::from_secs(30)).await;
    let report_b = drain_until_finished(&mut second_events, b, Duration::from_secs(30)).await;
    assert_eq!((report_a.id, report_b.id), (a, b));
    assert_eq!(report_a.state, TransferState::Done, "{report_a:?}");
    assert_eq!(report_b.state, TransferState::Done, "{report_b:?}");
}

// ---------------------------------------------------------------------------
// W12：改名 / 暂停 / 取消 / 重传之间，目标和进度状态不能丢
// ---------------------------------------------------------------------------

/// 总是选"改名"并给定新名字的回答者。
struct RenameTo(&'static str);

impl ConflictResolver for RenameTo {
    fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async move {
            ConflictDecision {
                action: ConflictAction::Rename,
                apply_to_all: false,
                rename_to: Some(self.0.to_owned()),
            }
        })
    }
}

/// 收事件直到任务进终态，途中每一条进度都不能超过总量。
async fn finish_with_bounded_progress(
    events: &mut mpsc::UnboundedReceiver<TransferEvent>,
    id: TransferId,
) -> ProgressReport {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "等不到任务 {id} 的终态");
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("等不到终态")
            .expect("事件通道关了");
        let (TransferEvent::Progress(report) | TransferEvent::Finished(report)) = &event;
        if report.id != id {
            continue;
        }
        assert!(
            report.done_bytes <= report.total_bytes,
            "进度超过了总量：{report:?}"
        );
        if let TransferEvent::Finished(report) = event {
            return report;
        }
    }
}

/// 等某个目标的临时文件比 `beyond` 长，然后暂停并等到真的停下。返回暂停时的临时文件长度。
async fn pause_when_partial_grows(
    engine: &Arc<TransferEngine>,
    endpoints: &MemoryEndpoints,
    id: TransferId,
    part_of: &str,
    beyond: u64,
) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while endpoints.remote_partial_len(part_of) <= beyond {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{part_of} 的临时文件一直没长大"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(engine.pause(id), "暂停要成功（文件还没传完）");
    while engine.state(id) != Some(TransferState::Paused) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "没停在暂停状态：{:?}",
            engine.state(id)
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    endpoints.remote_partial_len(part_of)
}

fn slow_rename_options() -> TransferOptions {
    TransferOptions {
        concurrency: 1,
        in_flight: 1,
        chunk_len: Some(1024),
        ..local_options()
    }
}

/// 改名之后传到一半，反复暂停再继续：每一轮都写同一个改名后的目标，原目标不被动，
/// 也不重新提问；进度始终不超过总量，最后正好等于总量。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renamed_target_survives_repeated_pause_and_resume() {
    let root = temp_root("w12-rename-pause");
    let source = root.join("big.bin");
    let size: usize = 16 * 1024 * 1024;
    let contents: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &contents).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/big.bin", vec![9u8; 1024]);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(RenameTo("other.bin")),
        slow_rename_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/big.bin".to_owned(),
        recursive: false,
        peer: None,
    });

    let mut paused_at = 0;
    for _ in 0..2 {
        paused_at =
            pause_when_partial_grows(&engine, &endpoints, id, "/remote/other.bin", paused_at).await;
        assert!(
            endpoints.remote_part_files("/remote/big.bin").is_empty(),
            "暂停时不该有原目标的临时文件：{:?}",
            endpoints.remote_part_files("/remote/big.bin")
        );
        let report = engine.report(id).unwrap();
        assert!(report.done_bytes <= report.total_bytes, "{report:?}");
        assert!(engine.resume(id));
        // 继续之后状态要离开「已暂停」（以前一直显示已暂停直到传完）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while engine.state(id) == Some(TransferState::Paused) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "继续之后状态一直停在已暂停"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(report.done_bytes, report.total_bytes, "{report:?}");
    assert_eq!(report.total_bytes, size as u64);
    assert_eq!(endpoints.remote_contents("/remote/other.bin"), contents);
    assert_eq!(
        endpoints.remote_contents("/remote/big.bin"),
        vec![9u8; 1024],
        "原目标不能被动"
    );
    assert!(endpoints.remote_part_files("/remote/other.bin").is_empty());
}

/// 改名之后暂停再取消：清掉的是改名目标的临时文件；原目标和别人的同名临时文件原样留着。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_rename_and_pause_cleans_the_renamed_partial() {
    let root = temp_root("w12-rename-cancel");
    let source = root.join("big.bin");
    std::fs::write(&source, vec![4u8; 16 * 1024 * 1024]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/big.bin", vec![9u8; 1024]);
    // 原目标旁边别人留下的旧写法临时文件：不属于这个任务，取消时也不能碰。
    endpoints.put_remote("/remote/big.bin.zssh-part", vec![1u8; 100]);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(RenameTo("other.bin")),
        slow_rename_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/big.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    pause_when_partial_grows(&engine, &endpoints, id, "/remote/other.bin", 0).await;
    assert_eq!(endpoints.remote_part_files("/remote/other.bin").len(), 1);

    assert!(engine.cancel(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Cancelled, "{report:?}");
    assert!(
        endpoints.remote_part_files("/remote/other.bin").is_empty(),
        "改名目标的临时文件要清掉：{:?}",
        endpoints.remote_part_files("/remote/other.bin")
    );
    assert!(!endpoints.remote_exists("/remote/other.bin"));
    assert_eq!(
        endpoints.remote_contents("/remote/big.bin"),
        vec![9u8; 1024]
    );
    assert_eq!(
        endpoints.remote_contents("/remote/big.bin.zssh-part"),
        vec![1u8; 100],
        "别人的临时文件不能碰"
    );
}

/// 一次失败的写之后，连续前缀之后的洞截不掉、只能从头传：重传前先扣掉之前已计入的字节，
/// 进度不能因为同一段数据传了两遍而超过总量，最后正好等于总量。
#[tokio::test]
async fn restarting_a_file_from_scratch_does_not_double_count_progress() {
    let data = nonzero_pattern(10 * 4096);
    let root = temp_root("w12-restart-count");
    let source = root.join("hole.bin");
    std::fs::write(&source, &data).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    // 和 `upload_that_leaves_a_hole` 同一个夹具，但允许任务内自动重试：第一轮已确认 16384
    // 字节并计入进度，之后清理一直失败（截不掉洞），第二轮只能从 0 开始。
    endpoints.fail_remote_write_at_offset(20_480);
    endpoints.set_remote_cleanup_fails(true);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(OverwriteAll),
        TransferOptions {
            max_retries: 2,
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/hole.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert!(
        report.notes.iter().any(|note| note.contains("从头传")),
        "前提：这一轮真的是从头重传：{:?}",
        report.notes
    );
    assert_eq!(report.done_bytes, report.total_bytes, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/hole.bin"), data);
}

/// 重试一个失败的任务（进度归零、接着上次的临时文件传）：续传前缀也要算进进度，
/// 最后 `done_bytes` 正好等于总量（以前只算新传的部分，少了续传前缀那一段）。
#[tokio::test]
async fn retry_counts_the_resumed_prefix_so_progress_ends_at_the_total() {
    let root = temp_root("w12-retry-count");
    let source = root.join("retry.bin");
    std::fs::write(&source, vec![7u8; 16 * 1024]).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.fail_remote_write_after(4096);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(OverwriteAll),
        TransferOptions {
            max_retries: 0,
            in_flight: 1,
            ..local_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/retry.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert!(
        matches!(report.state, TransferState::Failed(_)),
        "{report:?}"
    );
    assert!(endpoints.remote_partial_len("/remote/retry.bin") > 0);

    assert!(engine.retry(id));
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(report.done_bytes, 16 * 1024, "{report:?}");
    assert_eq!(report.done_bytes, report.total_bytes);
}

// ---------------------------------------------------------------------------
// W12 续：失败重试时目标名变了，上一次留下的 `.zssh-part` 不能一直留着
// ---------------------------------------------------------------------------

/// 按顺序回答冲突：第 n 次被问就用第 n 个回答，用完了一直用最后一个。
struct RetryAnswers {
    answers: Vec<ConflictDecision>,
    asked: std::sync::atomic::AtomicUsize,
}

impl RetryAnswers {
    fn new(answers: Vec<ConflictDecision>) -> Self {
        Self {
            answers,
            asked: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl ConflictResolver for RetryAnswers {
    fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        let index = self
            .asked
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .min(self.answers.len() - 1);
        let answer = self.answers[index].clone();
        Box::pin(async move { answer })
    }
}

fn answer_rename(name: &str) -> ConflictDecision {
    ConflictDecision {
        action: ConflictAction::Rename,
        apply_to_all: false,
        rename_to: Some(name.to_owned()),
    }
}

fn answer_action(action: ConflictAction) -> ConflictDecision {
    ConflictDecision {
        action,
        apply_to_all: false,
        rename_to: None,
    }
}

/// 造一个"目标已存在 + 第一次改名写到一半失败"的场景：返回引擎、事件、任务号，任务已处于失败，
/// `/remote/a.bin` 下留着本任务的临时文件。
async fn failed_after_rename_to_a(
    tag: &str,
    answers: Vec<ConflictDecision>,
) -> (
    crate::test_util::TestDir,
    Arc<MemoryEndpoints>,
    Arc<TransferEngine>,
    mpsc::UnboundedReceiver<TransferEvent>,
    TransferId,
    Vec<u8>,
) {
    let root = temp_root(tag);
    let source = root.join("f.bin");
    let data = nonzero_pattern(16 * 1024);
    std::fs::write(&source, &data).unwrap();
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    endpoints.put_remote("/remote/f.bin", vec![9u8; 64]);
    // 用户自己的、旧写法的 `.zssh-part`：不属于这个任务，什么时候都不能碰。
    endpoints.put_remote("/remote/a.bin.zssh-part", vec![1u8; 10]);
    endpoints.fail_remote_write_after(4096);
    let (engine, mut events) = TransferEngine::new(
        endpoints.clone(),
        Arc::new(RetryAnswers::new(answers)),
        TransferOptions {
            max_retries: 0,
            ..slow_rename_options()
        },
    );
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/f.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert!(
        matches!(report.state, TransferState::Failed(_)),
        "前提：第一次尝试写到一半失败：{report:?}"
    );
    assert_eq!(
        endpoints.remote_part_files("/remote/a.bin").len(),
        2,
        "前提：改名目标下有本任务的临时文件（外加用户自己那个）：{:?}",
        endpoints.remote_part_files("/remote/a.bin")
    );
    (root, endpoints, engine, events, id, data)
}

fn own_parts(endpoints: &MemoryEndpoints, path: &str) -> Vec<String> {
    endpoints
        .remote_part_files(path)
        .into_iter()
        .filter(|name| name != &format!("{path}.zssh-part"))
        .collect()
}

fn users_part_is_intact(endpoints: &MemoryEndpoints) {
    assert_eq!(
        endpoints.remote_contents("/remote/a.bin.zssh-part"),
        vec![1u8; 10],
        "用户自己的 .zssh-part 文件被动了"
    );
}

/// 重试时改成了另一个名字：上一次留在旧名字下的临时文件要删掉（以前一直留在服务器上）。
#[tokio::test]
async fn retry_with_another_rename_removes_the_old_names_part() {
    let (_root, endpoints, engine, mut events, id, data) = failed_after_rename_to_a(
        "w12-retry-other-name",
        vec![answer_rename("a.bin"), answer_rename("b.bin")],
    )
    .await;
    assert!(engine.retry(id));
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/b.bin"), data);
    assert!(
        own_parts(&endpoints, "/remote/a.bin").is_empty(),
        "旧名字下的临时文件没清：{:?}",
        endpoints.remote_part_files("/remote/a.bin")
    );
    assert!(own_parts(&endpoints, "/remote/b.bin").is_empty());
    assert!(!endpoints.remote_exists("/remote/a.bin"));
    assert_eq!(endpoints.remote_contents("/remote/f.bin"), vec![9u8; 64]);
    users_part_is_intact(&endpoints);
}

/// 重试时改成覆盖原目标：旧改名目标下的临时文件也不再有用。
#[tokio::test]
async fn retry_with_overwrite_removes_the_renamed_part() {
    let (_root, endpoints, engine, mut events, id, data) = failed_after_rename_to_a(
        "w12-retry-overwrite",
        vec![
            answer_rename("a.bin"),
            answer_action(ConflictAction::Overwrite),
        ],
    )
    .await;
    assert!(engine.retry(id));
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/f.bin"), data);
    assert!(
        own_parts(&endpoints, "/remote/a.bin").is_empty(),
        "{:?}",
        endpoints.remote_part_files("/remote/a.bin")
    );
    users_part_is_intact(&endpoints);
}

/// 重试时选了跳过：临时文件同样要删。
#[tokio::test]
async fn retry_with_skip_removes_the_renamed_part() {
    let (_root, endpoints, engine, mut events, id, _data) = failed_after_rename_to_a(
        "w12-retry-skip",
        vec![answer_rename("a.bin"), answer_action(ConflictAction::Skip)],
    )
    .await;
    assert!(engine.retry(id));
    let report = finish_with_bounded_progress(&mut events, id).await;
    assert_eq!(report.state, TransferState::Skipped, "{report:?}");
    assert!(
        own_parts(&endpoints, "/remote/a.bin").is_empty(),
        "{:?}",
        endpoints.remote_part_files("/remote/a.bin")
    );
    assert_eq!(endpoints.remote_contents("/remote/f.bin"), vec![9u8; 64]);
    users_part_is_intact(&endpoints);
}

/// 重试时又选了同一个名字：这份临时文件还能续传，不能被误删——重试的进度从续传前缀开始，
/// 不是从 0。
#[tokio::test]
async fn retry_with_the_same_rename_keeps_and_resumes_the_part() {
    let (_root, endpoints, engine, mut events, id, data) = failed_after_rename_to_a(
        "w12-retry-same-name",
        vec![answer_rename("a.bin"), answer_rename("a.bin")],
    )
    .await;
    let kept = endpoints.remote_partial_len("/remote/a.bin");
    assert!(kept >= 4096, "前提：失败时临时文件有内容：{kept}");
    assert!(engine.retry(id));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut lowest_nonzero = u64::MAX;
    let report = loop {
        assert!(tokio::time::Instant::now() < deadline, "等不到终态");
        let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .expect("等不到终态")
            .expect("事件通道关了");
        let (TransferEvent::Progress(report) | TransferEvent::Finished(report)) = &event;
        if report.id != id {
            continue;
        }
        if report.done_bytes > 0 {
            lowest_nonzero = lowest_nonzero.min(report.done_bytes);
        }
        if let TransferEvent::Finished(report) = event {
            break report;
        }
    };
    assert_eq!(report.state, TransferState::Done, "{report:?}");
    assert_eq!(endpoints.remote_contents("/remote/a.bin"), data);
    assert!(
        lowest_nonzero >= kept,
        "应当从续传前缀 {kept} 开始，而不是从头：最低的非零进度 {lowest_nonzero}"
    );
    assert!(own_parts(&endpoints, "/remote/a.bin").is_empty());
    users_part_is_intact(&endpoints);
}

/// 失败后重试，但在重新开始传这个文件之前就取消了：旧名字下的临时文件也要清。
#[tokio::test]
async fn cancelling_a_retry_before_the_file_restarts_cleans_the_old_part() {
    let (_root, endpoints, engine, mut events, id, _data) = failed_after_rename_to_a(
        "w12-retry-cancel",
        vec![answer_rename("a.bin"), answer_rename("b.bin")],
    )
    .await;
    // 把重试那一轮卡在队列里（拿着任务的运行锁），取消之后再放它走：它一上来就看到取消。
    let state = engine.job(id).expect("任务还在");
    let gate = state.run_lock.lock().await;
    assert!(engine.retry(id));
    assert!(engine.cancel(id));
    drop(gate);
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Cancelled, "{report:?}");
    // 清理在终态之后紧接着做（同一把运行锁里）：等锁放开就是做完了。
    drop(state.run_lock.lock().await);
    assert!(
        own_parts(&endpoints, "/remote/a.bin").is_empty(),
        "{:?}",
        endpoints.remote_part_files("/remote/a.bin")
    );
    assert_eq!(endpoints.remote_contents("/remote/f.bin"), vec![9u8; 64]);
    users_part_is_intact(&endpoints);
}

// ---------------------------------------------------------------------------
// W15：规划 / 建目录阶段的取消，以及目录树的深度上限
// ---------------------------------------------------------------------------

/// 立即取消一个空目录的本地复制：以前仍然建出目标目录、最后报「完成」。
#[tokio::test]
async fn cancelling_an_empty_directory_copy_right_away_creates_nothing() {
    let root = temp_root("w15-cancel-empty");
    let source = root.join("src");
    std::fs::create_dir(&source).unwrap();
    let target = root.join("dst");
    let (engine, mut events) = TransferEngine::new(
        Arc::new(MemoryEndpoints::new("/remote")),
        Arc::new(OverwriteAll),
        local_options(),
    );
    let id = engine.submit(TransferJob {
        direction: Direction::LocalCopy,
        source: source.display().to_string(),
        target: target.display().to_string(),
        recursive: true,
        peer: None,
    });
    // 单线程运行时：任务此刻还没开始跑，取消先到。
    assert!(engine.cancel(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Cancelled, "{report:?}");
    assert!(!target.exists(), "取消之后不该建出目标目录");
}

/// 包一层 `MemoryEndpoints`，让读目录 / 建目录卡住（模拟很慢的服务器），并记下建过哪些目录。
struct Stalling {
    inner: Arc<MemoryEndpoints>,
    stall_list: bool,
    stall_create: bool,
    entered: tokio::sync::Notify,
    created: Mutex<Vec<String>>,
    /// 被卡住的那次调用的 future 是否已经被丢掉（= 被取消打断，而不是一直挂着）。
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl Stalling {
    fn new(inner: &Arc<MemoryEndpoints>, stall_list: bool, stall_create: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(inner),
            stall_list,
            stall_create,
            entered: tokio::sync::Notify::new(),
            created: Mutex::new(Vec::new()),
            dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    async fn stall(&self) {
        struct MarkOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for MarkOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _mark = MarkOnDrop(Arc::clone(&self.dropped));
        self.entered.notify_one();
        std::future::pending::<()>().await;
    }
}

impl super::backend::RemoteLister for Stalling {
    fn list_dir<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<crate::sftp::fs::DirEntry>, SftpError>> {
        Box::pin(async move {
            if self.stall_list {
                self.stall().await;
            }
            self.inner.list_dir(path).await
        })
    }

    fn stat_remote<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        self.inner.stat_remote(path)
    }
}

impl super::backend::TransferEndpoints for Stalling {
    fn pair<'a>(
        &'a self,
        direction: Direction,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<super::backend::FilePair, SftpError>> {
        self.inner.pair(direction, peer, source, target, owner)
    }

    fn plan_remote_copy<'a>(
        &'a self,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        recursive: bool,
    ) -> BoxFuture<'a, Result<plan::TransferPlan, SftpError>> {
        self.inner.plan_remote_copy(peer, source, target, recursive)
    }

    fn create_target_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), SftpError>> {
        Box::pin(async move {
            if self.stall_create {
                self.stall().await;
            }
            self.created.lock().unwrap().push(path.to_owned());
            Ok(())
        })
    }

    fn reset<'a>(&'a self) -> BoxFuture<'a, ()> {
        self.inner.reset()
    }

    fn server_identity<'a>(&'a self) -> BoxFuture<'a, Result<String, SftpError>> {
        self.inner.server_identity()
    }
}

/// 规划（读远端目录）卡住时取消：规划被打断，任务以「已取消」结束，本地目标一点没建。
#[tokio::test]
async fn cancelling_while_the_remote_listing_is_stuck_stops_the_plan() {
    let root = temp_root("w15-cancel-listing");
    let inner = Arc::new(MemoryEndpoints::new("/remote"));
    inner.put_remote("/remote/tree/a/b.txt", vec![1u8; 10]);
    let stalling = Stalling::new(&inner, true, false);
    let (engine, mut events) =
        TransferEngine::new(stalling.clone(), Arc::new(OverwriteAll), local_options());
    let target = root.join("out");
    let id = engine.submit(TransferJob {
        direction: Direction::Download,
        source: "/remote/tree".to_owned(),
        target: target.display().to_string(),
        recursive: true,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(30), stalling.entered.notified())
        .await
        .expect("读目录没有开始");
    assert!(engine.cancel(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Cancelled, "{report:?}");
    assert!(
        stalling.dropped.load(std::sync::atomic::Ordering::SeqCst),
        "卡住的读目录要被打断，不能一直挂着"
    );
    assert!(!target.exists(), "规划被取消，不该建本地目录");
}

/// 建远端目录的途中取消：当前这一个被打断，后面的目录不再建，最终是「已取消」。
#[tokio::test]
async fn cancelling_while_target_directories_are_created_stops_creating_more() {
    let root = temp_root("w15-cancel-mkdir");
    let tree = root.join("tree");
    std::fs::create_dir_all(tree.join("a")).unwrap();
    std::fs::create_dir_all(tree.join("b")).unwrap();
    std::fs::write(tree.join("a").join("f.txt"), b"x").unwrap();
    std::fs::write(tree.join("b").join("g.txt"), b"y").unwrap();
    let inner = Arc::new(MemoryEndpoints::new("/remote"));
    let stalling = Stalling::new(&inner, false, true);
    let (engine, mut events) =
        TransferEngine::new(stalling.clone(), Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: tree.display().to_string(),
        target: "/remote/tree".to_owned(),
        recursive: true,
        peer: None,
    });
    tokio::time::timeout(Duration::from_secs(30), stalling.entered.notified())
        .await
        .expect("建目录没有开始");
    assert!(engine.cancel(id));
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    assert_eq!(report.state, TransferState::Cancelled, "{report:?}");
    assert!(stalling.dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        stalling.created.lock().unwrap().is_empty(),
        "取消之后不该再建目录：{:?}",
        stalling.created.lock().unwrap()
    );
    assert!(!endpoints_has_files(&inner));
}

fn endpoints_has_files(endpoints: &MemoryEndpoints) -> bool {
    endpoints.remote_exists("/remote/tree/a/f.txt")
        || endpoints.remote_exists("/remote/tree/b/g.txt")
}

/// 比默认上限更深的远端树：下载 / 复制都以明确的错误失败，不会只传一部分就报完成。
#[tokio::test]
async fn a_remote_tree_deeper_than_the_limit_fails_the_job() {
    let root = temp_root("w15-deep-tree");
    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let mut path = "/remote/tree".to_owned();
    endpoints.put_remote(&format!("{path}/top.txt"), vec![1u8; 4]);
    for level in 1..=(plan::MAX_PLAN_DEPTH + 5) {
        path = format!("{path}/d{level}");
    }
    endpoints.put_remote(&format!("{path}/leaf.txt"), vec![2u8; 4]);
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    for (direction, target) in [
        (Direction::Download, root.join("out").display().to_string()),
        (Direction::RemoteCopy, "/remote/copy".to_owned()),
    ] {
        let id = engine.submit(TransferJob {
            direction,
            source: "/remote/tree".to_owned(),
            target,
            recursive: true,
            peer: None,
        });
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        let TransferState::Failed(text) = &report.state else {
            panic!("太深的树必须失败：{report:?}");
        };
        assert!(text.contains(&plan::MAX_PLAN_DEPTH.to_string()), "{text}");
        assert_eq!(report.done_files, 0, "{report:?}");
    }
    assert!(!root.join("out").exists(), "规划失败不该建本地目录");
    assert!(!endpoints.remote_exists("/remote/copy/top.txt"));
}

/// 比默认上限更深的本地树：上传 / 本地复制以明确的错误失败（以前深处的子树被悄悄丢掉，任务还报完成）。
#[tokio::test]
async fn a_local_tree_deeper_than_the_limit_fails_the_job() {
    let root = temp_root("local-deep-tree");
    let source = root.join("src");
    let mut deep = source.clone();
    for level in 1..=(plan::MAX_PLAN_DEPTH + 2) {
        deep.push(format!("d{level}"));
    }
    std::fs::create_dir_all(crate::sftp::local::long_path(&deep)).unwrap();
    std::fs::write(
        crate::sftp::local::long_path(&deep.join("leaf.txt")),
        b"leaf",
    )
    .unwrap();
    std::fs::write(source.join("top.txt"), b"top").unwrap();

    let endpoints = Arc::new(MemoryEndpoints::new("/remote"));
    let (engine, mut events) =
        TransferEngine::new(endpoints.clone(), Arc::new(OverwriteAll), local_options());
    for (direction, target) in [
        (Direction::Upload, "/remote/up".to_owned()),
        (
            Direction::LocalCopy,
            root.join("copy").display().to_string(),
        ),
    ] {
        let id = engine.submit(TransferJob {
            direction,
            source: source.display().to_string(),
            target,
            recursive: true,
            peer: None,
        });
        let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
        let TransferState::Failed(text) = &report.state else {
            panic!("太深的本地树必须失败：{report:?}");
        };
        assert!(text.contains(&plan::MAX_PLAN_DEPTH.to_string()), "{text}");
        assert_eq!(report.done_files, 0, "{report:?}");
    }
    assert!(!root.join("copy").exists(), "规划失败不该建本地目录");
    assert!(!endpoints.remote_exists("/remote/up/top.txt"));
}

// ---------------------------------------------------------------------------
// W13：提交时「目标冒出来了」的重查必须有次数上限
// ---------------------------------------------------------------------------

/// 目标端点的包装：永远说「目标不存在」，提交却永远说「已存在」（对同一个路径的两种判断对不上）。
struct NeverSeesTarget {
    inner: Arc<dyn FileEndpoint>,
    commits: Arc<std::sync::atomic::AtomicUsize>,
}

impl FileEndpoint for NeverSeesTarget {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn size<'a>(&'a self) -> endpoint::EndpointFuture<'a, Result<Option<u64>, SftpError>> {
        self.inner.size()
    }
    fn open_read<'a>(
        &'a self,
    ) -> endpoint::EndpointFuture<'a, Result<Arc<dyn endpoint::ReadAt>, SftpError>> {
        self.inner.open_read()
    }
    fn open_write<'a>(
        &'a self,
        resume_len: u64,
    ) -> endpoint::EndpointFuture<'a, Result<Arc<dyn endpoint::WriteAt>, SftpError>> {
        self.inner.open_write(resume_len)
    }
    fn commit<'a>(
        &'a self,
        _total: u64,
        _overwrite: bool,
    ) -> endpoint::EndpointFuture<'a, Result<(), SftpError>> {
        self.commits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Err(SftpError::AlreadyExists {
                path: "endpoint-path".to_owned(),
            })
        })
    }
    fn existing_size<'a>(&'a self) -> endpoint::EndpointFuture<'a, Result<Option<u64>, SftpError>> {
        Box::pin(async { Ok(None) })
    }
    fn remove_existing<'a>(&'a self) -> endpoint::EndpointFuture<'a, Result<(), SftpError>> {
        self.inner.remove_existing()
    }
    fn remove_partial<'a>(&'a self) -> endpoint::EndpointFuture<'a, Result<(), SftpError>> {
        self.inner.remove_partial()
    }
    fn partial_len<'a>(&'a self) -> endpoint::EndpointFuture<'a, Result<u64, SftpError>> {
        self.inner.partial_len()
    }
    fn truncate_partial<'a>(
        &'a self,
        len: u64,
    ) -> endpoint::EndpointFuture<'a, Result<(), SftpError>> {
        self.inner.truncate_partial(len)
    }
    fn read_partial<'a>(
        &'a self,
        offset: u64,
        len: u32,
    ) -> endpoint::EndpointFuture<'a, Result<Option<Vec<u8>>, SftpError>> {
        self.inner.read_partial(offset, len)
    }
    fn set_attributes<'a>(
        &'a self,
        mtime: Option<u32>,
        permissions: Option<u32>,
    ) -> endpoint::EndpointFuture<'a, Result<(), SftpError>> {
        self.inner.set_attributes(mtime, permissions)
    }
    fn attributes<'a>(
        &'a self,
    ) -> endpoint::EndpointFuture<'a, Result<endpoint::FileAttributesPair, SftpError>> {
        self.inner.attributes()
    }
}

struct NeverSeesTargetEndpoints {
    inner: Arc<MemoryEndpoints>,
    commits: Arc<std::sync::atomic::AtomicUsize>,
}

impl backend::RemoteLister for NeverSeesTargetEndpoints {
    fn list_dir<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<crate::sftp::fs::DirEntry>, SftpError>> {
        self.inner.list_dir(path)
    }
    fn stat_remote<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        self.inner.stat_remote(path)
    }
}

impl backend::TransferEndpoints for NeverSeesTargetEndpoints {
    fn pair<'a>(
        &'a self,
        direction: Direction,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<backend::FilePair, SftpError>> {
        Box::pin(async move {
            let pair = self
                .inner
                .pair(direction, peer, source, target, owner)
                .await?;
            Ok(backend::FilePair {
                source: pair.source,
                target: Arc::new(NeverSeesTarget {
                    inner: pair.target,
                    commits: Arc::clone(&self.commits),
                }),
            })
        })
    }
    fn plan_remote_copy<'a>(
        &'a self,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        recursive: bool,
    ) -> BoxFuture<'a, Result<plan::TransferPlan, SftpError>> {
        self.inner.plan_remote_copy(peer, source, target, recursive)
    }
    fn create_target_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), SftpError>> {
        self.inner.create_target_dir(path)
    }
    fn reset<'a>(&'a self) -> BoxFuture<'a, ()> {
        self.inner.reset()
    }
    fn server_identity<'a>(&'a self) -> BoxFuture<'a, Result<String, SftpError>> {
        self.inner.server_identity()
    }
}

/// 端点永远说「目标不存在」、提交永远说「已存在」：以前会无限重查；现在重查有限次后报「已存在」，
/// 并清掉自己的临时文件。
#[tokio::test]
async fn commit_rechecks_stop_after_a_few_rounds_instead_of_looping_forever() {
    let root = temp_root("w13-recheck-cap");
    let source = root.join("a.bin");
    std::fs::write(&source, vec![3u8; 4096]).unwrap();
    let inner = Arc::new(MemoryEndpoints::new("/remote"));
    let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let endpoints = Arc::new(NeverSeesTargetEndpoints {
        inner: Arc::clone(&inner),
        commits: Arc::clone(&commits),
    });
    let (engine, mut events) =
        TransferEngine::new(endpoints, Arc::new(OverwriteAll), local_options());
    let id = engine.submit(TransferJob {
        direction: Direction::Upload,
        source: source.display().to_string(),
        target: "/remote/a.bin".to_owned(),
        recursive: false,
        peer: None,
    });
    let report = drain_until_finished(&mut events, id, Duration::from_secs(30)).await;
    let TransferState::Failed(text) = &report.state else {
        panic!("必须失败：{report:?}");
    };
    assert!(text.contains("/remote/a.bin"), "{text}");
    // 首次提交 + 每次重查之后各一次。
    assert_eq!(
        commits.load(std::sync::atomic::Ordering::SeqCst),
        MAX_COMMIT_RECHECKS as usize + 1
    );
    assert!(inner.remote_part_files("/remote/a.bin").is_empty());
    assert!(!inner.remote_exists("/remote/a.bin"));
}
