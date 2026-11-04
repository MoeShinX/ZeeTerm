use std::collections::HashMap;
use std::sync::Arc;

use super::*;
use crate::model::{AuthMethod, HostDraft, ProxyMode};
use crate::store::Database;
use crate::sync::onedrive::oauth::TokenSet;
use crate::sync::s3::S3Config;
use crate::sync::sealer::TestSealer;
use crate::sync::storage::MemoryStorage;
use crate::sync::webdav::WebDavConfig;

const PASSWORD: &str = "backup-test-password";

// 这些值要在“云端包里搜不到”的测试里逐个搜，所以每个都足够独特。
const DAV_PASSWORD: &str = "dav-pass-3f9a1c";
const S3_ACCESS: &str = "AKIA-ACCESS-7d21";
const S3_SECRET: &str = "s3-secret-key-8b44e0";
const GIST_TOKEN: &str = "ghp_token_91c7aa";
const MAIN_ACCESS_TOKEN: &str = "main-od-access-5521";
const MAIN_REFRESH_TOKEN: &str = "main-od-refresh-6632";
const TARGET_REFRESH_TOKEN: &str = "target-od-refresh-7743";
const REMEMBERED_PASSWORD: &str = "remembered-master-9954";

fn dir(name: &str) -> crate::test_util::TestDir {
    crate::test_util::TestDir::new(&format!("backup-targets-{name}"))
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

fn webdav(host: &str) -> StorageConfig {
    StorageConfig::Webdav(WebDavConfig {
        url: format!("https://{host}/dav/"),
        directory: "Z-SSH".to_owned(),
        username: "u".to_owned(),
        password: DAV_PASSWORD.to_owned(),
        allow_insecure_http: false,
    })
}

fn s3() -> StorageConfig {
    StorageConfig::S3(S3Config {
        endpoint: "https://s3.example.com".to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "demo-bucket".to_owned(),
        access_key: S3_ACCESS.to_owned(),
        secret_key: S3_SECRET.to_owned(),
        prefix: "zssh".to_owned(),
        path_style: true,
    })
}

fn gist() -> StorageConfig {
    StorageConfig::Github(crate::sync::github::GithubConfig {
        api_base: String::new(),
        token: GIST_TOKEN.to_owned(),
        gist_id: "0123456789abcdef0123".to_owned(),
        allow_insecure_http: false,
    })
}

fn onedrive(account: &str) -> StorageConfig {
    StorageConfig::Onedrive(OneDriveConfig {
        client_id: String::new(),
        account: account.to_owned(),
    })
}

fn tokens(refresh: &str) -> TokenSet {
    TokenSet {
        access_token: format!("access-of-{refresh}"),
        refresh_token: refresh.to_owned(),
        expires_at: 4_000_000_000,
    }
}

/// 把 id → 内存存储 的表当作各目标的云端。
#[derive(Default)]
struct Fake {
    stores: HashMap<String, Arc<MemoryStorage>>,
}

impl Fake {
    fn with(ids: &[&str]) -> Self {
        Self {
            stores: ids
                .iter()
                .map(|id| ((*id).to_owned(), Arc::new(MemoryStorage::new())))
                .collect(),
        }
    }

    fn store(&self, id: &str) -> Arc<MemoryStorage> {
        self.stores[id].clone()
    }
}

struct Shared(Arc<MemoryStorage>);

impl SyncStorage for Shared {
    fn read<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<Option<crate::sync::storage::StoredObject>, StorageError>> {
        self.0.read()
    }
    fn write<'a>(
        &'a self,
        bytes: &'a [u8],
        expected_version: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, StorageError>> {
        self.0.write(bytes, expected_version)
    }
    fn delete<'a>(
        &'a self,
        expected_version: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), StorageError>> {
        self.0.delete(expected_version)
    }
}

impl TargetOpener for Fake {
    fn open<'s>(
        &'s self,
        target: &'s BackupTarget,
    ) -> BoxFuture<'s, Result<Box<dyn SyncStorage + 's>, SyncError>> {
        Box::pin(async move {
            match self.stores.get(&target.id) {
                Some(store) => Ok(Box::new(Shared(store.clone())) as Box<dyn SyncStorage + 's>),
                None => Err(SyncError::Storage("没有这个测试存储".to_owned())),
            }
        })
    }
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
            dir: dir(tag),
            sealer: TestSealer(sealer),
        }
    }

    fn engine(&self) -> SyncEngine<'_> {
        SyncEngine::new(&self.db, &self.sealer, "dev-a", &*self.dir, None, None)
    }

    fn add(&self, config: StorageConfig) -> String {
        add_target(&self.db, &self.sealer, None, config, true)
            .unwrap()
            .id
    }

    fn targets(&self) -> Vec<BackupTarget> {
        load_targets(&self.db, &self.sealer).unwrap()
    }
}

// ---------------------------------------------------------------------------
// 增删改和启用开关

#[test]
fn targets_can_be_added_edited_toggled_and_deleted() {
    let m = Machine::new("crud", 1);
    assert!(m.targets().is_empty());

    let a = m.add(webdav("dav.example.com"));
    let b = m.add(s3());
    let targets = m.targets();
    assert_eq!(targets.len(), 2);
    assert!(targets.iter().all(|t| t.enabled && t.last.is_none()));
    assert_eq!(targets[0].describe(), "dav.example.com/Z-SSH");
    assert_eq!(targets[1].describe(), "demo-bucket/zssh（s3.example.com）");
    assert_eq!(provider_label(&targets[1].config), "S3");

    // 启用开关只改开关，不动上次结果
    record_result(
        &m.db,
        &m.sealer,
        &a,
        TargetResult {
            at: 100,
            ok: true,
            message: "已上传".to_owned(),
            digest: Some("d1".to_owned()),
        },
    )
    .unwrap();
    set_enabled(&m.db, &m.sealer, &a, false).unwrap();
    let after = m.targets();
    assert!(!after[0].enabled);
    assert_eq!(after[0].last.as_ref().unwrap().at, 100);
    set_enabled(&m.db, &m.sealer, &a, true).unwrap();

    // 改连接设置：上次结果清掉（指向的地方可能变了）
    update_target(&m.db, &m.sealer, &a, webdav("other.example.com")).unwrap();
    let after = m.targets();
    assert_eq!(after[0].describe(), "other.example.com/Z-SSH");
    assert!(after[0].last.is_none());
    assert!(after[0].enabled);

    delete_target(&m.db, &m.sealer, &a).unwrap();
    let left = m.targets();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, b);
    assert!(set_enabled(&m.db, &m.sealer, &a, true).is_err(), "已删除");
    // 目标被删之后后台才写回结果：什么也不做
    record_result(
        &m.db,
        &m.sealer,
        &a,
        TargetResult {
            at: 1,
            ok: false,
            message: String::new(),
            digest: None,
        },
    )
    .unwrap();
    assert_eq!(m.targets().len(), 1);
}

#[test]
fn at_most_five_targets_and_ids_are_checked() {
    let m = Machine::new("limit", 1);
    for index in 0..MAX_TARGETS {
        m.add(webdav(&format!("dav{index}.example.com")));
    }
    let err = add_target(&m.db, &m.sealer, None, s3(), true).unwrap_err();
    assert!(err.to_string().contains("最多"), "{err}");
    assert_eq!(m.targets().len(), MAX_TARGETS);

    let m = Machine::new("ids", 1);
    for bad in ["", "a/b", "../x", "id with space"] {
        assert!(
            add_target(&m.db, &m.sealer, Some(bad.to_owned()), s3(), true).is_err(),
            "{bad:?}"
        );
    }
    m.add(s3());
    let existing = m.targets()[0].id.clone();
    assert!(add_target(&m.db, &m.sealer, Some(existing), s3(), true).is_err());
}

#[test]
fn the_list_is_sealed_and_never_appears_in_clear_in_the_settings_table() {
    let m = Machine::new("sealed", 1);
    m.add(webdav("dav.example.com"));
    m.add(s3());
    m.add(gist());
    let raw = m.db.setting_get(TARGETS_KEY).unwrap().unwrap();
    for secret in [
        DAV_PASSWORD,
        S3_SECRET,
        S3_ACCESS,
        GIST_TOKEN,
        "dav.example.com",
    ] {
        assert!(!raw.contains(secret), "设置表里不能有明文：{secret}");
    }
    // 另一个 Windows 账户（另一个封装密钥）读不了：报错，不当成“没有目标”
    let other = TestSealer(2);
    assert!(load_targets(&m.db, &other).is_err());
    // 出路：清空整个列表
    reset_targets(&m.db, &other).unwrap();
    assert!(m.targets().is_empty());
}

#[test]
fn a_onedrive_target_needs_a_login_first() {
    let m = Machine::new("od-login", 1);
    let id = new_target_id();
    let err = add_target(
        &m.db,
        &m.sealer,
        Some(id.clone()),
        onedrive("a@example.com"),
        true,
    )
    .unwrap_err();
    assert!(err.to_string().contains("登录"), "{err}");
    token_store(&m.db, &m.sealer, &id)
        .save(&tokens(TARGET_REFRESH_TOKEN))
        .unwrap();
    add_target(&m.db, &m.sealer, Some(id), onedrive("a@example.com"), true).unwrap();
    assert_eq!(m.targets()[0].describe(), "a@example.com");
}

// ---------------------------------------------------------------------------
// OneDrive：主同步和备份目标的令牌互不清除

#[test]
fn onedrive_tokens_of_the_main_sync_and_of_targets_never_clear_each_other() {
    let m = Machine::new("od-tokens", 1);
    // 主同步：OneDrive + 令牌
    service::save_config(&m.db, &m.sealer, &onedrive("main@example.com")).unwrap();
    DbTokenStore::new(&m.db, &m.sealer)
        .save(&tokens(MAIN_REFRESH_TOKEN))
        .unwrap();
    // 两个 OneDrive 备份目标，各自一份令牌
    let (id1, id2) = (new_target_id(), new_target_id());
    for (id, refresh) in [
        (&id1, TARGET_REFRESH_TOKEN),
        (&id2, "second-target-refresh"),
    ] {
        token_store(&m.db, &m.sealer, id)
            .save(&tokens(refresh))
            .unwrap();
        add_target(
            &m.db,
            &m.sealer,
            Some(id.clone()),
            onedrive("t@example.com"),
            true,
        )
        .unwrap();
    }
    let main = || DbTokenStore::new(&m.db, &m.sealer).load().unwrap();
    let target = |id: &str| token_store(&m.db, &m.sealer, id).load().unwrap();
    assert_eq!(main().unwrap().refresh_token, MAIN_REFRESH_TOKEN);
    assert_eq!(target(&id1).unwrap().refresh_token, TARGET_REFRESH_TOKEN);
    assert_eq!(target(&id2).unwrap().refresh_token, "second-target-refresh");

    // 主同步换成 WebDAV（save_config 会清主同步的 OneDrive 令牌）：备份目标的令牌还在
    service::save_config(&m.db, &m.sealer, &webdav("dav.example.com")).unwrap();
    assert!(main().is_none());
    assert_eq!(target(&id1).unwrap().refresh_token, TARGET_REFRESH_TOKEN);
    assert_eq!(target(&id2).unwrap().refresh_token, "second-target-refresh");

    // 主同步又连上 OneDrive 再断开：同样不碰目标的令牌
    service::save_config(&m.db, &m.sealer, &onedrive("main@example.com")).unwrap();
    DbTokenStore::new(&m.db, &m.sealer)
        .save(&tokens(MAIN_REFRESH_TOKEN))
        .unwrap();
    service::onedrive_disconnect(&m.db).unwrap();
    assert!(main().is_none());
    assert_eq!(target(&id1).unwrap().refresh_token, TARGET_REFRESH_TOKEN);

    // 删掉一个目标：只清它自己的令牌
    delete_target(&m.db, &m.sealer, &id1).unwrap();
    assert!(target(&id1).is_none());
    assert_eq!(target(&id2).unwrap().refresh_token, "second-target-refresh");

    // 目标改成别的存储：它自己的令牌清掉，别的目标不受影响
    update_target(&m.db, &m.sealer, &id2, s3()).unwrap();
    assert!(target(&id2).is_none());
}

// ---------------------------------------------------------------------------
// 跑备份

fn backup_names(fake: &Fake, id: &str) -> Option<Vec<u8>> {
    fake.store(id).bytes()
}

#[tokio::test]
async fn after_a_sync_every_enabled_target_gets_the_package_and_failures_stay_isolated() {
    let m = Machine::new("run", 1);
    add_host(&m.db, "web-1");
    let (a, b, c, off) = (
        m.add(webdav("a.example.com")),
        m.add(s3()),
        m.add(gist()),
        m.add(webdav("off.example.com")),
    );
    set_enabled(&m.db, &m.sealer, &off, false).unwrap();
    let fake = Fake::with(&[&a, &b, &c, &off]);
    // 第二个目标写入一直超时
    fake.store(&b).inject_write_timeouts(100);

    let outcomes = backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    let status = |id: &str| {
        outcomes
            .iter()
            .find(|o| o.id == id)
            .map(|o| o.status.clone())
    };
    assert_eq!(status(&a), Some(OutcomeStatus::Uploaded));
    assert!(
        matches!(status(&b), Some(OutcomeStatus::Failed(_))),
        "{outcomes:?}"
    );
    assert_eq!(
        status(&c),
        Some(OutcomeStatus::Uploaded),
        "前面的失败不影响后面的目标"
    );
    assert_eq!(status(&off), None, "没启用的不做");
    assert_eq!(outcomes.len(), 3);

    assert!(backup_names(&fake, &a).is_some());
    assert!(backup_names(&fake, &b).is_none());
    assert!(backup_names(&fake, &c).is_some());
    assert!(backup_names(&fake, &off).is_none());

    // 结果只记在各自的目标上
    let targets = m.targets();
    let last = |id: &str| targets.iter().find(|t| t.id == id).unwrap().last.clone();
    assert!(last(&a).unwrap().ok);
    let failed = last(&b).unwrap();
    assert!(!failed.ok && !failed.message.is_empty() && failed.digest.is_none());
    assert!(last(&c).unwrap().ok);
    assert!(last(&off).is_none());

    // 包能用主密码解开，内容就是本机数据
    let opened = SyncSnapshot::open(&backup_names(&fake, &a).unwrap(), PASSWORD).unwrap();
    assert_eq!(opened.hosts.len(), 1);
    assert_eq!(opened.hosts[0].name, "web-1");
    assert!(matches!(
        SyncSnapshot::open(&backup_names(&fake, &a).unwrap(), "not-the-password"),
        Err(SyncError::WrongPassword)
    ));
}

#[tokio::test]
async fn automatic_backups_skip_unchanged_data_but_manual_ones_always_upload() {
    let m = Machine::new("unchanged", 1);
    add_host(&m.db, "web-1");
    let id = m.add(webdav("a.example.com"));
    let fake = Fake::with(&[&id]);

    let first = backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    assert_eq!(first[0].status, OutcomeStatus::Uploaded);
    let version = fake.store(&id).current_version();

    // 数据没变：自动备份不再上传（每 15 分钟自动同步一次，不能每次都重传）
    let second = backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    assert_eq!(second[0].status, OutcomeStatus::Unchanged);
    assert_eq!(fake.store(&id).current_version(), version);

    // 手动「立即备份」：不管变没变都传
    let manual = backup_with(&m.engine(), &fake, PASSWORD, Some(&id))
        .await
        .unwrap();
    assert_eq!(manual[0].status, OutcomeStatus::Uploaded);
    assert_ne!(fake.store(&id).current_version(), version);

    // 主密码改了：云端那份还是旧密码加密的，自动备份要重新传
    let renewed = backup_with(&m.engine(), &fake, "another-master-password", None)
        .await
        .unwrap();
    assert_eq!(renewed[0].status, OutcomeStatus::Uploaded);
    assert!(
        SyncSnapshot::open(&fake.store(&id).bytes().unwrap(), "another-master-password").is_ok()
    );
    let back = backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    assert_eq!(back[0].status, OutcomeStatus::Uploaded);

    // 数据变了：自动备份又会传
    add_host(&m.db, "web-2");
    let third = backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    assert_eq!(third[0].status, OutcomeStatus::Uploaded);
    let opened = SyncSnapshot::open(&fake.store(&id).bytes().unwrap(), PASSWORD).unwrap();
    assert_eq!(opened.hosts.len(), 2);

    // 失败之后不再认旧指纹（远端的内容不确定了）
    fake.store(&id).inject_write_timeouts(100);
    add_host(&m.db, "web-3");
    let failed = backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    assert!(matches!(failed[0].status, OutcomeStatus::Failed(_)));
    assert!(m.targets()[0].last.as_ref().unwrap().digest.is_none());
}

#[tokio::test]
async fn a_manual_backup_works_for_a_disabled_target_and_without_a_main_sync() {
    let m = Machine::new("manual", 1);
    add_host(&m.db, "web-1");
    let id = m.add(webdav("a.example.com"));
    set_enabled(&m.db, &m.sealer, &id, false).unwrap();
    assert!(
        service::load_config(&m.db, &m.sealer).unwrap().is_none(),
        "没有配置主同步"
    );
    let fake = Fake::with(&[&id]);
    let outcomes = backup_with(&m.engine(), &fake, PASSWORD, Some(&id))
        .await
        .unwrap();
    assert_eq!(outcomes[0].status, OutcomeStatus::Uploaded);
    assert!(fake.store(&id).bytes().is_some());

    assert!(backup_with(&m.engine(), &fake, PASSWORD, Some("nope"))
        .await
        .is_err());
    let short = backup_with(&m.engine(), &fake, "short", Some(&id))
        .await
        .unwrap_err();
    assert!(short.to_string().contains("至少"), "{short}");
    // 没有启用的目标时连主密码都不查：主同步成功后的自动备份什么也不做
    let none = backup_with(&m.engine(), &fake, "", None).await.unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn an_existing_backup_is_overwritten_and_only_one_copy_is_kept() {
    let m = Machine::new("overwrite", 1);
    add_host(&m.db, "web-1");
    let id = m.add(webdav("a.example.com"));
    let fake = Fake::with(&[&id]);
    fake.store(&id).put_external(b"older backup".to_vec());
    backup_with(&m.engine(), &fake, PASSWORD, Some(&id))
        .await
        .unwrap();
    backup_with(&m.engine(), &fake, PASSWORD, Some(&id))
        .await
        .unwrap();
    // 内存存储只有一个对象槽：覆盖写之后内容是新的备份，不是旧内容
    let bytes = fake.store(&id).bytes().unwrap();
    assert!(SyncSnapshot::open(&bytes, PASSWORD).is_ok());

    // 上传时另一个客户端抢先写：带着新读到的版本重来
    fake.store(&id).inject_write_conflicts(1);
    let outcomes = backup_with(&m.engine(), &fake, PASSWORD, Some(&id))
        .await
        .unwrap();
    assert_eq!(outcomes[0].status, OutcomeStatus::Uploaded);
}

// ---------------------------------------------------------------------------
// 云端包里没有任何凭据

/// 配齐所有“只在本机”的东西：主同步（WebDAV 密码）、记住的主密码、OneDrive 令牌、
/// 三个备份目标（S3 密钥、Gist 令牌、OneDrive）和各自的令牌。
fn fully_configured(m: &Machine) -> (String, String, String) {
    add_host(&m.db, "web-1");
    service::save_config(&m.db, &m.sealer, &webdav("main.example.com")).unwrap();
    service::save_master_password(&m.db, &m.sealer, REMEMBERED_PASSWORD).unwrap();
    DbTokenStore::new(&m.db, &m.sealer)
        .save(&TokenSet {
            access_token: MAIN_ACCESS_TOKEN.to_owned(),
            refresh_token: MAIN_REFRESH_TOKEN.to_owned(),
            expires_at: 4_000_000_000,
        })
        .unwrap();
    let od = new_target_id();
    token_store(&m.db, &m.sealer, &od)
        .save(&tokens(TARGET_REFRESH_TOKEN))
        .unwrap();
    let od = add_target(&m.db, &m.sealer, Some(od), onedrive("t@example.com"), true)
        .unwrap()
        .id;
    let s3 = m.add(s3());
    let gist = m.add(gist());
    (od, s3, gist)
}

const LOCAL_ONLY_VALUES: &[&str] = &[
    DAV_PASSWORD,
    S3_ACCESS,
    S3_SECRET,
    GIST_TOKEN,
    MAIN_ACCESS_TOKEN,
    MAIN_REFRESH_TOKEN,
    TARGET_REFRESH_TOKEN,
    REMEMBERED_PASSWORD,
];

fn assert_no_credentials(plaintext: &str, what: &str) {
    for value in LOCAL_ONLY_VALUES {
        assert!(!plaintext.contains(value), "{what} 里不能有：{value}");
    }
    // 设置键也一个都不能有：同步账号设置、备份目标列表、令牌的键
    for key in [
        "sync_storage",
        "sync_master_password",
        "sync_onedrive_tokens",
        "sync_backup_targets",
        "sync_backup_token",
        "sync_auto",
        "sync_backup_keep",
        "zeeterm_sync_accounts",
    ] {
        assert!(!plaintext.contains(key), "{what} 里不能有键：{key}");
    }
    // 目标指向哪也不该出现（目标列表是本机的事）
    for hint in ["demo-bucket", "main.example.com", "t@example.com"] {
        assert!(!plaintext.contains(hint), "{what} 里不能有：{hint}");
    }
}

#[tokio::test]
async fn neither_the_sync_package_nor_the_cloud_backup_contains_any_credential() {
    let m = Machine::new("no-creds", 1);
    let (od, s3, gist) = fully_configured(&m);
    m.db.setting_set("sync_backup_keep", "7").unwrap();
    service::save_auto(
        &m.db,
        &service::AutoSettings {
            enabled: false,
            interval_minutes: 33,
        },
    )
    .unwrap();

    // 主同步包
    let main_storage = MemoryStorage::new();
    let report = m
        .engine()
        .sync(
            &main_storage,
            PASSWORD,
            Some(crate::sync::guard::DeletionDecision::Continue),
        )
        .await
        .unwrap();
    assert_eq!(report.status, crate::sync::engine::SyncStatus::Completed);
    let main_plain = crate::sync::crypto::open(&main_storage.bytes().unwrap(), PASSWORD).unwrap();
    let main_text = String::from_utf8(main_plain.to_vec()).unwrap();
    assert!(main_text.contains("web-1"), "包里是有数据的");
    assert_no_credentials(&main_text, "主同步包");

    // 云端备份包（三个目标）
    let fake = Fake::with(&[&od, &s3, &gist]);
    backup_with(&m.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();
    for id in [&od, &s3, &gist] {
        let bytes = fake.store(id).bytes().unwrap();
        let plain = crate::sync::crypto::open(&bytes, PASSWORD).unwrap();
        let text = String::from_utf8(plain.to_vec()).unwrap();
        assert!(text.contains("web-1"));
        assert_no_credentials(&text, "云端备份包");
        // 也不能在密文外层明文里
        assert_no_credentials(&String::from_utf8_lossy(&bytes), "云端备份包（密文）");
    }
}

// ---------------------------------------------------------------------------
// 从备份目标恢复

#[tokio::test]
async fn restoring_from_a_target_makes_a_safety_backup_and_leaves_the_sync_baseline_alone() {
    let source = Machine::new("restore-src", 1);
    add_host(&source.db, "from-backup-1");
    add_host(&source.db, "from-backup-2");
    let id = source.add(webdav("a.example.com"));
    let fake = Fake::with(&[&id]);
    backup_with(&source.engine(), &fake, PASSWORD, None)
        .await
        .unwrap();

    // 另一台电脑（另一个封装密钥）：自己有一台主机，配了同一个目标
    let target = Machine::new("restore-dst", 2);
    add_host(&target.db, "only-local");
    let dst_id = target.add(webdav("a.example.com"));
    let shared = Fake {
        stores: HashMap::from([(dst_id.clone(), fake.store(&id))]),
    };
    let baseline_before = std::fs::read(target.dir.join("sync-baseline.bin")).ok();

    // 主密码错：什么都不动
    let wrong = restore_with(&target.engine(), &shared, &dst_id, "wrong-password-xx").await;
    assert!(matches!(wrong, Err(SyncError::WrongPassword)), "{wrong:?}");
    assert_eq!(host_names(&target.db), vec!["only-local"]);

    let report = restore_with(&target.engine(), &shared, &dst_id, PASSWORD)
        .await
        .unwrap();
    assert_eq!(
        host_names(&target.db),
        vec!["from-backup-1", "from-backup-2"]
    );
    // 恢复前先做了本地备份，里面是原来的数据
    assert!(report.safety_backup.name.starts_with("zssh-backup-"));
    let store = BackupStore::new(&target.dir, &target.sealer);
    let envelope = store.load(Path::new(&report.safety_backup.path)).unwrap();
    assert_eq!(envelope.snapshot.hosts[0].name, "only-local");
    // 同步基线没动（没有就还是没有）
    assert_eq!(
        std::fs::read(target.dir.join("sync-baseline.bin")).ok(),
        baseline_before
    );
    // 备份目标列表本身还在（它不在快照里，替换导入不会动它）
    assert_eq!(target.targets().len(), 1);

    // 目标上还没有备份
    let empty = Fake::with(&[&dst_id]);
    let err = restore_with(&target.engine(), &empty, &dst_id, PASSWORD)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("还没有备份"), "{err}");
}

use super::super::backup::BackupStore;
use std::path::Path;

// ---------------------------------------------------------------------------
// 名字

#[test]
fn the_backup_object_name_differs_from_the_sync_package_in_every_backend() {
    assert_ne!(OBJECT_NAME, crate::sync::webdav::OBJECT_NAME);
    // 主同步的文件名在四家都是同一个，`service::open_storage` 靠这一点用同一个常量
    assert_eq!(
        crate::sync::webdav::OBJECT_NAME,
        crate::sync::s3::OBJECT_NAME
    );
    assert_eq!(
        crate::sync::webdav::OBJECT_NAME,
        crate::sync::github::FILE_NAME
    );
    assert_eq!(
        crate::sync::webdav::OBJECT_NAME,
        crate::sync::onedrive::graph::OBJECT_NAME
    );
}

#[test]
fn descriptions_never_include_credentials() {
    for config in [
        webdav("dav.example.com"),
        s3(),
        gist(),
        onedrive("a@example.com"),
    ] {
        let text = describe(&config);
        for secret in [DAV_PASSWORD, S3_SECRET, S3_ACCESS, GIST_TOKEN] {
            assert!(!text.contains(secret), "{text}");
        }
    }
    assert_eq!(
        describe(&StorageConfig::Github(crate::sync::github::GithubConfig {
            api_base: String::new(),
            token: GIST_TOKEN.to_owned(),
            gist_id: String::new(),
            allow_insecure_http: false,
        })),
        "Gist（第一次备份时新建）"
    );
}
