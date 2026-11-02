// 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
// `#[test]` 属性宏，展开时直接撞递归上限。
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use zssh_core::sftp::SftpError;
use zssh_core::sftp::transfer::ConflictDecision;

use zssh_core::sftp::fs::{self, RecursiveDelete};
use zssh_core::sftp::transfer::backend::{SftpChannelProvider, SftpEndpoints};
use zssh_core::sftp::transfer::{
    ConflictAction, ConflictInfo, ConflictResolver, Direction, TransferEngine, TransferEvent,
    TransferId, TransferJob, TransferOptions, TransferState,
};

use super::{
    CloseDecision, ConflictAutomation, DeleteOutcome, DropKind, ListingGate, SftpEvent,
    UiConflictResolver, can_pause, classify_dropped_path, close_decision, conflict_compare,
    conflict_decision, conflict_rename_name, delete_outcome_text, direction_label, format_mode,
    format_mtime, format_size, mkdir_target, parse_chmod_hook, parse_conflict_hook,
    parse_octal_mode, parse_pause_hook_ms, parse_rename_hook, rename_target, resolve_local_path,
    resolve_remote_child, run_delete, same_scope, state_label, upload_dir_target, upload_target,
    valid_remote_name,
};
use zssh_core::sftp::local::download_target;

/// 测试用的临时目录：Drop 时整个删掉，断言失败也不留垃圾。
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "zssh-sftp-local-{name}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn format_size_switches_units() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1023), "1023 B");
    assert_eq!(format_size(1024), "1.0 KB");
    assert_eq!(format_size(1536), "1.5 KB");
    assert_eq!(format_size(1024 * 1024), "1.0 MB");
    assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
}

#[test]
fn upload_target_keeps_the_local_file_name() {
    assert_eq!(upload_target("/home/u", "a.txt").unwrap(), "/home/u/a.txt");
    assert_eq!(
        upload_target("/home/u/", r"C:\tmp\a.txt").unwrap(),
        "/home/u/a.txt"
    );
    assert_eq!(upload_target("/", "/tmp/a.txt").unwrap(), "/a.txt");
    assert!(upload_target("/home/u", "").is_err());
}

#[test]
fn download_target_uses_the_remote_file_name() {
    let dir = std::env::temp_dir();
    let target = download_target(&dir, "/var/log/app.log").unwrap();
    assert_eq!(target.parent(), Some(dir.as_path()));
    assert_eq!(
        target.file_name().and_then(|name| name.to_str()),
        Some("app.log")
    );
    assert!(download_target(&dir, "/").is_err());
}

#[test]
fn download_target_names_a_remote_directory() {
    let dir = std::env::temp_dir();
    // 整目录下载的本地根就是本地栏当前目录下的同名文件夹，尾部 `/` 不算名字。
    let target = download_target(&dir, "/var/log/app/").unwrap();
    assert_eq!(target.parent(), Some(dir.as_path()));
    assert_eq!(
        target.file_name().and_then(|name| name.to_str()),
        Some("app")
    );
    // 根目录没有名字：不能把整个 `/` 摊到本地目录里。
    assert!(download_target(&dir, "/").is_err());
}

#[test]
fn upload_dir_target_keeps_the_local_directory_name() {
    assert_eq!(
        upload_dir_target("/home/u", r"C:\tmp\proj").unwrap(),
        "/home/u/proj"
    );
    assert_eq!(
        upload_dir_target("/home/u/", "/tmp/proj/").unwrap(),
        "/home/u/proj"
    );
    assert_eq!(upload_dir_target("/", "/tmp/proj").unwrap(), "/proj");
    // 名字为空、盘符根、`..` 都不能拼出一个跑出远端当前目录的落点。
    assert!(upload_dir_target("/home/u", "").is_err());
    assert!(upload_dir_target("/home/u", r"C:\").is_err());
    assert!(upload_dir_target("/home/u", r"C:\tmp\..").is_err());
}

#[test]
fn dropped_paths_are_classified_by_kind() {
    let scratch = ScratchDir::new("drop");
    let file = scratch.path().join("a.txt");
    std::fs::write(&file, b"x").expect("写临时文件");
    let dir = scratch.path().join("sub");
    std::fs::create_dir(&dir).expect("建临时目录");

    assert_eq!(classify_dropped_path(&file), DropKind::File);
    assert_eq!(classify_dropped_path(&dir), DropKind::Dir);
    // 不存在的路径不能被当成空文件 / 空目录排进队列。
    assert_eq!(
        classify_dropped_path(&scratch.path().join("missing.txt")),
        DropKind::Missing
    );
}

#[test]
fn remote_names_are_one_level_only() {
    assert_eq!(valid_remote_name(" new.txt ").unwrap(), "new.txt");
    assert_eq!(valid_remote_name("中文 名字").unwrap(), "中文 名字");
    // 空名字、路径分隔符、`.` / `..` 都不是能用的名字：拼出来的落点会跑出当前目录。
    assert!(valid_remote_name("").is_err());
    assert!(valid_remote_name("   ").is_err());
    assert!(valid_remote_name("/").is_err());
    assert!(valid_remote_name(".").is_err());
    assert!(valid_remote_name("..").is_err());
    assert!(valid_remote_name("a/b").is_err());
    assert!(valid_remote_name(r"a\b").is_err());
}

#[test]
fn mkdir_target_is_under_the_current_dir() {
    assert_eq!(mkdir_target("/home/u", "proj").unwrap(), "/home/u/proj");
    assert_eq!(mkdir_target("/", "proj").unwrap(), "/proj");
    // 名字不合法或者远端目录还没读出来：一个任务都不排。
    assert!(mkdir_target("/home/u", "").is_err());
    assert!(mkdir_target("/home/u", "a/b").is_err());
    assert!(mkdir_target("", "proj").is_err());
}

#[test]
fn rename_target_keeps_the_same_parent() {
    assert_eq!(
        rename_target("/home/u/a.txt", "b.txt").unwrap(),
        "/home/u/b.txt"
    );
    assert_eq!(rename_target("/a", "b").unwrap(), "/b");
    // 末尾带 `/` 的条目（目录）也按名字处理。
    assert_eq!(
        rename_target("/home/u/proj/", "proj2").unwrap(),
        "/home/u/proj2"
    );
    assert!(rename_target("/home/u/a.txt", "").is_err());
    assert!(rename_target("/home/u/a.txt", "x/y").is_err());
    assert!(rename_target("/home/u/a.txt", "..").is_err());
}

#[test]
fn hook_paths_stay_under_the_current_dir() {
    assert_eq!(
        resolve_remote_child("/home/u", "a.txt").unwrap(),
        "/home/u/a.txt"
    );
    assert_eq!(
        resolve_remote_child("/home/u/", "sub/a.txt").unwrap(),
        "/home/u/sub/a.txt"
    );
    assert_eq!(
        resolve_remote_child("/home/u", "sub/../a.txt").unwrap(),
        "/home/u/a.txt"
    );
    // 绝对路径、`..` 跳出当前目录、空名字、`.`（等于当前目录本身）都拒绝。
    assert!(resolve_remote_child("/home/u", "/etc/passwd").is_err());
    assert!(resolve_remote_child("/home/u", "../etc/passwd").is_err());
    assert!(resolve_remote_child("/home/u", "sub/../../etc").is_err());
    assert!(resolve_remote_child("/home/u", "   ").is_err());
    assert!(resolve_remote_child("/home/u", ".").is_err());
    assert!(resolve_remote_child("", "a.txt").is_err());
}

#[test]
fn rename_hook_splits_on_the_first_colon() {
    assert_eq!(
        parse_rename_hook("a.txt:b.txt").unwrap(),
        ("a.txt".to_owned(), "b.txt".to_owned())
    );
    // 新名字里可以再有冒号（Windows 上不合法，远端文件名里合法）。
    assert_eq!(
        parse_rename_hook("a:b:c").unwrap(),
        ("a".to_owned(), "b:c".to_owned())
    );
    assert!(parse_rename_hook("a.txt").is_err());
    assert!(parse_rename_hook(":b.txt").is_err());
    assert!(parse_rename_hook("a.txt:").is_err());
}

#[test]
fn octal_modes_accept_three_or_four_digits() {
    assert_eq!(parse_octal_mode("644").unwrap(), 0o644);
    assert_eq!(parse_octal_mode("0644").unwrap(), 0o644);
    assert_eq!(parse_octal_mode("755").unwrap(), 0o755);
    assert_eq!(parse_octal_mode(" 0755 ").unwrap(), 0o755);
    assert_eq!(parse_octal_mode("000").unwrap(), 0o000);
    // 四位时和网页端一样只看后三位。
    assert_eq!(parse_octal_mode("4755").unwrap(), 0o755);
    for invalid in ["999", "12", "abcd", "", "   ", "06444", "0o644", "-644"] {
        assert!(parse_octal_mode(invalid).is_err(), "{invalid:?} 不该被接受");
    }
}

#[test]
fn modes_format_as_four_octal_digits() {
    assert_eq!(format_mode(0o644), "0644");
    assert_eq!(format_mode(0o100644), "0644");
    assert_eq!(format_mode(0o755), "0755");
    assert_eq!(format_mode(0o000), "0000");
}

#[test]
fn chmod_hook_splits_on_the_first_colon() {
    assert_eq!(
        parse_chmod_hook("a.txt:644").unwrap(),
        ("a.txt".to_owned(), 0o644)
    );
    assert_eq!(
        parse_chmod_hook("sub/a.txt:0644").unwrap(),
        ("sub/a.txt".to_owned(), 0o644)
    );
    // 名字里有冒号时第一个冒号后面就不像权限了：直接拒绝，不猜。
    assert!(parse_chmod_hook("a:b:644").is_err());
    assert!(parse_chmod_hook(":644").is_err());
    assert!(parse_chmod_hook("a.txt").is_err());
    assert!(parse_chmod_hook("a.txt:999").is_err());
}

#[test]
fn mtime_formats_as_utc() {
    assert_eq!(format_mtime(0), "1970-01-01 00:00 UTC");
    assert_eq!(format_mtime(1_700_000_000), "2023-11-14 22:13 UTC");
}

#[test]
fn labels_cover_transfer_states() {
    assert_eq!(direction_label(Direction::Download), "下载");
    assert_eq!(direction_label(Direction::Upload), "上传");
    assert_eq!(state_label(&TransferState::Running), "传输中");
    assert_eq!(state_label(&TransferState::Done), "已完成");
    assert!(state_label(&TransferState::Failed("磁盘满".to_owned())).contains("磁盘满"));
}

#[test]
fn the_pause_hook_presses_pause_on_every_state_the_engine_accepts() {
    // 引擎接受暂停的状态：排队 / 准备 / 传输 / 等重连 / 等同名回答。
    for state in [
        TransferState::Queued,
        TransferState::Planning,
        TransferState::Running,
        TransferState::WaitingReconnect,
        TransferState::WaitingConflict,
    ] {
        assert!(can_pause(&state), "{state:?} 应该能暂停");
    }
    // 已经暂停的不重复按。
    assert!(!can_pause(&TransferState::Paused));
    // 终态不按。
    for state in [
        TransferState::Done,
        TransferState::Failed("磁盘满".to_owned()),
        TransferState::Cancelled,
        TransferState::Skipped,
    ] {
        assert!(!can_pause(&state), "{state:?} 不该显示暂停");
    }
}

#[test]
fn pause_hook_only_accepts_a_positive_number() {
    assert_eq!(parse_pause_hook_ms(Some("1500")), Some(1500));
    assert_eq!(parse_pause_hook_ms(Some(" 20 ")), Some(20));
    // 没设置、0、负数、不是数字：一律当没设置（0 毫秒会马上暂停，没有意义）。
    assert_eq!(parse_pause_hook_ms(None), None);
    assert_eq!(parse_pause_hook_ms(Some("0")), None);
    assert_eq!(parse_pause_hook_ms(Some("-5")), None);
    assert_eq!(parse_pause_hook_ms(Some("soon")), None);
    assert_eq!(parse_pause_hook_ms(Some("  ")), None);
}

#[test]
fn local_paths_resolve_against_the_current_dir() {
    let cwd = Path::new(r"C:\root");
    assert_eq!(
        resolve_local_path(cwd, "sub").unwrap(),
        PathBuf::from(r"C:\root\sub")
    );
    assert_eq!(
        resolve_local_path(cwd, r"D:\other").unwrap(),
        PathBuf::from(r"D:\other")
    );
    assert!(resolve_local_path(cwd, "   ").is_none());
}

fn conflict_info() -> ConflictInfo {
    ConflictInfo {
        job: 1,
        direction: Direction::Upload,
        source: r"C:\tmp\a.txt".to_owned(),
        target: "/home/u/a.txt".to_owned(),
        source_size: Some(10),
        target_size: Some(5),
        source_mtime: None,
        target_mtime: None,
        can_resume: false,
        suggested_target: "/home/u/a (2).txt".to_owned(),
        batch: None,
    }
}

fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建单线程 runtime")
}

fn next_conflict_id(received: &std::sync::mpsc::Receiver<SftpEvent>) -> u64 {
    match received.recv_timeout(Duration::from_secs(5)) {
        Ok(SftpEvent::Conflict { id, .. }) => id,
        _ => panic!("应该收到同名文件提问"),
    }
}

#[test]
fn conflict_hook_parses_only_the_documented_actions() {
    assert_eq!(
        parse_conflict_hook("overwrite").unwrap(),
        ConflictAutomation::Overwrite
    );
    assert_eq!(
        parse_conflict_hook(" skip ").unwrap(),
        ConflictAutomation::Skip
    );
    assert_eq!(
        parse_conflict_hook("RENAME").unwrap(),
        ConflictAutomation::Rename
    );
    assert_eq!(
        parse_conflict_hook("overwrite-all").unwrap(),
        ConflictAutomation::OverwriteAll
    );
    assert_eq!(
        parse_conflict_hook("skip-all").unwrap(),
        ConflictAutomation::SkipAll
    );
    assert!(parse_conflict_hook("").is_err());
    assert!(parse_conflict_hook("overwrit").is_err());
}

#[test]
fn conflict_automation_maps_to_engine_decisions() {
    let cases = [
        (
            ConflictAutomation::Overwrite,
            ConflictAction::Overwrite,
            false,
        ),
        (ConflictAutomation::Skip, ConflictAction::Skip, false),
        (ConflictAutomation::Rename, ConflictAction::Rename, false),
        (
            ConflictAutomation::OverwriteAll,
            ConflictAction::Overwrite,
            true,
        ),
        (ConflictAutomation::SkipAll, ConflictAction::Skip, true),
    ];
    for (automation, action, apply_to_all) in cases {
        let decision = automation.decision();
        assert_eq!(decision.action, action);
        assert_eq!(decision.apply_to_all, apply_to_all);
        assert_eq!(decision.rename_to, None);
    }
}

#[test]
fn conflict_rename_empty_name_means_auto_name() {
    // 空着 = 交给引擎按 `名字 (2)` 自动起名（面板默认预填的就是建议名）。
    assert_eq!(conflict_rename_name(Direction::Upload, "").unwrap(), None);
    assert_eq!(
        conflict_rename_name(Direction::Download, "  ").unwrap(),
        None
    );
    assert_eq!(
        conflict_rename_name(Direction::Upload, "  b.txt ")
            .unwrap()
            .as_deref(),
        Some("b.txt")
    );
    let rename = conflict_decision(ConflictAction::Rename, Direction::Upload, "").unwrap();
    assert_eq!(rename.action, ConflictAction::Rename);
    assert_eq!(rename.rename_to, None);
    assert!(!rename.apply_to_all);

    // 覆盖 / 跳过 / 续传不带名字（引擎按动作处理，不会被输入框里的旧名字带偏）。
    for action in [
        ConflictAction::Overwrite,
        ConflictAction::Skip,
        ConflictAction::Resume,
    ] {
        let decision = conflict_decision(action, Direction::Upload, "b.txt").unwrap();
        assert_eq!(decision.action, action);
        assert_eq!(decision.rename_to, None);
    }
}

/// M2：下载的新名字落在 Windows 上，非法字符、保留名、备用数据流、尾部点 / 空格都要挡住。
#[test]
fn conflict_rename_for_downloads_rejects_windows_unsafe_names() {
    for bad in [
        "x.txt:evil",
        "CON",
        "con.txt",
        "NUL",
        "COM1.log",
        "a*b",
        "a?b.txt",
        "q<w>.txt",
        "trailing.",
        "..",
        ".",
        "C:evil.txt",
    ] {
        assert!(
            conflict_rename_name(Direction::Download, bad).is_err(),
            "{bad:?} 应该被拒绝"
        );
        assert!(
            conflict_decision(ConflictAction::Rename, Direction::Download, bad).is_err(),
            "{bad:?} 不应该被送给引擎"
        );
    }
    // 路径分隔符任何方向都不行：改名不换目录。
    for direction in [Direction::Download, Direction::Upload] {
        assert!(conflict_rename_name(direction, "sub/b.txt").is_err());
        assert!(conflict_rename_name(direction, "sub\\b.txt").is_err());
    }
    // 正常名字（含中文、空格、点开头）通过。
    for good in ["报告 (副本).txt", ".bashrc", "a b.txt"] {
        assert_eq!(
            conflict_rename_name(Direction::Download, good)
                .unwrap()
                .as_deref(),
            Some(good)
        );
    }
}

/// 上传的新名字落在远端 Linux 上：Windows 的限制不适用（`CON`、`a:b` 在那边是合法名字）。
#[test]
fn conflict_rename_for_uploads_only_needs_a_single_level_name() {
    assert_eq!(
        conflict_rename_name(Direction::Upload, "CON")
            .unwrap()
            .as_deref(),
        Some("CON")
    );
    assert_eq!(
        conflict_rename_name(Direction::Upload, "a:b")
            .unwrap()
            .as_deref(),
        Some("a:b")
    );
    assert!(conflict_rename_name(Direction::Upload, "..").is_err());
    assert!(conflict_rename_name(Direction::Upload, "a\0b").is_err());
}

/// H4：面板上两边的大小 / 时间对比和续传提示。
#[test]
fn conflict_compare_shows_both_sides_and_the_resume_hint() {
    let mut info = conflict_info();
    info.source_size = Some(2048);
    info.target_size = Some(1024);
    info.source_mtime = Some(1_700_000_100);
    info.target_mtime = Some(1_700_000_000);
    info.can_resume = true;
    let (source, target, hint) = conflict_compare(&info);
    assert!(
        source.starts_with("源：") && source.contains("2.0 KB"),
        "{source}"
    );
    assert!(
        target.starts_with("目标：") && target.contains("1.0 KB"),
        "{target}"
    );
    assert!(hint.contains("目标比源小"), "{hint}");
    assert!(hint.contains("源更新"), "{hint}");
    assert!(hint.contains("续传"), "{hint}");

    info.can_resume = false;
    info.source_size = Some(1024);
    info.source_mtime = None;
    let (_, _, hint) = conflict_compare(&info);
    assert!(hint.contains("两边大小相同"), "{hint}");
    assert!(!hint.contains("续传"), "{hint}");

    info.source_size = None;
    info.target_size = None;
    info.target_mtime = None;
    let (source, target, hint) = conflict_compare(&info);
    assert!(source.contains("未知") && target.contains("未知"));
    assert!(hint.is_empty());
}

#[test]
fn listing_gate_only_accepts_the_latest_request() {
    let mut gate = ListingGate::default();
    let first = gate.next();
    let second = gate.next();
    assert_ne!(first, second);
    // 慢的旧请求晚到：不算数。
    assert!(!gate.is_latest(first));
    assert!(gate.is_latest(second));
    // 又发了新的，之前"最新"的也立刻作废。
    let third = gate.next();
    assert!(!gate.is_latest(second));
    assert!(gate.is_latest(third));
}

#[test]
fn ui_resolver_answers_one_conflict() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(events, None));
    let decision = runtime.block_on(async {
        let task = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(conflict_info()).await })
        };
        // 让 decide 跑第一次 poll：把提问送进事件通道，然后停在等回答上。
        tokio::task::yield_now().await;
        let id = next_conflict_id(&received);
        let answer = ConflictDecision {
            action: ConflictAction::Rename,
            apply_to_all: false,
            rename_to: Some("b.txt".to_owned()),
        };
        assert!(resolver.respond(id, answer));
        task.await.expect("decide 任务")
    });
    assert_eq!(decision.action, ConflictAction::Rename);
    assert_eq!(decision.rename_to.as_deref(), Some("b.txt"));
}

fn info_in(job: TransferId, batch: Option<u64>) -> ConflictInfo {
    let mut info = conflict_info();
    info.job = job;
    info.batch = batch;
    info
}

/// H1：「全部覆盖」只管当前批次——同批次正在等的和后来到的按它答，
/// 别的批次照常提问。
#[test]
fn ui_resolver_apply_to_all_is_scoped_to_its_batch() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(events, None));
    runtime.block_on(async {
        let mut tasks = Vec::new();
        for job in [1, 2] {
            let resolver = Arc::clone(&resolver);
            tasks.push(tokio::spawn(async move {
                resolver.decide(info_in(job, Some(10))).await
            }));
        }
        // 另一次提交（批次 11）的任务也在等。
        let other = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(3, Some(11))).await })
        };
        tokio::task::yield_now().await;
        let ids = [
            next_conflict_id(&received),
            next_conflict_id(&received),
            next_conflict_id(&received),
        ];
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );

        let answered = resolver.respond_scope(ConflictAction::Overwrite, Some(10), 1);
        assert_eq!(answered, 2, "只回答批次 10 里正在等的两条");
        for task in tasks {
            let decision = task.await.expect("decide 任务");
            assert_eq!(decision.action, ConflictAction::Overwrite);
            assert!(decision.apply_to_all);
        }
        // 批次 11 的提问还在等。
        assert!(!other.is_finished());

        // 批次 10 后面到的冲突不再进事件通道。
        let decision = resolver.decide(info_in(4, Some(10))).await;
        assert_eq!(decision.action, ConflictAction::Overwrite);
        assert!(received.try_recv().is_err());

        // 批次 11 的新冲突照样要问。
        let third = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(5, Some(11))).await })
        };
        tokio::task::yield_now().await;
        let _ = next_conflict_id(&received);
        resolver.shutdown();
        assert_eq!(third.await.expect("任务").action, ConflictAction::Skip);
        assert_eq!(other.await.expect("任务").action, ConflictAction::Skip);
    });
}

/// H1 的原症状：点过一次「全部覆盖」之后，下一次提交（新批次）遇到同名文件必须重新问；
/// 老批次结束后记住的选择也被清掉。
#[test]
fn ui_resolver_forgets_apply_to_all_when_the_batch_ends() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(events, None));
    runtime.block_on(async {
        assert_eq!(resolver.respond_scope(ConflictAction::Skip, Some(20), 1), 0);
        // 批次 20 还在跑：后面的冲突按"全部跳过"直接答。
        let decision = resolver.decide(info_in(1, Some(20))).await;
        assert_eq!(decision.action, ConflictAction::Skip);
        assert!(received.try_recv().is_err());

        // 批次 20 里的任务都结束了。
        resolver.retain_batches(&std::collections::HashSet::new());

        // 之后同一个窗口里的新提交（批次 21）：重新弹面板。
        let next = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(9, Some(21))).await })
        };
        tokio::task::yield_now().await;
        let id = next_conflict_id(&received);
        assert!(resolver.respond(
            id,
            ConflictDecision {
                action: ConflictAction::Overwrite,
                apply_to_all: false,
                rename_to: None
            }
        ));
        assert_eq!(next.await.expect("任务").action, ConflictAction::Overwrite);
    });
}

/// 没有批次的任务（单独提交、重试过）：「全部应用」只管它自己，不会误伤别的任务。
#[test]
fn ui_resolver_without_a_batch_answers_only_its_own_job() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(events, None));
    runtime.block_on(async {
        let mine = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(1, None)).await })
        };
        let theirs = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(2, None)).await })
        };
        tokio::task::yield_now().await;
        let _ = next_conflict_id(&received);
        let _ = next_conflict_id(&received);
        assert_eq!(
            resolver.respond_scope(ConflictAction::Overwrite, None, 1),
            1
        );
        assert_eq!(mine.await.expect("任务").action, ConflictAction::Overwrite);
        assert!(!theirs.is_finished());
        // 没有批次，也就没有"以后都这样"的记忆。
        resolver.shutdown();
        assert_eq!(theirs.await.expect("任务").action, ConflictAction::Skip);
    });
}

/// M4：任务取消 / 结束时，它还在等的提问按跳过收尾，别的任务的不受影响。
#[test]
fn ui_resolver_cancel_job_answers_only_that_jobs_questions() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(events, None));
    runtime.block_on(async {
        let cancelled = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(1, Some(30))).await })
        };
        let kept = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(info_in(2, Some(30))).await })
        };
        tokio::task::yield_now().await;
        let _ = next_conflict_id(&received);
        let _ = next_conflict_id(&received);

        resolver.cancel_job(1);
        assert_eq!(cancelled.await.expect("任务").action, ConflictAction::Skip);
        assert!(!kept.is_finished(), "别的任务的提问不能被牵连");
        resolver.shutdown();
        let _ = kept.await;
    });
}

#[test]
fn same_scope_follows_batch_or_job() {
    assert!(same_scope(Some(1), 5, Some(1), 9));
    assert!(!same_scope(Some(1), 5, Some(2), 5));
    assert!(!same_scope(Some(1), 5, None, 5));
    assert!(same_scope(None, 5, None, 5));
    assert!(!same_scope(None, 5, None, 6));
    assert!(!same_scope(None, 5, Some(1), 5));
}

#[test]
fn ui_resolver_automation_answers_without_a_panel() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = UiConflictResolver::new(events, Some(ConflictAutomation::SkipAll));
    let decision = runtime.block_on(async { resolver.decide(conflict_info()).await });
    assert_eq!(decision.action, ConflictAction::Skip);
    assert!(decision.apply_to_all);
    assert!(received.try_recv().is_err());
}

#[test]
fn ui_resolver_shutdown_answers_pending_with_skip() {
    let runtime = test_runtime();
    let (events, received) = std::sync::mpsc::channel();
    let resolver = Arc::new(UiConflictResolver::new(events, None));
    runtime.block_on(async {
        let task = {
            let resolver = Arc::clone(&resolver);
            tokio::spawn(async move { resolver.decide(conflict_info()).await })
        };
        tokio::task::yield_now().await;
        let _ = next_conflict_id(&received);

        // 窗口关掉：正在等的按跳过收尾，之后的冲突也不再等界面。
        resolver.shutdown();
        let decision = task.await.expect("decide 任务");
        assert_eq!(decision.action, ConflictAction::Skip);

        let decision = resolver.decide(conflict_info()).await;
        assert_eq!(decision.action, ConflictAction::Skip);
        assert!(received.try_recv().is_err());
    });
}

// ---- 联网：真实引擎 + 真实服务器上验证「全部应用」只管一次提交 ----

/// 跑到这批任务全部结束；期间把窗口事件通道里的提问交给 `on_conflict` 回答。
/// 返回收到的提问数。
async fn drive_batch(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<TransferEvent>,
    received: &std::sync::mpsc::Receiver<SftpEvent>,
    mut pending: std::collections::HashSet<TransferId>,
    mut on_conflict: impl FnMut(u64, &ConflictInfo),
) -> usize {
    let mut prompts = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    while !pending.is_empty() {
        assert!(std::time::Instant::now() < deadline, "批次 90 秒内没结束");
        while let Ok(event) = received.try_recv() {
            if let SftpEvent::Conflict { id, info } = event {
                prompts += 1;
                on_conflict(id, &info);
            }
        }
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            let (TransferEvent::Progress(report) | TransferEvent::Finished(report)) = event;
            if report.state.is_terminal() {
                pending.remove(&report.id);
            }
        }
    }
    // 最后一批提问可能在结束事件之后才被取走。
    while let Ok(event) = received.try_recv() {
        if let SftpEvent::Conflict { id, info } = event {
            prompts += 1;
            on_conflict(id, &info);
        }
    }
    prompts
}

/// H1 联网：同一个窗口里先后两次提交，每次都遇到 3 个同名文件。
/// 第一次点「全部覆盖」只管第一次；第二次必须重新问，不能被悄悄覆盖。
#[test]
fn live_apply_to_all_covers_one_submission_only() {
    let Some(live) = crate::live_support::LiveSftp::connect("batch") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let local = ScratchDir::new("batch-live");
    let source = local.path().join("tree");
    std::fs::create_dir_all(&source).expect("建本地目录");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(source.join(name), format!("v1 {name}")).expect("写本地文件");
    }
    let remote_root = live.path("tree");
    let channel = Arc::clone(&live.channel);

    runtime.block_on(async {
        let provider = Arc::new(SftpChannelProvider::new(Arc::clone(&live.connection), None));
        let endpoints = Arc::new(SftpEndpoints::new(provider));
        let (sender, received) = std::sync::mpsc::channel();
        let resolver = Arc::new(UiConflictResolver::new(sender, None));
        let decision_maker: Arc<dyn ConflictResolver> = resolver.clone();
        let (engine, mut events) =
            TransferEngine::new(endpoints, decision_maker, TransferOptions::default());
        let job = TransferJob {
            direction: Direction::Upload,
            source: source.display().to_string(),
            target: remote_root.clone(),
            recursive: true,
            peer: None,
        };

        // 第 0 次：目标不存在，没有冲突。
        let id = engine.submit(job.clone());
        let prompts = drive_batch(&mut events, &received, [id].into(), |_, _| {}).await;
        assert_eq!(prompts, 0, "第一次上传不该有冲突");

        // 本地内容改了，后面的覆盖才看得出来。
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(source.join(name), format!("v2 {name}")).expect("改本地文件");
        }

        // 第 1 次提交（批次 A）：3 个同名文件。第一个提问点「全部覆盖」。
        let batch_a = engine.new_batch();
        let id = engine.submit_in_batch(job.clone(), &batch_a);
        let resolver_ref = Arc::clone(&resolver);
        let prompts = drive_batch(&mut events, &received, [id].into(), |_, info| {
            resolver_ref.respond_scope(ConflictAction::Overwrite, info.batch, info.job);
        })
        .await;
        assert!(
            (1..=3).contains(&prompts),
            "批次 A 至少问一次、最多问 3 次（并发提问会一起被回答），实际 {prompts}"
        );
        let a = fs::read_bytes(&channel, &format!("{remote_root}/a.txt"), 1024)
            .await
            .expect("读 a");
        assert_eq!(a, b"v2 a.txt", "批次 A 的全部覆盖应当生效");

        // 批次 A 结束：界面会清掉它记住的选择。
        resolver.retain_batches(&std::collections::HashSet::new());

        // 第 2 次提交（批次 B）：同样的 3 个同名文件，这次必须重新问。
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(source.join(name), format!("v3 {name}")).expect("再改本地文件");
        }
        let batch_b = engine.new_batch();
        let id = engine.submit_in_batch(job.clone(), &batch_b);
        let resolver_ref = Arc::clone(&resolver);
        let prompts = drive_batch(&mut events, &received, [id].into(), |_, info| {
            // 这次选「全部跳过」。
            resolver_ref.respond_scope(ConflictAction::Skip, info.batch, info.job);
        })
        .await;
        assert!(prompts >= 1, "第二次提交必须重新问，实际提问数 {prompts}");
        let a = fs::read_bytes(&channel, &format!("{remote_root}/a.txt"), 1024)
            .await
            .expect("读 a");
        assert_eq!(a, b"v2 a.txt", "批次 B 选了全部跳过，远端应该保持 v2");
        resolver.shutdown();
    });
}

// ---- H3 / M6 ----

#[test]
fn delete_outcomes_report_what_was_and_was_not_removed() {
    assert_eq!(
        delete_outcome_text("a.txt", &DeleteOutcome::Done(1)),
        "已删除：a.txt"
    );
    assert!(delete_outcome_text("tree", &DeleteOutcome::Done(12)).contains("12 项"));
    let cancelled = delete_outcome_text(
        "tree",
        &DeleteOutcome::Cancelled {
            deleted: 20,
            remaining: Some(44),
        },
    );
    assert!(
        cancelled.contains("已删 20 项") && cancelled.contains("不回滚"),
        "{cancelled}"
    );
    assert!(cancelled.contains("还剩 44 项"), "{cancelled}");
    let capped = delete_outcome_text(
        "tree",
        &DeleteOutcome::Cancelled {
            deleted: 1,
            remaining: Some(100_000),
        },
    );
    assert!(capped.contains("至少"), "{capped}");
    let unknown = delete_outcome_text(
        "tree",
        &DeleteOutcome::Cancelled {
            deleted: 3,
            remaining: None,
        },
    );
    assert!(unknown.contains("没能数出来"), "{unknown}");
    let failed = delete_outcome_text(
        "tree",
        &DeleteOutcome::Failed {
            deleted: 2,
            message: "没有权限".to_owned(),
        },
    );
    assert!(
        failed.contains("已删 2 项") && failed.contains("没有权限"),
        "{failed}"
    );
}

#[test]
fn closing_asks_only_while_work_is_unfinished() {
    assert_eq!(close_decision(false, 0), CloseDecision::Close);
    assert_eq!(close_decision(false, 2), CloseDecision::Ask);
    assert_eq!(close_decision(true, 2), CloseDecision::Close);
}

/// H3 联网：真实服务器上建一棵 64 项的树，删到一半取消——停在当前项、已删的不回滚、
/// 剩余数对得上；再删一次把剩下的清完。全程只在 `/tmp/zssh-f3-*` 自己的目录里。
#[test]
fn live_recursive_delete_can_be_cancelled_and_reports_the_remainder() {
    let Some(live) = crate::live_support::LiveSftp::connect("delete") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let root = live.path("tree");
    runtime.block_on(async {
        channel.create_dir(&root).await.expect("建根");
        let mut total = 1u64;
        for sub in ["d1", "d2", "d3"] {
            let dir = format!("{root}/{sub}");
            channel.create_dir(&dir).await.expect("建子目录");
            total += 1;
            for n in 0..20 {
                fs::write_bytes(&channel, &format!("{dir}/f{n}.txt"), b"x", u64::MAX)
                    .await
                    .expect("写文件");
                total += 1;
            }
        }
        assert_eq!(total, 64);

        let deleter = RecursiveDelete::new();
        let cancel = deleter.cancel_handle();
        let outcome = run_delete(&channel, &root, &deleter, |progress| {
            if progress.deleted >= 20 {
                cancel.cancel();
            }
        })
        .await;
        let DeleteOutcome::Cancelled { deleted, remaining } = outcome else {
            panic!("应该是取消：{outcome:?}");
        };
        assert!(
            (20..=21).contains(&deleted),
            "取消后停在当前项，不会多删很多：{deleted}"
        );
        assert_eq!(
            remaining,
            Some(total - deleted),
            "已删 + 剩余 = 总数，已删的不回滚"
        );
        assert!(
            fs::stat(&channel, &root, false).await.is_ok(),
            "根还在：没删完"
        );

        // 再删一次：把剩下的清干净。
        let outcome = run_delete(&channel, &root, &RecursiveDelete::new(), |_| {}).await;
        assert_eq!(outcome, DeleteOutcome::Done(total - deleted));
        assert!(
            matches!(
                fs::stat(&channel, &root, false).await,
                Err(SftpError::NotFound { .. })
            ),
            "删完之后根不在了"
        );
    });
}

// ---- 联网：本卡新增的远端操作（真实服务器，`/tmp/zssh-live-*`，测试自己的目录，结束清掉）----

/// 在服务器上跑一条 shell 命令（造符号链接这类 SFTP 做不了的准备工作）。
async fn live_shell(connection: &zssh_core::ssh::Connection, command: &str) {
    let result = connection
        .exec_capture(command, Duration::from_secs(20), 16 * 1024)
        .await
        .expect("执行命令");
    assert_eq!(result.exit_code, Some(0), "命令失败：{command}");
}

#[test]
fn live_recursive_chmod_changes_the_tree_and_never_follows_links() {
    let Some(live) = crate::live_support::LiveSftp::connect("chmod") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let root = live.path("tree");
    let outside = live.path("outside.txt");
    runtime.block_on(async {
        channel.create_dir(&root).await.expect("建根");
        channel
            .create_dir(&format!("{root}/sub"))
            .await
            .expect("建子目录");
        for path in [
            format!("{root}/a.txt"),
            format!("{root}/sub/b.txt"),
            outside.clone(),
        ] {
            fs::write_bytes(&channel, &path, b"x", u64::MAX)
                .await
                .expect("写文件");
        }
        // 链接指到树外面的文件：递归改权限不能顺着它改到别人的文件。
        live_shell(
            &live.connection,
            &format!("chmod 600 '{outside}' && ln -s '{outside}' '{root}/escape'"),
        )
        .await;

        let summary = super::chmod::recursive_chmod(
            channel.as_ref(),
            &root,
            0o750,
            &std::sync::atomic::AtomicBool::new(false),
            |_| {},
        )
        .await
        .expect("递归改权限");
        assert_eq!(summary.changed, 4, "根、子目录、两个文件：{summary:?}");
        assert_eq!(summary.skipped_links, 1);
        assert!(summary.is_clean());

        let mode_of = |path: String| {
            let channel = Arc::clone(&channel);
            async move {
                fs::stat(&channel, &path, false)
                    .await
                    .expect("读属性")
                    .permissions
                    .map(|m| m & 0o777)
            }
        };
        assert_eq!(mode_of(root.clone()).await, Some(0o750));
        assert_eq!(mode_of(format!("{root}/sub")).await, Some(0o750));
        assert_eq!(mode_of(format!("{root}/sub/b.txt")).await, Some(0o750));
        assert_eq!(
            mode_of(outside.clone()).await,
            Some(0o600),
            "树外面的文件不能被动"
        );
    });
}

#[test]
fn live_remote_size_counts_files_folders_bytes_and_skips_links() {
    let Some(live) = crate::live_support::LiveSftp::connect("size") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let root = live.path("tree");
    runtime.block_on(async {
        channel.create_dir(&root).await.expect("建根");
        channel
            .create_dir(&format!("{root}/sub"))
            .await
            .expect("建子目录");
        for (name, body) in [
            ("a.txt", &b"1"[..]),
            ("sub/b.txt", b"22"),
            ("sub/c.txt", b"333"),
        ] {
            fs::write_bytes(&channel, &format!("{root}/{name}"), body, u64::MAX)
                .await
                .expect("写文件");
        }
        live_shell(&live.connection, &format!("ln -s a.txt '{root}/link'")).await;

        let provider = Arc::new(SftpChannelProvider::new(live.connection.clone(), None));
        let endpoints = SftpEndpoints::new(provider);
        let totals = zssh_core::sftp::size::remote_size(
            &endpoints,
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            |_| {},
        )
        .await
        .expect("数大小");
        assert_eq!((totals.files, totals.dirs, totals.bytes), (3, 2, 6));
        assert_eq!(totals.links, 1, "链接跳过并计数");
        assert_eq!(totals.unreadable, 0);

        // 取消：置位后返回 Cancelled。
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let cancelled = zssh_core::sftp::size::remote_size(&endpoints, &root, &cancel, |_| {})
            .await
            .expect_err("取消");
        assert!(matches!(cancelled, SftpError::Cancelled));
    });
}

#[test]
fn live_compress_then_extract_round_trips_into_a_new_folder() {
    let Some(live) = crate::live_support::LiveSftp::connect("archive") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let connection = Arc::clone(&live.connection);
    let dir = live.dir.clone();
    runtime.block_on(async {
        let caps = zssh_core::sftp::archive::probe_caps(&connection).await;
        if !(caps.posix && caps.tar) {
            println!("跳过 live_compress_then_extract：服务器上没有 tar");
            return;
        }
        fs::write_bytes(&channel, &format!("{dir}/one.txt"), b"first", u64::MAX)
            .await
            .expect("写一");
        fs::write_bytes(&channel, &format!("{dir}/two.txt"), b"second", u64::MAX)
            .await
            .expect("写二");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let names = vec!["one.txt".to_owned(), "two.txt".to_owned()];
        let outcome = zssh_core::sftp::archive::compress(
            connection.as_ref(),
            channel.as_ref(),
            &zssh_core::sftp::archive::CompressRequest {
                dir: &dir,
                names: &names,
                format: zssh_core::sftp::archive::CompressFormat::TarGz,
                owner: "g08atest",
                server: channel.server_key(),
            },
            &cancel,
            None,
        )
        .await
        .expect("压缩");
        assert!(outcome.name.ends_with(".tar.gz"), "{}", outcome.name);

        // 解压到还不存在的文件夹：先检查（没有同名文件），再解，临时文件夹改名到位。
        let dest = format!("{dir}/out");
        let plan = zssh_core::sftp::archive::plan_extract(
            connection.as_ref(),
            channel.as_ref(),
            &outcome.path,
            &dest,
            &cancel,
        )
        .await
        .expect("检查压缩包");
        assert_eq!((plan.total_files, plan.conflict_count), (2, 0));
        assert!(!plan.dest_exists);
        let done = zssh_core::sftp::archive::extract(
            connection.as_ref(),
            channel.as_ref(),
            &zssh_core::sftp::archive::ExtractRequest {
                archive: &outcome.path,
                dest: &dest,
                policy: zssh_core::sftp::archive::ConflictPolicy::SkipExisting,
                plan_fingerprint: plan.fingerprint,
                plan_dest_exists: plan.dest_exists,
                plan_total_files: plan.total_files,
                plan_conflict_count: plan.conflict_count,
                owner: "g08atest",
                server: channel.server_key(),
            },
            &cancel,
        )
        .await
        .expect("解压");
        assert_eq!((done.extracted, done.skipped), (2, 0));
        let back = fs::read_bytes(&channel, &format!("{dest}/two.txt"), u64::MAX)
            .await
            .expect("读回来");
        assert_eq!(back, b"second");

        // 再检查一次：目标已存在、两个文件都冲突。
        let again = zssh_core::sftp::archive::plan_extract(
            connection.as_ref(),
            channel.as_ref(),
            &outcome.path,
            &dest,
            &cancel,
        )
        .await
        .expect("再检查");
        assert!(again.dest_exists);
        assert_eq!(again.conflict_count, 2);
    });
}

#[test]
fn live_rename_reports_an_existing_target_and_only_overwrites_when_told_to() {
    let Some(live) = crate::live_support::LiveSftp::connect("rename") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let channel = Arc::clone(&live.channel);
    let (from, to) = (live.path("from.txt"), live.path("to.txt"));
    runtime.block_on(async {
        fs::write_bytes(&channel, &from, b"new", u64::MAX)
            .await
            .expect("写来源");
        fs::write_bytes(&channel, &to, b"old", u64::MAX)
            .await
            .expect("写目标");
        // 对话框的第一步：不覆盖地试，服务器说已存在。
        let err = channel
            .rename(&from, &to, false)
            .await
            .expect_err("目标已存在");
        assert!(matches!(err, SftpError::AlreadyExists { .. }), "{err:?}");
        assert_eq!(
            fs::read_bytes(&channel, &to, u64::MAX)
                .await
                .expect("目标没动"),
            b"old"
        );
        // 用户确认覆盖之后才真的替换。
        channel.rename(&from, &to, true).await.expect("覆盖式改名");
        assert_eq!(
            fs::read_bytes(&channel, &to, u64::MAX).await.expect("读回"),
            b"new"
        );
        assert!(matches!(
            fs::stat(&channel, &from, false).await,
            Err(SftpError::NotFound { .. })
        ));
    });
}
