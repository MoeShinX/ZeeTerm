//! 更新流程的端到端测试（本机，不联网）：用测试密钥签名、本机 HTTP 服务模拟 GitHub Release，
//! 从取清单一路走到"安装计划 / 启动安装器"，其中**启动安装器用桩**（只记录参数，不运行任何程序，
//! 也不往本机装东西）。
//!
//! 覆盖：发现新版本 -> 下载（有进度）-> 验签 + 版本绑定 -> 两段确认 -> 启动；
//! 以及签名不对、版本不符、包被改、下载中断、清单 404、已是最新、公钥未配置、取消 这些该拒绝的情形：
//! 每一种都有清楚的中文提示，缓存目录里一个文件都不留，启动桩一次都不被调用。

// 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]` 属性宏。
use super::{
    CheckKind, Checked, DownloadContext, DownloadEvent, DownloadFailure, DownloadState,
    InstallState, ManifestSource, Platform, SIGNING_UNCONFIGURED, advance_install,
    build_install_plan, check_with, download_with, finish_event, launch_if_unchanged,
    proxy_from_settings, run_install, settle_download, update_client_builder,
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
        notes_en: None,
        pub_date: "2026-11-03T08:00:00Z",
        url: &format!("https://fixture.invalid/download/v{announced}/{name}"),
        signature: &signature,
    })
    .expect("生成 latest.json")
    .replace("https://fixture.invalid", &fixture.base);
    let path = format!("/download/v{announced}/{name}");
    fixture.set("/latest.json", Route::Ok(json.into_bytes()));
    fixture.set(&path, served);
    (fixture, path)
}

fn check_full(fixture: &Fixture, current: &str, pubkey: &str) -> Checked {
    let sources = [ManifestSource::Url(fixture.url("/latest.json"))];
    block_on(check_with(
        current,
        &sources,
        &client(),
        true,
        Ok(pubkey),
        &Platform::Windows,
    ))
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
fn a_new_release_goes_from_manifest_to_a_one_click_install_with_the_stub_launcher() {
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
    assert!(status.contains("windows-x86_64"), "{status}");
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

    // 3. 安装：点一下就生成计划（/P /UPDATE /R /ARGS …）并调用启动桩；桩只记录，不运行任何东西。
    let args = vec!["--minimized".to_owned()];
    let mut launched_with: Vec<Vec<String>> = Vec::new();
    let (state, launched) = advance_install(
        InstallState::Idle,
        || build_install_plan(&path, &cache, "1.4.0", true, &args, &sha256),
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

/// 界面收到一次下载的全部事件（进度、校验中，再加最后的结果）后的做法：返回 (下载阶段, 是否该立刻安装)。
/// `consent` 是用户有没有点「下载并安装」。
fn replay(
    consent: bool,
    result: DownloadResult,
    events: Vec<DownloadEvent>,
) -> (DownloadState, bool) {
    let mut phase = DownloadState::Idle;
    phase.start();
    let mut consent = consent;
    let mut install_now = false;
    for event in events
        .iter()
        .chain(std::iter::once(&finish_event(7, result)))
    {
        install_now |= settle_download(&mut phase, &mut consent, event);
    }
    (phase, install_now)
}

/// 界面在"该立刻安装"时做的事：生成计划、调用启动桩（只记录参数，不运行任何东西）。
/// 返回桩被调用时收到的参数，每次调用一项。
fn install_with_stub(phase: &DownloadState, cache: &Path) -> Vec<Vec<String>> {
    let DownloadState::Verified { path, sha256, .. } = phase else {
        panic!("没有验签通过的包，不该走到安装：{phase:?}");
    };
    let mut launches = Vec::new();
    let (state, _blocked, launched) = run_install(
        None,
        InstallState::Idle,
        || build_install_plan(path, cache, "1.4.0", true, &[], sha256),
        |plan| {
            launch_if_unchanged(plan, |checked| {
                launches.push(checked.args.clone());
                Ok(())
            })
        },
    );
    assert!(launched);
    assert_eq!(state, InstallState::Launched);
    launches
}

#[test]
fn download_and_install_goes_straight_from_a_verified_signature_to_the_installer() {
    let keys = keys();
    let installer = vec![0x5a; 50_000];
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");
    let cache = temp_dir("auto-install");
    let (result, events) = download(&package, &keys.pubkey_b64, &cache);

    // 用户点了「下载并安装」：验签一过就该安装，启动桩恰好被调用一次，中间没有再点一次的环节。
    let (phase, install_now) = replay(true, result, events);
    assert!(matches!(phase, DownloadState::Verified { .. }));
    assert!(install_now);
    let launches = install_with_stub(&phase, &cache);
    assert_eq!(launches, vec![vec!["/P", "/UPDATE", "/R", "/ARGS"]]);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_verified_package_nobody_asked_to_install_is_not_installed() {
    // 自动检查只是弹出窗口、用户没点「下载并安装」：不下载；就算有别的路径把包下完了，也不会自己装。
    let keys = keys();
    let installer = vec![0x5a; 50_000];
    let (fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");
    let hits = fixture.hits();
    assert_eq!(hits, 1, "只取过清单，没碰安装包");
    let cache = temp_dir("no-consent");
    let (result, events) = download(&package, &keys.pubkey_b64, &cache);
    let (phase, install_now) = replay(false, result, events);
    assert!(matches!(phase, DownloadState::Verified { .. }));
    assert!(!install_now);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_failed_signature_check_never_reaches_the_installer_even_after_download_and_install() {
    // 安全底线：用户点了「下载并安装」，但签名是另一把密钥签的 -> 停在失败，安装桩一次都不被调用，缓存里没有文件。
    let keys = keys();
    let other = self::keys();
    let installer = vec![0x5a; 50_000];
    let (fixture, _) = publish(
        &other,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(installer.clone()),
    );
    let package = found_package(&fixture, &keys, "1.3.0");
    let cache = temp_dir("bad-signature");
    let (result, events) = download(&package, &keys.pubkey_b64, &cache);
    let (phase, install_now) = replay(true, result, events);
    assert!(
        matches!(phase, DownloadState::Failed(_)),
        "应停在失败：{phase:?}"
    );
    assert!(!install_now);
    assert_eq!(file_count(&cache), 0);

    // 下载中断、安装包被改过同理：都是失败阶段，不进入安装。
    let (tampered_fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        &installer,
        Route::Ok(vec![0x5b; 50_000]),
    );
    let tampered = found_package(&tampered_fixture, &keys, "1.3.0");
    let (result, events) = download(&tampered, &keys.pubkey_b64, &cache);
    let (phase, install_now) = replay(true, result, events);
    assert!(matches!(phase, DownloadState::Failed(_)), "{phase:?}");
    assert!(!install_now);
    assert_eq!(file_count(&cache), 0);
    let _ = std::fs::remove_dir_all(&cache);
}

/// 英文更新说明从发版工具生成的清单一路走到更新窗口：中文 `notes` 不变（老客户端只认它），`notes_en` 单独带着，
/// 窗口按界面语言挑；老清单（没有 `notes_en`）两种语言都显示中文那份。
#[test]
fn english_notes_travel_with_the_manifest_and_the_window_picks_by_language() {
    use crate::i18n::Lang;
    use crate::update::notes::display_for_language;

    let keys = keys();
    let installer = b"installer".to_vec();
    let name = "ZeeTerm-Setup-1.4.0.exe";
    let signature = sign_installer(&keys, &installer, "1.4.0", name);
    let json = build_latest_json(&ReleaseManifest {
        version: "1.4.0",
        notes: "### 新功能\n\n- 中文要点",
        notes_en: Some("### New\n\n- English point"),
        pub_date: "2026-11-08T08:00:00Z",
        url: &format!("https://fixture.invalid/download/v1.4.0/{name}"),
        signature: &signature,
    })
    .expect("生成 latest.json");
    let fixture = Fixture::start();
    fixture.set(
        "/latest.json",
        Route::Ok(
            json.replace("https://fixture.invalid", &fixture.base)
                .into_bytes(),
        ),
    );
    fixture.set(&format!("/download/v1.4.0/{name}"), Route::Ok(installer));

    let checked = check_full(&fixture, "1.3.0", &keys.pubkey_b64);
    assert_eq!(checked.kind, CheckKind::Found, "{}", checked.status);
    assert_eq!(checked.notes.as_deref(), Some("### 新功能\n\n- 中文要点"));
    assert_eq!(
        checked.notes_en.as_deref(),
        Some("### New\n\n- English point")
    );
    let shown = |lang| {
        display_for_language(lang, checked.notes.as_deref(), checked.notes_en.as_deref())
            .unwrap_or_default()
    };
    assert!(shown(Lang::En).contains("English point"));
    assert!(!shown(Lang::En).contains("中文要点"));
    assert!(shown(Lang::Zh).contains("中文要点"));

    // 老清单：没有 notes_en，英文界面也只能显示中文那份，不会空着。
    let (old_fixture, _) = publish(
        &keys,
        "1.4.0",
        "1.4.0",
        b"old installer",
        Route::Ok(b"old installer".to_vec()),
    );
    let old = check_full(&old_fixture, "1.3.0", &keys.pubkey_b64);
    assert_eq!(old.kind, CheckKind::Found, "{}", old.status);
    assert_eq!(old.notes_en, None);
    for lang in [Lang::Zh, Lang::En] {
        let text = display_for_language(lang, old.notes.as_deref(), old.notes_en.as_deref())
            .unwrap_or_default();
        assert!(text.contains("修复若干问题"), "{lang:?}：{text}");
    }
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
        &Platform::Windows,
    ));
    assert_eq!(unconfigured.status, SIGNING_UNCONFIGURED);
    assert!(unconfigured.status.contains("更新签名未配置"));
    assert!(unconfigured.package.is_none());
    assert_eq!(unconfigured.kind, CheckKind::Failed);
    // 公钥拿不到（配置坏了）同理：原样给出原因，不联网。
    let reason = "内置的更新公钥不可用，无法校验更新包：坏了".to_owned();
    let broken = block_on(check_with(
        "1.3.0",
        &sources,
        &client(),
        true,
        Err(&reason),
        &Platform::Windows,
    ));
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

/// Linux：同一套"取清单 → 下载 → 验签 → 安装"，只是最后一步是 pkexec / 替换文件，而且**全部用桩**：
/// deb 的命令执行器是假的（不弹授权框、不碰系统），tar.gz 只在临时目录里换文件。
mod linux_chain {
    use super::{
        CheckKind, Checked, DownloadEvent, Fixture, InstallState, Keys, ManifestSource, Platform,
        Route, block_on, check_with, client, download, failure, file_count, keys, publish,
        sign_installer, temp_dir,
    };
    use crate::update::linux::{self as glue, Job, Outcome};
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use zssh_update::linux::{
        CommandOutput, CommandRunner, DebTools, Flavor, ManualReason, TarLayout,
    };
    use zssh_update::{
        PLATFORM_LINUX_DEB, PLATFORM_LINUX_TAR, PlatformAsset, ReleaseManifest,
        build_latest_json_with_platforms,
    };

    /// 清单里的一个 Linux 包：平台 key、文件名、夹具实际提供的字节、签名时用的字节。
    struct Asset<'a> {
        key: &'static str,
        file: &'a str,
        served: Vec<u8>,
        signed: Vec<u8>,
    }

    /// 发布一个同时带 Windows 和 Linux 条目的版本（和发版流水线生成 latest.json 用的是同一个函数）。
    fn publish_linux(keys: &Keys, announced: &str, assets: &[Asset<'_>]) -> Fixture {
        let fixture = Fixture::start();
        let windows_name = format!("ZeeTerm-Setup-{announced}.exe");
        let windows_signature =
            sign_installer(keys, b"windows installer", announced, &windows_name);
        let urls: Vec<String> = assets
            .iter()
            .map(|asset| {
                format!(
                    "https://fixture.invalid/download/v{announced}/{}",
                    asset.file
                )
            })
            .collect();
        let signatures: Vec<String> = assets
            .iter()
            .map(|asset| sign_installer(keys, &asset.signed, announced, asset.file))
            .collect();
        let extra: Vec<PlatformAsset<'_>> = assets
            .iter()
            .zip(urls.iter().zip(signatures.iter()))
            .map(|(asset, (url, signature))| PlatformAsset {
                key: asset.key,
                url,
                signature,
            })
            .collect();
        let json = build_latest_json_with_platforms(
            &ReleaseManifest {
                version: announced,
                notes: "新增 Linux 版",
                notes_en: None,
                pub_date: "2026-11-06T08:00:00Z",
                url: &format!("https://fixture.invalid/download/v{announced}/{windows_name}"),
                signature: &windows_signature,
            },
            &extra,
        )
        .expect("生成 latest.json")
        .replace("https://fixture.invalid", &fixture.base);
        fixture.set("/latest.json", Route::Ok(json.into_bytes()));
        for asset in assets {
            fixture.set(
                &format!("/download/v{announced}/{}", asset.file),
                Route::Ok(asset.served.clone()),
            );
        }
        fixture
    }

    fn check_on(fixture: &Fixture, current: &str, pubkey: &str, platform: &Platform) -> Checked {
        let sources = [ManifestSource::Url(fixture.url("/latest.json"))];
        block_on(check_with(
            current,
            &sources,
            &client(),
            true,
            Ok(pubkey),
            platform,
        ))
    }

    /// 记录 pkexec 调用、按预设退出码回答的假执行器。
    struct FakePkexec {
        code: i32,
        calls: RefCell<Vec<Vec<OsString>>>,
    }

    impl FakePkexec {
        fn exits(code: i32) -> Self {
            Self {
                code,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakePkexec {
        fn run(&self, _program: &Path, args: &[OsString]) -> std::io::Result<CommandOutput> {
            self.calls.borrow_mut().push(args.to_vec());
            Ok(CommandOutput {
                code: Some(self.code),
                ..CommandOutput::default()
            })
        }
    }

    fn deb_tools() -> DebTools {
        DebTools {
            pkexec: Some(PathBuf::from("/usr/bin/pkexec")),
            apt_get: PathBuf::from("/usr/bin/apt-get"),
        }
    }

    fn unused_tar(_: &Path, _: &TarLayout, _: &str) -> Result<(), zssh_update::linux::TarError> {
        panic!("deb 路径不该走 tar.gz 更新");
    }

    #[test]
    fn a_deb_install_goes_from_manifest_to_a_verified_package_and_a_single_pkexec_call() {
        let keys = keys();
        let deb = b"fake deb bytes".to_vec();
        let fixture = publish_linux(
            &keys,
            "1.4.0",
            &[Asset {
                key: PLATFORM_LINUX_DEB,
                file: "ZeeTerm-1.4.0-linux-x86_64.deb",
                served: deb.clone(),
                signed: deb.clone(),
            }],
        );
        let platform = Platform::Linux(Flavor::Deb);

        let checked = check_on(&fixture, "1.3.0", &keys.pubkey_b64, &platform);
        assert_eq!(checked.kind, CheckKind::Found, "{}", checked.status);
        let package = checked.package.expect("应选出 deb");
        assert_eq!(package.platform_key, "linux-x86_64-deb");
        assert_eq!(checked.release_version.as_deref(), Some("1.4.0"));

        let cache = temp_dir("linux-deb");
        let (result, _) = download(&package, &keys.pubkey_b64, &cache);
        let (path, size, sha256) = result.unwrap_or_else(|_| panic!("下载并验签应通过"));
        assert_eq!(size, deb.len() as u64);
        assert!(path.starts_with(&cache));

        let job = Job {
            flavor: Flavor::Deb,
            package: path.clone(),
            cache_root: cache.clone(),
            version: "1.4.0".to_owned(),
            sha256: sha256.clone(),
            exe: PathBuf::from("/usr/lib/zeeterm/zeeterm"),
        };
        // 装成功：apt 拿到的是缓存里那个已验签的文件；重启走 /usr/bin/zeeterm。
        let ok = FakePkexec::exits(0);
        assert_eq!(
            glue::run(&job, &ok, &deb_tools(), &unused_tar),
            Outcome::Installed {
                restart: PathBuf::from("/usr/bin/zeeterm")
            }
        );
        assert_eq!(ok.calls.borrow().len(), 1);
        // 位置参数：验签时的哈希（sha256）原样传给 root 侧脚本，由它对复制品核对。
        {
            let calls = ok.calls.borrow();
            assert_eq!(
                calls[0][4..6],
                [
                    OsString::from(path.canonicalize().expect("规范化")),
                    OsString::from(sha256.as_str())
                ]
            );
        }

        // 授权框被取消：不报错，改成手动安装，文件还在缓存里。
        assert_eq!(
            glue::run(&job, &FakePkexec::exits(126), &deb_tools(), &unused_tar),
            Outcome::Manual(ManualReason::NotAuthorized)
        );
        assert!(path.exists());

        // 验签之后包被动过：pkexec 不会被叫起来。
        std::fs::write(&path, b"tampered").expect("改包");
        let untouched = FakePkexec::exits(0);
        assert!(matches!(
            glue::run(&job, &untouched, &deb_tools(), &unused_tar),
            Outcome::Failed(_)
        ));
        assert!(untouched.calls.borrow().is_empty());
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_bad_signature_never_reaches_the_installer() {
        let keys = keys();
        let fixture = publish_linux(
            &keys,
            "1.4.0",
            &[Asset {
                key: PLATFORM_LINUX_DEB,
                file: "ZeeTerm-1.4.0-linux-x86_64.deb",
                served: b"swapped on the server".to_vec(),
                signed: b"good deb".to_vec(),
            }],
        );
        let checked = check_on(
            &fixture,
            "1.3.0",
            &keys.pubkey_b64,
            &Platform::Linux(Flavor::Deb),
        );
        let package = checked.package.expect("清单本身没问题");
        let cache = temp_dir("linux-bad-sig");
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
    fn a_manifest_without_a_matching_linux_entry_points_to_the_release_page_and_windows_is_unchanged()
     {
        let keys = keys();
        // 老版本发布的清单：只有 Windows 条目。
        let installer = b"installer".to_vec();
        let (fixture, _) = publish(
            &keys,
            "1.4.0",
            "1.4.0",
            &installer,
            Route::Ok(installer.clone()),
        );
        for flavor in [Flavor::Deb, Flavor::Tar] {
            let checked = check_on(
                &fixture,
                "1.3.0",
                &keys.pubkey_b64,
                &Platform::Linux(flavor),
            );
            assert_eq!(checked.kind, CheckKind::FoundManual, "{}", checked.status);
            assert!(checked.package.is_none());
            assert_eq!(checked.release_version.as_deref(), Some("1.4.0"));
            assert_eq!(checked.notes.as_deref(), Some("修复若干问题\n新增在线更新"));
            assert!(
                checked.status.contains("发现新版本 1.4.0"),
                "{}",
                checked.status
            );
        }
        // Windows：同一份清单，选中的包和以前完全一样。
        let windows = check_on(&fixture, "1.3.0", &keys.pubkey_b64, &Platform::Windows);
        assert_eq!(windows.kind, CheckKind::Found);
        assert_eq!(
            windows.package.map(|package| package.platform_key),
            Some("windows-x86_64".to_owned())
        );
        // 已是最新：Linux 也不提示。
        let latest = check_on(
            &fixture,
            "1.4.0",
            &keys.pubkey_b64,
            &Platform::Linux(Flavor::Tar),
        );
        assert_eq!(latest.kind, CheckKind::Latest);
    }

    #[test]
    fn a_tar_gz_install_does_not_pick_the_deb_and_the_other_way_round() {
        let keys = keys();
        let fixture = publish_linux(
            &keys,
            "1.4.0",
            &[
                Asset {
                    key: PLATFORM_LINUX_DEB,
                    file: "ZeeTerm-1.4.0-linux-x86_64.deb",
                    served: b"deb".to_vec(),
                    signed: b"deb".to_vec(),
                },
                Asset {
                    key: PLATFORM_LINUX_TAR,
                    file: "ZeeTerm-1.4.0-linux-x86_64.tar.gz",
                    served: b"tar".to_vec(),
                    signed: b"tar".to_vec(),
                },
            ],
        );
        for (flavor, key) in [
            (Flavor::Deb, "linux-x86_64-deb"),
            (Flavor::Tar, "linux-x86_64-tar"),
        ] {
            let checked = check_on(
                &fixture,
                "1.3.0",
                &keys.pubkey_b64,
                &Platform::Linux(flavor),
            );
            assert_eq!(
                checked.package.map(|package| package.platform_key),
                Some(key.to_owned())
            );
        }
    }

    #[test]
    fn the_windows_install_transition_is_unchanged_and_the_linux_states_are_distinct() {
        // Windows 的状态迁移没变：启动成功 → Launched。
        let (state, launched) = crate::update::advance_install(
            InstallState::Idle,
            || {
                Ok(zssh_update::InstallPlan {
                    program: PathBuf::from("setup.exe"),
                    args: Vec::new(),
                    restart: false,
                    version: "1.4.0".to_owned(),
                    sha256: "0".repeat(64),
                })
            },
            |_| Ok(()),
        );
        assert!(launched);
        assert_eq!(state, InstallState::Launched);
        let installed = InstallState::Installed {
            restart: PathBuf::from("/usr/bin/zeeterm"),
        };
        assert_ne!(installed, InstallState::Idle);
        assert_ne!(installed, InstallState::Installing(Flavor::Deb));
    }

    #[cfg(unix)]
    mod tar_gz {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Command;
        use zssh_update::linux::apply_tar_update_system;

        /// 做一个 tar.gz：顶层目录里放会报告指定版本的 `zeeterm` 和 `zssh-mcp`。
        fn tarball(version: &str) -> Vec<u8> {
            let root = temp_dir(&format!("linux-tarball-{version}"));
            let top = root.join(format!("ZeeTerm-{version}-linux-x86_64"));
            std::fs::create_dir_all(&top).expect("建目录");
            for (name, body) in [
                ("zeeterm", format!("#!/bin/sh\necho 'ZeeTerm {version}'\n")),
                ("zssh-mcp", format!("#!/bin/sh\necho 'mcp {version}'\n")),
            ] {
                let path = top.join(name);
                std::fs::write(&path, body).expect("写程序");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("权限");
            }
            let out = root.join("package.tar.gz");
            let status = Command::new("tar")
                .arg("-czf")
                .arg(&out)
                .arg("-C")
                .arg(&root)
                .arg(top.file_name().expect("名字"))
                .status()
                .expect("需要系统 tar");
            assert!(status.success());
            let bytes = std::fs::read(&out).expect("读包");
            let _ = std::fs::remove_dir_all(&root);
            bytes
        }

        /// 模拟手动解压的旧版本。
        fn old_install(name: &str, version: &str) -> PathBuf {
            let dir = temp_dir(name);
            for (file, body) in [
                ("zeeterm", format!("#!/bin/sh\necho 'ZeeTerm {version}'\n")),
                ("zssh-mcp", format!("#!/bin/sh\necho 'mcp {version}'\n")),
            ] {
                let target = dir.join(file);
                std::fs::write(&target, body).expect("写旧程序");
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                    .expect("权限");
            }
            dir
        }

        fn read(path: &Path) -> String {
            std::fs::read_to_string(path).expect("读文件")
        }

        fn run_tar(job: &Job) -> Outcome {
            glue::run(
                job,
                &zssh_update::linux::SystemRunner,
                &deb_tools(),
                &|archive, layout, version| apply_tar_update_system(archive, layout, version),
            )
        }

        #[test]
        fn a_tar_gz_goes_from_manifest_through_signature_check_to_replaced_programs() {
            let keys = keys();
            let tar = tarball("1.4.0");
            let fixture = publish_linux(
                &keys,
                "1.4.0",
                &[Asset {
                    key: PLATFORM_LINUX_TAR,
                    file: "ZeeTerm-1.4.0-linux-x86_64.tar.gz",
                    served: tar.clone(),
                    signed: tar,
                }],
            );
            let checked = check_on(
                &fixture,
                "1.3.0",
                &keys.pubkey_b64,
                &Platform::Linux(Flavor::Tar),
            );
            let package = checked.package.expect("应选出 tar.gz");
            assert_eq!(package.platform_key, "linux-x86_64-tar");

            let cache = temp_dir("linux-tar-cache");
            let (result, _) = download(&package, &keys.pubkey_b64, &cache);
            let (path, _, sha256) = result.unwrap_or_else(|_| panic!("下载并验签应通过"));

            let install = old_install("linux-tar-install", "1.3.0");
            let exe = install.join("zeeterm");
            let outcome = run_tar(&Job {
                flavor: Flavor::Tar,
                package: path,
                cache_root: cache.clone(),
                version: "1.4.0".to_owned(),
                sha256,
                exe: exe.clone(),
            });
            assert_eq!(
                outcome,
                Outcome::Installed {
                    restart: exe.clone()
                }
            );
            assert_eq!(read(&exe), "#!/bin/sh\necho 'ZeeTerm 1.4.0'\n");
            assert_eq!(
                read(&install.join("zssh-mcp")),
                "#!/bin/sh\necho 'mcp 1.4.0'\n"
            );
            let _ = std::fs::remove_dir_all(&cache);
            let _ = std::fs::remove_dir_all(&install);
        }

        #[test]
        fn a_package_whose_version_differs_from_the_announcement_is_signed_but_still_refused() {
            // 清单公告 1.4.0，包里的程序却报告 1.3.9（比如把旧包重新签了名）：验签通过，替换这一步拒绝。
            let keys = keys();
            let tar = tarball("1.3.9");
            let fixture = publish_linux(
                &keys,
                "1.4.0",
                &[Asset {
                    key: PLATFORM_LINUX_TAR,
                    file: "ZeeTerm-1.4.0-linux-x86_64.tar.gz",
                    served: tar.clone(),
                    signed: tar,
                }],
            );
            let checked = check_on(
                &fixture,
                "1.3.0",
                &keys.pubkey_b64,
                &Platform::Linux(Flavor::Tar),
            );
            let package = checked.package.expect("选包");
            let cache = temp_dir("linux-tar-mismatch-cache");
            let (result, _) = download(&package, &keys.pubkey_b64, &cache);
            let (path, _, sha256) = result.unwrap_or_else(|_| panic!("验签应通过"));
            let install = old_install("linux-tar-mismatch-install", "1.3.0");
            let exe = install.join("zeeterm");
            let outcome = run_tar(&Job {
                flavor: Flavor::Tar,
                package: path,
                cache_root: cache.clone(),
                version: "1.4.0".to_owned(),
                sha256,
                exe: exe.clone(),
            });
            assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
            assert_eq!(
                read(&exe),
                "#!/bin/sh\necho 'ZeeTerm 1.3.0'\n",
                "旧版本原样留着"
            );
            assert_eq!(
                read(&install.join("zssh-mcp")),
                "#!/bin/sh\necho 'mcp 1.3.0'\n"
            );
            let _ = std::fs::remove_dir_all(&cache);
            let _ = std::fs::remove_dir_all(&install);
        }
    }
}

/// macOS：只提醒有新版本，引导到发布页。检查链路用同一套本机服务，Windows / Linux 上也能跑。
mod macos_notice {
    use super::{
        CheckKind, Checked, Fixture, ManifestSource, Platform, Route, block_on, check_with, client,
        keys, publish,
    };
    use crate::update::DownloadState;
    use crate::update::layout::{self, Action};

    fn check_mac(fixture: &Fixture, current: &str, pubkey: Result<&str, &String>) -> Checked {
        let sources = [ManifestSource::Url(fixture.url("/latest.json"))];
        block_on(check_with(
            current,
            &sources,
            &client(),
            true,
            pubkey,
            &Platform::MacOs,
        ))
    }

    #[test]
    fn a_newer_manifest_is_only_a_notice_and_nothing_but_the_manifest_is_requested() {
        let keys = keys();
        let installer = b"installer".to_vec();
        let (fixture, _) = publish(
            &keys,
            "1.4.0",
            "1.4.0",
            &installer,
            Route::Ok(installer.clone()),
        );
        // 公钥是占位值也无所谓：macOS 不验签，只读版本号和说明。
        let checked = check_mac(&fixture, "1.3.0", Ok("PLACEHOLDER"));
        assert_eq!(checked.kind, CheckKind::FoundManual, "{}", checked.status);
        assert!(checked.package.is_none(), "不能带安装包");
        assert_eq!(checked.release_version.as_deref(), Some("1.4.0"));
        assert_eq!(checked.notes.as_deref(), Some("修复若干问题\n新增在线更新"));
        assert_eq!(checked.published.as_deref(), Some("2026-11-03T08:00:00Z"));
        assert!(
            checked.status.contains("发现新版本 1.4.0") && checked.status.contains("应用程序"),
            "{}",
            checked.status
        );
        assert!(
            !checked.status.contains(&fixture.base),
            "清单里的网址不能进提示：{}",
            checked.status
        );
        assert_eq!(fixture.hits(), 1, "只读了清单，没有碰安装包");

        // 界面：这一屏只有「跳过 / 关闭 / 打开发布页」，没有任何下载、安装、重启动作。
        let screen = layout::screen(Some(checked.kind), &DownloadState::Idle);
        let actions: Vec<Action> = layout::buttons(screen)
            .iter()
            .map(|button| button.action)
            .collect();
        assert_eq!(
            actions,
            vec![Action::Skip, Action::Close, Action::OpenReleasePage]
        );
    }

    #[test]
    fn latest_and_unreadable_manifests_behave_like_other_platforms() {
        let keys = keys();
        let installer = b"installer".to_vec();
        let (fixture, _) = publish(
            &keys,
            "1.4.0",
            "1.4.0",
            &installer,
            Route::Ok(installer.clone()),
        );
        let latest = check_mac(&fixture, "1.4.0", Ok("PLACEHOLDER"));
        assert_eq!(latest.kind, CheckKind::Latest, "{}", latest.status);
        assert!(latest.package.is_none() && latest.release_version.is_none());

        // 清单读不到：和其他平台一样给检查失败的原因。
        let missing = Fixture::start();
        let failed = check_mac(&missing, "1.3.0", Ok("PLACEHOLDER"));
        assert_eq!(failed.kind, CheckKind::Failed);
        assert!(failed.status.contains("404"), "{}", failed.status);
    }
}

/// Windows 免安装版：同一套"取清单 → 下载 → 验签"，选的是清单里的 `windows-x86_64-portable`（zip），
/// 最后一步是原地替换程序文件——这里换成桩（真正的解压 / 替换 / 回滚在 `zssh_update::portable` 的测试里），
/// 不碰任何正在用的程序。
mod portable_chain {
    use super::{
        CheckKind, Checked, DownloadEvent, Fixture, ManifestSource, Platform, Route, block_on,
        check_with, client, download, failure, file_count, keys, publish, sign_installer, temp_dir,
    };
    use crate::update::linux::Outcome;
    use crate::update::portable::{self as glue, Job};
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use zssh_update::portable::{PortableError, PortableLayout};
    use zssh_update::{
        PLATFORM_LINUX_TAR, PLATFORM_WINDOWS_PORTABLE, PlatformAsset, ReleaseManifest,
        build_latest_json_with_platforms,
    };

    const ZIP_NAME: &str = "ZeeTerm-1.4.0-windows-x64-portable.zip";

    /// 发布一个带 Windows 安装包、Linux tar.gz 和免安装版 zip 的版本（和发版流水线生成 latest.json 用的是同一个函数）。
    /// `served` 是夹具实际提供的 zip 字节，`signed` 是签名时用的字节。
    fn publish_all(keys: &super::Keys, served: &[u8], signed: &[u8]) -> Fixture {
        let fixture = Fixture::start();
        let installer_sig = sign_installer(keys, b"installer", "1.4.0", "ZeeTerm-Setup-1.4.0.exe");
        let zip_sig = sign_installer(keys, signed, "1.4.0", ZIP_NAME);
        let tar_sig = sign_installer(keys, b"tar", "1.4.0", "ZeeTerm-1.4.0-linux-x86_64.tar.gz");
        let json = build_latest_json_with_platforms(
            &ReleaseManifest {
                version: "1.4.0",
                notes: "新增免安装版",
                notes_en: None,
                pub_date: "2026-11-08T08:00:00Z",
                url: "https://fixture.invalid/download/v1.4.0/ZeeTerm-Setup-1.4.0.exe",
                signature: &installer_sig,
            },
            &[
                PlatformAsset {
                    key: PLATFORM_LINUX_TAR,
                    url: "https://fixture.invalid/download/v1.4.0/ZeeTerm-1.4.0-linux-x86_64.tar.gz",
                    signature: &tar_sig,
                },
                PlatformAsset {
                    key: PLATFORM_WINDOWS_PORTABLE,
                    url: &format!("https://fixture.invalid/download/v1.4.0/{ZIP_NAME}"),
                    signature: &zip_sig,
                },
            ],
        )
        .expect("生成 latest.json")
        .replace("https://fixture.invalid", &fixture.base);
        fixture.set("/latest.json", Route::Ok(json.into_bytes()));
        fixture.set(
            &format!("/download/v1.4.0/{ZIP_NAME}"),
            Route::Ok(served.to_vec()),
        );
        fixture
    }

    fn check_on(
        fixture: &Fixture,
        current: &str,
        pubkey: Result<&str, &String>,
        platform: &Platform,
    ) -> Checked {
        let sources = [ManifestSource::Url(fixture.url("/latest.json"))];
        block_on(check_with(
            current,
            &sources,
            &client(),
            true,
            pubkey,
            platform,
        ))
    }

    fn layout() -> PortableLayout {
        PortableLayout::new(Path::new(r"D:\Tools\ZeeTerm\ZeeTerm.exe"))
    }

    #[test]
    fn a_portable_copy_picks_the_zip_downloads_and_verifies_it_and_then_replaces_in_place() {
        let keys = keys();
        let zip = b"PK fake zip bytes".to_vec();
        let fixture = publish_all(&keys, &zip, &zip);
        let platform = Platform::WindowsPortable { writable: true };

        let checked = check_on(&fixture, "1.3.0", Ok(&keys.pubkey_b64), &platform);
        assert_eq!(checked.kind, CheckKind::Found, "{}", checked.status);
        let package = checked.package.expect("应选出 zip");
        assert_eq!(package.platform_key, "windows-x86_64-portable");
        assert!(package.url.ends_with(ZIP_NAME), "{}", package.url);
        assert_eq!(checked.release_version.as_deref(), Some("1.4.0"));

        // 下载 + 验签：缓存里是验过的那份字节，文件名保持 zip 的名字。
        let cache = temp_dir("portable-ok");
        let (result, _) = download(&package, &keys.pubkey_b64, &cache);
        let (path, size, sha256) = result.unwrap_or_else(|_| panic!("下载并验签应通过"));
        assert_eq!(size, zip.len() as u64);
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(ZIP_NAME));
        assert_eq!(sha256, zssh_update::sha256_hex(&zip));

        // 原地替换（桩）：拿到的是缓存里已验签的 zip 和正在运行的程序布局；成功后重启的就是原来的路径。
        let job = Job {
            package: path.clone(),
            cache_root: cache.clone(),
            version: "1.4.0".to_owned(),
            sha256,
            layout: layout(),
        };
        let applied = Cell::new(0);
        let outcome = glue::run(&job, &|archive, layout_seen| {
            applied.set(applied.get() + 1);
            assert_eq!(archive, path.canonicalize().expect("规范化"));
            assert_eq!(layout_seen, &layout());
            Ok(())
        });
        assert_eq!(
            outcome,
            Outcome::Installed {
                restart: PathBuf::from(r"D:\Tools\ZeeTerm\ZeeTerm.exe")
            }
        );
        assert_eq!(applied.get(), 1);

        // 替换失败：给出原因，不是"已安装"。
        let failed = glue::run(&job, &|_, _| {
            Err(PortableError::MissingFile("ZeeTerm.exe".into()))
        });
        assert!(matches!(failed, Outcome::Failed(_)), "{failed:?}");

        // 验签之后 zip 被动过：替换不会被调用。
        std::fs::write(&path, b"tampered").expect("改包");
        let untouched = glue::run(&job, &|_, _| panic!("包不对就不能去替换程序文件"));
        assert!(matches!(untouched, Outcome::Failed(_)), "{untouched:?}");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn the_same_manifest_still_gives_an_installed_copy_the_installer_and_never_the_zip() {
        let keys = keys();
        let zip = b"PK fake zip bytes".to_vec();
        let fixture = publish_all(&keys, &zip, &zip);
        // 安装版（和老客户端）选的还是 Windows 安装包。
        let installed = check_on(&fixture, "1.3.0", Ok(&keys.pubkey_b64), &Platform::Windows);
        assert_eq!(installed.kind, CheckKind::Found, "{}", installed.status);
        assert_eq!(
            installed.package.map(|package| package.platform_key),
            Some("windows-x86_64".to_owned())
        );
    }

    #[test]
    fn a_bad_signature_never_reaches_the_replacement() {
        let keys = keys();
        let fixture = publish_all(&keys, b"swapped on the server", b"good zip");
        let checked = check_on(
            &fixture,
            "1.3.0",
            Ok(&keys.pubkey_b64),
            &Platform::WindowsPortable { writable: true },
        );
        let package = checked.package.expect("清单本身没问题");
        let cache = temp_dir("portable-bad-sig");
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
    fn an_unwritable_folder_is_only_a_notice_that_needs_no_signing_key_and_touches_no_package() {
        let keys = keys();
        let zip = b"PK fake zip bytes".to_vec();
        let fixture = publish_all(&keys, &zip, &zip);
        // 公钥不可用也无所谓：不下载、不验签，只读版本号和说明。
        let checked = check_on(
            &fixture,
            "1.3.0",
            Ok("PLACEHOLDER"),
            &Platform::WindowsPortable { writable: false },
        );
        assert_eq!(checked.kind, CheckKind::FoundManual, "{}", checked.status);
        assert!(checked.package.is_none(), "不能带安装包");
        assert_eq!(checked.release_version.as_deref(), Some("1.4.0"));
        assert_eq!(checked.notes.as_deref(), Some("新增免安装版"));
        assert!(
            checked.status.contains("发现新版本 1.4.0") && checked.status.contains("data"),
            "{}",
            checked.status
        );
        assert!(
            !checked.status.contains(&fixture.base),
            "清单里的网址不能进提示：{}",
            checked.status
        );
        assert_eq!(fixture.hits(), 1, "只读了清单，没有碰 zip");
        // 已是最新时不提示。
        let latest = check_on(
            &fixture,
            "1.4.0",
            Ok("PLACEHOLDER"),
            &Platform::WindowsPortable { writable: false },
        );
        assert_eq!(latest.kind, CheckKind::Latest);
    }

    #[test]
    fn a_manifest_without_a_zip_entry_points_a_portable_copy_to_the_release_page() {
        let keys = keys();
        // 老版本发布的清单：只有 Windows 安装包条目。
        let installer = b"installer".to_vec();
        let (fixture, _) = publish(
            &keys,
            "1.4.0",
            "1.4.0",
            &installer,
            Route::Ok(installer.clone()),
        );
        let checked = check_on(
            &fixture,
            "1.3.0",
            Ok(&keys.pubkey_b64),
            &Platform::WindowsPortable { writable: true },
        );
        assert_eq!(checked.kind, CheckKind::FoundManual, "{}", checked.status);
        assert!(checked.package.is_none(), "免安装版不能退而求其次装安装包");
        assert_eq!(checked.release_version.as_deref(), Some("1.4.0"));
        // 公钥占位时照常拒绝（可写的免安装版会下载，没有公钥就不能验）。
        let unconfigured = check_on(
            &fixture,
            "1.3.0",
            Ok("PLACEHOLDER"),
            &Platform::WindowsPortable { writable: true },
        );
        assert_eq!(unconfigured.kind, CheckKind::Failed);
    }
}
