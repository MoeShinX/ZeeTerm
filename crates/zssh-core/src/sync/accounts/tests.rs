use std::path::Path;

use super::*;
use crate::model::{AuthMethod, HostDraft, ProxyMode};
use crate::sync::backup::{BackupEnvelope, BackupStore};
use crate::sync::backup_targets::{
    add_target, load_targets, new_target_id, set_enabled, token_store,
};
use crate::sync::crypto::KdfParams;
use crate::sync::engine::SyncEngine;
use crate::sync::local_extras::LocalExtras;
use crate::sync::offline::OfflineImportMode;
use crate::sync::onedrive::config::OneDriveConfig;
use crate::sync::s3::S3Config;
use crate::sync::sealer::TestSealer;
use crate::sync::snapshot::snapshot_local;
use crate::sync::webdav::WebDavConfig;

const EXPORT_PASSWORD: &str = "export-test-password";

fn fast() -> KdfParams {
    KdfParams {
        memory_kib: 8 * 1024,
        iterations: 1,
        ..KdfParams::default()
    }
}

fn add_host(db: &Database, name: &str) {
    db.host_save(&HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: name.to_owned(),
        host: format!("{name}.example.invalid"),
        port: 22,
        username: "tester".to_owned(),
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

struct Machine {
    db: Database,
    dir: crate::test_util::TestDir,
    sealer: TestSealer,
}

impl Machine {
    fn new(tag: &str, sealer: u8) -> Self {
        Self {
            db: Database::open_in_memory().unwrap(),
            dir: crate::test_util::TestDir::new(&format!("accounts-{tag}")),
            sealer: TestSealer(sealer),
        }
    }

    fn engine(&self) -> SyncEngine<'_> {
        SyncEngine::new(&self.db, &self.sealer, "dev-x", &*self.dir, None, None)
    }
}

fn dav(host: &str, password: &str) -> StorageConfig {
    StorageConfig::Webdav(WebDavConfig {
        url: format!("https://{host}/dav/"),
        directory: "Z-SSH".to_owned(),
        username: "u".to_owned(),
        password: password.to_owned(),
        allow_insecure_http: false,
    })
}

fn tokens(refresh: &str) -> TokenSet {
    TokenSet {
        access_token: format!("access-{refresh}"),
        refresh_token: refresh.to_owned(),
        expires_at: 4_000_000_000,
    }
}

/// 一台配齐了的电脑：主同步 OneDrive（带令牌）、记住的主密码、自动同步和保留份数设置、
/// 两个备份目标（S3、OneDrive 带自己的令牌）。返回 OneDrive 备份目标的 id。
fn configured(m: &Machine) -> String {
    add_host(&m.db, "web-1");
    service::save_config(
        &m.db,
        &m.sealer,
        &StorageConfig::Onedrive(OneDriveConfig {
            client_id: String::new(),
            account: "main@example.com".to_owned(),
        }),
    )
    .unwrap();
    DbTokenStore::new(&m.db, &m.sealer)
        .save(&tokens("main-refresh"))
        .unwrap();
    service::save_master_password(&m.db, &m.sealer, "master-password-1").unwrap();
    service::save_auto(
        &m.db,
        &AutoSettings {
            enabled: false,
            interval_minutes: 42,
        },
    )
    .unwrap();
    m.db.setting_set(SYNC_BACKUP_KEEP_KEY, "9").unwrap();
    add_target(
        &m.db,
        &m.sealer,
        None,
        StorageConfig::S3(S3Config {
            endpoint: "https://s3.example.com".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "demo-bucket".to_owned(),
            access_key: "AK".to_owned(),
            secret_key: "SK-secret".to_owned(),
            prefix: String::new(),
            path_style: true,
        }),
        true,
    )
    .unwrap();
    let od = new_target_id();
    token_store(&m.db, &m.sealer, &od)
        .save(&tokens("target-refresh"))
        .unwrap();
    add_target(
        &m.db,
        &m.sealer,
        Some(od.clone()),
        StorageConfig::Onedrive(OneDriveConfig {
            client_id: String::new(),
            account: "backup@example.com".to_owned(),
        }),
        false,
    )
    .unwrap();
    od
}

/// 两台电脑上读出来的同步账号设置应当一样（令牌、密码、目标都在）。
fn assert_same_accounts(a: &Machine, b: &Machine) {
    let (left, right) = (
        SyncAccounts::read(&a.db, &a.sealer).unwrap().unwrap(),
        SyncAccounts::read(&b.db, &b.sealer).unwrap().unwrap(),
    );
    assert_eq!(left, right);
}

#[test]
fn nothing_configured_means_nothing_to_carry() {
    let m = Machine::new("empty", 1);
    assert!(SyncAccounts::read(&m.db, &m.sealer).unwrap().is_none());
    assert!(!SyncAccounts::local_is_configured(&m.db, &m.sealer).unwrap());
}

#[test]
fn accounts_survive_a_round_trip_to_another_computer() {
    let a = Machine::new("rt-a", 1);
    let od = configured(&a);
    let read = SyncAccounts::read(&a.db, &a.sealer).unwrap().unwrap();
    assert!(matches!(read.storage, Some(StorageConfig::Onedrive(_))));
    assert_eq!(
        read.onedrive_tokens.as_ref().unwrap().refresh_token,
        "main-refresh"
    );
    assert_eq!(read.master_password.as_deref(), Some("master-password-1"));
    assert_eq!(read.auto.as_ref().unwrap().interval_minutes, 42);
    assert_eq!(read.backup_keep, Some(serde_json::json!(9)));
    assert_eq!(read.backup_targets.len(), 2);
    assert_eq!(read.target_tokens.len(), 1);
    assert_eq!(read.target_tokens[&od].refresh_token, "target-refresh");

    // 过 JSON（备份信封 / 导出文件里就是这样存的）再写到另一台电脑（另一个封装密钥）
    let json = serde_json::to_string(&read).unwrap();
    let back: SyncAccounts = serde_json::from_str(&json).unwrap();
    assert_eq!(back, read);
    let b = Machine::new("rt-b", 2);
    back.apply(&b.db, &b.sealer).unwrap();
    assert_same_accounts(&a, &b);
    assert!(b.db.setting_get(SYNC_BACKUP_KEEP_KEY).unwrap().is_some());
    // 本机的密文是 B 自己的封装，不是 A 的
    assert!(service::load_config(&b.db, &TestSealer(1)).is_err());
}

#[test]
fn applying_replaces_what_the_computer_had_and_leaves_no_stale_tokens() {
    let a = Machine::new("rep-a", 1);
    configured(&a);
    let accounts = SyncAccounts::read(&a.db, &a.sealer).unwrap().unwrap();

    // B 原来配的是别的：一个 WebDAV 主同步 + 一个带令牌的 OneDrive 备份目标
    let b = Machine::new("rep-b", 1);
    service::save_config(&b.db, &b.sealer, &dav("old.example.com", "old-pass")).unwrap();
    let old_target = new_target_id();
    token_store(&b.db, &b.sealer, &old_target)
        .save(&tokens("old-target-refresh"))
        .unwrap();
    add_target(
        &b.db,
        &b.sealer,
        Some(old_target.clone()),
        StorageConfig::Onedrive(OneDriveConfig::default()),
        true,
    )
    .unwrap();

    accounts.apply(&b.db, &b.sealer).unwrap();
    assert_same_accounts(&a, &b);
    assert!(
        token_store(&b.db, &b.sealer, &old_target)
            .load()
            .unwrap()
            .is_none(),
        "旧目标的令牌不能留下"
    );
    assert!(!load_targets(&b.db, &b.sealer)
        .unwrap()
        .iter()
        .any(|t| t.id == old_target));
}

fn raw_settings(db: &Database) -> BTreeMap<String, String> {
    db.with_conn_as(|conn| -> rusqlite::Result<_> {
        let mut statement = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    })
    .unwrap()
}

fn replacement_accounts(m: &Machine) -> SyncAccounts {
    let mut accounts = SyncAccounts::read(&m.db, &m.sealer).unwrap().unwrap();
    accounts.storage = Some(StorageConfig::Onedrive(OneDriveConfig::default()));
    accounts.onedrive_tokens = Some(tokens("replacement-main"));
    accounts.master_password = Some("replacement-master-password".to_owned());
    accounts.auto = Some(AutoSettings {
        enabled: true,
        interval_minutes: 17,
    });
    accounts.backup_keep = Some(serde_json::json!(3));
    accounts.backup_targets = vec![BackupTarget {
        id: "replacement-target".to_owned(),
        config: StorageConfig::Onedrive(OneDriveConfig::default()),
        enabled: true,
        last: None,
    }];
    accounts.target_tokens.clear();
    accounts.target_tokens.insert(
        "replacement-target".to_owned(),
        tokens("replacement-target"),
    );
    accounts
}

struct FailNthSeal {
    inner: TestSealer,
    fail_at: usize,
    calls: std::sync::atomic::AtomicUsize,
}

impl SecretSealer for FailNthSeal {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if call == self.fail_at {
            return Err(SyncError::Internal(Reason::text(
                "injected seal failure".to_owned(),
            )));
        }
        self.inner.seal(plaintext)
    }

    fn open(&self, blob: &[u8]) -> Result<Vec<u8>, SyncError> {
        self.inner.open(blob)
    }
}

#[test]
fn applying_accounts_seal_failure_preserves_all_old_settings_and_tokens() {
    let m = Machine::new("seal-failure", 1);
    configured(&m);
    service::set_ready(&m.db, true).unwrap();
    m.db.setting_set(service::CONCURRENCY_KEY, "{}").unwrap();
    let old = SyncAccounts::read(&m.db, &m.sealer).unwrap();
    let before = raw_settings(&m.db);
    let replacement = replacement_accounts(&m);
    // 存储、主令牌、主密码、目标列表、目标令牌：每一步失败都不得开始改库。
    for fail_at in 1..=5 {
        let sealer = FailNthSeal {
            inner: TestSealer(1),
            fail_at,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let error = replacement.apply(&m.db, &sealer).unwrap_err();
        assert!(error.to_string().contains("injected seal failure"));
        assert_eq!(
            sealer.calls.load(std::sync::atomic::Ordering::SeqCst),
            fail_at
        );
        assert_eq!(
            raw_settings(&m.db),
            before,
            "密封第 {fail_at} 项失败后密文必须原样保留"
        );
        assert_eq!(SyncAccounts::read(&m.db, &m.sealer).unwrap(), old);
    }
}

#[test]
fn applying_accounts_sql_failure_rolls_back_all_settings_and_tokens() {
    let m = Machine::new("sql-failure", 1);
    configured(&m);
    service::set_ready(&m.db, true).unwrap();
    m.db.setting_set(service::CONCURRENCY_KEY, "{}").unwrap();
    token_store(&m.db, &m.sealer, "orphan")
        .save(&tokens("orphan"))
        .unwrap();
    // 下划线必须按字面匹配，不能误删相似名称的无关设置。
    m.db.setting_set("syncXbackupXtoken.keep", "true").unwrap();
    let old = SyncAccounts::read(&m.db, &m.sealer).unwrap();
    let before = raw_settings(&m.db);
    let replacement = replacement_accounts(&m);
    m.db.with_conn_as(|conn| {
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_accounts_restore BEFORE INSERT ON settings
         WHEN NEW.key = 'sync_backup_token.replacement-target'
         BEGIN
             SELECT CASE WHEN EXISTS (
                 SELECT 1 FROM settings WHERE key = 'sync_backup_keep' AND value = '3'
             ) AND NOT EXISTS (
                 SELECT 1 FROM settings WHERE key = 'sync_backup_token.orphan'
             ) THEN RAISE(ABORT, 'injected late SQL failure')
             ELSE RAISE(ABORT, 'failure happened before replacement writes') END;
         END;",
        )
    })
    .unwrap();
    let error = replacement.apply(&m.db, &m.sealer).unwrap_err();
    assert!(
        error.to_string().contains("injected late SQL failure"),
        "{error}"
    );
    assert_eq!(raw_settings(&m.db), before, "删除及前面所有写入都必须回滚");
    assert_eq!(SyncAccounts::read(&m.db, &m.sealer).unwrap(), old);

    m.db.with_conn_as(|conn| conn.execute_batch("DROP TRIGGER fail_accounts_restore"))
        .unwrap();
    replacement.apply(&m.db, &m.sealer).unwrap();
    assert_eq!(
        SyncAccounts::read(&m.db, &m.sealer).unwrap(),
        Some(replacement)
    );
    assert!(token_store(&m.db, &m.sealer, "orphan")
        .load()
        .unwrap()
        .is_none());
    assert!(m.db.setting_get(service::READY_KEY).unwrap().is_none());
    assert!(m
        .db
        .setting_get(service::CONCURRENCY_KEY)
        .unwrap()
        .is_none());
    assert_eq!(
        m.db.setting_get("syncXbackupXtoken.keep")
            .unwrap()
            .as_deref(),
        Some("true")
    );
}

#[test]
fn a_backup_without_a_main_onedrive_clears_stale_main_tokens() {
    // 备份里主同步是 WebDAV：本机原来 OneDrive 的令牌不能留着
    let a = Machine::new("stale-a", 1);
    service::save_config(&a.db, &a.sealer, &dav("a.example.com", "pw")).unwrap();
    let accounts = SyncAccounts::read(&a.db, &a.sealer).unwrap().unwrap();
    let b = Machine::new("stale-b", 1);
    service::save_config(
        &b.db,
        &b.sealer,
        &StorageConfig::Onedrive(OneDriveConfig::default()),
    )
    .unwrap();
    DbTokenStore::new(&b.db, &b.sealer)
        .save(&tokens("b-main"))
        .unwrap();
    accounts.apply(&b.db, &b.sealer).unwrap();
    assert!(DbTokenStore::new(&b.db, &b.sealer)
        .load()
        .unwrap()
        .is_none());
}

#[test]
fn a_hostile_list_in_a_backup_is_refused() {
    let mut accounts = SyncAccounts::default();
    for index in 0..=MAX_TARGETS {
        accounts.backup_targets.push(BackupTarget {
            id: format!("t{index}"),
            config: dav("a.example.com", "p"),
            enabled: true,
            last: None,
        });
    }
    let m = Machine::new("hostile", 1);
    assert!(accounts.apply(&m.db, &m.sealer).is_err(), "太多");
    accounts.backup_targets.truncate(1);
    accounts.backup_targets[0].id = "../etc".to_owned();
    assert!(accounts.apply(&m.db, &m.sealer).is_err(), "id 不合法");
}

#[test]
fn debug_output_never_shows_credentials() {
    let a = Machine::new("debug", 1);
    configured(&a);
    let accounts = SyncAccounts::read(&a.db, &a.sealer).unwrap().unwrap();
    let text = format!("{accounts:?}");
    for secret in [
        "master-password-1",
        "main-refresh",
        "target-refresh",
        "SK-secret",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
}

// ---------------------------------------------------------------------------
// 本地自动备份

fn make_local_backup(m: &Machine) -> std::path::PathBuf {
    let store = BackupStore::new(&m.dir, &m.sealer);
    let snapshot = snapshot_local(&m.db, &m.sealer, "dev-x", None, 1_760_000_000).unwrap();
    let accounts = SyncAccounts::read(&m.db, &m.sealer).unwrap();
    let info = store
        .create(
            &snapshot,
            &LocalExtras::default(),
            accounts.as_ref(),
            1_760_000_000,
            20,
            None,
        )
        .unwrap();
    std::path::PathBuf::from(info.path)
}

#[test]
fn a_local_backup_carries_the_accounts_and_restoring_on_an_unconfigured_computer_writes_them_back()
{
    let a = Machine::new("bk-a", 1);
    configured(&a);
    let path = make_local_backup(&a);
    let envelope = BackupStore::new(&a.dir, &a.sealer).load(&path).unwrap();
    assert!(envelope.sync_accounts.is_some(), "信封里带了同步账号设置");
    // 密文文件里看不到任何明文
    let raw = std::fs::read(&path).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(!raw.contains("master-password-1") && !raw.contains("SK-secret"));

    // 数据被弄丢、同步设置也没了的 A，从备份恢复：直接写回
    let b = Machine::new("bk-b", 1);
    std::fs::create_dir_all(b.dir.join("backups")).unwrap();
    std::fs::copy(&path, b.dir.join("backups").join(path.file_name().unwrap())).unwrap();
    let report = b
        .engine()
        .restore_backup(&b.dir.join("backups").join(path.file_name().unwrap()))
        .unwrap();
    assert!(
        matches!(report.accounts, AccountsRestore::Applied),
        "{:?}",
        report.accounts
    );
    assert_same_accounts(&a, &b);
    assert_eq!(host_names(&b.db), vec!["web-1"]);
}

#[test]
fn restoring_on_a_configured_computer_asks_first_and_changes_nothing_until_confirmed() {
    let a = Machine::new("ask-a", 1);
    configured(&a);
    let path = make_local_backup(&a);

    let b = Machine::new("ask-b", 1);
    service::save_config(&b.db, &b.sealer, &dav("mine.example.com", "mine-pass")).unwrap();
    service::save_master_password(&b.db, &b.sealer, "my-own-password").unwrap();
    std::fs::create_dir_all(b.dir.join("backups")).unwrap();
    let copy = b.dir.join("backups").join(path.file_name().unwrap());
    std::fs::copy(&path, &copy).unwrap();
    let report = b.engine().restore_backup(&copy).unwrap();
    // 数据照常恢复了
    assert_eq!(host_names(&b.db), vec!["web-1"]);
    // 同步设置没动，等用户确认
    let AccountsRestore::NeedsConfirmation(offered) = report.accounts else {
        panic!("本机已配置同步，应当先问：{:?}", report.accounts);
    };
    assert_eq!(
        service::load_config(&b.db, &b.sealer).unwrap(),
        Some(dav("mine.example.com", "mine-pass"))
    );
    assert_eq!(
        service::load_master_password(&b.db, &b.sealer)
            .unwrap()
            .unwrap()
            .as_str(),
        "my-own-password"
    );
    // 用户选「替换」
    offered.apply(&b.db, &b.sealer).unwrap();
    assert_same_accounts(&a, &b);
}

#[test]
fn an_old_backup_without_the_block_still_loads_and_restores() {
    let a = Machine::new("old-a", 1);
    add_host(&a.db, "web-1");
    let path = make_local_backup(&a);
    // 老版本的信封 JSON 里没有 sync_accounts 这个键
    let envelope = BackupStore::new(&a.dir, &a.sealer).load(&path).unwrap();
    let mut value = serde_json::to_value(&envelope).unwrap();
    assert!(value
        .as_object_mut()
        .unwrap()
        .remove("sync_accounts")
        .is_some());
    let old: BackupEnvelope = serde_json::from_value(value).unwrap();
    assert!(old.sync_accounts.is_none());

    // 没配置过同步的电脑做的备份也不带这块
    assert!(envelope.sync_accounts.is_none());
    let b = Machine::new("old-b", 1);
    service::save_config(&b.db, &b.sealer, &dav("mine.example.com", "mine-pass")).unwrap();
    std::fs::create_dir_all(b.dir.join("backups")).unwrap();
    let copy = b.dir.join("backups").join(path.file_name().unwrap());
    std::fs::copy(&path, &copy).unwrap();
    let report = b.engine().restore_backup(&copy).unwrap();
    assert!(matches!(report.accounts, AccountsRestore::NotInBackup));
    assert_eq!(
        service::load_config(&b.db, &b.sealer).unwrap(),
        Some(dav("mine.example.com", "mine-pass")),
        "老备份不会动本机的同步设置"
    );
}

#[test]
fn the_safety_backup_taken_before_a_restore_includes_the_current_accounts() {
    let b = Machine::new("safety", 1);
    service::save_config(&b.db, &b.sealer, &dav("mine.example.com", "mine-pass")).unwrap();
    let a = Machine::new("safety-src", 1);
    add_host(&a.db, "web-1");
    let path = make_local_backup(&a);
    std::fs::create_dir_all(b.dir.join("backups")).unwrap();
    let copy = b.dir.join("backups").join(path.file_name().unwrap());
    std::fs::copy(&path, &copy).unwrap();
    let report = b.engine().restore_backup(&copy).unwrap();
    let safety = BackupStore::new(&b.dir, &b.sealer)
        .load(Path::new(&report.safety_backup.path))
        .unwrap();
    let carried = safety
        .sync_accounts
        .expect("恢复前的备份带着当时的同步设置");
    assert_eq!(carried.storage, Some(dav("mine.example.com", "mine-pass")));
}

// ---------------------------------------------------------------------------
// 导出的 .zsshbak

#[test]
fn the_exported_file_carries_the_accounts_inside_the_password_encrypted_part() {
    let a = Machine::new("exp-a", 1);
    configured(&a);
    let bytes = a
        .engine()
        .export_offline_with(EXPORT_PASSWORD, fast())
        .unwrap();
    // 密文外层看不到任何明文
    let outer = String::from_utf8_lossy(&bytes);
    for secret in ["master-password-1", "SK-secret", "main-refresh", "web-1"] {
        assert!(!outer.contains(secret), "{secret}");
    }
    let plain = crate::sync::crypto::open(&bytes, EXPORT_PASSWORD).unwrap();
    let text = String::from_utf8(plain.to_vec()).unwrap();
    assert!(text.contains(EXPORT_FIELD), "独立字段名和常量一致");
    assert!(text.contains("master-password-1") && text.contains("SK-secret"));
    // 同步账号设置是快照之外的独立字段：快照本身（Z-SSH 0.2.14 也这样解析）照常读得出，
    // 不认识的字段被丢掉，不影响导入
    let snapshot = crate::sync::package::SyncSnapshot::parse(&plain).unwrap();
    assert_eq!(snapshot.hosts.len(), 1);
    assert!(!snapshot.settings.contains_key("sync_storage"));
    assert!(!snapshot
        .settings
        .keys()
        .any(|k| k.starts_with("sync_backup")));
}

#[test]
fn importing_a_file_writes_the_accounts_back_on_a_fresh_computer() {
    let a = Machine::new("imp-a", 1);
    configured(&a);
    let bytes = a
        .engine()
        .export_offline_with(EXPORT_PASSWORD, fast())
        .unwrap();

    let b = Machine::new("imp-b", 2);
    let report = b
        .engine()
        .import_offline(&bytes, EXPORT_PASSWORD, OfflineImportMode::Merge)
        .unwrap();
    assert!(
        matches!(report.accounts, AccountsRestore::Applied),
        "{:?}",
        report.accounts
    );
    assert_eq!(host_names(&b.db), vec!["web-1"]);
    assert_same_accounts(&a, &b);
    // 报告序列化出去（给界面 / 日志）也不带凭据
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("master-password-1") && !json.contains("SK-secret"));
}

#[test]
fn importing_on_a_configured_computer_asks_instead_of_replacing() {
    let a = Machine::new("imp2-a", 1);
    configured(&a);
    let bytes = a
        .engine()
        .export_offline_with(EXPORT_PASSWORD, fast())
        .unwrap();

    let b = Machine::new("imp2-b", 1);
    service::save_config(&b.db, &b.sealer, &dav("mine.example.com", "mine-pass")).unwrap();
    // 只有备份目标、没有主同步，也算「已经配置了」
    let c = Machine::new("imp2-c", 1);
    add_target(&c.db, &c.sealer, None, dav("t.example.com", "t-pass"), true).unwrap();
    set_enabled(
        &c.db,
        &c.sealer,
        &load_targets(&c.db, &c.sealer).unwrap()[0].id,
        false,
    )
    .unwrap();

    for machine in [&b, &c] {
        let before = SyncAccounts::read(&machine.db, &machine.sealer).unwrap();
        let report = machine
            .engine()
            .import_offline(&bytes, EXPORT_PASSWORD, OfflineImportMode::Merge)
            .unwrap();
        let AccountsRestore::NeedsConfirmation(offered) = report.accounts else {
            panic!("应当先问：{:?}", report.accounts);
        };
        assert_eq!(
            SyncAccounts::read(&machine.db, &machine.sealer).unwrap(),
            before,
            "没确认之前同步设置一个字都不动"
        );
        offered.apply(&machine.db, &machine.sealer).unwrap();
        assert_same_accounts(&a, machine);
    }
}

#[test]
fn a_file_without_the_block_imports_as_before() {
    // 没配置同步的电脑导出的文件没有这个字段；老版本导出的也一样
    let a = Machine::new("oldfile-a", 1);
    add_host(&a.db, "web-1");
    let bytes = a
        .engine()
        .export_offline_with(EXPORT_PASSWORD, fast())
        .unwrap();
    let plain = crate::sync::crypto::open(&bytes, EXPORT_PASSWORD).unwrap();
    assert!(!String::from_utf8_lossy(&plain).contains(EXPORT_FIELD));

    let b = Machine::new("oldfile-b", 1);
    service::save_config(&b.db, &b.sealer, &dav("mine.example.com", "mine-pass")).unwrap();
    let report = b
        .engine()
        .import_offline(&bytes, EXPORT_PASSWORD, OfflineImportMode::Replace)
        .unwrap();
    assert!(matches!(report.accounts, AccountsRestore::NotInBackup));
    assert_eq!(host_names(&b.db), vec!["web-1"]);
    assert_eq!(
        service::load_config(&b.db, &b.sealer).unwrap(),
        Some(dav("mine.example.com", "mine-pass"))
    );
}

#[test]
fn a_garbled_block_is_ignored_instead_of_failing_the_import() {
    // 字段在、格式认不出来（比如以后的版本改了结构）：数据照常导入，只是不带账号设置
    let mut value: serde_json::Value = serde_json::to_value(
        crate::sync::package::SyncSnapshot::empty("dev-z", 1_700_000_000),
    )
    .unwrap();
    value[EXPORT_FIELD] = serde_json::json!({"storage": "not-a-config", "backup_targets": 5});
    let plain = serde_json::to_vec(&value).unwrap();
    let bytes = crate::sync::crypto::seal(&plain, EXPORT_PASSWORD).unwrap();
    let b = Machine::new("garbled", 1);
    let report = b
        .engine()
        .import_offline(&bytes, EXPORT_PASSWORD, OfflineImportMode::Merge)
        .unwrap();
    assert!(matches!(report.accounts, AccountsRestore::NotInBackup));
}

#[test]
fn neither_cloud_package_ever_contains_the_block() {
    // 云端同步包（engine.export_package / sync）和快照走的是另一条路，不经过 Exported
    let a = Machine::new("cloud-a", 1);
    configured(&a);
    let package = a.engine().export_package("cloud-test-password").unwrap();
    let plain = crate::sync::crypto::open(&package, "cloud-test-password").unwrap();
    let text = String::from_utf8_lossy(&plain);
    for needle in [
        EXPORT_FIELD,
        "master-password-1",
        "SK-secret",
        "main-refresh",
        "backup@example.com",
    ] {
        assert!(!text.contains(needle), "{needle}");
    }
}
