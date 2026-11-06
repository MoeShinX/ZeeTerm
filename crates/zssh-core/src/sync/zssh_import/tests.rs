//! 从 Z-SSH 迁移的测试。Z-SSH 的数据全是在临时目录里造的假数据（按它 0.2.14 的库结构：
//! 迁移 1～11），不读也不碰真实的 Z-SSH 数据目录。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use super::*;
use crate::store::Database;
use crate::sync::crypto::{seal_with, KdfParams, NONCE_BYTES, SALT_BYTES};
use crate::sync::engine::SyncEngine;
use crate::sync::offline::OfflineImportMode;
#[cfg(windows)]
use crate::sync::sealer::DpapiSealer;
use crate::sync::sealer::{SecretSealer, TestSealer};
use crate::test_util::TestDir;

const QUICK_COMMANDS: &str = r#"[{"id":"q1","name":"磁盘","command":"df -h","enter":true},{"id":"q2","name":"进程","command":"ps aux","enter":true}]"#;
const KNOWN_HOSTS: &str = "web-1.example.invalid ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n";

/// 按 Z-SSH 0.2.14 的库结构（迁移 1～11）造一份假数据，返回数据目录。
fn fake_zssh(root: &Path, sealer: &dyn SecretSealer) -> PathBuf {
    let dir = root.join("Z-ssh");
    std::fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join(ZSSH_DB_FILE)).unwrap();
    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= 11) {
        conn.execute_batch(sql).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )
        .unwrap();
    }
    let seal = |text: &str| sealer.seal(text.as_bytes()).unwrap();
    conn.execute(
        "INSERT INTO groups (id, name, parent_id, sort, uid, updated_at) VALUES \
         (1, '生产', NULL, 0, 'g-prod', 100), (2, '数据库', 1, 1, 'g-db', 100)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO identities (id, name, kind, key_path, secret_blob, uid, updated_at) \
         VALUES (1, '部署密码', 'password', NULL, ?1, 'i-pw', 100), \
                (2, '私钥', 'key_inline', NULL, ?2, 'i-key', 100)",
        params![seal("pw-example-1"), seal("FAKE-PRIVATE-KEY-BODY")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO proxies (id, name, kind, host, port, username, secret_blob, uid, updated_at) \
         VALUES (1, '办公室代理', 'socks5', 'proxy.example.invalid', 1080, 'proxy-user', ?1, 'p-1', 100)",
        params![seal("proxy-pass-1")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hosts (id, group_id, name, host, port, username, auth_method, identity_id, \
         proxy_id, jump_host_id, tags, sort, created_at, updated_at, uid, proxy_mode) VALUES \
         (1, 1, 'web-1', 'web-1.example.invalid', 22, 'deploy', 'password', 1, NULL, NULL, \
          '[\"prod\"]', 0, 90, 100, 'h-web', 'default'), \
         (2, 2, 'db-1', '192.0.2.10', 2222, 'dba', 'key', 2, 1, 1, NULL, 1, 90, 100, 'h-db', 'custom')",
        [],
    )
    .unwrap();
    let quick = serde_json::to_string(QUICK_COMMANDS).unwrap();
    for (key, value) in [
        ("quick_commands", quick.as_str()),
        ("ui_theme_mode", "\"dark\""),
        ("default_proxy_id", "1"),
        // 本机布局：不该被带过来
        ("layout.vault_view_mode", "\"list\""),
    ] {
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .unwrap();
    }
    drop(conn);
    std::fs::write(dir.join("known_hosts"), KNOWN_HOSTS).unwrap();
    dir
}

fn env_with_appdata(appdata: &Path) -> DetectEnv {
    DetectEnv {
        appdata: Some(appdata.to_path_buf()),
        ..DetectEnv::default()
    }
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ---- 探测 ----

#[test]
fn finds_the_zssh_dir_under_appdata() {
    let root = TestDir::new("zi-detect");
    let dir = fake_zssh(&root, &TestSealer(1));
    let found = detect(&env_with_appdata(&root)).expect("应该找到");
    assert_eq!(found.data_dir, dir);
    assert_eq!(found.db_path, dir.join("data.db"));
    assert!(found.db_bytes > 0);
    assert!(found.modified_at.is_some());
}

#[test]
fn nothing_is_found_without_a_database() {
    let root = TestDir::new("zi-none");
    assert!(
        detect(&env_with_appdata(&root)).is_none(),
        "没有 Z-ssh 目录"
    );
    std::fs::create_dir_all(root.join("Z-ssh")).unwrap();
    assert!(detect(&env_with_appdata(&root)).is_none(), "目录里没有库");
    std::fs::write(root.join("Z-ssh").join("data.db"), b"").unwrap();
    assert!(detect(&env_with_appdata(&root)).is_none(), "空文件不算");
    assert!(detect(&DetectEnv::default()).is_none(), "没有 APPDATA");
}

#[test]
fn an_explicit_dir_wins_and_zeeterms_own_dir_is_never_a_source() {
    let appdata = TestDir::new("zi-appdata");
    let other = TestDir::new("zi-other");
    let default_dir = fake_zssh(&appdata, &TestSealer(1));
    let custom_dir = fake_zssh(&other, &TestSealer(1));

    let env = DetectEnv {
        explicit_dir: Some(custom_dir.clone()),
        appdata: Some(appdata.to_path_buf()),
        own_data_dir: None,
    };
    assert_eq!(detect(&env).unwrap().data_dir, custom_dir);

    // 两个产品的数据目录被设成同一个：自己不能导入自己，退回下一个候选。
    let env = DetectEnv {
        own_data_dir: Some(custom_dir),
        ..env
    };
    assert_eq!(detect(&env).unwrap().data_dir, default_dir);
    let env = DetectEnv {
        own_data_dir: Some(default_dir),
        explicit_dir: None,
        ..env
    };
    assert!(detect(&env).is_none());
}

#[test]
fn same_dir_ignores_case_and_trailing_separators() {
    assert!(same_dir(
        Path::new(r"C:\No\Such\Dir"),
        Path::new(r"c:\no\such\dir\")
    ));
    assert!(!same_dir(
        Path::new(r"C:\No\Such\A"),
        Path::new(r"C:\No\Such\B")
    ));
}

#[test]
fn spaces_and_non_ascii_in_the_path_are_fine() {
    let root = TestDir::new("zi-unicode");
    let appdata = root.join("张 三").join("Roaming");
    let dir = fake_zssh(&appdata, &TestSealer(1));
    let found = detect(&env_with_appdata(&appdata)).unwrap();
    assert_eq!(found.data_dir, dir);
    let scratch = root.join("数据 目录");
    let read = read_zssh(&found, &TestSealer(1), &scratch).unwrap();
    assert_eq!(read.snapshot.hosts.len(), 2);
}

// ---- 读库 ----

#[test]
fn reads_hosts_groups_credentials_proxies_fingerprints_and_settings() {
    let root = TestDir::new("zi-read");
    let sealer = TestSealer(7);
    let dir = fake_zssh(&root, &sealer);
    let scratch = TestDir::new("zi-read-scratch");
    let source = source_at(&dir).unwrap();
    let read = read_zssh(&source, &sealer, &scratch).unwrap();
    assert_eq!(read.schema_version, 11, "0.2.14 的库是迁移版本 11");
    assert!(read.notes.is_empty(), "{:?}", read.notes);
    let snap = &read.snapshot;

    assert_eq!(snap.groups.len(), 2);
    let db_group = snap.groups.iter().find(|g| g.uid == "g-db").unwrap();
    assert_eq!(db_group.parent_uid.as_deref(), Some("g-prod"));

    assert_eq!(snap.hosts.len(), 2);
    let web = snap.hosts.iter().find(|h| h.uid == "h-web").unwrap();
    assert_eq!(
        (web.host.as_str(), web.port, web.username.as_str()),
        ("web-1.example.invalid", 22, "deploy")
    );
    assert_eq!(web.group_uid.as_deref(), Some("g-prod"));
    assert_eq!(web.identity_uid.as_deref(), Some("i-pw"));
    assert_eq!(web.tags, vec!["prod".to_owned()]);
    assert_eq!(web.protocol, "ssh", "0.2.14 没有协议字段，按 SSH 读");
    let db = snap.hosts.iter().find(|h| h.uid == "h-db").unwrap();
    assert_eq!(db.jump_host_uid.as_deref(), Some("h-web"));
    assert_eq!(db.proxy_uid.as_deref(), Some("p-1"));
    assert_eq!(db.proxy_mode, "custom");

    // 密码、私钥、代理密码都在这里解成了明文（导入时再用 ZeeTerm 这边的密钥重新加密）
    let secret_of = |uid: &str| {
        snap.identities
            .iter()
            .find(|i| i.uid == uid)
            .and_then(|i| i.secret.clone())
    };
    assert_eq!(secret_of("i-pw").as_deref(), Some("pw-example-1"));
    assert_eq!(secret_of("i-key").as_deref(), Some("FAKE-PRIVATE-KEY-BODY"));
    assert_eq!(snap.proxies.len(), 1);
    assert_eq!(snap.proxies[0].secret.as_deref(), Some("proxy-pass-1"));
    assert_eq!(snap.proxies[0].username.as_deref(), Some("proxy-user"));

    assert_eq!(snap.known_hosts.len(), 1);
    assert_eq!(snap.known_hosts[0].host, "web-1.example.invalid");

    // 快捷命令和主题是可同步的设置；本机布局不带；默认代理的本机 id 换成了 uid
    assert!(snap.settings.contains_key("quick_commands"));
    assert!(snap.settings.contains_key("ui_theme_mode"));
    assert!(!snap.settings.contains_key("layout.vault_view_mode"));
    assert_eq!(
        snap.settings.get("default_proxy_id"),
        Some(&serde_json::Value::String("p-1".to_owned()))
    );
    assert!(snap.tombstones.is_empty());
}

// 直接读 Z-SSH 的原机密文靠 DPAPI（当前 Windows 用户）。没有 DPAPI 的平台
// 走加密导出/导入，这条只在 Windows 上验证。
#[cfg(windows)]
#[test]
fn the_real_dpapi_secrets_of_the_same_windows_user_can_be_read() {
    // 真正的 Z-SSH 用 DPAPI 加密；同一个 Windows 用户下 ZeeTerm 直接解得开。
    // 这里的"Z-SSH 库"是测试里用同一个 DPAPI（同一进程用户）造的，不是真实的 Z-SSH 数据。
    let root = TestDir::new("zi-dpapi");
    let dir = fake_zssh(&root, &DpapiSealer);
    let scratch = TestDir::new("zi-dpapi-scratch");
    let read = read_zssh(&source_at(&dir).unwrap(), &DpapiSealer, &scratch).unwrap();
    let pw = read
        .snapshot
        .identities
        .iter()
        .find(|i| i.uid == "i-pw")
        .and_then(|i| i.secret.clone());
    assert_eq!(pw.as_deref(), Some("pw-example-1"));
}

#[test]
fn reading_never_modifies_zssh_and_leaves_no_scratch_behind() {
    let root = TestDir::new("zi-readonly");
    let sealer = TestSealer(7);
    let dir = fake_zssh(&root, &sealer);
    let before_db = std::fs::read(dir.join("data.db")).unwrap();
    let before_files = entries(&dir);
    let scratch = TestDir::new("zi-readonly-scratch");
    read_zssh(&source_at(&dir).unwrap(), &sealer, &scratch).unwrap();
    assert_eq!(std::fs::read(dir.join("data.db")).unwrap(), before_db);
    assert_eq!(entries(&dir), before_files, "原目录里没有多出任何文件");
    assert!(entries(&scratch).is_empty(), "临时副本已删除");
}

#[test]
fn a_leftover_wal_file_is_read_through_the_copy() {
    // Z-SSH 上次没正常退出：改动还在 -wal 里。用 WAL 模式造一份，不检查点就关不掉连接，
    // 所以直接把"连接还开着时"的文件复制出来当作崩溃后的现场。
    let root = TestDir::new("zi-wal");
    let sealer = TestSealer(7);
    let dir = fake_zssh(&root, &sealer);
    let live = Connection::open(dir.join("data.db")).unwrap();
    let mode: String = live
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    live.execute(
        "UPDATE hosts SET name = 'web-renamed' WHERE uid = 'h-web'",
        [],
    )
    .unwrap();
    let crashed = root.join("crashed").join("Z-ssh");
    std::fs::create_dir_all(&crashed).unwrap();
    std::fs::copy(dir.join("data.db"), crashed.join("data.db")).unwrap();
    std::fs::copy(dir.join("data.db-wal"), crashed.join("data.db-wal")).unwrap();
    drop(live);

    let scratch = TestDir::new("zi-wal-scratch");
    let read = read_zssh(&source_at(&crashed).unwrap(), &sealer, &scratch).unwrap();
    assert!(
        read.snapshot.hosts.iter().any(|h| h.name == "web-renamed"),
        "-wal 里的改动也读到了"
    );
}

#[test]
fn credentials_from_another_windows_user_are_reported_not_dropped() {
    let root = TestDir::new("zi-otheruser");
    let dir = fake_zssh(&root, &TestSealer(1));
    let scratch = TestDir::new("zi-otheruser-scratch");
    // 用另一把"DPAPI"去解：和换了 Windows 用户一样
    let err = read_zssh(&source_at(&dir).unwrap(), &TestSealer(2), &scratch).unwrap_err();
    assert!(matches!(err, ZsshImportError::Undecryptable), "{err}");
    assert!(err.to_string().contains("备份文件"), "提示换一条路：{err}");
    assert!(entries(&scratch).is_empty());
}

#[test]
fn a_newer_database_is_refused_with_an_upgrade_hint() {
    let root = TestDir::new("zi-new");
    let dir = fake_zssh(&root, &TestSealer(1));
    let conn = Connection::open(dir.join("data.db")).unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (99)", [])
        .unwrap();
    drop(conn);
    let scratch = TestDir::new("zi-new-scratch");
    let err = read_zssh(&source_at(&dir).unwrap(), &TestSealer(1), &scratch).unwrap_err();
    match err {
        ZsshImportError::TooNew { found: 99, .. } => {}
        other => panic!("{other}"),
    }
    assert!(err_text_has_upgrade_hint(99));
}

fn err_text_has_upgrade_hint(found: i64) -> bool {
    ZsshImportError::TooNew {
        found,
        supported: 13,
    }
    .to_string()
    .contains("升级 ZeeTerm")
}

#[test]
fn something_that_is_not_a_database_is_not_zssh_data() {
    let root = TestDir::new("zi-garbage");
    let dir = root.join("Z-ssh");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.db"), b"this is not sqlite at all, just text").unwrap();
    let scratch = TestDir::new("zi-garbage-scratch");
    let err = read_zssh(&source_at(&dir).unwrap(), &TestSealer(1), &scratch).unwrap_err();
    assert!(matches!(err, ZsshImportError::NotZsshData(_)), "{err}");

    // 是 SQLite 但不是 Z-SSH 的库
    let other = root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    Connection::open(other.join("data.db"))
        .unwrap()
        .execute_batch("CREATE TABLE t (x INTEGER);")
        .unwrap();
    let err = read_zssh(&source_at(&other).unwrap(), &TestSealer(1), &scratch).unwrap_err();
    assert!(matches!(err, ZsshImportError::NotZsshData(_)), "{err}");
}

#[test]
fn missing_or_broken_fingerprints_do_not_stop_the_import() {
    let root = TestDir::new("zi-kh");
    let sealer = TestSealer(7);
    let dir = fake_zssh(&root, &sealer);
    let scratch = TestDir::new("zi-kh-scratch");

    std::fs::remove_file(dir.join("known_hosts")).unwrap();
    let read = read_zssh(&source_at(&dir).unwrap(), &sealer, &scratch).unwrap();
    assert!(read.snapshot.known_hosts.is_empty());
    assert!(read.notes.is_empty(), "没有指纹文件不是问题");

    std::fs::write(dir.join("known_hosts"), [0xff, 0xfe, 0x00, 0x9f]).unwrap();
    let read = read_zssh(&source_at(&dir).unwrap(), &sealer, &scratch).unwrap();
    assert_eq!(read.snapshot.hosts.len(), 2, "主机照常读出来");
    assert_eq!(read.notes.len(), 1, "{:?}", read.notes);
}

#[cfg(windows)]
#[test]
fn a_running_zssh_is_detected_and_asks_to_be_closed() {
    let root = TestDir::new("zi-inuse");
    let sealer = TestSealer(7);
    let dir = fake_zssh(&root, &sealer);
    let source = source_at(&dir).unwrap();
    let scratch = TestDir::new("zi-inuse-scratch");
    assert!(!is_in_use(&source.db_path));

    // 和 Z-SSH 一样：进程里一直开着一条 SQLite 连接
    let running = Connection::open(&source.db_path).unwrap();
    running
        .query_row("SELECT count(*) FROM hosts", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(is_in_use(&source.db_path));
    let err = read_zssh(&source, &sealer, &scratch).unwrap_err();
    assert!(matches!(err, ZsshImportError::InUse), "{err}");
    assert!(err.to_string().contains("先退出 Z-SSH"));
    assert!(entries(&scratch).is_empty(), "占用时不留副本");

    drop(running);
    assert!(!is_in_use(&source.db_path));
    read_zssh(&source, &sealer, &scratch).expect("退出之后能读");
}

// ---- 预览与导入（走 ZeeTerm 现有的导入引擎）----

fn local_host(db: &Database, name: &str, host: &str, port: u16, user: &str) {
    use crate::model::{AuthMethod, HostDraft, ProxyMode};
    db.host_save(&HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: name.to_owned(),
        host: host.to_owned(),
        port,
        username: user.to_owned(),
        auth_method: AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::Default,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    })
    .unwrap();
}

fn host_names(db: &Database) -> Vec<String> {
    let mut names: Vec<String> = db
        .host_list()
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
    names.sort();
    names
}

#[test]
fn preview_counts_match_what_the_import_does() {
    let root = TestDir::new("zi-import");
    let zssh_sealer = TestSealer(7);
    let dir = fake_zssh(&root, &zssh_sealer);
    let scratch = TestDir::new("zi-import-scratch");
    let read = read_zssh(&source_at(&dir).unwrap(), &zssh_sealer, &scratch).unwrap();

    // ZeeTerm 这边（另一把"DPAPI"也无妨：导入时用本机的密钥重新加密）
    let zeeterm = TestSealer(9);
    let machine = TestDir::new("zi-import-machine");
    let db = Database::open_in_memory().unwrap();
    local_host(&db, "web-1", "web-1.example.invalid", 22, "deploy"); // 和 Z-SSH 里那台一模一样
    local_host(&db, "only-here", "198.51.100.7", 22, "root");
    let known_hosts = machine.join("known_hosts");
    let engine = SyncEngine::new(
        &db,
        &zeeterm,
        "dev-zeeterm",
        &*machine,
        Some(known_hosts.clone()),
        None,
    );

    let merge = engine
        .preview_import(&read.snapshot, OfflineImportMode::Merge)
        .unwrap();
    assert_eq!(merge.source.hosts, 2);
    assert_eq!(merge.source.groups, 2);
    assert_eq!(merge.source.identities, 2);
    assert_eq!(merge.source.proxies, 1);
    assert_eq!(merge.source.known_hosts, 1);
    assert_eq!(merge.source.quick_commands, 2);
    assert_eq!((merge.hosts.inserted, merge.hosts.deleted), (2, 0));
    assert_eq!(merge.duplicate_hosts, 1, "web-1 本机已经有一台一样的");
    assert_eq!(merge.identities.inserted, 2);
    assert_eq!(merge.conflicts, 0);
    assert!(merge.settings_changed >= 2);
    assert_eq!(merge.known_hosts_added, 1);
    // 预览只读：本机一个字节没动，也没有生成备份
    assert_eq!(host_names(&db), vec!["only-here", "web-1"]);
    assert!(!machine.join("backups").exists());

    let replace = engine
        .preview_import(&read.snapshot, OfflineImportMode::Replace)
        .unwrap();
    assert_eq!((replace.hosts.inserted, replace.hosts.deleted), (2, 2));
    assert_eq!(replace.duplicate_hosts, 0);

    let report = engine
        .import_snapshot(read.snapshot.clone(), OfflineImportMode::Merge)
        .unwrap();
    assert_eq!(report.applied.hosts.inserted, merge.hosts.inserted);
    assert_eq!(
        report.applied.identities.inserted,
        merge.identities.inserted
    );
    assert_eq!(report.applied.groups.inserted, merge.groups.inserted);
    assert_eq!(report.applied.proxies.inserted, merge.proxies.inserted);
    assert_eq!(report.applied.known_hosts_added, merge.known_hosts_added);
    assert!(std::fs::read_to_string(&known_hosts)
        .unwrap()
        .contains("web-1.example.invalid"));
    assert!(report.safety_backup.name.starts_with("zssh-backup-"));
    assert_eq!(
        host_names(&db),
        vec!["db-1", "only-here", "web-1", "web-1"],
        "合并：两边都留，重复的那台各留一份"
    );

    // 凭据换成了本机的密钥重新加密：能解出来，也有密码
    let identities = db.identity_list().unwrap();
    assert_eq!(identities.len(), 2);
    assert!(identities.iter().all(|i| i.has_secret));
    let proxies = db.proxy_list().unwrap();
    assert_eq!(proxies.len(), 1);
    // 快捷命令成了 ZeeTerm 的设置
    let quick = db
        .setting_get("quick_commands")
        .unwrap()
        .expect("导入了快捷命令");
    assert!(quick.contains("df -h"));
}

#[test]
fn replace_makes_zeeterm_match_zssh_and_can_be_undone_from_the_safety_backup() {
    let root = TestDir::new("zi-replace");
    let zssh_sealer = TestSealer(7);
    let dir = fake_zssh(&root, &zssh_sealer);
    let scratch = TestDir::new("zi-replace-scratch");
    let read = read_zssh(&source_at(&dir).unwrap(), &zssh_sealer, &scratch).unwrap();

    let zeeterm = TestSealer(9);
    let machine = TestDir::new("zi-replace-machine");
    let db = Database::open_in_memory().unwrap();
    local_host(&db, "only-here", "198.51.100.7", 22, "root");
    let engine = SyncEngine::new(&db, &zeeterm, "dev-zeeterm", &*machine, None, None);
    let report = engine
        .import_snapshot(read.snapshot, OfflineImportMode::Replace)
        .unwrap();
    assert_eq!(host_names(&db), vec!["db-1", "web-1"]);
    assert_eq!(report.applied.hosts.deleted, 1);

    // 导入前的状态在自动备份里：恢复回来
    engine
        .restore_backup(Path::new(&report.safety_backup.path))
        .unwrap();
    assert_eq!(host_names(&db), vec!["only-here"]);
}

#[test]
fn importing_twice_in_merge_mode_does_not_duplicate_anything() {
    let root = TestDir::new("zi-twice");
    let zssh_sealer = TestSealer(7);
    let dir = fake_zssh(&root, &zssh_sealer);
    let scratch = TestDir::new("zi-twice-scratch");
    let source = source_at(&dir).unwrap();

    let zeeterm = TestSealer(9);
    let machine = TestDir::new("zi-twice-machine");
    let db = Database::open_in_memory().unwrap();
    let engine = SyncEngine::new(&db, &zeeterm, "dev-zeeterm", &*machine, None, None);
    for round in 0..2 {
        let read = read_zssh(&source, &zssh_sealer, &scratch).unwrap();
        if round == 1 {
            let preview = engine
                .preview_import(&read.snapshot, OfflineImportMode::Merge)
                .unwrap();
            assert_eq!(
                (preview.hosts.inserted, preview.hosts.updated),
                (0, 0),
                "再导一次不该有变化"
            );
        }
        engine
            .import_snapshot(read.snapshot, OfflineImportMode::Merge)
            .unwrap();
    }
    assert_eq!(host_names(&db), vec!["db-1", "web-1"]);
    assert_eq!(db.identity_list().unwrap().len(), 2);
}

// ---- 路径二：Z-SSH 导出的 .zsshbak ----

/// Z-SSH 0.2.14 写的明文（schema_version 1，主机记录没有协议字段）。字段取自它的 `package.rs`，
/// 和 `package/compat_tests.rs` 里冻结的 0.2.14 解析逻辑一致。
fn zsshbak_0214(password: &str) -> Vec<u8> {
    let plaintext = serde_json::json!({
        "schema_version": 1,
        "device_id": "zssh-0214-device",
        "created_at": 1_700_000_000,
        "groups": [{"uid": "g-old", "name": "旧分组", "parent_uid": null, "sort": 0,
                     "updated_at": 100, "device_id": "zssh-0214-device"}],
        "hosts": [{"uid": "h-old", "group_uid": "g-old", "name": "old-web",
                    "host": "203.0.113.5", "port": 22, "username": "ops",
                    "auth_method": "password", "identity_uid": "i-old", "proxy_uid": null,
                    "jump_host_uid": null, "proxy_mode": "default", "tags": [], "sort": 0,
                    "legacy_algorithms": false, "created_at": 90, "updated_at": 100,
                    "device_id": "zssh-0214-device"}],
        "identities": [{"uid": "i-old", "name": "旧密码", "kind": "password", "key_path": null,
                         "secret": "old-secret-1", "updated_at": 100,
                         "device_id": "zssh-0214-device"}],
        "proxies": [],
        "settings": {"quick_commands": "[{\"id\":\"q\",\"name\":\"n\",\"command\":\"ls\",\"enter\":true}]"},
        "known_hosts": [],
        "tombstones": []
    });
    let params = KdfParams {
        memory_kib: 8 * 1024,
        iterations: 1,
        ..KdfParams::default()
    };
    seal_with(
        plaintext.to_string().as_bytes(),
        password,
        params,
        &[3u8; SALT_BYTES],
        &[4u8; NONCE_BYTES],
    )
    .unwrap()
}

#[test]
fn a_zssh_0214_backup_file_imports_directly_and_previews_first() {
    let bytes = zsshbak_0214("the-old-password");
    let zeeterm = TestSealer(9);
    let machine = TestDir::new("zi-bak");
    let db = Database::open_in_memory().unwrap();
    let engine = SyncEngine::new(&db, &zeeterm, "dev-zeeterm", &*machine, None, None);

    // 先解开、预览（和界面一样），再导入
    let snapshot = SyncSnapshot::open(&bytes, "the-old-password").unwrap();
    let preview = engine
        .preview_import(&snapshot, OfflineImportMode::Merge)
        .unwrap();
    assert_eq!(
        (preview.hosts.inserted, preview.source.quick_commands),
        (1, 1)
    );

    let report = engine
        .import_offline(&bytes, "the-old-password", OfflineImportMode::Merge)
        .unwrap();
    assert_eq!(report.applied.hosts.inserted, 1);
    assert_eq!(host_names(&db), vec!["old-web"]);
    assert!(db.identity_list().unwrap()[0].has_secret);

    // 密码不对：不动本机
    let err = engine
        .import_offline(&bytes, "wrong-password-1", OfflineImportMode::Replace)
        .unwrap_err();
    assert!(matches!(err, SyncError::WrongPassword), "{err}");
    assert_eq!(host_names(&db), vec!["old-web"]);
}

#[test]
fn preview_and_import_both_skip_match_everything_fingerprints() {
    let machine = TestDir::new("zi-import-wildcard");
    let db = Database::open_in_memory().unwrap();
    let known_hosts = machine.join("known_hosts");
    let engine = SyncEngine::new(
        &db,
        &TestSealer(9),
        "dev-zeeterm",
        &*machine,
        Some(known_hosts.clone()),
        None,
    );
    let record = |host: &str| crate::sync::package::KnownHostRecord {
        host: host.to_owned(),
        port: 22,
        key_type: "ssh-ed25519".to_owned(),
        key: "AAAAC3NzaC1lZDI1NTE5AAAAInew".to_owned(),
        marker: None,
        updated_at: 1,
        device_id: "dev-a".to_owned(),
    };
    let mut snapshot = crate::sync::package::SyncSnapshot::empty("dev-a", 1_760_000_000);
    snapshot.known_hosts = vec![record("*"), record("web-1.example.invalid")];

    let preview = engine
        .preview_import(&snapshot, OfflineImportMode::Merge)
        .unwrap();
    assert_eq!(
        (preview.known_hosts_added, preview.known_hosts_skipped),
        (1, 1)
    );
    let report = engine
        .import_snapshot(snapshot, OfflineImportMode::Merge)
        .unwrap();
    assert_eq!(report.applied.known_hosts_added, 1);
    assert_eq!(report.applied.known_hosts_skipped, 1);
    let content = std::fs::read_to_string(&known_hosts).unwrap();
    assert_eq!(content.lines().count(), 1, "{content}");
}
