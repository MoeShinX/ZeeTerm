//! 一键下载 opencode：查官方 GitHub Release、下载当前平台的资产、解压出可执行文件。
//! Windows x64 是 zip（里面 `opencode.exe`），Linux x64 是 `.tar.gz`（里面 `opencode`），
//! 来源、摘要校验、跳转限制两边完全一样。代理走 Z-SSH 的默认代理设置（拼好 URL 传进来）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

/// 官方 Release API（GitHub 会把 sst/opencode 重定向到这里）。
pub const RELEASE_API: &str = "https://api.github.com/repos/anomalyco/opencode/releases/latest";
/// 要下载的资产名（Windows x64 普通版；baseline 和 arm64 不选）。
pub const WINDOWS_X64_ASSET: &str = "opencode-windows-x64.zip";
/// Linux x64（glibc）普通版：官方安装脚本 `opencode.ai/install` 在 Linux 上下载的就是
/// `opencode-linux-<架构>[-baseline][-musl].tar.gz`，包里根目录一个 `opencode`。
pub const LINUX_X64_ASSET: &str = "opencode-linux-x64.tar.gz";
/// Linux x64 的 baseline 版：CPU 不支持 AVX2 时官方脚本选它（`/proc/cpuinfo` 里没有 `avx2`）。
pub const LINUX_X64_BASELINE_ASSET: &str = "opencode-linux-x64-baseline.tar.gz";
/// 下载地址必须在 GitHub 上（Release 资产的直链都是这个前缀，之后的重定向由 GitHub 发起）。
const DOWNLOAD_URL_PREFIX: &str = "https://github.com/";
/// 资产大小上限：远大于真实包（几十 MiB），只防一个异常的响应把磁盘写满。
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// 解出来的 opencode.exe 最大多少字节：压缩包大小管不住解压后的大小。
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;
/// 最多跟几次跳转（GitHub 的下载地址会跳到 objects / release-assets 子域名）。
const MAX_REDIRECTS: usize = 10;

/// 最新 Release 里我们关心的信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseInfo {
    /// 版本号（tag，如 `v1.19.0`）。
    pub version: String,
    /// 资产名。
    pub asset_name: String,
    /// 字节数。
    pub size: u64,
    /// GitHub 给这个资产算的 SHA-256（小写十六进制）。下载完必须一致才解压、才运行（R-10 M8）。
    pub sha256: String,
    /// 资产直链。
    pub download_url: String,
    /// Release 页面（展示给用户看的来源）。
    pub page_url: String,
}

/// 下载出错（含用户取消）。
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("用户取消")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

/// 组装 HTTP 客户端。`proxy_url` 形如 `socks5://127.0.0.1:1080` 或
/// `http://user:pass@127.0.0.1:8080`，None 表示直连。
pub fn build_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        // GitHub API 拒绝没有 User-Agent 的请求。
        .user_agent("Z-SSH")
        .connect_timeout(std::time::Duration::from_secs(20))
        // 跳转也只能去 GitHub 自己的 https 地址：初始地址检查管不到中途跳去哪。
        // 下载完还要核对摘要，这一层是让请求根本不去别处。
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("跳转次数太多")
            } else if redirect_allowed(attempt.url()) {
                attempt.follow()
            } else {
                let target = attempt.url().to_string();
                attempt.error(format!("下载被跳转到了 GitHub 以外的地址：{target}"))
            }
        }));
    if let Some(url) = proxy_url {
        let proxy = reqwest::Proxy::all(url).map_err(|e| format!("代理地址无效：{e}"))?;
        builder = builder.proxy(proxy);
    } else {
        // 没有代理 = 直连：用户选了“不使用代理”时，不能被系统代理或 HTTP_PROXY 环境变量带走。
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))
}

/// 跳转目标是否可信：只接受 https 的 github.com / *.github.com / *.githubusercontent.com。
fn redirect_allowed(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ["github.com", "githubusercontent.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// 当前平台该下载哪个资产；这个平台没有官方包时返回原因。
///
/// Linux 上照官方安装脚本的选法：CPU 没有 AVX2 就用 baseline 版（用运行时特性检测，
/// 和脚本读 `/proc/cpuinfo` 等价）。musl（Alpine）的包 ZeeTerm 自己不支持运行，不选。
pub fn platform_asset_name() -> Result<&'static str, String> {
    #[cfg(windows)]
    {
        Ok(WINDOWS_X64_ASSET)
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok(if std::arch::is_x86_feature_detected!("avx2") {
            LINUX_X64_ASSET
        } else {
            LINUX_X64_BASELINE_ASSET
        })
    }
    #[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
    {
        Err("这个平台没有 opencode 官方一键下载包，请手动安装并在设置里指定路径".to_string())
    }
}

/// 下载下来的文件在 tools 目录里先叫什么（扩展名跟资产走，解压才认得出格式）。
pub fn archive_file_name(asset_name: &str) -> &'static str {
    if asset_name.ends_with(".tar.gz") {
        "opencode-download.tar.gz"
    } else {
        "opencode-download.zip"
    }
}

/// 装好之后的可执行文件名。
pub const fn installed_binary_name() -> &'static str {
    if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    }
}

/// 查最新 Release 里当前平台的资产。
pub async fn latest_release(client: &reqwest::Client) -> Result<ReleaseInfo, String> {
    let resp = client
        .get(RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("访问 GitHub 失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub 返回 {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 Release 信息失败：{e}"))?;
    parse_release_for(&body, platform_asset_name()?)
}

/// 从 Release JSON 里挑出 Windows x64 资产。
pub fn parse_release(body: &serde_json::Value) -> Result<ReleaseInfo, String> {
    parse_release_for(body, WINDOWS_X64_ASSET)
}

/// 从 Release JSON 里挑出名为 `wanted` 的资产（摘要、大小、地址的检查对所有平台一样严）。
pub fn parse_release_for(body: &serde_json::Value, wanted: &str) -> Result<ReleaseInfo, String> {
    let version = body["tag_name"]
        .as_str()
        .ok_or("Release 里没有 tag_name")?
        .to_string();
    let page_url = body["html_url"].as_str().unwrap_or("").to_string();
    let assets = body["assets"].as_array().ok_or("Release 里没有 assets")?;
    let asset = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(wanted))
        .ok_or_else(|| format!("最新 Release 里没有 {wanted}"))?;
    // 下载的东西随后会被直接运行：没有能核对的摘要就不下（R-10 M8）。TLS 之外只有它能证明
    // 下下来的就是 Release 里那一份（代理 / 中间盒 / CDN 一侧被改过都能发现）。
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| {
            "这个 Release 没有提供 SHA-256 摘要，无法核对下载内容，已停止。可以到发布页手动下载安装"
                .to_string()
        })?;
    let size = asset["size"].as_u64().unwrap_or(0);
    if size == 0 || size > MAX_DOWNLOAD_BYTES {
        return Err(format!("Release 里的文件大小不正常（{size} 字节），已停止"));
    }
    let download_url = asset["browser_download_url"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if !download_url.starts_with(DOWNLOAD_URL_PREFIX) {
        return Err(format!("下载地址不在 GitHub 上（{download_url}），已停止"));
    }
    Ok(ReleaseInfo {
        version,
        asset_name: wanted.to_string(),
        size,
        sha256,
        download_url,
        page_url,
    })
}

/// 核对下载下来的文件：大小和 SHA-256 都要和 Release 公布的一致。不一致时**删掉文件**，
/// 调用方不能再解压或运行它。
pub fn verify_download(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};

    let check = || -> Result<(), String> {
        let mut file = std::fs::File::open(path).map_err(|e| format!("读下载文件失败：{e}"))?;
        let mut hasher = Sha256::new();
        let size =
            std::io::copy(&mut file, &mut hasher).map_err(|e| format!("读下载文件失败：{e}"))?;
        if size != expected_size {
            return Err(format!(
                "下载的文件大小（{size} 字节）和 Release 公布的（{expected_size} 字节）不一致"
            ));
        }
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(
                "下载的文件和 Release 公布的 SHA-256 不一致（可能被代理或中间人改过）".to_string(),
            );
        }
        Ok(())
    };
    check().map_err(|reason| {
        let _ = std::fs::remove_file(path);
        format!("{reason}，已删除下载的文件，没有安装")
    })
}

/// 下载到本地文件；每次回调 `(已下载, 总量)`，1MiB 节流；`cancel` 置位就中断。
///
/// `max_bytes`：最多收这么多字节（Release 公布的大小），多了就中断——下载完还要
/// [`verify_download`]，这里只防异常的响应把磁盘写满。
pub async fn download_to(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    max_bytes: u64,
    mut on_progress: impl FnMut(u64, u64) + Send,
    cancel: &AtomicBool,
) -> Result<(), DownloadError> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| DownloadError::Failed(format!("下载失败：{e}")))?;
    if !resp.status().is_success() {
        return Err(DownloadError::Failed(format!("下载返回 {}", resp.status())));
    }
    let total = resp.content_length().unwrap_or(0);
    if total > max_bytes {
        return Err(DownloadError::Failed(format!(
            "服务器给的大小（{total} 字节）超过了 Release 公布的大小（{max_bytes} 字节），没有下载"
        )));
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| DownloadError::Failed(format!("创建下载文件失败：{e}")))?;
    let mut received: u64 = 0;
    let mut last_reported: u64 = 0;
    on_progress(0, total);

    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| DownloadError::Failed(format!("下载中断：{e}")))?
    {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(DownloadError::Cancelled);
        }
        // 先算再写：超出的那一块不落盘，磁盘占用严格不超过 max_bytes。
        received += chunk.len() as u64;
        if received > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(DownloadError::Failed(format!(
                "下载的内容超过了 Release 公布的大小（{max_bytes} 字节），已停止"
            )));
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|e| DownloadError::Failed(format!("写文件失败：{e}")))?;
        // 1MiB 节流，别把进度事件刷爆。
        if received - last_reported >= 1024 * 1024 {
            last_reported = received;
            on_progress(received, total);
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| DownloadError::Failed(format!("刷新下载文件失败：{e}")))?;
    drop(file);
    on_progress(received, total);

    if total > 0 && received != total {
        return Err(DownloadError::Failed(format!(
            "下载不完整：{received}/{total} 字节"
        )));
    }
    Ok(())
}

/// 从 zip 里解出 `opencode.exe` 到 `out_dir`（平铺），返回 exe 路径。
///
/// zip 没有 exe 或者解压失败都报错，调用方负责把半成品目录清掉。
pub fn extract_exe(zip_path: &Path, out_dir: &Path) -> Result<PathBuf, String> {
    extract_exe_limited(zip_path, out_dir, MAX_EXTRACTED_BYTES)
}

/// 同 [`extract_exe`]，解出来的文件超过 `limit` 字节就删掉并报错。
fn extract_exe_limited(zip_path: &Path, out_dir: &Path, limit: u64) -> Result<PathBuf, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("打开压缩包失败：{e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("压缩包损坏或格式不对：{e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取压缩包条目失败：{e}"))?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let is_exe = name
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("opencode.exe"))
            .unwrap_or(false);
        if !is_exe {
            continue;
        }
        std::fs::create_dir_all(out_dir).map_err(|e| format!("创建目录失败：{e}"))?;
        let dest = out_dir.join("opencode.exe");
        if entry.size() > limit {
            return Err(format!(
                "压缩包里的 opencode.exe 声称有 {} 字节，超过上限 {limit} 字节，没有解压",
                entry.size()
            ));
        }
        let mut out = std::fs::File::create(&dest).map_err(|e| format!("写 {dest:?} 失败：{e}"))?;
        // 条目头里的大小可以是假的：实际解压时也只读到上限多一个字节为止。
        let written = std::io::copy(&mut std::io::Read::take(&mut entry, limit + 1), &mut out)
            .map_err(|e| format!("解压失败：{e}"))?;
        if written > limit {
            drop(out);
            let _ = std::fs::remove_file(&dest);
            return Err(format!(
                "解出来的 opencode.exe 超过上限 {limit} 字节，已删除"
            ));
        }
        return Ok(dest);
    }
    Err(format!(
        "压缩包里没有 opencode.exe（{}）",
        zip_path.display()
    ))
}

/// 按压缩包类型解出可执行文件：`.tar.gz` 走 [`extract_tar_gz_binary`]，其余按 zip 处理。
pub fn extract_binary(archive_path: &Path, out_dir: &Path) -> Result<PathBuf, String> {
    let is_tar_gz = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar.gz"));
    if is_tar_gz {
        extract_tar_gz_binary(archive_path, out_dir, MAX_EXTRACTED_BYTES)
    } else {
        extract_exe(archive_path, out_dir)
    }
}

/// tar 块大小。
const TAR_BLOCK: usize = 512;
/// 解压过程中最多读多少字节的 tar 流（可执行文件上限之外再留一点给头和别的小文件）：
/// 防止压缩炸弹把时间和磁盘耗光。
const TAR_STREAM_SLACK: u64 = 64 * 1024 * 1024;

/// 从 `.tar.gz` 里解出名叫 `opencode` 的那一个普通文件到 `out_dir/opencode`（平铺），
/// 在 Unix 上设成 `0o755`，返回路径。
///
/// 为什么自己读 tar 而不是调系统的 `tar`：不依赖用户机器上装了什么，条目名也不会被
/// 用来拼目标路径——输出位置永远是 `out_dir` 下固定的文件名，`../` 之类的条目名碰不到磁盘。
/// 符号链接 / 硬链接条目一律跳过，不会被"跟着写"。文件超过 `limit` 字节就删掉并报错。
pub fn extract_tar_gz_binary(
    archive_path: &Path,
    out_dir: &Path,
    limit: u64,
) -> Result<PathBuf, String> {
    use std::io::Read;

    let file = std::fs::File::open(archive_path).map_err(|e| format!("打开压缩包失败：{e}"))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = std::io::Read::take(decoder, limit.saturating_add(TAR_STREAM_SLACK));
    let wanted = "opencode";
    // 下一个条目的名字被 GNU `L` 记录 / pax `path` 覆盖时记在这里。
    let mut long_name: Option<String> = None;

    loop {
        let mut header = [0u8; TAR_BLOCK];
        read_tar_block(&mut tar, &mut header)?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        verify_tar_checksum(&header)?;
        let size = tar_number(&header[124..136]).ok_or("压缩包损坏：条目大小读不出来")?;
        let kind = header[156];
        let padded = size
            .checked_add(TAR_BLOCK as u64 - 1)
            .ok_or("压缩包损坏：条目大小溢出")?
            / TAR_BLOCK as u64
            * TAR_BLOCK as u64;

        match kind {
            // GNU 长文件名 / pax 扩展头：数据是给下一个条目用的元信息，量很小。
            b'L' | b'x' => {
                if size > 64 * 1024 {
                    return Err("压缩包损坏：元信息条目过大".to_string());
                }
                let mut data = vec![0u8; padded as usize];
                tar.read_exact(&mut data)
                    .map_err(|e| format!("读取压缩包失败：{e}"))?;
                data.truncate(size as usize);
                long_name = if kind == b'L' {
                    Some(
                        String::from_utf8_lossy(&data)
                            .trim_end_matches('\0')
                            .to_string(),
                    )
                } else {
                    pax_path(&data).or(long_name)
                };
                continue;
            }
            b'g' => {
                skip_tar_bytes(&mut tar, padded)?;
                continue;
            }
            _ => {}
        }

        let name = long_name.take().unwrap_or_else(|| tar_entry_name(&header));
        let base = name.rsplit('/').find(|part| !part.is_empty()).unwrap_or("");
        let is_regular = kind == b'0' || kind == 0;
        if !(is_regular && base == wanted) {
            skip_tar_bytes(&mut tar, padded)?;
            continue;
        }
        if size > limit {
            return Err(format!(
                "压缩包里的 {wanted} 声称有 {size} 字节，超过上限 {limit} 字节，没有解压"
            ));
        }
        std::fs::create_dir_all(out_dir).map_err(|e| format!("创建目录失败：{e}"))?;
        let dest = out_dir.join(wanted);
        // Unix：先写到同目录的临时名，设好可执行权限，再 `rename` 覆盖。直接截断一个正在运行的程序
        // 会得到 `ETXTBSY`（opencode 智能体运行中更新就是这样），而替换目录项不受它限制，
        // 失败时旧版本也原样保留。Windows 保持原来的直接写（运行中的 exe 本来就不能被替换）。
        #[cfg(unix)]
        let write_to = out_dir.join(format!(
            ".{wanted}.{}.{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        #[cfg(not(unix))]
        let write_to = dest.clone();
        let mut out =
            std::fs::File::create(&write_to).map_err(|e| format!("写 {write_to:?} 失败：{e}"))?;
        let copied = std::io::copy(&mut Read::take(&mut tar, size), &mut out)
            .map_err(|e| format!("解压失败：{e}"));
        drop(out);
        let copied = match copied {
            Ok(copied) if copied == size => copied,
            Ok(copied) => {
                let _ = std::fs::remove_file(&write_to);
                return Err(format!(
                    "压缩包不完整：{wanted} 应有 {size} 字节，只读到 {copied} 字节"
                ));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&write_to);
                return Err(err);
            }
        };
        debug_assert_eq!(copied, size);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&write_to, std::fs::Permissions::from_mode(0o755)).map_err(
                |e| {
                    let _ = std::fs::remove_file(&write_to);
                    format!("设置可执行权限失败：{e}")
                },
            )?;
            std::fs::rename(&write_to, &dest).map_err(|e| {
                let _ = std::fs::remove_file(&write_to);
                format!("替换 {dest:?} 失败：{e}")
            })?;
        }
        return Ok(dest);
    }
    Err(format!(
        "压缩包里没有 {wanted}（{}）",
        archive_path.display()
    ))
}

/// 读满一个 tar 块；流在块中间结束算压缩包损坏。
fn read_tar_block(
    reader: &mut impl std::io::Read,
    block: &mut [u8; TAR_BLOCK],
) -> Result<(), String> {
    reader
        .read_exact(block)
        .map_err(|e| format!("压缩包损坏或格式不对：{e}"))
}

/// 丢掉 `count` 个字节（跳过不要的条目内容）。
fn skip_tar_bytes(reader: &mut impl std::io::Read, count: u64) -> Result<(), String> {
    let skipped = std::io::copy(
        &mut std::io::Read::take(reader, count),
        &mut std::io::sink(),
    )
    .map_err(|e| format!("读取压缩包失败：{e}"))?;
    if skipped != count {
        return Err("压缩包不完整：条目内容被截断".to_string());
    }
    Ok(())
}

/// 头部校验和：所有字节相加（校验和字段本身按 8 个空格算）要等于记录的值。
fn verify_tar_checksum(header: &[u8; TAR_BLOCK]) -> Result<(), String> {
    let recorded = tar_number(&header[148..156]).ok_or("压缩包损坏：校验和读不出来")?;
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum();
    if sum == recorded {
        Ok(())
    } else {
        Err("压缩包损坏：头部校验和不一致".to_string())
    }
}

/// tar 数字字段：八进制文本（以空格 / NUL 结尾），或者最高位置 1 的二进制大端（GNU 扩展，用于超大文件）。
fn tar_number(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        let mut value: u64 = u64::from(field[0] & 0x7f);
        for byte in &field[1..] {
            value = value.checked_mul(256)?.checked_add(u64::from(*byte))?;
        }
        return Some(value);
    }
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

/// ustar 头里的完整条目名：`prefix`（345..500）+ `/` + `name`（0..100）。
fn tar_entry_name(header: &[u8; TAR_BLOCK]) -> String {
    fn text(field: &[u8]) -> String {
        let end = field
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(field.len());
        String::from_utf8_lossy(&field[..end]).to_string()
    }
    let name = text(&header[0..100]);
    let is_ustar = header[257..262] == *b"ustar";
    let prefix = if is_ustar {
        text(&header[345..500])
    } else {
        String::new()
    };
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

/// pax 扩展头里的 `path` 记录（格式 `<长度> path=<值>\n`）。
fn pax_path(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.lines().find_map(|line| {
        let record = line.split_once(' ')?.1;
        record.strip_prefix("path=").map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_picks_x64_asset() {
        let body = serde_json::json!({
            "tag_name": "v1.19.0",
            "html_url": "https://github.com/anomalyco/opencode/releases/tag/v1.19.0",
            "assets": [
                {"name": "opencode-windows-x64-baseline.zip", "size": 62173954,
                 "browser_download_url": "https://example.invalid/baseline.zip"},
                {"name": "opencode-windows-arm64.zip", "size": 55000000,
                 "browser_download_url": "https://example.invalid/arm64.zip"},
                {"name": "opencode-windows-x64.zip", "size": 62173956,
                 "digest": format!("sha256:{}", "AB".repeat(32)),
                 "browser_download_url": "https://github.com/anomalyco/opencode/releases/download/v1.19.0/opencode-windows-x64.zip"}
            ]
        });
        let info = parse_release(&body).unwrap();
        assert_eq!(info.version, "v1.19.0");
        assert_eq!(info.asset_name, "opencode-windows-x64.zip");
        // 不能挑成 baseline。
        assert!(info.download_url.ends_with("x64.zip"));
        assert_eq!(info.size, 62173956);
        assert_eq!(info.sha256, "ab".repeat(32), "摘要统一成小写");
    }

    fn release_with(asset: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"tag_name": "v1.19.0", "assets": [asset]})
    }

    #[test]
    fn a_release_without_a_usable_digest_or_github_url_is_refused() {
        // R-10 M8：没有摘要就没法核对下载内容，不能下了直接运行。
        let url =
            "https://github.com/anomalyco/opencode/releases/download/v1/opencode-windows-x64.zip";
        let no_digest = release_with(serde_json::json!({
            "name": WINDOWS_X64_ASSET, "size": 10, "browser_download_url": url
        }));
        assert!(parse_release(&no_digest).unwrap_err().contains("SHA-256"));
        let bad_digest = release_with(serde_json::json!({
            "name": WINDOWS_X64_ASSET, "size": 10, "digest": "sha1:abcd", "browser_download_url": url
        }));
        assert!(parse_release(&bad_digest).is_err());
        let elsewhere = release_with(serde_json::json!({
            "name": WINDOWS_X64_ASSET, "size": 10, "digest": format!("sha256:{}", "0".repeat(64)),
            "browser_download_url": "http://example.invalid/opencode-windows-x64.zip"
        }));
        assert!(parse_release(&elsewhere).unwrap_err().contains("GitHub"));
        let huge = release_with(serde_json::json!({
            "name": WINDOWS_X64_ASSET, "size": MAX_DOWNLOAD_BYTES + 1,
            "digest": format!("sha256:{}", "0".repeat(64)), "browser_download_url": url
        }));
        assert!(parse_release(&huge).is_err());
    }

    #[test]
    fn a_download_that_does_not_match_the_digest_is_deleted() {
        use sha2::{Digest, Sha256};

        let tmp = crate::test_util::TestDir::new("download-verify");
        let path = tmp.join("opencode-download.zip");
        let content = b"pretend this is the release zip";
        let good: String = Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        std::fs::write(&path, content).unwrap();
        verify_download(&path, content.len() as u64, &good).expect("一致就通过");
        assert!(path.exists());

        // 内容被改过：摘要不一致，文件被删掉。
        let tampered = b"tampered content";
        std::fs::write(&path, tampered).unwrap();
        let err = verify_download(&path, tampered.len() as u64, &good).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!path.exists(), "不一致的文件要删掉，不能留给后面解压运行");

        // 大小不对也一样。
        std::fs::write(&path, content).unwrap();
        assert!(verify_download(&path, 1, &good).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn parse_release_rejects_missing_assets() {
        let body = serde_json::json!({"tag_name": "v1.0.0", "assets": []});
        let err = parse_release(&body).unwrap_err();
        assert!(err.contains("opencode-windows-x64.zip"));
    }

    /// 伪造一个 zip（里面是随便什么内容的 opencode.exe）验证解压逻辑，不连网。
    #[test]
    fn extracts_exe_from_local_zip() {
        let tmp = crate::test_util::TestDir::new("download-test");
        let zip_path = tmp.join("fake.zip");

        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            // 真实包里 exe 在根目录，前面可能还有别的文件。
            writer.start_file("README.md", options).unwrap();
            std::io::Write::write_all(&mut writer, b"docs").unwrap();
            writer.start_file("opencode.exe", options).unwrap();
            std::io::Write::write_all(&mut writer, b"FAKE-EXE-CONTENT").unwrap();
            writer.finish().unwrap();
        }

        let out_dir = tmp.join("out");
        let exe = extract_exe(&zip_path, &out_dir).unwrap();
        assert_eq!(exe, out_dir.join("opencode.exe"));
        let bytes = std::fs::read(&exe).unwrap();
        assert_eq!(bytes, b"FAKE-EXE-CONTENT");
    }

    #[test]
    fn an_exe_larger_than_the_limit_is_not_left_on_disk() {
        // R7 K02：压缩包大小有上限，但解压后的大小要另外管。
        let tmp = crate::test_util::TestDir::new("download-limit");
        let zip_path = tmp.join("big.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("opencode.exe", options).unwrap();
            std::io::Write::write_all(&mut writer, &[0u8; 4096]).unwrap();
            writer.finish().unwrap();
        }
        let out_dir = tmp.join("out");
        let err = extract_exe_limited(&zip_path, &out_dir, 1000).expect_err("超过上限要拒绝");
        assert!(err.contains("上限"), "{err}");
        assert!(!out_dir.join("opencode.exe").exists(), "超限的文件不能留下");
        // 刚好在上限内照常解压。
        extract_exe_limited(&zip_path, &out_dir, 4096).expect("上限内可以解压");
    }

    #[test]
    fn redirects_only_go_to_github_over_https() {
        let ok = |url: &str| redirect_allowed(&reqwest::Url::parse(url).unwrap());
        assert!(ok(
            "https://github.com/anomalyco/opencode/releases/download/v1/x.zip"
        ));
        assert!(ok(
            "https://objects.githubusercontent.com/github-production-release-asset/x"
        ));
        assert!(ok("https://release-assets.githubusercontent.com/x"));
        assert!(ok("https://api.github.com/repos/x"));
        assert!(
            !ok("http://objects.githubusercontent.com/x"),
            "不能降成 http"
        );
        assert!(!ok("https://evil.example/x"));
        assert!(
            !ok("https://github.com.evil.example/x"),
            "后缀要按域名边界比"
        );
        assert!(!ok("https://notgithub.com/x"));
    }

    // ---- Linux 的 .tar.gz 资产 ----
    //
    // 解 tar 的代码是平台无关的纯 Rust，所以这些测试在 Windows 上也跑；
    // 只有"解出来带可执行权限"和"本平台选哪个资产"随平台走。

    /// 手写一个 ustar 头（不依赖 tar crate）。
    fn tar_header(name: &str, size: usize, kind: u8, link_target: &str) -> [u8; 512] {
        let mut header = [0u8; 512];
        let put = |header: &mut [u8; 512], at: usize, text: &str| {
            header[at..at + text.len()].copy_from_slice(text.as_bytes());
        };
        put(&mut header, 0, name);
        put(&mut header, 100, "0000755\0");
        put(&mut header, 108, "0000000\0");
        put(&mut header, 116, "0000000\0");
        put(&mut header, 124, &format!("{size:011o}\0"));
        put(&mut header, 136, "00000000000\0");
        put(&mut header, 148, "        ");
        header[156] = kind;
        put(&mut header, 157, link_target);
        put(&mut header, 257, "ustar\0");
        put(&mut header, 263, "00");
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        put(&mut header, 148, &format!("{sum:06o}\0 "));
        header
    }

    /// 一个完整条目：头 + 内容（补齐到 512 的整数倍）。
    fn tar_entry(name: &str, data: &[u8], kind: u8) -> Vec<u8> {
        let mut out = tar_header(name, data.len(), kind, "").to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    /// 把若干条目拼成 `.tar.gz` 文件（带结尾的两个全零块）。
    fn write_tar_gz(path: &Path, entries: &[Vec<u8>]) {
        use std::io::Write;
        let mut tar: Vec<u8> = entries.concat();
        tar.extend_from_slice(&[0u8; 1024]);
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn extracts_the_binary_from_a_tar_gz_and_makes_it_executable() {
        let tmp = crate::test_util::TestDir::new("download-targz");
        let archive = tmp.join("opencode-linux-x64.tar.gz");
        // 真实包的根目录就是一个 `opencode`；前面放点别的文件、带 `./` 前缀，确认能挑对。
        write_tar_gz(
            &archive,
            &[
                tar_entry("./README.md", b"docs", b'0'),
                tar_entry("./opencode", b"FAKE-ELF-CONTENT", b'0'),
            ],
        );
        let out_dir = tmp.join("out");
        let exe = extract_binary(&archive, &out_dir).expect("解压");
        assert_eq!(exe, out_dir.join("opencode"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"FAKE-ELF-CONTENT");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "解出来要能直接运行");
        }
        assert!(!out_dir.join("README.md").exists(), "只解出要的那一个文件");
    }

    /// 智能体正在运行时再下载一次：直接截断会得到 ETXTBSY，先写临时名再 rename 就能换上新版本，
    /// 正在跑的旧进程不受影响。
    #[cfg(target_os = "linux")]
    #[test]
    fn updating_a_running_binary_replaces_it_without_etxtbsy() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = crate::test_util::TestDir::new("download-targz-running");
        let out_dir = tmp.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let running = out_dir.join("opencode");
        std::fs::copy("/bin/sh", &running).unwrap();
        std::fs::set_permissions(&running, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = std::process::Command::new(&running)
            .args(["-c", "sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // 前提：确实会 ETXTBSY（这个环境里直接写正在运行的文件会失败）。
        let direct = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&running);
        let busy = direct.is_err();

        let archive = tmp.join("opencode-linux-x64.tar.gz");
        write_tar_gz(&archive, &[tar_entry("./opencode", b"NEW-BINARY", b'0')]);
        let result = extract_tar_gz_binary(&archive, &out_dir, 1024);
        let _ = child.kill();
        let _ = child.wait();
        let exe = result.expect("运行中的程序也要能被替换");

        assert!(busy, "测试前提不成立：直接写没有得到 ETXTBSY");
        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW-BINARY");
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let leftovers: Vec<_> = std::fs::read_dir(&out_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "opencode")
            .collect();
        assert!(leftovers.is_empty(), "不能留下临时文件：{leftovers:?}");
    }

    /// 解压失败时旧版本原样保留（不再被先删掉）。
    #[cfg(unix)]
    #[test]
    fn a_failed_extraction_keeps_the_previous_binary() {
        let tmp = crate::test_util::TestDir::new("download-targz-keep-old");
        let out_dir = tmp.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("opencode"), b"OLD").unwrap();
        // 压缩流被截掉一半 -> 解压中途失败。
        let archive = tmp.join("short.tar.gz");
        write_tar_gz(&archive, &[tar_entry("opencode", &[7u8; 4096], b'0')]);
        let truncated = tmp.join("truncated.tar.gz");
        let bytes = std::fs::read(&archive).unwrap();
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        assert!(extract_tar_gz_binary(&truncated, &out_dir, 1 << 20).is_err());
        assert_eq!(std::fs::read(out_dir.join("opencode")).unwrap(), b"OLD");
        assert_eq!(
            std::fs::read_dir(&out_dir).unwrap().count(),
            1,
            "临时文件要清掉"
        );
    }

    #[test]
    fn a_tar_entry_cannot_choose_where_the_file_lands() {
        let tmp = crate::test_util::TestDir::new("download-targz-evil");
        let archive = tmp.join("evil.tar.gz");
        write_tar_gz(&archive, &[tar_entry("../../outside/opencode", b"X", b'0')]);
        let out_dir = tmp.join("a").join("b").join("out");
        let exe = extract_tar_gz_binary(&archive, &out_dir, 1024).expect("按文件名解到固定位置");
        assert_eq!(exe, out_dir.join("opencode"));
        assert!(!tmp.join("a").join("outside").exists());
        assert!(!tmp.join("outside").exists());
    }

    #[test]
    fn links_and_directories_named_opencode_are_not_extracted() {
        let tmp = crate::test_util::TestDir::new("download-targz-link");
        let archive = tmp.join("link.tar.gz");
        let link = tar_header("opencode", 0, b'2', "/etc/passwd").to_vec();
        write_tar_gz(&archive, &[link, tar_entry("opencode/", b"", b'5')]);
        let err = extract_tar_gz_binary(&archive, &tmp.join("out"), 1024).unwrap_err();
        assert!(err.contains("没有 opencode"), "{err}");
        assert!(!tmp.join("out").join("opencode").exists());
    }

    #[test]
    fn a_tar_binary_larger_than_the_limit_is_not_left_on_disk() {
        let tmp = crate::test_util::TestDir::new("download-targz-limit");
        let archive = tmp.join("big.tar.gz");
        write_tar_gz(&archive, &[tar_entry("opencode", &[7u8; 4096], b'0')]);
        let out_dir = tmp.join("out");
        let err = extract_tar_gz_binary(&archive, &out_dir, 1000).expect_err("超过上限要拒绝");
        assert!(err.contains("上限"), "{err}");
        assert!(!out_dir.join("opencode").exists(), "超限的文件不能留下");
        extract_tar_gz_binary(&archive, &out_dir, 4096).expect("上限内可以解压");
    }

    #[test]
    fn a_tar_without_the_binary_or_with_damage_is_refused() {
        let tmp = crate::test_util::TestDir::new("download-targz-bad");
        let out_dir = tmp.join("out");

        let no_binary = tmp.join("none.tar.gz");
        write_tar_gz(&no_binary, &[tar_entry("only-readme.txt", b"nope", b'0')]);
        let err = extract_tar_gz_binary(&no_binary, &out_dir, 1024).unwrap_err();
        assert!(err.contains("没有 opencode"), "{err}");

        // 头部校验和被改坏。
        let mut broken_header = tar_entry("opencode", b"data", b'0');
        broken_header[0] ^= 0x01;
        let bad_sum = tmp.join("badsum.tar.gz");
        write_tar_gz(&bad_sum, &[broken_header]);
        let err = extract_tar_gz_binary(&bad_sum, &out_dir, 1024).unwrap_err();
        assert!(err.contains("校验和"), "{err}");

        // 内容在中途被截断：不能留下半截可执行文件。
        let truncated = tmp.join("cut.tar.gz");
        {
            use std::io::Write;
            let mut tar = tar_header("opencode", 5000, b'0', "").to_vec();
            tar.extend_from_slice(&[1u8; 1000]);
            let file = std::fs::File::create(&truncated).unwrap();
            let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            encoder.write_all(&tar).unwrap();
            encoder.finish().unwrap();
        }
        let err = extract_tar_gz_binary(&truncated, &out_dir, 1 << 20).unwrap_err();
        assert!(err.contains("不完整"), "{err}");
        assert!(!out_dir.join("opencode").exists());

        // 根本不是 gzip。
        let junk = tmp.join("junk.tar.gz");
        std::fs::write(&junk, b"this is not gzip").unwrap();
        assert!(extract_tar_gz_binary(&junk, &out_dir, 1024).is_err());
    }

    #[test]
    fn long_names_from_gnu_and_pax_records_are_understood() {
        let tmp = crate::test_util::TestDir::new("download-targz-long");
        let long_dir = "d".repeat(150);
        let long_name = format!("{long_dir}/opencode");

        let gnu = tmp.join("gnu.tar.gz");
        let mut name_record = long_name.clone().into_bytes();
        name_record.push(0);
        write_tar_gz(
            &gnu,
            &[
                tar_entry("././@LongLink", &name_record, b'L'),
                // 真正的头里名字被截断成了前 99 个字符。
                tar_entry(&long_name[..99], b"GNU", b'0'),
            ],
        );
        let exe = extract_tar_gz_binary(&gnu, &tmp.join("gnu-out"), 1024).expect("GNU 长名字");
        assert_eq!(std::fs::read(exe).unwrap(), b"GNU");

        let pax = tmp.join("pax.tar.gz");
        let record = format!(" path={long_name}\n");
        let record = format!("{}{record}", record.len() + 3);
        write_tar_gz(
            &pax,
            &[
                tar_entry("PaxHeaders/opencode", record.as_bytes(), b'x'),
                tar_entry(&long_name[..99], b"PAX", b'0'),
            ],
        );
        let exe = extract_tar_gz_binary(&pax, &tmp.join("pax-out"), 1024).expect("pax 长名字");
        assert_eq!(std::fs::read(exe).unwrap(), b"PAX");
    }

    #[test]
    fn the_linux_asset_is_picked_by_name_with_the_same_digest_rules() {
        let url =
            "https://github.com/anomalyco/opencode/releases/download/v1/opencode-linux-x64.tar.gz";
        let body = serde_json::json!({
            "tag_name": "v1.19.5",
            "assets": [
                {"name": "opencode-linux-x64-musl.tar.gz", "size": 10,
                 "digest": format!("sha256:{}", "11".repeat(32)),
                 "browser_download_url": "https://github.com/x/musl.tar.gz"},
                {"name": "opencode-linux-x64-baseline.tar.gz", "size": 11,
                 "digest": format!("sha256:{}", "22".repeat(32)),
                 "browser_download_url": "https://github.com/x/baseline.tar.gz"},
                {"name": LINUX_X64_ASSET, "size": 79651896,
                 "digest": format!("sha256:{}", "AB".repeat(32)),
                 "browser_download_url": url}
            ]
        });
        let info = parse_release_for(&body, LINUX_X64_ASSET).unwrap();
        assert_eq!(info.asset_name, LINUX_X64_ASSET);
        assert_eq!(info.download_url, url);
        assert_eq!(info.size, 79651896);
        assert_eq!(info.sha256, "ab".repeat(32));
        // baseline 只在被点名时才选。
        let baseline = parse_release_for(&body, LINUX_X64_BASELINE_ASSET).unwrap();
        assert_eq!(baseline.size, 11);

        // 没摘要照样拒绝：Linux 包不能比 Windows 包松。
        let no_digest = serde_json::json!({"tag_name": "v1", "assets": [
            {"name": LINUX_X64_ASSET, "size": 10, "browser_download_url": url}]});
        assert!(parse_release_for(&no_digest, LINUX_X64_ASSET)
            .unwrap_err()
            .contains("SHA-256"));
        assert!(parse_release_for(&no_digest, "opencode-nope.tar.gz")
            .unwrap_err()
            .contains("opencode-nope.tar.gz"));
    }

    #[test]
    fn the_platform_picks_its_own_asset_and_file_names() {
        assert!(archive_file_name(WINDOWS_X64_ASSET).ends_with(".zip"));
        assert!(archive_file_name(LINUX_X64_ASSET).ends_with(".tar.gz"));
        assert!(archive_file_name(LINUX_X64_BASELINE_ASSET).ends_with(".tar.gz"));
        #[cfg(windows)]
        {
            assert_eq!(platform_asset_name().unwrap(), WINDOWS_X64_ASSET);
            assert_eq!(installed_binary_name(), "opencode.exe");
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let asset = platform_asset_name().unwrap();
            assert!(asset == LINUX_X64_ASSET || asset == LINUX_X64_BASELINE_ASSET);
            assert_eq!(installed_binary_name(), "opencode");
        }
    }

    #[test]
    fn extract_reports_missing_exe() {
        let tmp = crate::test_util::TestDir::new("download-test-noexe");
        let zip_path = tmp.join("fake.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("only-readme.txt", options).unwrap();
            std::io::Write::write_all(&mut writer, b"nope").unwrap();
            writer.finish().unwrap();
        }
        let err = extract_exe(&zip_path, &tmp.join("out")).unwrap_err();
        assert!(err.contains("没有 opencode.exe"));
    }
}
