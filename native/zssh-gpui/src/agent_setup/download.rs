//! opencode 的一键下载（0.2.14 的 `ai_agent_download_info` / `ai_agent_download` /
//! `ai_agent_download_cancel` 加设置页的确认框）。
//!
//! 来源、校验、解压全是 core 的 `ai::agent::download`：只认 GitHub Release 的 Windows x64 资产，
//! 没有 SHA-256 摘要就不下，跳转只能去 GitHub 自己的域名，下完先核对大小和摘要才解压、才运行。
//! 代理走"软件网络代理"设置（`app_proxy`），和更新检查、配置同步同一个。只有 opencode 提供下载，
//! codex 和 Claude Code 在 0.2.14 里也没有，这里不新增。
//!
//! 这里只放流程的拼装（下载 → 校验 → 解压 → 删掉压缩包），测试用本地桩服务器，不碰网络、
//! 不往用户机器装东西。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use zssh_core::ai::agent::discovery::{self, AgentInstall, OPENCODE};
use zssh_core::ai::agent::download::{self, DownloadError, ReleaseInfo};
use zssh_core::store::Database;

/// 软件网络代理的 URL（带账号密码，**不能打印**）；没有设置或读不出来就是直连。
pub fn proxy_url(data_dir: &Path) -> Option<String> {
    let db = Database::open(&data_dir.join("zssh.db")).ok()?;
    zssh_core::sync::app_proxy::app_proxy(&db)
        .ok()
        .flatten()
        .map(|proxy| proxy.url_with_credentials())
}

/// 查最新 Release 的下载信息（来源、版本、大小），供确认框显示。
pub async fn fetch_info(proxy: Option<&str>) -> Result<ReleaseInfo, String> {
    let client = download::build_client(proxy)?;
    download::latest_release(&client).await
}

/// 把 Release 里的 zip 下载到 `tools_dir`，核对大小和 SHA-256，解出 `opencode.exe`，删掉压缩包。
///
/// - `on_progress(已下载, 总量)`：core 按 1 MiB 节流；
/// - `cancel` 置位就中断并删掉半截文件，返回「用户取消」；
/// - 校验不过：core 会把下载的文件删掉，这里直接把原因带出去，不解压、不运行；
/// - 解压失败不留压缩包。
pub async fn install_release(
    client: &reqwest::Client,
    release: &ReleaseInfo,
    tools_dir: &Path,
    on_progress: impl FnMut(u64, u64) + Send,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(tools_dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let zip_path = tools_dir.join("opencode-download.zip");
    download::download_to(
        client,
        &release.download_url,
        &zip_path,
        release.size,
        on_progress,
        cancel,
    )
    .await
    .map_err(|err| match err {
        DownloadError::Cancelled => err.to_string(),
        DownloadError::Failed(_) => {
            let _ = std::fs::remove_file(&zip_path);
            err.to_string()
        }
    })?;
    // 先核对大小和 SHA-256，一致才解压、才运行；不一致时文件已经删掉。
    download::verify_download(&zip_path, release.size, &release.sha256)?;
    let extracted = download::extract_exe(&zip_path, tools_dir);
    let _ = std::fs::remove_file(&zip_path);
    extracted
}

/// 完整的一次下载：下载安装之后跑一次 `--version`（摘要已经核对过，这一步不是校验，只确认能跑、
/// 读出版本）。
pub async fn download_and_probe(
    release: &ReleaseInfo,
    data_dir: &Path,
    proxy: Option<&str>,
    on_progress: impl FnMut(u64, u64) + Send,
    cancel: &AtomicBool,
) -> Result<AgentInstall, String> {
    let client = download::build_client(proxy)?;
    let tools_dir = discovery::tools_dir(data_dir, &OPENCODE);
    let exe = install_release(&client, release, &tools_dir, on_progress, cancel).await?;
    discovery::probe_path(&OPENCODE, &exe)
        .await
        .map_err(|e| format!("下载完成但运行 --version 失败：{e}"))
}

/// 大小的显示（确认框里）：0 字节显示"未知"。
pub fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "未知".to_owned();
    }
    if bytes < 1024 * 1024 {
        return format!("{} KB", (bytes as f64 / 1024.0).round() as u64);
    }
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

/// 进度条的百分比（总量未知时 `None`）。
pub fn percent(received: u64, total: u64) -> Option<u32> {
    if total == 0 {
        return None;
    }
    Some(((received.min(total) * 100) / total) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 只有 `opencode.exe` 一个条目的小 zip（内容是一行文字，不是真的可执行文件）。
    const STUB_ZIP: &[u8] = include_bytes!("fixtures/opencode-stub.zip");
    /// `STUB_ZIP` 的 SHA-256（PowerShell `Get-FileHash` 算的）。
    const STUB_SHA256: &str = "9ab4c3fcf1d0ab2b6a66d02a2134cb78eb745c4fd02c96b1912432e6e14669ee";

    /// 本地桩：每个连接回答一个固定响应（`body` 是整包字节），可以故意发慢。
    async fn serve_once(body: Vec<u8>, slow: bool) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("绑定");
        let port = listener.local_addr().expect("端口").port();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    if slow {
                        // 先发一半，等一会儿再发，给取消留出窗口。
                        let half = body.len() / 2;
                        let _ = stream.write_all(&body[..half]).await;
                        let _ = stream.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        let _ = stream.write_all(&body[half..]).await;
                    } else {
                        let _ = stream.write_all(&body).await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}/opencode.zip"), task)
    }

    fn release(url: &str, size: u64, sha256: &str) -> ReleaseInfo {
        ReleaseInfo {
            version: "v9.9.9".to_owned(),
            asset_name: download::WINDOWS_X64_ASSET.to_owned(),
            size,
            sha256: sha256.to_owned(),
            download_url: url.to_owned(),
            page_url: "https://github.com/example/opencode/releases/tag/v9.9.9".to_owned(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-g10a-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn client() -> reqwest::Client {
        // 不走任何代理：桩在本机。
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("客户端")
    }

    #[tokio::test]
    async fn a_matching_download_is_verified_extracted_and_the_zip_removed() {
        let (url, server) = serve_once(STUB_ZIP.to_vec(), false).await;
        let dir = temp_dir("ok");
        let cancel = AtomicBool::new(false);
        let mut progress = Vec::new();
        let exe = install_release(
            &client(),
            &release(&url, STUB_ZIP.len() as u64, STUB_SHA256),
            &dir,
            |received, total| progress.push((received, total)),
            &cancel,
        )
        .await
        .expect("下载并解压");
        server.abort();
        assert_eq!(exe, dir.join("opencode.exe"));
        assert!(exe.is_file());
        assert!(!dir.join("opencode-download.zip").exists(), "压缩包要删掉");
        let last = progress.last().copied().expect("至少回调一次进度");
        assert_eq!(last.0, STUB_ZIP.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_wrong_digest_stops_before_anything_is_extracted() {
        let (url, server) = serve_once(STUB_ZIP.to_vec(), false).await;
        let dir = temp_dir("bad-digest");
        let cancel = AtomicBool::new(false);
        let wrong = "0".repeat(64);
        let err = install_release(
            &client(),
            &release(&url, STUB_ZIP.len() as u64, &wrong),
            &dir,
            |_, _| {},
            &cancel,
        )
        .await
        .expect_err("摘要不一致必须失败");
        server.abort();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(err.contains("没有安装"), "{err}");
        assert!(!dir.join("opencode.exe").exists(), "不能解压");
        assert!(
            !dir.join("opencode-download.zip").exists(),
            "校验失败要删掉下载的文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_response_bigger_than_the_published_size_is_refused() {
        let (url, server) = serve_once(STUB_ZIP.to_vec(), false).await;
        let dir = temp_dir("too-big");
        let cancel = AtomicBool::new(false);
        // Release 公布的比实际小：服务器给的大小超过了公布值，一个字节都不落盘。
        let err = install_release(
            &client(),
            &release(&url, (STUB_ZIP.len() as u64) - 1, STUB_SHA256),
            &dir,
            |_, _| {},
            &cancel,
        )
        .await
        .expect_err("超过公布大小必须失败");
        server.abort();
        assert!(err.contains("超过"), "{err}");
        assert!(!dir.join("opencode-download.zip").exists());
        assert!(!dir.join("opencode.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancelling_mid_download_removes_the_partial_file() {
        let (url, server) = serve_once(STUB_ZIP.to_vec(), true).await;
        let dir = temp_dir("cancel");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cancel);
        // 收到第一块（进度回调）就取消。
        let result = install_release(
            &client(),
            &release(&url, STUB_ZIP.len() as u64, STUB_SHA256),
            &dir,
            move |received, _| {
                if received == 0 {
                    // 开始下载的 0 进度回调之后，等下一块到来时 core 会看到取消标志。
                    flag.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .await;
        server.abort();
        let err = result.expect_err("取消要返回错误");
        assert_eq!(err, "用户取消");
        assert!(
            !dir.join("opencode-download.zip").exists(),
            "半截文件要删掉"
        );
        assert!(!dir.join("opencode.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes_and_percentages_read_like_the_confirm_dialog() {
        assert_eq!(format_bytes(0), "未知");
        assert_eq!(format_bytes(512), "1 KB");
        assert_eq!(format_bytes(200 * 1024), "200 KB");
        assert_eq!(format_bytes(52_428_800), "50.0 MB");
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(50, 200), Some(25));
        assert_eq!(percent(300, 200), Some(100));
    }

    #[test]
    fn only_github_assets_with_a_digest_are_accepted_by_core() {
        // 来源和校验是 core 的规则：这里确认 ZeeTerm 没有绕开它（不带摘要的 Release 解析即失败）。
        let body = serde_json::json!({
            "tag_name": "v1.20.0",
            "html_url": "https://github.com/x/y/releases/tag/v1.20.0",
            "assets": [{
                "name": download::WINDOWS_X64_ASSET,
                "size": 1024,
                "browser_download_url": "https://github.com/x/y/releases/download/v1.20.0/opencode-windows-x64.zip"
            }]
        });
        let err = download::parse_release(&body).expect_err("没有摘要必须拒绝");
        assert!(err.contains("SHA-256"), "{err}");
    }
}
