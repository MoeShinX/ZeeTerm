//! 迁移块的状态测试。"Z-SSH 的数据"都是临时目录里造的假数据（用 ZeeTerm 自己的库格式写，
//! 老版本 v11 库的读取在核心的 `zssh_import` 测试里覆盖）；不读也不碰真实的 Z-SSH / ZeeTerm 数据目录。
//!
//! 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]` 属性宏。

use std::path::{Path, PathBuf};

use zssh_core::model::{AuthMethod, HostDraft, IdentityDraft, IdentityKind, ProxyMode};
use zssh_core::store::Database;
use zssh_core::sync::engine::SyncEngine;
use zssh_core::sync::offline::OfflineImportMode;
use zssh_core::sync::sealer::DpapiSealer;
use zssh_core::sync::zssh_import::ZsshSource;

use super::view::{done_rows, found_text, preview_rows, preview_warnings};
use super::{
    Busy, Failure, FailureKind, Found, Loaded, MigrateEvent, MigrateState, Origin, Stage,
    file_request, is_dismissed, load_direct, load_file, pick_folder, run_import, set_dismissed,
    tip_visible,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zssh-migrate-demo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn host(name: &str, address: &str, identity_id: Option<i64>) -> HostDraft {
    HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: name.to_owned(),
        host: address.to_owned(),
        port: 22,
        username: "tester".to_owned(),
        auth_method: AuthMethod::Password,
        identity_id,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::Default,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

/// 假的"Z-SSH 数据目录"：data.db 里有两台主机、一条密码凭据，旁边有 known_hosts。
fn fake_zssh(root: &Path) -> ZsshSource {
    let dir = root.join("Z-ssh");
    std::fs::create_dir_all(&dir).unwrap();
    let db = Database::open(&dir.join("data.db")).unwrap();
    let identity = db
        .identity_save(&IdentityDraft {
            id: None,
            name: "部署密码".to_owned(),
            kind: IdentityKind::Password,
            key_path: None,
            secret: Some("not-a-real-password".to_owned()),
            passphrase: None,
        })
        .unwrap();
    db.host_save(&host("web-1", "192.0.2.10", Some(identity.id)))
        .unwrap();
    db.host_save(&host("db-1", "192.0.2.11", None)).unwrap();
    drop(db);
    std::fs::write(
        dir.join("known_hosts"),
        "192.0.2.10 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n",
    )
    .unwrap();
    zssh_core::sync::zssh_import::source_at(&dir).expect("假数据目录应当被认出来")
}

fn host_names(data_dir: &Path) -> Vec<String> {
    let db = Database::open(&data_dir.join("zssh.db")).unwrap();
    let mut names: Vec<String> = db
        .host_list()
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
    names.sort();
    names
}

// ---- 首启提示 ----

#[test]
fn the_tip_needs_zssh_data_an_empty_library_and_no_dismissal() {
    assert!(tip_visible(true, true, false));
    assert!(!tip_visible(false, true, false), "没找到 Z-SSH 数据不提示");
    assert!(!tip_visible(true, false, false), "库里已经有主机不提示");
    assert!(!tip_visible(true, true, true), "点过「不再提示」不提示");
}

#[test]
fn dismissal_is_remembered_in_the_local_database() {
    let db = Database::open_in_memory().unwrap();
    assert!(!is_dismissed(&db));
    set_dismissed(&db).unwrap();
    assert!(is_dismissed(&db));
}

#[test]
fn the_dismissal_is_not_a_synced_setting() {
    // 这台电脑自己的事：不能进同步包 / 离线备份，别的电脑才不会被"不再提示"。
    assert!(!zssh_core::sync::settings::is_synced(
        zssh_core::sync::zssh_import::MIGRATE_DISMISSED_KEY
    ));
}

// ---- 路径一：直接导入 ----

#[test]
fn a_folder_that_is_not_zssh_data_is_refused() {
    let root = temp_dir("folder");
    let source = fake_zssh(&root);
    assert!(pick_folder(&source.data_dir).is_ok());
    let err = pick_folder(&root).unwrap_err();
    assert!(err.contains("data.db"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn direct_import_previews_then_merges_and_makes_a_safety_backup() {
    let root = temp_dir("direct");
    let source = fake_zssh(&root);
    let data_dir = root.join("zeeterm");
    // ZeeTerm 里已有一台同名同地址的主机
    {
        std::fs::create_dir_all(&data_dir).unwrap();
        let db = Database::open(&data_dir.join("zssh.db")).unwrap();
        db.host_save(&host("web-1", "192.0.2.10", None)).unwrap();
    }

    let loaded = load_direct(&source, &data_dir, &DpapiSealer::for_test(), OfflineImportMode::Merge)
        .unwrap_or_else(|failure| panic!("{}", failure.message));
    assert_eq!(loaded.origin, Origin::Direct);
    assert_eq!(loaded.preview.source.hosts, 2);
    assert_eq!(loaded.preview.source.identities, 1);
    assert_eq!(loaded.preview.hosts.inserted, 2);
    assert_eq!(loaded.preview.duplicate_hosts, 1);
    assert_eq!(host_names(&data_dir), vec!["web-1"], "预览不写 ZeeTerm");
    assert!(!data_dir.join("backups").exists(), "预览不做备份");

    let report = run_import(
        &data_dir,
        &DpapiSealer::for_test(),
        &loaded.snapshot,
        OfflineImportMode::Merge,
    )
    .unwrap();
    assert_eq!(report.applied.hosts.inserted, 2);
    assert!(report.safety_backup.name.starts_with("zssh-backup-"));
    assert!(
        Path::new(&report.safety_backup.path).is_file(),
        "导入前的备份在"
    );
    assert_eq!(host_names(&data_dir), vec!["db-1", "web-1", "web-1"]);
    let db = Database::open(&data_dir.join("zssh.db")).unwrap();
    assert!(db.identity_list().unwrap().iter().all(|i| i.has_secret));
    assert!(is_dismissed(&db), "导入完成后欢迎页不再提示");
    drop(db);
    assert!(source.db_path.is_file());
    let _ = std::fs::remove_dir_all(&root);
}

// Z-SSH 的直接导入靠 DPAPI 解开原机密文；没有 DPAPI 的平台按设计拒绝，
// 所以这条只在 Windows 上验证。
#[cfg(windows)]
#[test]
fn replace_mode_matches_zssh_and_the_preview_says_what_will_be_deleted() {
    let root = temp_dir("replace");
    let source = fake_zssh(&root);
    let data_dir = root.join("zeeterm");
    std::fs::create_dir_all(&data_dir).unwrap();
    {
        let db = Database::open(&data_dir.join("zssh.db")).unwrap();
        db.host_save(&host("only-here", "198.51.100.7", None))
            .unwrap();
    }
    let loaded = load_direct(&source, &data_dir, &DpapiSealer::for_test(), OfflineImportMode::Replace).unwrap();
    assert_eq!(loaded.preview.hosts.deleted, 1);
    let warnings = preview_warnings(&loaded.preview, &loaded.origin);
    assert!(warnings.iter().any(|w| w.contains("删除")), "{warnings:?}");
    run_import(
        &data_dir,
        &DpapiSealer::for_test(),
        &loaded.snapshot,
        OfflineImportMode::Replace,
    )
    .unwrap();
    assert_eq!(host_names(&data_dir), vec!["db-1", "web-1"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn a_running_zssh_is_reported_as_in_use() {
    use std::os::windows::fs::OpenOptionsExt;

    let root = temp_dir("inuse");
    let source = fake_zssh(&root);
    let data_dir = root.join("zeeterm");
    // 像 Z-SSH 一样一直开着库文件（读写、允许别人读写）
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(3)
        .open(&source.db_path)
        .unwrap();
    let failure = load_direct(&source, &data_dir, &DpapiSealer::for_test(), OfflineImportMode::Merge)
        .err()
        .expect("应当失败");
    assert_eq!(failure.kind, FailureKind::InUse);
    assert!(
        failure.message.contains("先退出 Z-SSH"),
        "{}",
        failure.message
    );
    assert!(!data_dir.join("zssh.db").exists(), "什么都没写");
    drop(held);
    assert!(
        load_direct(&source, &data_dir, &DpapiSealer::for_test(), OfflineImportMode::Merge).is_ok(),
        "退出之后重试能读"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 路径二：备份文件 ----

/// 用 ZeeTerm 的离线导出造一份 .zsshbak（格式和 Z-SSH 0.2.14 的相同；0.2.14 样本的导入在核心测试里）。
fn make_backup(root: &Path, password: &str) -> PathBuf {
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let db = Database::open(&src_dir.join("zssh.db")).unwrap();
    db.host_save(&host("from-file", "192.0.2.20", None))
        .unwrap();
    let engine = SyncEngine::new(&db, &DpapiSealer::for_test(), "dev-src", &src_dir, None, None);
    let bytes = engine.export_offline(password).unwrap();
    let path = root.join("zeeterm-20261103.zsshbak");
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn a_backup_file_needs_the_right_password_and_changes_nothing_when_wrong() {
    let root = temp_dir("file");
    let path = make_backup(&root, "file-password-1");
    let data_dir = root.join("zeeterm");
    let merge = OfflineImportMode::Merge;

    let wrong = load_file(&path, "not-the-password", &data_dir, &DpapiSealer::for_test(), merge)
        .err()
        .expect("密码不对应当失败");
    assert_eq!(wrong.kind, FailureKind::Other);
    assert!(wrong.message.contains("密码不对"), "{}", wrong.message);
    assert_eq!(wrong.origin, Origin::File);
    assert!(!data_dir.join("zssh.db").exists(), "预览阶段不动本机");

    let loaded = load_file(&path, "file-password-1", &data_dir, &DpapiSealer::for_test(), merge)
        .unwrap_or_else(|f| panic!("{}", f.message));
    assert_eq!(loaded.origin, Origin::File);
    assert_eq!(loaded.preview.hosts.inserted, 1);
    run_import(&data_dir, &DpapiSealer::for_test(), &loaded.snapshot, merge).unwrap();
    assert_eq!(host_names(&data_dir), vec!["from-file"]);

    let missing = load_file(
        &root.join("nope.zsshbak"),
        "x",
        &data_dir,
        &DpapiSealer::for_test(),
        merge,
    )
    .err()
    .unwrap();
    assert!(
        missing.message.contains("读取备份文件失败"),
        "{}",
        missing.message
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn file_requests_are_checked_before_anything_runs() {
    assert!(file_request("", "pw").unwrap_err().contains("选择"));
    assert!(file_request("  ", "pw").is_err());
    assert!(
        file_request("C:\\x.zsshbak", "  ")
            .unwrap_err()
            .contains("密码")
    );
    let (path, password) = file_request("  C:\\x.zsshbak ", " pw ").unwrap();
    assert_eq!(path, PathBuf::from("C:\\x.zsshbak"));
    assert_eq!(password, " pw ", "密码不裁剪（前后空格可能是密码的一部分）");
}

// ---- 状态机 ----

fn loaded_for_state(root: &Path, source: &ZsshSource, mode: OfflineImportMode) -> Box<Loaded> {
    let data_dir = root.join("zeeterm");
    Box::new(load_direct(source, &data_dir, &DpapiSealer::for_test(), mode).unwrap())
}

#[test]
fn the_state_walks_detect_preview_import_done() {
    let root = temp_dir("state");
    let mut state = MigrateState::default();
    assert_eq!(state.busy, Some(Busy::Detecting));
    assert_eq!(state.found, Found::Checking);

    let source = fake_zssh(&root);
    state.on_event(MigrateEvent::Detected(Some(source.clone())));
    assert_eq!(state.found, Found::Yes(source.clone()));
    assert!(state.busy.is_none());
    state.on_event(MigrateEvent::Detected(None));
    assert_eq!(state.found, Found::No);

    let data_dir = root.join("zeeterm");
    let loaded = loaded_for_state(&root, &source, OfflineImportMode::Merge);
    let snapshot = loaded.snapshot.clone();
    state.on_event(MigrateEvent::Loaded(loaded));
    assert!(matches!(state.stage, Stage::Preview(_)));

    // 合并：直接放行；换成替换：先要确认
    assert!(state.confirm_import());
    assert!(
        state.set_mode(OfflineImportMode::Replace),
        "有预览时换方式要重算"
    );
    assert!(!state.confirm_import(), "替换第一次只置为确认状态");
    assert!(state.replace_armed);
    assert!(state.confirm_import(), "第二次才放行");
    // 换回合并：确认作废
    state.replace_armed = true;
    assert!(state.set_mode(OfflineImportMode::Merge));
    assert!(!state.replace_armed);

    let report = run_import(&data_dir, &DpapiSealer::for_test(), &snapshot, OfflineImportMode::Merge).unwrap();
    state.on_event(MigrateEvent::Imported(Box::new(report)));
    match &state.stage {
        Stage::Done(done) => {
            assert_eq!(done.report.applied.hosts.inserted, 2);
            let rows = done_rows(&done.preview, &done.report);
            let hosts = rows.iter().find(|(label, _)| label == "主机").unwrap();
            assert!(hosts.1.contains("新增 2"), "{}", hosts.1);
            assert!(
                rows.iter()
                    .any(|(label, text)| label == "导入前的备份" && text.contains("zssh-backup-"))
            );
        }
        _ => panic!("导入后应当是结果页"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn failures_replace_the_stage_and_cancel_goes_back_to_idle() {
    let mut state = MigrateState::default();
    state.on_event(MigrateEvent::LoadFailed(Failure {
        kind: FailureKind::InUse,
        message: "Z-SSH 正在运行".to_owned(),
        origin: Origin::Direct,
    }));
    assert!(matches!(&state.stage, Stage::Failed(f) if f.kind == FailureKind::InUse));
    state.on_event(MigrateEvent::ImportFailed("写库失败".to_owned()));
    assert!(matches!(&state.stage, Stage::Failed(f) if f.kind == FailureKind::Other));
    state.cancel();
    assert!(matches!(state.stage, Stage::Idle));
    // 没有预览时换方式不用重算
    assert!(!state.set_mode(OfflineImportMode::Replace));
}

// ---- 文字 ----

#[test]
fn texts_describe_what_was_found_and_what_will_happen() {
    let root = temp_dir("text");
    let source = fake_zssh(&root);
    assert!(found_text(&Found::Checking, None).contains("正在查找"));
    let yes = found_text(&Found::Yes(source.clone()), None);
    assert!(
        yes.contains("已找到 Z-SSH 的数据") && yes.contains("不会改动"),
        "{yes}"
    );
    let no = found_text(&Found::No, Some("这个文件夹里没有 Z-SSH 的数据库"));
    assert!(
        no.contains("指定文件夹") && no.contains("没有 Z-SSH 的数据库"),
        "{no}"
    );

    let loaded = loaded_for_state(&root, &source, OfflineImportMode::Merge);
    let rows = preview_rows(&loaded.preview);
    let labels: Vec<&str> = rows.iter().map(|(label, _)| *label).collect();
    assert_eq!(
        labels,
        vec![
            "主机",
            "分组",
            "凭据（含密码和私钥）",
            "代理",
            "已信任的主机指纹",
            "快捷命令",
            "可同步的设置",
            "冲突"
        ]
    );
    assert_eq!(rows[0].1, "2 台：新增 2，更新 0");
    assert!(rows[4].1.contains("1 条"), "{}", rows[4].1);
    let notes = preview_warnings(&loaded.preview, &Origin::Direct);
    assert!(
        notes.iter().any(|n| n.contains("端口转发规则")),
        "{notes:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn done_rows_count_skipped_records() {
    // Z-SSH 的数据导入两次：第二次全部"已有且相同"，只算跳过。
    let root = temp_dir("twice");
    let source = fake_zssh(&root);
    let data_dir = root.join("zeeterm");
    let merge = OfflineImportMode::Merge;
    let first = load_direct(&source, &data_dir, &DpapiSealer::for_test(), merge).unwrap();
    run_import(&data_dir, &DpapiSealer::for_test(), &first.snapshot, merge).unwrap();
    let second = load_direct(&source, &data_dir, &DpapiSealer::for_test(), merge).unwrap();
    assert_eq!(second.preview.hosts.inserted, 0);
    let report = run_import(&data_dir, &DpapiSealer::for_test(), &second.snapshot, merge).unwrap();
    let rows = done_rows(&second.preview, &report);
    let hosts = rows.iter().find(|(label, _)| label == "主机").unwrap();
    assert!(
        hosts.1.contains("新增 0") && hosts.1.contains("跳过 2"),
        "{}",
        hosts.1
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_debug_hook_replaces_every_other_candidate() {
    use super::env_for;
    let own = PathBuf::from("C:/own");
    let appdata = Some(PathBuf::from("C:/some/Roaming"));
    // 没有钩子：手动指定的最优先，然后是 %APPDATA%\Z-ssh
    let env = env_for(Some("C:/picked".into()), None, appdata.clone(), own.clone());
    assert_eq!(
        env.candidate_dirs(),
        vec![
            PathBuf::from("C:/picked"),
            PathBuf::from("C:/some/Roaming").join("Z-ssh")
        ]
    );
    // 有钩子（假数据目录）：只剩它，不会去 stat 真实的目录
    let env = env_for(None, Some("C:/fake".into()), appdata, own.clone());
    assert_eq!(env.candidate_dirs(), vec![PathBuf::from("C:/fake")]);
    assert_eq!(env.own_data_dir, Some(own));
    // 没有 APPDATA：什么候选都没有（不猜路径）
    assert!(
        env_for(None, None, None, PathBuf::from("C:/own"))
            .candidate_dirs()
            .is_empty()
    );
}
