//! 集成测试（U33）：云端备份目标，对着进程内模拟的 WebDAV / S3 / GitHub / OneDrive 服务跑真实的
//! 存储实现（不是内存替身）。不连任何真实云账号。
//!
//! 重点：备份对象名和主同步包不同，所以主同步和备份目标指向**同一个**目录 / 桶 / Gist / 应用文件夹时
//! 互不覆盖；备份失败不影响主同步和别的目标；备份包里没有任何凭据；从目标恢复。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
#[path = "common/fake_browser.rs"]
mod fake_browser;

#[path = "common/harness.rs"]
mod harness;
#[path = "common/mock_cloud.rs"]
mod mock_cloud;
#[path = "common/mock_github.rs"]
mod mock_github;
#[path = "common/mock_microsoft.rs"]
mod mock_microsoft;

use std::time::Duration;

use fake_browser::FakeBrowser;
use harness::*;
use mock_cloud::{Behavior, Kind, MockCloud, S3Conditional};
use mock_github::{GithubOptions, MockGithub};
use mock_microsoft::{MockMicrosoft, CLIENT_ID};
use tokio::sync::Notify;
use zssh_core::sync::backup_targets::{
    self, add_target, load_targets, OutcomeStatus, TargetOutcome, OBJECT_NAME,
};
use zssh_core::sync::engine::SyncStatus;
use zssh_core::sync::github::{self, GithubConfig};
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::onedrive::config::OneDriveConfig;
use zssh_core::sync::onedrive::oauth::Endpoints;
use zssh_core::sync::onedrive::tokens::{DbTokenStore, TokenStore};
use zssh_core::sync::remote::ConcurrencyMode;
use zssh_core::sync::s3::{self, S3Config};
use zssh_core::sync::sealer::DpapiSealer;
use zssh_core::sync::service::{self, StorageConfig, SyncContext};
use zssh_core::sync::storage::SyncStorage;
use zssh_core::sync::webdav::{self, WebDavConfig};
use zssh_core::sync::{SyncError, SyncSnapshot};

const DAV_USER: &str = "alice";
const DAV_PASSWORD: &str = "dav-test-password";
const S3_ACCESS: &str = "AKIATESTKEY";
const S3_SECRET: &str = "s3-test-secret-key";
const GH_TOKEN: &str = "ghp_mocktoken_not_a_real_token_0123456789";

fn context<'a>(machine: &'a Machine, endpoints: &Endpoints) -> SyncContext<'a> {
    SyncContext {
        db: &machine.db,
        sealer: &DpapiSealer,
        device_id: machine.device.to_owned(),
        known_hosts_path: Some(machine.dir.join("known_hosts")),
        data_dir: machine.dir.clone(),
        app_version: "0.1.0".to_owned(),
        onedrive_endpoints: endpoints.clone(),
    }
}

/// 全部走直连：测试里的服务都在本机回环地址上。
fn direct(machine: &Machine) {
    machine
        .db
        .setting_set(
            zssh_core::sync::app_proxy::APP_NETWORK_PROXY_KEY,
            "\"none\"",
        )
        .unwrap();
}

fn dav_kind() -> Kind {
    Kind::WebDav {
        user: DAV_USER.to_owned(),
        password: DAV_PASSWORD.to_owned(),
        behavior: Behavior::Full,
        put_returns_etag: true,
    }
}

fn dav_config(cloud: &MockCloud, directory: &str) -> WebDavConfig {
    WebDavConfig {
        url: format!("{}/dav/", cloud.base_url()),
        directory: directory.to_owned(),
        username: DAV_USER.to_owned(),
        password: DAV_PASSWORD.to_owned(),
        allow_insecure_http: true,
    }
}

fn s3_kind() -> Kind {
    Kind::S3 {
        access_key: S3_ACCESS.to_owned(),
        secret_key: S3_SECRET.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "demo-bucket".to_owned(),
        path_style: true,
        conditional: S3Conditional::Full,
    }
}

fn s3_config(cloud: &MockCloud) -> S3Config {
    S3Config {
        endpoint: cloud.base_url(),
        region: "us-east-1".to_owned(),
        bucket: "demo-bucket".to_owned(),
        access_key: S3_ACCESS.to_owned(),
        secret_key: S3_SECRET.to_owned(),
        prefix: "team/zssh".to_owned(),
        path_style: true,
    }
}

/// 读备份对象的内容（用和生产一样的实现，不绕过它）。
async fn read_webdav_backup(config: &WebDavConfig) -> Option<Vec<u8>> {
    webdav::open_storage_named(config, None, ConcurrencyMode::ReadBeforeWrite, OBJECT_NAME)
        .unwrap()
        .read()
        .await
        .unwrap()
        .map(|object| object.bytes)
}

async fn read_s3_backup(config: &S3Config) -> Option<Vec<u8>> {
    s3::open_storage_named(config, None, ConcurrencyMode::ReadBeforeWrite, OBJECT_NAME)
        .unwrap()
        .read()
        .await
        .unwrap()
        .map(|object| object.bytes)
}

fn status_of(outcomes: &[TargetOutcome], id: &str) -> OutcomeStatus {
    outcomes
        .iter()
        .find(|outcome| outcome.id == id)
        .unwrap_or_else(|| panic!("没有目标 {id} 的结果：{outcomes:?}"))
        .status
        .clone()
}

fn open_package(bytes: &[u8]) -> SyncSnapshot {
    SyncSnapshot::open(bytes, PASSWORD).expect("用主密码解开")
}

// ---------------------------------------------------------------------------
// WebDAV：主同步和备份在同一个目录里

#[tokio::test]
async fn webdav_sync_and_backup_in_the_same_directory_do_not_overwrite_each_other() {
    let cloud = MockCloud::start(dav_kind()).await;
    let endpoints = Endpoints::microsoft();
    let a = Machine::new("bt-dav-a", "dev-a");
    direct(&a);
    a.add_host("web-1");
    let config = dav_config(&cloud, "Z-SSH/sync");
    service::save_config(&a.db, &DpapiSealer, &StorageConfig::Webdav(config.clone())).unwrap();
    // 备份目标指向和主同步完全相同的 URL + 目录 + 账号
    let id = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Webdav(config.clone()),
        true,
    )
    .unwrap()
    .id;

    let ctx = context(&a, &endpoints);
    let report = ctx
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert_eq!(report.status, SyncStatus::Completed);
    let outcomes = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert_eq!(status_of(&outcomes, &id), OutcomeStatus::Uploaded);

    // 同一个目录里是两个对象，名字不同
    let mut paths = cloud.object_paths();
    paths.sort();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths[0].ends_with("/Z-SSH/sync/zeeterm-backup.json"),
        "{paths:?}"
    );
    assert!(
        paths[1].ends_with("/Z-SSH/sync/zssh-sync.json"),
        "{paths:?}"
    );
    let sync_bytes = cloud.sync_object().unwrap();
    let backup_bytes = read_webdav_backup(&config).await.unwrap();
    assert_ne!(sync_bytes, backup_bytes);
    assert_eq!(open_package(&sync_bytes).hosts.len(), 1);
    assert_eq!(open_package(&backup_bytes).hosts.len(), 1);

    // 再同步一轮（本机多了一台主机）：只有同步包变，备份包原样
    a.add_host("web-2");
    let report = ctx
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert_eq!(report.status, SyncStatus::Completed);
    assert_eq!(open_package(&cloud.sync_object().unwrap()).hosts.len(), 2);
    assert_eq!(
        read_webdav_backup(&config).await.unwrap(),
        backup_bytes,
        "同步不会碰备份包"
    );

    // 主同步成功后自动备份：数据变了就传，同步包原样
    let sync_after = cloud.sync_object().unwrap();
    let outcomes = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert_eq!(status_of(&outcomes, &id), OutcomeStatus::Uploaded);
    assert_eq!(cloud.sync_object().unwrap(), sync_after, "备份不会碰同步包");
    assert_eq!(
        open_package(&read_webdav_backup(&config).await.unwrap())
            .hosts
            .len(),
        2
    );
    // 还是只有两个对象
    assert_eq!(cloud.object_paths().len(), 2);

    // 数据没变：不重复上传（自动同步每隔几分钟一次）
    let unchanged = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert_eq!(status_of(&unchanged, &id), OutcomeStatus::Unchanged);

    // 另一台电脑：用主同步照常拿到数据（备份对象的存在不影响它）
    let b = Machine::new("bt-dav-b", "dev-b");
    direct(&b);
    service::save_config(&b.db, &DpapiSealer, &StorageConfig::Webdav(config.clone())).unwrap();
    let report = context(&b, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert_eq!(report.status, SyncStatus::Completed);
    assert_eq!(b.host_names(), vec!["web-1", "web-2"]);
}

#[tokio::test]
async fn webdav_restore_from_a_target_replaces_local_data_after_a_safety_backup() {
    let cloud = MockCloud::start(dav_kind()).await;
    let endpoints = Endpoints::microsoft();
    let a = Machine::new("bt-rest-a", "dev-a");
    direct(&a);
    a.add_host("web-1");
    a.add_host("web-2");
    let config = dav_config(&cloud, "backup");
    let id = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Webdav(config.clone()),
        true,
    )
    .unwrap()
    .id;
    // 没有配置主同步也能单独备份（用本机当前数据）
    assert!(service::load_config(&a.db, &DpapiSealer).unwrap().is_none());
    let outcomes = context(&a, &endpoints)
        .backup_to_targets(PASSWORD, Some(&id))
        .await
        .unwrap();
    assert_eq!(status_of(&outcomes, &id), OutcomeStatus::Uploaded);
    assert_eq!(cloud.object_paths().len(), 1);

    // 新电脑：配同一个目标，从这里恢复
    let b = Machine::new("bt-rest-b", "dev-b");
    direct(&b);
    b.add_host("only-on-b");
    let id_b = add_target(
        &b.db,
        &DpapiSealer,
        None,
        StorageConfig::Webdav(config.clone()),
        true,
    )
    .unwrap()
    .id;
    let wrong = context(&b, &endpoints)
        .restore_from_target(&id_b, "not-the-master-password")
        .await;
    assert!(matches!(wrong, Err(SyncError::WrongPassword)), "{wrong:?}");
    assert_eq!(b.host_names(), vec!["only-on-b"], "密码错一个字节不动");

    let report = context(&b, &endpoints)
        .restore_from_target(&id_b, PASSWORD)
        .await
        .unwrap();
    assert_eq!(b.host_names(), vec!["web-1", "web-2"]);
    assert!(report.safety_backup.name.starts_with("zssh-backup-"));
    let backups = b.engine().list_backups().unwrap();
    assert_eq!(backups.len(), 1, "恢复前做了一份本地备份");
    assert_eq!(backups[0].host_count, Some(1));
    // 没动同步基线
    assert!(b.engine().load_baseline().unwrap().is_none());
    // 备份目标还在
    assert_eq!(load_targets(&b.db, &DpapiSealer).unwrap().len(), 1);
}

#[tokio::test]
async fn a_failing_target_does_not_stop_the_others_or_the_main_sync() {
    let good = MockCloud::start(dav_kind()).await;
    let bad = MockCloud::start(dav_kind()).await;
    let endpoints = Endpoints::microsoft();
    let a = Machine::new("bt-fail", "dev-a");
    direct(&a);
    a.add_host("web-1");
    service::save_config(
        &a.db,
        &DpapiSealer,
        &StorageConfig::Webdav(dav_config(&good, "sync")),
    )
    .unwrap();
    // 第一个目标密码是错的（401），第二个正常
    let mut wrong = dav_config(&bad, "backup");
    wrong.password = "wrong-password".to_owned();
    let first = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Webdav(wrong),
        true,
    )
    .unwrap()
    .id;
    let second = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Webdav(dav_config(&good, "backup")),
        true,
    )
    .unwrap()
    .id;

    let ctx = context(&a, &endpoints);
    let report = ctx
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert_eq!(report.status, SyncStatus::Completed, "主同步不受影响");
    let outcomes = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert!(
        matches!(status_of(&outcomes, &first), OutcomeStatus::Failed(_)),
        "{outcomes:?}"
    );
    assert_eq!(status_of(&outcomes, &second), OutcomeStatus::Uploaded);

    let targets = load_targets(&a.db, &DpapiSealer).unwrap();
    let last = |id: &str| {
        targets
            .iter()
            .find(|t| t.id == id)
            .unwrap()
            .last
            .clone()
            .unwrap()
    };
    assert!(!last(&first).ok);
    assert!(
        !last(&first).message.contains("wrong-password"),
        "失败原因里不能带密码"
    );
    assert!(last(&second).ok);
    assert!(bad.object_paths().is_empty());
}

// ---------------------------------------------------------------------------
// S3：同一个桶、同一个前缀

#[tokio::test]
async fn s3_sync_and_backup_in_the_same_bucket_and_prefix_do_not_collide() {
    let cloud = MockCloud::start(s3_kind()).await;
    let endpoints = Endpoints::microsoft();
    let a = Machine::new("bt-s3", "dev-a");
    direct(&a);
    a.add_host("web-1");
    let config = s3_config(&cloud);
    service::save_config(&a.db, &DpapiSealer, &StorageConfig::S3(config.clone())).unwrap();
    let id = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::S3(config.clone()),
        true,
    )
    .unwrap()
    .id;

    let ctx = context(&a, &endpoints);
    ctx.run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    let outcomes = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert_eq!(
        status_of(&outcomes, &id),
        OutcomeStatus::Uploaded,
        "{outcomes:?}"
    );

    let mut paths = cloud.object_paths();
    paths.sort();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths[0].ends_with("team/zssh/zeeterm-backup.json"),
        "{paths:?}"
    );
    assert!(paths[1].ends_with("team/zssh/zssh-sync.json"), "{paths:?}");
    let backup = read_s3_backup(&config).await.unwrap();
    assert_eq!(open_package(&backup).hosts.len(), 1);

    // 备份包里看不到任何凭据（包括 S3 自己的 Access Key / Secret）
    let plain = zssh_core::sync::crypto::open(&backup, PASSWORD).unwrap();
    let text = String::from_utf8_lossy(&plain).into_owned();
    for needle in [
        S3_ACCESS,
        S3_SECRET,
        "demo-bucket",
        "sync_storage",
        "sync_backup_targets",
    ] {
        assert!(!text.contains(needle), "备份包里不能有：{needle}");
    }
    let sync_plain =
        zssh_core::sync::crypto::open(&cloud.sync_object().unwrap(), PASSWORD).unwrap();
    for needle in [S3_ACCESS, S3_SECRET, "sync_storage", "sync_backup_targets"] {
        assert!(
            !String::from_utf8_lossy(&sync_plain).contains(needle),
            "同步包里不能有：{needle}"
        );
    }
}

// ---------------------------------------------------------------------------
// GitHub Gist：同一个 Gist 里两个文件；ID 留空时自动新建

#[tokio::test]
async fn gist_sync_and_backup_share_one_gist_with_two_files() {
    let gh = MockGithub::start(GithubOptions::classic(GH_TOKEN, "demo-user")).await;
    let endpoints = Endpoints::microsoft();
    let a = Machine::new("bt-gist", "dev-a");
    direct(&a);
    a.add_host("web-1");
    let base = GithubConfig {
        api_base: gh.api_base(),
        token: GH_TOKEN.to_owned(),
        gist_id: String::new(),
        allow_insecure_http: true,
    };
    // 主同步先新建一个 Gist，备份目标指向同一个
    let gist_id = github::create_gist(&base, None).await.unwrap();
    let with_id = GithubConfig {
        gist_id: gist_id.clone(),
        ..base.clone()
    };
    service::save_config(&a.db, &DpapiSealer, &StorageConfig::Github(with_id.clone())).unwrap();
    let id = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Github(with_id),
        true,
    )
    .unwrap()
    .id;
    let ctx = context(&a, &endpoints);
    ctx.run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    let outcomes = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert_eq!(
        status_of(&outcomes, &id),
        OutcomeStatus::Uploaded,
        "{outcomes:?}"
    );

    let files = gh.gist_files(&gist_id);
    assert_eq!(
        files.keys().cloned().collect::<Vec<_>>(),
        vec![
            "zeeterm-backup.json".to_owned(),
            "zssh-sync.json".to_owned()
        ]
    );
    assert_ne!(files["zeeterm-backup.json"], files["zssh-sync.json"]);

    // 主同步照常能读到自己的包（备份文件不会被当成同步包，也不会被覆盖）
    let again = ctx
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert_eq!(again.status, SyncStatus::Completed);
    assert_eq!(
        gh.gist_files(&gist_id)["zeeterm-backup.json"],
        files["zeeterm-backup.json"]
    );
}

#[tokio::test]
async fn a_gist_target_without_an_id_creates_one_on_the_first_backup() {
    let gh = MockGithub::start(GithubOptions::classic(GH_TOKEN, "demo-user")).await;
    let endpoints = Endpoints::microsoft();
    let a = Machine::new("bt-gist-new", "dev-a");
    direct(&a);
    a.add_host("web-1");
    let id = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Github(GithubConfig {
            api_base: gh.api_base(),
            token: GH_TOKEN.to_owned(),
            gist_id: String::new(),
            allow_insecure_http: true,
        }),
        true,
    )
    .unwrap()
    .id;
    let ctx = context(&a, &endpoints);
    let outcomes = ctx.backup_to_targets(PASSWORD, Some(&id)).await.unwrap();
    assert_eq!(
        status_of(&outcomes, &id),
        OutcomeStatus::Uploaded,
        "{outcomes:?}"
    );
    let ids = gh.gist_ids();
    assert_eq!(ids.len(), 1);
    assert_eq!(
        gh.gist_is_public(&ids[0]),
        Some(false),
        "必须是 secret Gist"
    );
    assert_eq!(
        gh.gist_files(&ids[0]).keys().cloned().collect::<Vec<_>>(),
        vec!["zeeterm-backup.json".to_owned()],
        "新建的 Gist 里只有备份文件，没有多余的同步包占位"
    );
    // 新 ID 记下来了，第二次不会再建
    let targets = load_targets(&a.db, &DpapiSealer).unwrap();
    assert!(matches!(&targets[0].config, StorageConfig::Github(c) if c.gist_id == ids[0]));
    ctx.backup_to_targets(PASSWORD, Some(&id)).await.unwrap();
    assert_eq!(gh.gist_ids().len(), 1);
}

// ---------------------------------------------------------------------------
// OneDrive：两份令牌互不清除

#[tokio::test]
async fn onedrive_main_sync_and_target_use_separate_tokens_and_the_same_app_folder() {
    let mock = MockMicrosoft::start().await;
    let endpoints = mock.endpoints();
    let a = Machine::new("bt-od", "dev-a");
    direct(&a);
    a.add_host("web-1");

    // 主同步登录
    service::onedrive_sign_in(
        &a.db,
        &DpapiSealer,
        &endpoints,
        CLIENT_ID,
        Duration::from_secs(20),
        &FakeBrowser::approving(),
        &Notify::new(),
    )
    .await
    .expect("主同步登录");
    // 备份目标登录（另一个 id 下的令牌）
    let target_id = backup_targets::new_target_id();
    let signed = backup_targets::onedrive_sign_in_for_target(
        &a.db,
        &DpapiSealer,
        &endpoints,
        &target_id,
        CLIENT_ID,
        Duration::from_secs(20),
        &FakeBrowser::approving(),
        &Notify::new(),
    )
    .await
    .expect("备份目标登录");
    add_target(
        &a.db,
        &DpapiSealer,
        Some(target_id.clone()),
        StorageConfig::Onedrive(OneDriveConfig {
            client_id: CLIENT_ID.to_owned(),
            account: signed.account,
        }),
        true,
    )
    .unwrap();
    // 登录目标没有动主同步的连接设置
    assert!(matches!(
        service::load_config(&a.db, &DpapiSealer).unwrap(),
        Some(StorageConfig::Onedrive(_))
    ));
    let main_tokens = DbTokenStore::new(&a.db, &DpapiSealer)
        .load()
        .unwrap()
        .unwrap();
    let target_tokens = backup_targets::token_store(&a.db, &DpapiSealer, &target_id)
        .load()
        .unwrap()
        .unwrap();
    assert_ne!(main_tokens.refresh_token, target_tokens.refresh_token);

    let ctx = context(&a, &endpoints);
    let report = ctx
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert_eq!(report.status, SyncStatus::Completed);
    let outcomes = ctx.backup_to_targets(PASSWORD, None).await.unwrap();
    assert_eq!(
        status_of(&outcomes, &target_id),
        OutcomeStatus::Uploaded,
        "{outcomes:?}"
    );
    let mut names = mock.file_names();
    names.sort();
    assert_eq!(names, vec!["zeeterm-backup.json", "zssh-sync.json"]);
    assert_eq!(
        open_package(&mock.file("zeeterm-backup.json").unwrap())
            .hosts
            .len(),
        1
    );

    // 主同步断开：目标的令牌还在、还能备份
    service::onedrive_disconnect(&a.db).unwrap();
    assert!(DbTokenStore::new(&a.db, &DpapiSealer)
        .load()
        .unwrap()
        .is_none());
    assert!(backup_targets::token_store(&a.db, &DpapiSealer, &target_id)
        .load()
        .unwrap()
        .is_some());
    a.add_host("web-2");
    let outcomes = ctx
        .backup_to_targets(PASSWORD, Some(&target_id))
        .await
        .unwrap();
    assert_eq!(
        status_of(&outcomes, &target_id),
        OutcomeStatus::Uploaded,
        "{outcomes:?}"
    );
    assert_eq!(
        open_package(&mock.file("zeeterm-backup.json").unwrap())
            .hosts
            .len(),
        2
    );

    // 主同步换成别的存储（save_config 清主同步令牌）也不影响目标
    service::save_config(
        &a.db,
        &DpapiSealer,
        &StorageConfig::Webdav(WebDavConfig {
            url: "https://dav.example.invalid/".to_owned(),
            directory: String::new(),
            username: "u".to_owned(),
            password: "p".to_owned(),
            allow_insecure_http: false,
        }),
    )
    .unwrap();
    assert!(backup_targets::token_store(&a.db, &DpapiSealer, &target_id)
        .load()
        .unwrap()
        .is_some());
    // 设置表里这个令牌也是密文
    let raw =
        a.db.setting_get(&backup_targets::token_key(&target_id))
            .unwrap()
            .unwrap();
    assert!(!raw.contains(&target_tokens.refresh_token));
}

#[tokio::test]
async fn a_revoked_onedrive_login_fails_only_that_target() {
    let mock = MockMicrosoft::start().await;
    let endpoints = mock.endpoints();
    let cloud = MockCloud::start(dav_kind()).await;
    let a = Machine::new("bt-od-revoked", "dev-a");
    direct(&a);
    a.add_host("web-1");
    let target_id = backup_targets::new_target_id();
    backup_targets::onedrive_sign_in_for_target(
        &a.db,
        &DpapiSealer,
        &endpoints,
        &target_id,
        CLIENT_ID,
        Duration::from_secs(20),
        &FakeBrowser::approving(),
        &Notify::new(),
    )
    .await
    .unwrap();
    let od = add_target(
        &a.db,
        &DpapiSealer,
        Some(target_id),
        StorageConfig::Onedrive(OneDriveConfig {
            client_id: CLIENT_ID.to_owned(),
            account: String::new(),
        }),
        true,
    )
    .unwrap()
    .id;
    let dav = add_target(
        &a.db,
        &DpapiSealer,
        None,
        StorageConfig::Webdav(dav_config(&cloud, "backup")),
        true,
    )
    .unwrap()
    .id;
    mock.revoke_all();
    mock.expire_access_tokens();
    let outcomes = context(&a, &endpoints)
        .backup_to_targets(PASSWORD, None)
        .await
        .unwrap();
    assert!(
        matches!(status_of(&outcomes, &od), OutcomeStatus::Failed(_)),
        "{outcomes:?}"
    );
    assert_eq!(status_of(&outcomes, &dav), OutcomeStatus::Uploaded);
}
