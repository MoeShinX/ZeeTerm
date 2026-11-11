//! G11：四类存储都用本机桩跑正反路径（不连任何真实云存储）。
//!
//! 每个用例自己起一个桩（互不干扰）、用独立的临时数据目录；「另一台电脑」是桩模块里的 `peer_run_at`
//! （自己的库和设备 ID，和被测的这台共用同一个云端）。被测的入口就是设置页按钮调的那几个函数：
//! `probe_connection_with`（测试连接）、`run_sync_with`（立即同步）、`change_password_with`（修改主密码）、
//! `service::onedrive_sign_in`（OneDrive 登录）。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::Notify;
use zssh_core::store::Database;
use zssh_core::sync::engine::SyncStatus;
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::onedrive::login::LoginError;
use zssh_core::sync::remote::ConcurrencyMode;
use zssh_core::sync::service::{self, StorageConfig};

use super::actions::change_password_with;
use super::{FailureKind, ensure_github_gist, probe_connection_with, run_sync_with};
use crate::sync_stub::{
    DAV_PASS, GH_TOKEN, MASTER_PASSWORD, OD_CLIENT_ID, OneDriveMode, S3_ACCESS, StubServer,
    XorSealer, peer_run_at,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// 一台被测的电脑：独立的数据目录和库。
struct Device {
    // 字段按声明顺序析构：`db` 必须排在 `_guard` 前面，先关库再删目录；
    // 在 Drop 里删目录会先于字段析构执行，Windows 上库文件还开着，目录删不掉。
    db: Database,
    dir: PathBuf,
    _guard: tempfile::TempDir,
}

impl Device {
    fn new(name: &str) -> Self {
        let guard = tempfile::Builder::new()
            .prefix(&format!("zssh-g11-{name}-"))
            .tempdir()
            .expect("建目录");
        let dir = guard.path().to_path_buf();
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        Self {
            db,
            dir,
            _guard: guard,
        }
    }

    fn configure(&self, config: &StorageConfig) {
        service::save_config(&self.db, &XorSealer(0x5A), config).expect("保存存储设置");
    }

    fn host_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .db
            .host_list()
            .expect("读主机")
            .into_iter()
            .map(|host| host.name)
            .collect();
        names.sort();
        names
    }

    fn sync(
        &self,
        handle: &tokio::runtime::Handle,
        password: &str,
        decision: Option<DeletionDecision>,
    ) -> Result<zssh_core::sync::engine::SyncReport, super::SyncFailure> {
        run_sync_with(handle, &self.dir, &XorSealer(0x5A), password, decision)
    }
}

/// 「另一台电脑」的数据目录（每个用例一个，随机唯一的名字，用完删）。
struct PeerDir(tempfile::TempDir);

impl PeerDir {
    fn new(name: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(&format!("zssh-g11-peer-{name}-"))
                .tempdir()
                .expect("建目录"),
        )
    }
}

fn peer(server: &StubServer, handle: &tokio::runtime::Handle, kind: &str, op: &str, dir: &PeerDir) {
    peer_run_at(server, handle, &XorSealer(0x6B), dir.0.path(), kind, op).expect("另一台电脑");
}

/// 每种存储第一次同步需要的 Gist ID（只有 GitHub 要）。
fn gist_for(server: &StubServer, kind: &str) -> String {
    if kind == "github" {
        server.first_gist_id().unwrap_or_default()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// 连接：成功 / 凭据错 / 并发保护能力
// ---------------------------------------------------------------------------

/// 测试连接通过：读、写都通；并发保护的结论和存储种类一致。
fn assert_connects(kind: &str, expected_mode: ConcurrencyMode) {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let device = Device::new(&format!("connect-{kind}"));
    // GitHub 要先有个 Gist：测试连接要 Gist ID，第一次同步才会自动新建。
    if kind == "github" {
        let config = server.storage_config(kind, "").expect("设置");
        device.configure(&config);
        ensure_github_gist(runtime.handle(), &device.db, &XorSealer(0x5A)).expect("新建 Gist");
    } else {
        device.configure(&server.storage_config(kind, "").expect("设置"));
    }
    let config = service::load_config(&device.db, &XorSealer(0x5A))
        .expect("读设置")
        .expect("有设置");
    let probe = probe_connection_with(runtime.handle(), &device.db, &XorSealer(0x5A), &config)
        .expect("测试连接");
    assert!(probe.read.ok, "{}", probe.read.message);
    assert!(probe.write.ok, "{}", probe.write.message);
    assert_eq!(probe.concurrency.expect("检测结果").mode, expected_mode);
    assert!(!probe.has_existing_data, "云端还是空的");
    // 检测结果落了库：之后的同步按它走。
    assert_eq!(
        service::load_concurrency(&device.db)
            .expect("读")
            .expect("有记录")
            .mode,
        expected_mode
    );
}

#[test]
fn webdav_connects_and_reports_conditional_writes() {
    assert_connects("webdav", ConcurrencyMode::Conditional);
}

#[test]
fn s3_connects_and_reports_conditional_writes() {
    assert_connects("s3", ConcurrencyMode::Conditional);
}

#[test]
fn github_connects_and_says_it_cannot_do_conditional_writes() {
    assert_connects("github", ConcurrencyMode::ReadBeforeWrite);
}

/// 凭据错：测试连接说明失败，同步按「凭据被拒」分类（调度器据此暂停，不再每个间隔打一次）。
fn assert_wrong_credentials(kind: &str, break_it: impl FnOnce(&mut StorageConfig)) {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let device = Device::new(&format!("wrong-{kind}"));
    let mut config = server
        .storage_config(kind, "a1b2c3d4e5f600000001")
        .expect("设置");
    break_it(&mut config);
    device.configure(&config);
    let probe = probe_connection_with(runtime.handle(), &device.db, &XorSealer(0x5A), &config)
        .expect("测试连接本身不报错，结果里写明失败");
    assert!(
        !probe.read.ok,
        "凭据错时读取应当失败：{}",
        probe.read.message
    );
    assert!(!probe.write.ok);
    assert!(probe.concurrency.is_none(), "没通就不检测并发保护");
    let failure = device
        .sync(runtime.handle(), MASTER_PASSWORD, None)
        .expect_err("凭据错时同步应当失败");
    assert!(
        matches!(failure.kind, FailureKind::AuthFailed(_)),
        "{kind}：{:?} {}",
        failure.kind,
        failure.message
    );
    // 桩没有收到过任何同步包的写入。
    assert_eq!(server.with(|state| state.package_requests("PUT")), 0);
}

#[test]
fn webdav_wrong_password_is_an_auth_failure() {
    assert_wrong_credentials("webdav", |config| {
        if let StorageConfig::Webdav(webdav) = config {
            webdav.password = format!("{DAV_PASS}-wrong");
        }
    });
}

#[test]
fn s3_wrong_access_key_is_an_auth_failure() {
    assert_wrong_credentials("s3", |config| {
        if let StorageConfig::S3(s3) = config {
            s3.access_key = format!("{S3_ACCESS}X");
        }
    });
}

#[test]
fn github_wrong_token_is_an_auth_failure() {
    assert_wrong_credentials("github", |config| {
        if let StorageConfig::Github(github) = config {
            github.token = format!("{GH_TOKEN}-wrong");
        }
    });
}

// ---------------------------------------------------------------------------
// 同步：第二台电脑、并发写、防误删（取消 / 继续 / 用云端覆盖）
// ---------------------------------------------------------------------------

/// 第二台电脑和并发写（同一条链，省 Argon2 的时间）：
/// - 另一台电脑先传了 3 台主机；被测的这台测试连接看到「云端已有数据」，用主密码解开并合并，拿到 3 台；
///   主密码填错的另一台被拒（按「主密码不对」分类），本地一条也没多；
/// - 对方再加一台、这边也加一台；这次同步读完之后云端又被别处写了一次：第一次写被拒，
///   core 重新读、合并、再写，最后成功，两边新加的都在。
fn assert_second_computer_and_concurrent_write(kind: &str) {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let peer_dir = PeerDir::new(&format!("second-{kind}"));
    peer(&server, runtime.handle(), kind, "hosts:3", &peer_dir);
    let device = Device::new(&format!("second-{kind}"));
    device.configure(
        &server
            .storage_config(kind, &gist_for(&server, kind))
            .expect("设置"),
    );
    let config = service::load_config(&device.db, &XorSealer(0x5A))
        .expect("读")
        .expect("有");
    let probe = probe_connection_with(runtime.handle(), &device.db, &XorSealer(0x5A), &config)
        .expect("测试");
    assert!(probe.has_existing_data, "{kind}：云端已有同步数据");
    let report = device
        .sync(runtime.handle(), MASTER_PASSWORD, None)
        .expect("同步");
    assert_eq!(report.status, SyncStatus::Completed, "{}", report.summary);
    assert_eq!(device.host_names().len(), 3);

    let other = Device::new(&format!("second-wrong-{kind}"));
    other.configure(
        &server
            .storage_config(kind, &gist_for(&server, kind))
            .expect("设置"),
    );
    let failure = other
        .sync(runtime.handle(), "not-the-master-pass", None)
        .expect_err("主密码不对");
    assert_eq!(
        failure.kind,
        FailureKind::WrongPassword,
        "{}",
        failure.message
    );
    assert!(other.host_names().is_empty());

    peer(&server, runtime.handle(), kind, "hosts:1", &peer_dir);
    device
        .db
        .host_save(&crate::sync_stub::draft_for_tests("local-extra"))
        .expect("本地加一台");
    server.with(|state| state.touch_after_next_read = true);
    let report = device
        .sync(runtime.handle(), MASTER_PASSWORD, None)
        .expect("并发写之后重试");
    assert_eq!(report.status, SyncStatus::Completed, "{}", report.summary);
    assert!(
        report.retries >= 1,
        "{kind}：应当重试过：{}",
        report.retries
    );
    assert!(device.host_names().contains(&"local-extra".to_owned()));
    assert!(device.host_names().contains(&"peer-4".to_owned()));
}

#[test]
fn webdav_second_computer_unlocks_and_a_concurrent_write_is_retried() {
    assert_second_computer_and_concurrent_write("webdav");
}

#[test]
fn s3_second_computer_unlocks_and_a_concurrent_write_is_retried() {
    assert_second_computer_and_concurrent_write("s3");
}

#[test]
fn github_second_computer_unlocks_and_a_concurrent_write_is_retried() {
    assert_second_computer_and_concurrent_write("github");
}

/// 防误删：对方删了 10 台里的 8 台，这台同步要删本地的 8 台（超过一半）→ 停在确认上，什么也没动；
/// 取消 = 本地和云端都不动；然后再同步一次选 `last`：继续 = 按合并结果删，用云端版本覆盖本地 =
/// 以云端为准（两者这里的结果都是只剩 2 台）。
fn assert_deletion_confirmation(kind: &str, last: DeletionDecision) {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let peer_dir = PeerDir::new(&format!("delete-{kind}"));
    peer(&server, runtime.handle(), kind, "hosts:10", &peer_dir);
    let device = Device::new(&format!("delete-{kind}"));
    device.configure(
        &server
            .storage_config(kind, &gist_for(&server, kind))
            .expect("设置"),
    );
    device
        .sync(runtime.handle(), MASTER_PASSWORD, None)
        .expect("先拿到 10 台");
    assert_eq!(device.host_names().len(), 10);

    peer(&server, runtime.handle(), kind, "delete:8", &peer_dir);
    let report = device
        .sync(runtime.handle(), MASTER_PASSWORD, None)
        .expect("同步");
    assert_eq!(
        report.status,
        SyncStatus::NeedsConfirmation,
        "{}",
        report.summary
    );
    assert_eq!(report.deletion.deleted_records, 8);
    assert_eq!(report.deletion.local_records, 10);
    assert_eq!(report.deletion.deleted_hosts, 8);
    assert!(!report.deletion.samples.is_empty(), "要列出几条具体的名字");
    assert_eq!(device.host_names().len(), 10, "确认之前一条都不能删");

    // 取消：本地不动。
    let cancelled = device
        .sync(
            runtime.handle(),
            MASTER_PASSWORD,
            Some(DeletionDecision::Cancel),
        )
        .expect("取消");
    assert_eq!(cancelled.status, SyncStatus::Cancelled);
    assert_eq!(device.host_names().len(), 10);

    // 和界面一样：确认的是面板上那份计划，带着它的身份回去。
    let confirmed = match last {
        DeletionDecision::Continue => DeletionDecision::ContinuePlan(report.deletion.token),
        DeletionDecision::UseCloud => DeletionDecision::UseCloudPlan(report.deletion.token),
        other => other,
    };
    let report = device
        .sync(runtime.handle(), MASTER_PASSWORD, Some(confirmed))
        .expect("第二次选择");
    assert_eq!(report.status, SyncStatus::Completed, "{}", report.summary);
    assert_eq!(
        device.host_names().len(),
        2,
        "{kind}：{last:?} 之后只剩 2 台"
    );
}

#[test]
fn webdav_deletion_needs_confirmation_cancel_then_continue() {
    assert_deletion_confirmation("webdav", DeletionDecision::Continue);
}

#[test]
fn s3_deletion_needs_confirmation_cancel_then_use_cloud() {
    assert_deletion_confirmation("s3", DeletionDecision::UseCloud);
}

#[test]
fn github_deletion_needs_confirmation_cancel_then_use_cloud() {
    assert_deletion_confirmation("github", DeletionDecision::UseCloud);
}

/// 限流：GitHub 把 429 + `Retry-After` 翻成「限流，要等多久」，调度器据此到点之前不发请求；
/// WebDAV / S3 的 429 只是临时不可用（core 自己重试几次后报错），不会被当成要暂停的限流。
#[test]
fn rate_limits_are_told_apart_per_storage() {
    let runtime = runtime();
    for (kind, limited) in [("github", true), ("webdav", false), ("s3", false)] {
        let server = StubServer::start().expect("桩");
        let peer_dir = PeerDir::new(&format!("limit-{kind}"));
        peer(&server, runtime.handle(), kind, "hosts:1", &peer_dir);
        let device = Device::new(&format!("limit-{kind}"));
        device.configure(
            &server
                .storage_config(kind, &gist_for(&server, kind))
                .expect("设置"),
        );
        server.with(|state| state.rate_limit_left = 100);
        let failure = device
            .sync(runtime.handle(), MASTER_PASSWORD, None)
            .expect_err("被限流时同步失败");
        match (&failure.kind, limited) {
            (
                FailureKind::RateLimited {
                    retry_after_secs, ..
                },
                true,
            ) => {
                assert_eq!(*retry_after_secs, 120, "取自 Retry-After");
            }
            (FailureKind::Other, false) => {
                assert!(
                    failure.message.contains("429"),
                    "{kind}：{}",
                    failure.message
                );
            }
            other => panic!("{kind}：分类不对：{other:?} {}", failure.message),
        }
        assert!(device.host_names().is_empty(), "失败时本地不能有半截数据");
    }
}

// ---------------------------------------------------------------------------
// 修改主密码
// ---------------------------------------------------------------------------

#[test]
fn changing_the_master_password_reencrypts_the_cloud_package() {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let peer_dir = PeerDir::new("pw");
    peer(&server, runtime.handle(), "webdav", "hosts:2", &peer_dir);
    let device = Device::new("pw");
    device.configure(&server.storage_config("webdav", "").expect("设置"));

    // 旧密码不对：被拒，云端不变（旧密码照常能解）。
    let wrong = change_password_with(
        runtime.handle(),
        &device.dir,
        &XorSealer(0x5A),
        "not-the-old-pass",
        "new-master-pass-9",
    )
    .expect_err("旧密码不对");
    assert!(!wrong.is_empty());
    // 新密码太短：本地先拦住，不碰云端。
    assert!(
        change_password_with(
            runtime.handle(),
            &device.dir,
            &XorSealer(0x5A),
            MASTER_PASSWORD,
            "short"
        )
        .is_err()
    );

    change_password_with(
        runtime.handle(),
        &device.dir,
        &XorSealer(0x5A),
        MASTER_PASSWORD,
        "new-master-pass-9",
    )
    .expect("改成功");
    // 之后：新密码能同步，旧密码解不开。
    let report = device
        .sync(runtime.handle(), "new-master-pass-9", None)
        .expect("新密码");
    assert_eq!(report.status, SyncStatus::Completed);
    let failure = device
        .sync(runtime.handle(), MASTER_PASSWORD, None)
        .expect_err("旧密码已经解不开");
    assert_eq!(failure.kind, FailureKind::WrongPassword);
}

// ---------------------------------------------------------------------------
// OneDrive：只到「打开授权网址 → 回调 → 换令牌 → 读账号名」
// ---------------------------------------------------------------------------

fn sign_in(
    server: &StubServer,
    runtime: &tokio::runtime::Runtime,
    device: &Device,
    client_id: &str,
    timeout: Duration,
    cancel: &Notify,
) -> Result<zssh_core::sync::service::SignedIn, LoginError> {
    let endpoints = server.onedrive_endpoints();
    let opener = server.onedrive_opener();
    runtime.block_on(service::onedrive_sign_in(
        &device.db,
        &XorSealer(0x5A),
        &endpoints,
        client_id,
        timeout,
        opener.as_ref(),
        cancel,
    ))
}

#[test]
fn onedrive_login_opens_the_authorize_url_and_handles_the_callback() {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let device = Device::new("od-ok");
    let signed = sign_in(
        &server,
        &runtime,
        &device,
        OD_CLIENT_ID,
        Duration::from_secs(20),
        &Notify::new(),
    )
    .expect("登录");
    assert_eq!(signed.config.account, "桩账号");
    assert_eq!(signed.config.client_id, OD_CLIENT_ID);
    // 打开的网址就是授权页，带着我们的 client ID 和 PKCE 参数；令牌和连接设置都落了库。
    let opened = server.with(|state| state.opened_urls());
    assert_eq!(opened.len(), 1);
    assert!(opened[0].contains("code_challenge="), "{}", opened[0]);
    assert!(opened[0].contains(&format!("client_id={OD_CLIENT_ID}")));
    assert_eq!(
        server.with(|state| state.client_ids_seen()),
        vec![OD_CLIENT_ID.to_owned()]
    );
    assert!(matches!(
        service::load_config(&device.db, &XorSealer(0x5A))
            .expect("读")
            .expect("有"),
        StorageConfig::Onedrive(_)
    ));
    // 退出：令牌和设置都删。
    service::onedrive_disconnect(&device.db).expect("退出");
    assert!(
        service::load_config(&device.db, &XorSealer(0x5A))
            .expect("读")
            .is_none()
    );
}

#[test]
fn onedrive_login_failures_leave_nothing_behind() {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let device = Device::new("od-bad");

    // 用户在浏览器里点了拒绝。
    server.with(|state| state.set_onedrive_mode(OneDriveMode::Deny));
    let denied = sign_in(
        &server,
        &runtime,
        &device,
        OD_CLIENT_ID,
        Duration::from_secs(20),
        &Notify::new(),
    )
    .expect_err("拒绝");
    assert!(matches!(denied, LoginError::Denied), "{denied}");

    // 浏览器一直不回来：取消。
    server.with(|state| state.set_onedrive_mode(OneDriveMode::Hang));
    let cancel = std::sync::Arc::new(Notify::new());
    let trigger = std::sync::Arc::clone(&cancel);
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        trigger.notify_one();
    });
    let cancelled = sign_in(
        &server,
        &runtime,
        &device,
        OD_CLIENT_ID,
        Duration::from_secs(20),
        &cancel,
    )
    .expect_err("取消");
    assert!(matches!(cancelled, LoginError::Cancelled), "{cancelled}");

    // 超时。
    let timed_out = sign_in(
        &server,
        &runtime,
        &device,
        OD_CLIENT_ID,
        Duration::from_millis(300),
        &Notify::new(),
    )
    .expect_err("超时");
    assert!(matches!(timed_out, LoginError::Timeout), "{timed_out}");

    // client ID 格式不对：根本不会打开浏览器。
    let before = server.with(|state| state.opened_urls().len());
    let invalid = sign_in(
        &server,
        &runtime,
        &device,
        "not-a-guid",
        Duration::from_secs(5),
        &Notify::new(),
    )
    .expect_err("格式不对");
    assert!(matches!(invalid, LoginError::InvalidClientId), "{invalid}");
    assert_eq!(server.with(|state| state.opened_urls().len()), before);

    // 失败都没有留下任何连接设置。
    assert!(
        service::load_config(&device.db, &XorSealer(0x5A))
            .expect("读")
            .is_none()
    );
}

#[test]
fn the_stub_browser_only_opens_the_local_authorize_page() {
    let server = StubServer::start().expect("桩");
    let opener = server.onedrive_opener();
    for url in [
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize",
        "https://account.live.com/consent/Manage",
        "file:///C:/Windows/System32/calc.exe",
    ] {
        assert!(opener.open(url).is_err(), "{url} 不该被打开");
    }
    assert!(server.with(|state| state.opened_urls().is_empty()));
}
