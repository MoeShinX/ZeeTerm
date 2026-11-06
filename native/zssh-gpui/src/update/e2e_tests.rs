//! 更新流程的端到端测试（本机，不联网）：用测试密钥签名、本机 HTTP 服务模拟 GitHub Release，
//! 从取清单一路走到"安装计划 / 启动安装器"，其中**启动安装器用桩**（只记录参数，不运行任何程序，
//! 也不往本机装东西）。
//!
//! 覆盖：发现新版本 -> 下载（有进度）-> 验签 + 版本绑定 -> 两段确认 -> 启动；
//! 以及签名不对、版本不符、包被改、下载中断、清单 404、已是最新、公钥未配置、取消 这些该拒绝的情形：
//! 每一种都有清楚的中文提示，缓存目录里一个文件都不留，启动桩一次都不被调用。

// 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]` 属性宏。
use super::{
    CheckKind, Checked, DownloadContext, DownloadEvent, DownloadFailure, InstallState,
    ManifestSource, SIGNING_UNCONFIGURED, advance_install, build_install_plan, check_with,
    download_with, launch_if_unchanged, proxy_from_settings, update_client_builder,
};
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use minisign::KeyPair;
use zssh_update::{Package, ReleaseManifest, build_latest_json, trusted_comment};

/// 夹具服务器对一个路径的反应。
#[derive(Clone)]
enum Route {
    /// 200 + 正文。
    Ok(Vec<u8>),
    /// 声称有 `claimed` 字节，只发 `sent` 就断开（下载中断）。
    Cut { claimed: usize, sent: Vec<u8> },
}

type Routes = Arc<Mutex<HashMap<String, Route>>>;

/// 一个只在本机回环地址上听的小 HTTP 服务；没有路由的路径回 404；`hits` 数收到过多少次请求。
struct Fixture {
    base: String,
    routes: Routes,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        listener.set_nonblocking(true).expect("非阻塞");
        let port = listener.local_addr().expect("端口").port();
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (routes, hits, stop) = (Arc::clone(&routes), Arc::clone(&hits), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (routes, hits) = (Arc::clone(&routes), Arc::clone(&hits));
                            std::thread::spawn(move || serve(stream, &hits, &routes));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        Self {
            base: format!("http://127.0.0.1:{port}"),
            routes,
            hits,
            stop,
            thread: Some(thread),
        }
    }

    fn set(&self, path: &str, route: Route) {
        if let Ok(mut map) = self.routes.lock() {
            map.insert(path.to_owned(), route);
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, hits: &AtomicUsize, routes: &Mutex<HashMap<String, Route>>) {
    let _ = stream.set_nonblocking(false);
    let mut seen = Vec::new();
    let mut buffer = [0u8; 1024];
    while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => seen.extend_from_slice(&buffer[..read]),
        }
    }
    hits.fetch_add(1, Ordering::Relaxed);
    let request = String::from_utf8_lossy(&seen);
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();
    let route = routes.lock().ok().and_then(|map| map.get(&path).cloned());
    let _ = match route {
        Some(Route::Ok(body)) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(head.as_bytes())
                .and_then(|()| stream.write_all(&body))
        }
        Some(Route::Cut { claimed, sent }) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {claimed}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(head.as_bytes())
                .and_then(|()| stream.write_all(&sent))
        }
        None => stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试用 runtime")
        .block_on(future)
}

/// 不带系统代理、放行 http 的客户端：测试只连本机，不能被开发机上的代理截走。
fn client() -> reqwest::Client {
    update_client_builder(true)
        .no_proxy()
        .build()
        .expect("测试客户端")
}

struct Keys {
    pair: KeyPair,
    pubkey_b64: String,
}

fn keys() -> Keys {
    let pair = KeyPair::generate_unencrypted_keypair().expect("生成测试密钥");
    let text = pair.pk.to_box().expect("公钥文本").to_string();
    Keys {
        pair,
        pubkey_b64: base64::engine::general_purpose::STANDARD.encode(text),
    }
}

/// 用 `zssh_update` 的同一套函数签名：和发版工具（release_tool sign）产出的格式一致。
fn sign_installer(keys: &Keys, bytes: &[u8], signed_version: &str, file_name: &str) -> String {
    let comment = trusted_comment(signed_version, file_name, 1_700_000_000).expect("签名注释");
    let signed = minisign::sign(
        Some(&keys.pair.pk),
        &keys.pair.sk,
        bytes,
        Some(&comment),
        Some("ZeeTerm update signature"),
    )
    .expect("签名");
    base64::engine::general_purpose::STANDARD.encode(signed.to_string())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zeeterm-r1-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, Iterator::count)
}

/// 发布一个版本：`announced` 写进清单，`signed` 写进签名；`served` 是夹具实际提供的安装包字节
/// （和签名时的字节不同就是"包被改过"）。返回 (夹具, 安装包路径)。
fn publish(
    keys: &Keys,
    announced: &str,
    signed: &str,
    signed_bytes: &[u8],
    served: Route,
) -> (Fixture, String) {
    let fixture = Fixture::start();
    let name = format!("ZeeTerm-Setup-{announced}.exe");
    let signature = sign_installer(keys, signed_bytes, signed, &name);
    // 生成器只肯写 https 地址：先按假主机生成，再换成夹具的本机地址。
    let json = build_latest_json(&ReleaseManifest {
        version: announced,
        notes: "修复若干问题\n新增在线更新",
        pub_date: "2026-11-03T08:00:00Z",
        url: &format!("https://fixture.invalid/download/v{announced}/{name}"),
        signature: &signature,
    })
    .expect("生成 latest.json")
    .replace("https://fixture.invalid", &fixture.base);
    // 此处测试可注入的下载/验签链；实际 macOS 入口在请求前选择手动更新。
    // 让同一份内存包在宿主平台可选，安装阶段始终使用假 launcher。
    let json = json.replace(
        "windows-x86_64",
        zssh_update::platform_keys()
            .first()
            .copied()
            .unwrap_or("unsupported"),
    );
    let path = format!("/download/v{announced}/{name}");
    fixture.set("/latest.json", Route::Ok(json.into_bytes()));
    fixture.set(&path, served);
    (fixture, path)
}

fn check_full(fixture: &Fixture, current: &str, pubkey: &str) -> Checked {
    let sources = [ManifestSource::Url(fixture.url("/latest.json"))];
    block_on(check_with(current, &sources, &client(), true, Ok(pubkey)))
}

fn check(fixture: &Fixture, current: &str, pubkey: &str) -> (String, Option<Package>) {
    let checked = check_full(fixture, current, pubkey);
    (checked.status, checked.package)
}

type DownloadResult = Result<(PathBuf, u64, String), DownloadFailure>;

fn download_with_cancel(
    package: &Package,
    pubkey: &str,
    cache: &Path,
    cancel: &AtomicBool,
) -> (DownloadResult, Vec<DownloadEvent>) {
    let (sender, receiver) = channel();
    let client = client();
    let result = block_on(download_with(
        &DownloadContext {
            pubkey,
            client: &client,
            allow_insecure: true,
        },
        7,
        package,
        cache,
        cancel,
        &sender,
    ));
    drop(sender);
    (result, receiver.try_iter().collect())
}

fn download(package: &Package, pubkey: &str, cache: &Path) -> (DownloadResult, Vec<DownloadEvent>) {
    download_with_cancel(package, pubkey, cache, &AtomicBool::new(false))
}

fn failure(result: DownloadResult) -> String {
    match result {
        Ok((path, ..)) => panic!("本该被拒绝，却通过了：{}", path.display()),
        Err(DownloadFailure::Cancelled) => "已取消".to_owned(),
        Err(DownloadFailure::Failed(message)) => message,
    }
}

/// 公共前半段：取清单 -> 选出包。
fn found_package(fixture: &Fixture, keys: &Keys, current: &str) -> Package {
    let (status, package) = check(fixture, current, &keys.pubkey_b64);
    package.unwrap_or_else(|| panic!("本该发现新版本，状态是：{status}"))
}

#[test]
fn a_new_release_goes_from_manifest_to_a_confirmed_install_with_the_stub_launcher() {
    let keys = keys();
    let installer = vec![0x5a; 200_000];
    let name = "ZeeTerm-Setup-1.4.0.exe";
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );

    // 1. 取清单：发现新版本，更新说明、平台都在状态里。
    let (status, package) = check(&fixture, "1.3.0", &keys.pubkey_b64);
    assert!(status.contains("发现新版本 1.4.0"), "{status}");
    assert!(status.contains("修复若干问题"), "{status}");
    assert!(status.contains(zssh_update::platform_keys()[0]), "{status}");
    let package = package.expect("应选出安装包");
    assert_eq!(package.version, "1.4.0");

    // 2. 下载 + 验签：有进度、有"校验中"，缓存里是验过的那份字节。
    let cache = temp_dir("happy");
    let (result, events) = download(&package, &keys.pubkey_b64, &cache);
    let (path, size, sha256) = result.unwrap_or_else(|failed| match failed {
        DownloadFailure::Failed(message) => panic!("下载应通过，实际：{message}"),
        DownloadFailure::Cancelled => panic!("下载应通过，实际被取消"),
    });
    assert_eq!(size, installer.len() as u64);
    assert_eq!(sha256, zssh_update::sha256_hex(&installer));
    assert_eq!(std::fs::read(&path).expect("读缓存"), installer);
    assert!(path.starts_with(&cache));
    assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(name));
    assert!(
        events.iter().any(
            |event| matches!(event, DownloadEvent::Progress { received, .. } if *received == size)
        ),
        "最后一次进度应是满的"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DownloadEvent::Verifying { .. }))
    );

    // 3. 安装：第一次点只出计划（/P /UPDATE /R /ARGS …），第二次才调用启动桩；桩只记录，不运行任何东西。
    let args = vec!["--minimized".to_owned()];
    let (state, launched) = advance_install(
        InstallState::Idle,
        false,
        || build_install_plan(&path, &cache, "1.4.0", true, &args, &sha256),
        |_| unreachable!("第一次点击不能启动"),
    );
    assert!(!launched && state.awaiting_confirm());
    let mut launched_with: Vec<Vec<String>> = Vec::new();
    let (state, launched) = advance_install(
        state,
        false,
        || unreachable!("计划已生成，不该重做"),
        |plan| {
            // 启动前复验：文件还是验签的那份；通过后才轮到（桩）启动函数。
            launch_if_unchanged(plan, |checked| {
                launched_with.push(checked.args.clone());
                Ok(())
            })
        },
    );
    assert!(launched);
    assert_eq!(state, InstallState::Launched);
    assert_eq!(
        launched_with,
        vec![vec!["/P", "/UPDATE", "/R", "/ARGS", "--minimized"]]
    );
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn the_same_or_an_older_release_is_reported_as_up_to_date() {
    let keys = keys();
    let installer = b"installer".to_vec();
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    for current in ["1.4.0", "1.5.0"] {
        let (status, package) = check(&fixture, current, &keys.pubkey_b64);
        assert_eq!(status, "已是最新版本", "当前 {current}");
        assert!(package.is_none());
    }
}

#[test]
fn a_signature_from_another_key_is_refused_and_nothing_is_cached() {
    let keys = keys();
    let other = self::keys();
    let installer = b"installer".to_vec();
    let (fixture, _) = publish(
        &other,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");

    let cache = temp_dir("wrong-key");
    let (result, events) = download(&package, &keys.pubkey_b64, &cache);
    let message = failure(result);
    assert!(message.contains("签名校验未通过"), "{message}");
    assert_eq!(file_count(&cache), 0, "验不过的包一个字节都不能落盘");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, DownloadEvent::Verified { .. }))
    );
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_signature_for_a_different_version_than_announced_is_refused() {
    let keys = keys();
    let installer = b"installer".to_vec();
    // 清单公告 1.4.0，签名里写的却是 1.3.5（比如把旧版的签名搬过来）。
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.3.5",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");

    let cache = temp_dir("version-mismatch");
    let message = failure(download(&package, &keys.pubkey_b64, &cache).0);
    assert!(
        message.contains("签名版本 1.3.5 与公告版本 1.4.0 不一致"),
        "{message}"
    );
    assert_eq!(file_count(&cache), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn an_installer_changed_after_signing_is_refused() {
    let keys = keys();
    let signed = b"the installer the publisher signed".to_vec();
    let served = b"the installer an attacker swapped in".to_vec();
    let (fixture, _) = publish(&keys, "1.4.0", "1.4.0", &signed, Route::Ok(served));
    let package = found_package(&fixture, &keys, "1.3.0");

    let cache = temp_dir("tampered");
    let message = failure(download(&package, &keys.pubkey_b64, &cache).0);
    assert!(message.contains("签名校验未通过"), "{message}");
    assert_eq!(file_count(&cache), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn an_interrupted_download_fails_with_a_message_and_leaves_no_file() {
    let keys = keys();
    let installer = vec![7u8; 100_000];
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Cut {
            claimed: installer.len(),
            sent: installer[..10_000].to_vec(),
        },
    );
    let package = found_package(&fixture, &keys, "1.3.0");

    let cache = temp_dir("cut");
    let (result, events) = download(&package, &keys.pubkey_b64, &cache);
    let message = failure(result);
    assert!(message.starts_with("下载失败："), "{message}");
    assert_eq!(file_count(&cache), 0, "半截的包不能留在缓存里");
    assert!(!events.iter().any(|event| matches!(
        event,
        DownloadEvent::Verifying { .. } | DownloadEvent::Verified { .. }
    )));
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_missing_manifest_says_so_in_chinese_and_offers_no_package() {
    let keys = keys();
    let fixture = Fixture::start(); // 没有任何路由：清单 404
    let (status, package) = check(&fixture, "1.3.0", &keys.pubkey_b64);
    assert!(package.is_none());
    assert!(status.contains("读取更新说明失败"), "{status}");
    assert!(status.contains("HTTP 404"), "{status}");
    assert!(status.contains("找不到"), "{status}");
}

#[test]
fn a_missing_installer_is_a_clear_download_failure() {
    let keys = keys();
    let installer = b"installer".to_vec();
    let (fixture, path) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    // 清单还在，但安装包被撤了（比如 Release 里删了附件）。
    if let Ok(mut map) = fixture.routes.lock() {
        map.remove(&path);
    }
    let package = found_package(&fixture, &keys, "1.3.0");
    let cache = temp_dir("pkg-404");
    let message = failure(download(&package, &keys.pubkey_b64, &cache).0);
    assert!(
        message.contains("HTTP 404") && message.contains("找不到"),
        "{message}"
    );
    assert_eq!(file_count(&cache), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_cancelled_download_leaves_nothing_behind() {
    let keys = keys();
    let installer = vec![1u8; 50_000];
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");
    let cache = temp_dir("cancel");
    let cancel = AtomicBool::new(true);
    let (result, _) = download_with_cancel(&package, &keys.pubkey_b64, &cache, &cancel);
    assert!(matches!(result, Err(DownloadFailure::Cancelled)));
    assert_eq!(file_count(&cache), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_placeholder_key_refuses_before_any_request_and_before_any_download() {
    let keys = keys();
    let installer = b"installer".to_vec();
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let sources = [ManifestSource::Url(fixture.url("/latest.json"))];

    // 占位公钥：检查更新直接说"更新签名未配置"，夹具一个请求都没收到。
    let unconfigured = block_on(check_with(
        "1.3.0",
        &sources,
        &client(),
        true,
        Ok("PLACEHOLDER-run-the-keygen-script"),
    ));
    assert_eq!(unconfigured.status, SIGNING_UNCONFIGURED);
    assert!(unconfigured.status.contains("更新签名未配置"));
    assert!(unconfigured.package.is_none());
    assert_eq!(unconfigured.kind, CheckKind::Failed);
    // 公钥拿不到（配置坏了）同理：原样给出原因，不联网。
    let reason = "内置的更新公钥不可用，无法校验更新包：坏了".to_owned();
    let broken = block_on(check_with("1.3.0", &sources, &client(), true, Err(&reason)));
    assert_eq!(broken.status, reason);
    assert!(broken.package.is_none());
    assert_eq!(broken.kind, CheckKind::Failed);
    assert_eq!(fixture.hits(), 0, "没配公钥时不能向更新源发任何请求");

    // 即使有人绕过检查、直接拿着一个包来下载：下载这一层也拒绝，同样不发请求、不落盘。
    let package = found_package(&fixture, &keys, "1.3.0");
    let hits_before = fixture.hits();
    let cache = temp_dir("placeholder");
    let message = failure(download(&package, "PLACEHOLDER", &cache).0);
    assert_eq!(message, SIGNING_UNCONFIGURED);
    assert_eq!(fixture.hits(), hits_before, "下载层也不能发请求");
    assert_eq!(file_count(&cache), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn the_installer_stub_is_never_called_for_a_package_that_failed_verification() {
    // 把"验签失败 -> 没有已校验的包 -> 点不了安装"串起来：失败时拿不到 (路径, 哈希)，
    // 所以 build_install_plan / 启动桩根本没有可用的输入。
    let keys = keys();
    let other = self::keys();
    let installer = b"installer".to_vec();
    let (fixture, _) = publish(
        &other,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");
    let cache = temp_dir("no-install");
    let (result, _) = download(&package, &keys.pubkey_b64, &cache);
    assert!(result.is_err());
    // 用一份"没验过"的文件硬凑计划也过不了：哈希对不上验签时记下的那份。
    let stray = cache.join("ZeeTerm-Setup-1.4.0.exe");
    std::fs::write(&stray, b"not verified").expect("写文件");
    let plan = build_install_plan(
        &stray,
        &cache,
        "1.4.0",
        false,
        &[],
        &zssh_update::sha256_hex(&installer),
    );
    assert!(plan.is_err(), "未验签的文件不能生成安装计划");
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn updates_use_the_software_network_proxy_setting_and_report_a_broken_one() {
    use zssh_core::model::{ProxyDraft, ProxyKind};
    use zssh_core::store::Database;
    use zssh_core::sync::app_proxy::APP_NETWORK_PROXY_KEY;

    let db = Database::open_in_memory().expect("内存库");
    let draft = |name: &str, port: u16| ProxyDraft {
        id: None,
        name: name.to_owned(),
        kind: ProxyKind::Socks5,
        host: "127.0.0.1".to_owned(),
        port,
        username: None,
        secret: None,
    };
    let app = db.proxy_save(&draft("app", 7890)).expect("存代理");
    let ssh = db.proxy_save(&draft("ssh", 1080)).expect("存代理");
    db.set_default_proxy_id(Some(ssh.id)).expect("默认代理");

    // 明确选了库里的某个代理：更新用它（socks5h：域名交给代理解析）。
    db.setting_set(APP_NETWORK_PROXY_KEY, &format!("\"proxy:{}\"", app.id))
        .expect("写设置");
    assert_eq!(
        proxy_from_settings(&db),
        Ok(Some("socks5h://127.0.0.1:7890".to_owned()))
    );
    // 选了「使用 SSH 默认代理」：跟 SSH 默认代理一样。
    db.setting_set(APP_NETWORK_PROXY_KEY, "\"ssh_default\"")
        .expect("写设置");
    assert_eq!(
        proxy_from_settings(&db),
        Ok(Some("socks5h://127.0.0.1:1080".to_owned()))
    );
    // 选了「不使用代理」：直连。
    db.setting_set(APP_NETWORK_PROXY_KEY, "\"none\"")
        .expect("写设置");
    assert_eq!(proxy_from_settings(&db), Ok(None));
    // 指定的代理被删了：说清楚设置有问题，而不是悄悄改成直连。
    db.setting_set(APP_NETWORK_PROXY_KEY, "\"proxy:99999\"")
        .expect("写设置");
    let err = proxy_from_settings(&db).expect_err("该报错");
    assert!(err.starts_with("软件网络的代理设置有问题"), "{err}");
}
