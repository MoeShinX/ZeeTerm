//! 集成测试（S-4）：OneDrive 在“设置页背后的服务层”的整条链路——登录并保存、按保存的设置同步、
//! 测试连接、断开、换存储时清掉令牌。用的是和界面命令同一批函数（`service::onedrive_sign_in`、
//! `SyncContext::run_sync`、`service::test_connection`、`service::onedrive_disconnect`），
//! 微软服务是进程内的模拟。

#[path = "common/fake_browser.rs"]
mod fake_browser;
#[path = "common/harness.rs"]
mod harness;
#[path = "common/mock_microsoft.rs"]
mod mock_microsoft;

use std::sync::Arc;
use std::time::Duration;

use fake_browser::FakeBrowser;
use harness::*;
use mock_microsoft::{MockMicrosoft, ACCOUNT_NAME, CLIENT_ID};
use tokio::sync::Notify;
use zssh_core::sync::engine::SyncStatus;
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::onedrive::login::LoginError;
use zssh_core::sync::onedrive::oauth::Endpoints;
use zssh_core::sync::onedrive::tokens::{DbTokenStore, TokenStore, TOKENS_KEY};
use zssh_core::sync::remote::ConcurrencyMode;
use zssh_core::sync::sealer::DpapiSealer;
use zssh_core::sync::service::{self, StorageConfig, SyncContext};
use zssh_core::sync::webdav::WebDavConfig;
use zssh_core::sync::SyncError;

fn context<'a>(machine: &'a Machine, endpoints: &Endpoints) -> SyncContext<'a> {
    SyncContext {
        db: &machine.db,
        sealer: &DpapiSealer,
        device_id: machine.device.to_owned(),
        data_dir: machine.dir.clone(),
        known_hosts_path: Some(machine.dir.join("known_hosts")),
        app_version: "0.1.0".to_owned(),
        onedrive_endpoints: endpoints.clone(),
    }
}

async fn sign_in(
    machine: &Machine,
    endpoints: &Endpoints,
    client_id: &str,
) -> Result<service::SignedIn, LoginError> {
    service::onedrive_sign_in(
        &machine.db,
        &DpapiSealer,
        endpoints,
        client_id,
        Duration::from_secs(20),
        &FakeBrowser::approving(),
        &Notify::new(),
    )
    .await
}

#[tokio::test]
async fn sign_in_sync_test_connection_and_disconnect_use_the_same_chain_as_the_ui() {
    let mock = MockMicrosoft::start().await;
    let endpoints = mock.endpoints();
    let a = Machine::new("od-service-a", "dev-a");
    let b = Machine::new("od-service-b", "dev-b");
    seed(&a);
    // 测试数据里的默认代理是个不存在的地址；软件网络代理默认“跟 SSH 默认代理走”，所以要显式选直连。
    for machine in [&a, &b] {
        machine
            .db
            .setting_set(
                zssh_core::sync::app_proxy::APP_NETWORK_PROXY_KEY,
                "\"none\"",
            )
            .unwrap();
    }

    // 登录：令牌（DPAPI）和连接设置都存进本机数据库。
    let signed = sign_in(&a, &endpoints, CLIENT_ID).await.expect("登录");
    assert!(signed.config_changed, "第一次登录");
    assert_eq!(signed.config.account, ACCOUNT_NAME);
    let saved = service::load_config(&a.db, &DpapiSealer)
        .unwrap()
        .expect("连接设置已保存");
    assert!(matches!(&saved, StorageConfig::Onedrive(c) if c.client_id == CLIENT_ID));
    let view = saved.view();
    assert_eq!(view.provider, "onedrive");
    assert_eq!(view.account, ACCOUNT_NAME);
    assert!(view.has_secret && !view.plain_http);
    // 设置表里是密文：看不到账号名、client ID、令牌。
    let stored = a.db.setting_get(service::STORAGE_KEY).unwrap().unwrap();
    for needle in [ACCOUNT_NAME, CLIENT_ID] {
        assert!(!stored.contains(needle), "设置表里不该出现 {needle}");
    }
    let tokens = DbTokenStore::new(&a.db, &DpapiSealer)
        .load()
        .unwrap()
        .expect("令牌已保存");
    let raw_tokens = a.db.setting_get(TOKENS_KEY).unwrap().unwrap();
    assert!(!raw_tokens.contains(&tokens.refresh_token));

    // 测试连接：读、写、并发保护都通过，检测结果存下来给之后的同步用。
    let config = saved.clone();
    let probe = service::test_connection(
        &config,
        None,
        Arc::new(DbTokenStore::new(&a.db, &DpapiSealer)),
        &endpoints,
    )
    .await;
    assert!(probe.read.ok && probe.write.ok, "{probe:?}");
    let concurrency = probe.concurrency.expect("检测并发保护");
    assert_eq!(concurrency.mode, ConcurrencyMode::Conditional);
    service::save_concurrency(
        &a.db,
        &service::ConcurrencyRecord {
            mode: concurrency.mode,
            notes: concurrency.notes,
            checked_at: 1,
        },
    )
    .unwrap();
    assert_eq!(
        service::effective_mode(&a.db).unwrap(),
        ConcurrencyMode::Conditional
    );
    assert!(mock.file_names().is_empty(), "测试连接不留临时文件");

    // 按保存的设置同步（和界面“立即同步”同一个入口）。
    let report = context(&a, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .expect("A 同步");
    assert_eq!(report.status, SyncStatus::Completed);
    assert!(service::is_ready(&a.db).unwrap());
    assert!(mock.file("zssh-sync.json").is_some());

    // 再登录同一个账号：设置没变，不会清掉“已设置主密码”的标记。
    let again = sign_in(&a, &endpoints, CLIENT_ID).await.expect("再次登录");
    assert!(!again.config_changed);
    assert!(
        service::is_ready(&a.db).unwrap(),
        "重新登录不该让主密码设置作废"
    );

    // 第二台电脑登录并同步，拿到同样的数据。
    sign_in(&b, &endpoints, CLIENT_ID).await.expect("B 登录");
    let report = context(&b, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .expect("B 同步");
    assert_eq!(report.status, SyncStatus::Completed);
    assert_same_content(&a, &b, "两台电脑通过服务层同步之后");

    // 断开连接：令牌和连接设置都没了，不能再同步。
    service::onedrive_disconnect(&a.db).unwrap();
    assert_eq!(a.db.setting_get(TOKENS_KEY).unwrap(), None);
    assert_eq!(service::load_config(&a.db, &DpapiSealer).unwrap(), None);
    assert!(!service::is_ready(&a.db).unwrap());
    let err = context(&a, &endpoints)
        .run_sync(PASSWORD, None)
        .await
        .expect_err("断开之后");
    assert!(matches!(err, SyncError::NotConfigured), "{err:?}");
    assert!(
        mock.file("zssh-sync.json").is_some(),
        "断开不动云端的同步包"
    );
}

#[tokio::test]
async fn switching_to_another_storage_removes_the_saved_onedrive_tokens() {
    let mock = MockMicrosoft::start().await;
    let a = Machine::new("od-switch", "dev-a");
    sign_in(&a, &mock.endpoints(), CLIENT_ID)
        .await
        .expect("登录");
    assert!(a.db.setting_get(TOKENS_KEY).unwrap().is_some());
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
    assert_eq!(
        a.db.setting_get(TOKENS_KEY).unwrap(),
        None,
        "换存储 = 断开 OneDrive，令牌不留在本机"
    );
}

#[tokio::test]
async fn without_a_custom_client_id_the_login_uses_the_built_in_one() {
    let mock = MockMicrosoft::start().await;
    let a = Machine::new("od-noid", "dev-a");
    let browser = FakeBrowser::idle();
    // 模拟的微软服务只认测试用的 client ID，所以这次登录走不完（不会有回调）；这里只看交给浏览器的
    // 授权地址：没填自己的就带内置的 client ID，不会不带、也不会带别的。
    let result = service::onedrive_sign_in(
        &a.db,
        &DpapiSealer,
        &mock.endpoints(),
        "",
        Duration::from_secs(1),
        &browser,
        &Notify::new(),
    )
    .await;
    assert!(
        !matches!(result, Err(LoginError::InvalidClientId)),
        "{result:?}"
    );
    let urls = browser.urls();
    assert_eq!(urls.len(), 1, "{urls:?}");
    assert!(
        urls[0].contains(&format!(
            "client_id={}",
            zssh_core::sync::onedrive::config::DEFAULT_CLIENT_ID
        )),
        "{}",
        urls[0]
    );
    assert!(!urls[0].contains(CLIENT_ID), "{}", urls[0]);
    assert_eq!(service::load_config(&a.db, &DpapiSealer).unwrap(), None);

    // 填得不合法的不会悄悄换成内置的：直接报格式错误，不打开浏览器。
    let err = sign_in(&a, &mock.endpoints(), "not-a-guid")
        .await
        .expect_err("格式不对");
    assert!(matches!(err, LoginError::InvalidClientId), "{err:?}");
    assert!(mock.authorize_requests().is_empty());
}

#[tokio::test]
async fn a_failed_or_timed_out_login_leaves_nothing_saved() {
    let mock = MockMicrosoft::start().await;
    let a = Machine::new("od-failed", "dev-a");
    let err = service::onedrive_sign_in(
        &a.db,
        &DpapiSealer,
        &mock.endpoints(),
        CLIENT_ID,
        Duration::from_millis(300),
        &FakeBrowser::idle(),
        &Notify::new(),
    )
    .await
    .expect_err("超时");
    assert!(matches!(err, LoginError::Timeout), "{err:?}");
    assert_eq!(service::load_config(&a.db, &DpapiSealer).unwrap(), None);
    assert_eq!(a.db.setting_get(TOKENS_KEY).unwrap(), None);

    mock.set_mode(mock_microsoft::AuthorizeMode::Deny);
    let err = sign_in(&a, &mock.endpoints(), CLIENT_ID)
        .await
        .expect_err("拒绝");
    assert!(matches!(err, LoginError::Denied), "{err:?}");
    assert_eq!(service::load_config(&a.db, &DpapiSealer).unwrap(), None);
    assert_eq!(a.db.setting_get(TOKENS_KEY).unwrap(), None);
}

#[tokio::test]
async fn the_login_goes_through_the_app_network_proxy_setting() {
    // 软件网络代理（和 SSH 默认代理分开的那个设置）：选“不用代理”时直连，不读系统代理。
    let mock = MockMicrosoft::start().await;
    let a = Machine::new("od-proxy-setting", "dev-a");
    a.db.setting_set(
        zssh_core::sync::app_proxy::APP_NETWORK_PROXY_KEY,
        "\"none\"",
    )
    .unwrap();
    sign_in(&a, &mock.endpoints(), CLIENT_ID)
        .await
        .expect("直连登录");
    assert_eq!(mock.token_requests().len(), 1);
}

/// 关掉软件网络代理的默认"跟 SSH 默认代理走"（测试数据里的默认代理是个不存在的地址）。
#[allow(clippy::unwrap_used)] // 测试辅助：失败就让测试 panic
fn direct_network(machine: &Machine) {
    machine
        .db
        .setting_set(
            zssh_core::sync::app_proxy::APP_NETWORK_PROXY_KEY,
            "\"none\"",
        )
        .unwrap();
}

/// 回归：同步存储的身份认 Graph 的稳定账号 id，不认显示名。两个显示名一样的账号互换登录，
/// 旧账号留下的同步基线不能被沿用；换回原账号，基线又对得上。
#[tokio::test]
async fn a_same_named_but_different_account_does_not_inherit_the_old_baseline() {
    let mock = MockMicrosoft::start().await;
    let endpoints = mock.endpoints();
    let a = Machine::new("od-same-name", "dev-a");
    seed(&a);
    direct_network(&a);

    mock.set_account(ACCOUNT_NAME, Some("account-id-one"));
    sign_in(&a, &endpoints, CLIENT_ID)
        .await
        .expect("账号一登录");
    context(&a, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .expect("账号一同步");
    assert!(
        a.engine().load_baseline().unwrap().is_some(),
        "同一个账号：基线可用"
    );

    mock.set_account(ACCOUNT_NAME, Some("account-id-two"));
    let signed = sign_in(&a, &endpoints, CLIENT_ID)
        .await
        .expect("账号二登录");
    assert!(
        signed.config_changed,
        "显示名一样，但账号 id 不同 = 换了账号"
    );
    assert!(
        a.engine().load_baseline().unwrap().is_none(),
        "同名的另一个账号不能沿用旧基线"
    );

    mock.set_account(ACCOUNT_NAME, Some("account-id-one"));
    sign_in(&a, &endpoints, CLIENT_ID)
        .await
        .expect("换回账号一");
    assert!(
        a.engine().load_baseline().unwrap().is_some(),
        "换回原账号，原来的基线仍然对得上"
    );
}

/// Graph 不给账号 id 时，每次登录各记一个独有的随机值：认不出是不是同一个账号，
/// 宁可按首次同步处理，也不能让两个同名账号共用一个身份。
#[tokio::test]
async fn an_account_without_a_graph_id_gets_a_login_unique_identity() {
    let mock = MockMicrosoft::start().await;
    let endpoints = mock.endpoints();
    let a = Machine::new("od-no-id", "dev-a");
    seed(&a);
    direct_network(&a);
    mock.set_account(ACCOUNT_NAME, None);

    let first = sign_in(&a, &endpoints, CLIENT_ID).await.expect("登录");
    assert!(first.config.account_id.starts_with("unverified-"));
    context(&a, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .expect("同步");
    assert!(a.engine().load_baseline().unwrap().is_some());

    let second = sign_in(&a, &endpoints, CLIENT_ID).await.expect("再次登录");
    assert_ne!(first.config.account_id, second.config.account_id);
    assert!(second.config_changed);
    assert!(
        a.engine().load_baseline().unwrap().is_none(),
        "没有 id 可认，重新登录后不能沿用旧基线"
    );
}

/// 升级：升级前保存的 OneDrive 配置没有账号 id（身份不确定）。它对应的旧基线不能被沿用，
/// 下一次同步按首次同步处理；同步完成后基线就绑定新的身份，之后照常使用。
#[tokio::test]
async fn a_saved_onedrive_config_without_an_account_id_drops_the_old_baseline_once() {
    let mock = MockMicrosoft::start().await;
    let endpoints = mock.endpoints();
    let a = Machine::new("od-legacy", "dev-a");
    seed(&a);
    direct_network(&a);
    sign_in(&a, &endpoints, CLIENT_ID).await.expect("登录");
    context(&a, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .expect("同步");
    assert!(a.engine().load_baseline().unwrap().is_some());

    // 把连接设置改成升级前的样子：只有显示名，没有账号 id。
    let Some(StorageConfig::Onedrive(mut config)) =
        service::load_config(&a.db, &DpapiSealer).unwrap()
    else {
        panic!("应该是 OneDrive 配置");
    };
    config.account_id = String::new();
    service::save_config(&a.db, &DpapiSealer, &StorageConfig::Onedrive(config)).unwrap();
    assert!(
        a.engine().load_baseline().unwrap().is_none(),
        "身份不确定：旧基线不能沿用"
    );

    // 按首次同步合并后，基线绑到当前（旧配置）的身份，之后正常使用。
    let report = context(&a, &endpoints)
        .run_sync(PASSWORD, Some(DeletionDecision::Continue))
        .await
        .expect("升级后第一次同步");
    assert_eq!(report.status, SyncStatus::Completed);
    assert!(a.engine().load_baseline().unwrap().is_some());
}
