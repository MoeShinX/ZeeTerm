//! 一键安装 VcXsrv：下载官方安装包 → 校验 SHA-256 → 静默安装（UAC 由用户确认）。
//!
//! 来源（2026-11-02 核实）：VcXsrv 官方发布仓库 `marchaesen/vcxsrv` 的 GitHub Release，
//! 版本 21.1.16.1（2025-03-10），文件 `vcxsrv-64.21.1.16.1.installer.exe`。GitHub 没有给这个
//! 资产提供 `digest`，所以下面的 SHA-256 和大小固定在代码里；换版本时必须同时更新 [`PINNED`]。
//! 校验不过的文件会被删除，绝不运行。
//!
//! 许可：VcXsrv 以 GPL-3.0 发布，其中的 X.Org 组件是 MIT/X11 系许可。ZeeTerm 不随包分发
//! 它的任何二进制：只在用户确认后从官方地址下载到用户机器上安装，并把它当独立程序启动。
//!
//! 安装包是 NSIS 做的（官方 `vcxsrv-64.nsi`：`RequestExecutionLevel admin`），支持 `/S` 静默；
//! 静默时按默认勾选安装（包含开始菜单和桌面快捷方式，没有命令行开关能单独关掉）。
//! 管理员权限只走系统标准的 UAC（`ShellExecuteEx` 的 `runas`），不用别的提权办法。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::ai::agent::download::{download_to, verify_download, DownloadError};

use super::detect::{find_vcxsrv, VcXsrvInstall};
use super::system::LocalSystem;

/// 固定的、已核实的安装包信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerSource {
    pub version: String,
    pub file_name: String,
    pub url: String,
    pub size: u64,
    /// 小写十六进制 SHA-256。
    pub sha256: String,
    /// 给用户看的来源页面。
    pub page_url: String,
}

pub const PINNED_VERSION: &str = "21.1.16.1";
pub const PINNED_FILE_NAME: &str = "vcxsrv-64.21.1.16.1.installer.exe";
pub const PINNED_URL: &str =
    "https://github.com/marchaesen/vcxsrv/releases/download/21.1.16.1/vcxsrv-64.21.1.16.1.installer.exe";
pub const PINNED_PAGE: &str = "https://github.com/marchaesen/vcxsrv/releases/tag/21.1.16.1";
pub const PINNED_SIZE: u64 = 42_993_397;
/// 来自 winget-pkgs 的 `marha.VcXsrv` 21.1.16.1 安装清单（InstallerSha256）。
pub const PINNED_SHA256: &str = "df7fed8f49665d0592528ab6be9d07111ea73c6848283d128b77690e05b8f90b";

/// 静默安装参数（NSIS）。
pub const SILENT_ARGS: &[&str] = &["/S"];

/// 官方的、已固定版本的安装包。
pub fn pinned_source() -> InstallerSource {
    InstallerSource {
        version: PINNED_VERSION.to_owned(),
        file_name: PINNED_FILE_NAME.to_owned(),
        url: PINNED_URL.to_owned(),
        size: PINNED_SIZE,
        sha256: PINNED_SHA256.to_owned(),
        page_url: PINNED_PAGE.to_owned(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("用户取消")]
    Cancelled,
    #[error("{0}")]
    Download(String),
    #[error("{0}")]
    Verify(String),
    #[error("你没有在系统弹出的权限确认（UAC）里点「是」，没有安装")]
    UacDenied,
    #[error("没能运行安装程序：{0}")]
    Launch(String),
    #[error("安装程序失败，退出码 {0}")]
    InstallerFailed(i32),
    #[error("安装程序已结束，但没有找到 VcXsrv，请到发布页手动安装")]
    NotFoundAfterInstall,
}

/// 以管理员身份运行安装程序并等它结束（阻塞）。真实实现弹 UAC；测试换成假的。
pub trait InstallerRunner: Send + Sync {
    /// 返回安装程序的退出码。
    fn run_elevated(&self, installer: &Path, args: &[&str]) -> Result<i32, InstallError>;
}

/// 下载安装包到 `work_dir` 并校验。校验不过文件已被删除。
pub async fn download_installer(
    client: &reqwest::Client,
    source: &InstallerSource,
    work_dir: &Path,
    on_progress: impl FnMut(u64, u64) + Send,
    cancel: &AtomicBool,
) -> Result<PathBuf, InstallError> {
    std::fs::create_dir_all(work_dir)
        .map_err(|err| InstallError::Download(format!("创建下载目录失败：{err}")))?;
    let dest = work_dir.join(&source.file_name);
    download_to(client, &source.url, &dest, source.size, on_progress, cancel)
        .await
        .map_err(|err| {
            let _ = std::fs::remove_file(&dest);
            match err {
                DownloadError::Cancelled => InstallError::Cancelled,
                DownloadError::Failed(message) => InstallError::Download(message),
            }
        })?;
    verify_download(&dest, source.size, &source.sha256).map_err(InstallError::Verify)?;
    Ok(dest)
}

/// 完整的一次安装：下载 → 校验 → 静默安装 → 重新检测。成功返回找到的安装。
///
/// 安装包文件无论成败都会删除。
pub async fn install_vcxsrv(
    client: &reqwest::Client,
    source: &InstallerSource,
    work_dir: &Path,
    runner: Arc<dyn InstallerRunner>,
    sys: Arc<dyn LocalSystem>,
    on_progress: impl FnMut(u64, u64) + Send,
    cancel: &AtomicBool,
) -> Result<VcXsrvInstall, InstallError> {
    let installer = download_installer(client, source, work_dir, on_progress, cancel).await?;
    let run_path = installer.clone();
    let result = tokio::task::spawn_blocking(move || runner.run_elevated(&run_path, SILENT_ARGS))
        .await
        .map_err(|err| InstallError::Launch(err.to_string()))
        .and_then(|inner| inner);
    let _ = std::fs::remove_file(&installer);

    match result? {
        0 => {}
        code => return Err(InstallError::InstallerFailed(code)),
    }
    let sys_for_find = Arc::clone(&sys);
    tokio::task::spawn_blocking(move || find_vcxsrv(sys_for_find.as_ref()))
        .await
        .ok()
        .flatten()
        .ok_or(InstallError::NotFoundAfterInstall)
}

/// 真实的提权运行器：`ShellExecuteExW` + `runas`，系统弹标准 UAC。
pub struct UacRunner;

#[cfg(windows)]
impl InstallerRunner for UacRunner {
    fn run_elevated(&self, installer: &Path, args: &[&str]) -> Result<i32, InstallError> {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, WaitForSingleObject, INFINITE,
        };
        use windows::Win32::UI::Shell::{
            ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        // 用户在 UAC 里点「否」时系统返回的错误码。
        const ERROR_CANCELLED: i32 = 1223;

        let file = HSTRING::from(installer.as_os_str());
        let parameters = HSTRING::from(args.join(" "));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: w!("runas"),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY：info 的字符串指针指向上面的 HSTRING，调用期间一直存活。
        if let Err(err) = unsafe { ShellExecuteExW(&mut info) } {
            return Err(if err.code().0 & 0xFFFF == ERROR_CANCELLED {
                InstallError::UacDenied
            } else {
                InstallError::Launch(err.message())
            });
        }
        let process = info.hProcess;
        if process.is_invalid() {
            return Err(InstallError::Launch("系统没有返回安装进程".to_owned()));
        }
        // SAFETY：process 是上面取得的有效进程句柄，用完关闭。
        unsafe {
            let waited = WaitForSingleObject(process, INFINITE);
            let mut code = 0u32;
            let got = GetExitCodeProcess(process, &mut code);
            let _ = CloseHandle(process);
            if waited != WAIT_OBJECT_0 || got.is_err() {
                return Err(InstallError::Launch("等待安装程序结束失败".to_owned()));
            }
            Ok(code as i32)
        }
    }
}

#[cfg(not(windows))]
impl InstallerRunner for UacRunner {
    fn run_elevated(&self, _installer: &Path, _args: &[&str]) -> Result<i32, InstallError> {
        Err(InstallError::Launch("只支持在 Windows 上安装".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::super::detect::fake::FakeSystem;
    use super::super::system::RegistryInstall;
    use super::*;
    use crate::test_util::TestDir;

    fn hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 本地桩：每个连接回答同一个固定响应。
    async fn serve(body: Vec<u8>, slow: bool) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("绑定");
        let port = listener.local_addr().expect("端口").port();
        tokio::spawn(async move {
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
        format!("http://127.0.0.1:{port}/installer.exe")
    }

    fn source_for(url: String, body: &[u8]) -> InstallerSource {
        InstallerSource {
            version: "0.0.0-test".into(),
            file_name: "stub-installer.exe".into(),
            url,
            size: body.len() as u64,
            sha256: hex(body),
            page_url: String::new(),
        }
    }

    struct FakeRunner {
        code: Result<i32, &'static str>,
        calls: Mutex<Vec<(PathBuf, Vec<String>, bool)>>,
        /// 运行时让 VcXsrv "出现"在这个路径（模拟安装成功）。
        install_into: Option<(FakeSystem, PathBuf)>,
    }

    impl InstallerRunner for FakeRunner {
        fn run_elevated(&self, installer: &Path, args: &[&str]) -> Result<i32, InstallError> {
            self.calls.lock().unwrap().push((
                installer.to_path_buf(),
                args.iter().map(|a| (*a).to_owned()).collect(),
                installer.exists(),
            ));
            if let Some((sys, exe)) = &self.install_into {
                sys.with(|s| {
                    s.files.insert(exe.clone());
                });
            }
            match self.code {
                Ok(code) => Ok(code),
                Err("uac") => Err(InstallError::UacDenied),
                Err(other) => Err(InstallError::Launch(other.to_owned())),
            }
        }
    }

    fn client() -> reqwest::Client {
        crate::ai::agent::download::build_client(None).expect("客户端")
    }

    #[test]
    fn the_pinned_release_is_the_official_github_asset() {
        let source = pinned_source();
        assert!(source
            .url
            .starts_with("https://github.com/marchaesen/vcxsrv/releases/download/"));
        assert!(source.url.ends_with(&source.file_name));
        assert!(source.url.contains(&source.version));
        assert_eq!(source.sha256.len(), 64);
        assert!(source.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(source.sha256, source.sha256.to_ascii_lowercase());
        assert!(source.size > 1_000_000);
        assert_eq!(SILENT_ARGS, &["/S"]);
    }

    #[tokio::test]
    async fn a_verified_download_runs_the_installer_silently_and_cleans_up() {
        let body = b"pretend installer".to_vec();
        let url = serve(body.clone(), false).await;
        let work = TestDir::new("x11-install-ok");
        let exe = PathBuf::from(r"C:\Program Files\VcXsrv\vcxsrv.exe");
        let sys = FakeSystem::default();
        let runner = Arc::new(FakeRunner {
            code: Ok(0),
            calls: Mutex::new(Vec::new()),
            install_into: Some((sys.clone(), exe.clone())),
        });
        sys.with(|s| {
            s.registry.push(RegistryInstall {
                dir: PathBuf::from(r"C:\Program Files\VcXsrv"),
                version: Some("0.0.0-test".into()),
            })
        });
        let progress = Mutex::new(Vec::new());
        let installed = install_vcxsrv(
            &client(),
            &source_for(url, &body),
            &work,
            runner.clone(),
            Arc::new(sys),
            |done, total| progress.lock().unwrap().push((done, total)),
            &AtomicBool::new(false),
        )
        .await
        .expect("安装成功");

        assert_eq!(installed.exe, exe);
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec!["/S".to_owned()]);
        assert!(calls[0].2, "运行时安装包在盘上");
        assert!(!calls[0].0.exists(), "运行完删掉安装包");
        assert!(!progress.lock().unwrap().is_empty(), "有进度回调");
    }

    #[tokio::test]
    async fn a_hash_mismatch_deletes_the_file_and_never_runs_it() {
        let body = b"tampered installer".to_vec();
        let url = serve(body.clone(), false).await;
        let work = TestDir::new("x11-install-bad-hash");
        let mut source = source_for(url, &body);
        source.sha256 = hex(b"the real installer");
        let runner = Arc::new(FakeRunner {
            code: Ok(0),
            calls: Mutex::new(Vec::new()),
            install_into: None,
        });
        let err = install_vcxsrv(
            &client(),
            &source,
            &work,
            runner.clone(),
            Arc::new(FakeSystem::default()),
            |_, _| {},
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InstallError::Verify(_)), "{err}");
        assert!(err.to_string().contains("SHA-256"), "{err}");
        assert!(runner.calls.lock().unwrap().is_empty(), "校验不过绝不运行");
        assert!(!work.join(&source.file_name).exists(), "文件已删除");
    }

    #[tokio::test]
    async fn a_download_larger_than_the_pinned_size_is_refused() {
        let body = vec![7u8; 4096];
        let url = serve(body.clone(), false).await;
        let work = TestDir::new("x11-install-big");
        let mut source = source_for(url, &body);
        source.size = 1024;
        let err = download_installer(
            &client(),
            &source,
            &work,
            |_, _| {},
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InstallError::Download(_)), "{err}");
        assert!(!work.join(&source.file_name).exists());
    }

    #[tokio::test]
    async fn cancelling_mid_download_stops_and_removes_the_partial_file() {
        let body = vec![1u8; 3 * 1024 * 1024];
        let url = serve(body.clone(), true).await;
        let work = TestDir::new("x11-install-cancel");
        let source = source_for(url, &body);
        let cancel = AtomicBool::new(false);
        let cancel_ref = &cancel;
        let err = download_installer(
            &client(),
            &source,
            &work,
            move |done, _| {
                if done > 0 {
                    cancel_ref.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InstallError::Cancelled), "{err}");
        assert!(!work.join(&source.file_name).exists());
    }

    #[tokio::test]
    async fn uac_refusal_and_installer_failures_are_distinguished() {
        let body = b"stub".to_vec();
        let url = serve(body.clone(), false).await;
        for (code, expect) in [
            (Err("uac"), "UAC"),
            (Ok(2), "退出码 2"),
            (Err("boom"), "boom"),
        ] {
            let work = TestDir::new("x11-install-fail");
            let runner = Arc::new(FakeRunner {
                code,
                calls: Mutex::new(Vec::new()),
                install_into: None,
            });
            let err = install_vcxsrv(
                &client(),
                &source_for(url.clone(), &body),
                &work,
                runner.clone(),
                Arc::new(FakeSystem::default()),
                |_, _| {},
                &AtomicBool::new(false),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains(expect), "{err}");
            let calls = runner.calls.lock().unwrap();
            assert!(!calls[0].0.exists(), "失败也要删安装包");
        }
    }

    #[tokio::test]
    async fn success_without_a_detectable_install_is_reported() {
        let body = b"stub".to_vec();
        let url = serve(body.clone(), false).await;
        let work = TestDir::new("x11-install-ghost");
        let runner = Arc::new(FakeRunner {
            code: Ok(0),
            calls: Mutex::new(Vec::new()),
            install_into: None,
        });
        let err = install_vcxsrv(
            &client(),
            &source_for(url, &body),
            &work,
            runner,
            Arc::new(FakeSystem::default()),
            |_, _| {},
            &AtomicBool::new(false),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InstallError::NotFoundAfterInstall), "{err}");
    }
}
