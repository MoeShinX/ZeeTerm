//! NSIS 安装动作的离线计划：只做路径/参数校验和参数拼装，不启动安装器。
//!
//! 与 Tauri updater 2.14.0 的 Windows 分支保持同一组参数：
//! `install_mode` 参数 + `/UPDATE`，需要重启时再加 `/R` 和 `/ARGS <原参数>`。
//! 真正调用 `ShellExecuteW`、退出进程放到 G12；这里先把"能执行什么"钉死：
//! 安装器必须位于更新缓存目录内、必须是存在的 `.exe`，版本不能为空，
//! 并且文件内容的 SHA-256 必须等于验签时记下的那一个。
//!
//! 验签和启动之间可能隔很久（"退出后安装"），缓存目录又在用户目录下，所以计划里带上
//! 验签通过的字节的哈希，启动前用 [`verify_plan_file`] 再算一次。

use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub installer: PathBuf,
    /// 更新缓存根目录；安装器必须在这里面，防止执行任意路径。
    pub cache_root: PathBuf,
    pub version: String,
    pub restart: bool,
    /// 当前进程的参数（不含 exe），重启时原样传回去。
    pub current_exe_args: Vec<String>,
    /// 验签通过的那份字节的 SHA-256（十六进制）；文件内容不是它就不生成计划。
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub restart: bool,
    pub version: String,
    /// 验签通过的字节的 SHA-256（小写十六进制），启动前要复验。
    pub sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InstallError {
    EmptyVersion,
    MissingInstaller,
    NotAFile,
    NotExe,
    OutsideCache,
    Path(String),
    /// 读不到文件，没法确认内容。
    Unreadable(String),
    /// 文件内容和验签时记下的哈希不一致。
    HashMismatch,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyVersion => write!(f, "安装包版本为空"),
            Self::MissingInstaller => write!(f, "安装包不存在"),
            Self::NotAFile => write!(f, "安装包不是文件"),
            Self::NotExe => write!(f, "安装包不是 .exe"),
            Self::OutsideCache => write!(f, "安装包不在更新缓存目录内，拒绝执行"),
            Self::Path(err) => write!(f, "安装包路径无法解析：{err}"),
            Self::Unreadable(err) => write!(f, "读取安装包失败：{err}"),
            Self::HashMismatch => write!(f, "安装包内容与验签时不一致"),
        }
    }
}

impl std::error::Error for InstallError {}

pub fn plan_install(request: &InstallRequest) -> Result<InstallPlan, InstallError> {
    if request.version.trim().is_empty() {
        return Err(InstallError::EmptyVersion);
    }
    if !request.installer.exists() {
        return Err(InstallError::MissingInstaller);
    }
    if !request.installer.is_file() {
        return Err(InstallError::NotAFile);
    }
    let is_exe = request
        .installer
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    if !is_exe {
        return Err(InstallError::NotExe);
    }

    let installer = request
        .installer
        .canonicalize()
        .map_err(|err| InstallError::Path(err.to_string()))?;
    let cache_root = request
        .cache_root
        .canonicalize()
        .map_err(|err| InstallError::Path(err.to_string()))?;
    if !installer.starts_with(&cache_root) {
        return Err(InstallError::OutsideCache);
    }

    // 内容必须还是验签时的那份：缓存目录可被同用户的其他进程写入。
    check_file_hash(&installer, &request.sha256)?;

    // 与 Tauri 的 passive 安装模式一致：/P 走 passive，/UPDATE 表示升级。
    let mut args = vec!["/P".to_owned(), "/UPDATE".to_owned()];
    if request.restart {
        args.push("/R".to_owned());
        args.push("/ARGS".to_owned());
        args.extend(request.current_exe_args.iter().cloned());
    }

    Ok(InstallPlan {
        program: installer,
        args,
        restart: request.restart,
        version: request.version.trim().to_owned(),
        sha256: request.sha256.trim().to_ascii_lowercase(),
    })
}

/// 字节的 SHA-256，小写十六进制。
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// 启动安装器前复验：计划里的文件内容必须还是验签时的那份。
///
/// 不一致或读不到都拒绝，调用方不能启动，只记日志 / 提示。
pub fn verify_plan_file(plan: &InstallPlan) -> Result<(), InstallError> {
    check_file_hash(&plan.program, &plan.sha256)
}

fn check_file_hash(path: &Path, expected: &str) -> Result<(), InstallError> {
    let actual = file_sha256_hex(path).map_err(|err| InstallError::Unreadable(err.to_string()))?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(InstallError::HashMismatch)
    }
}

/// 分块读，安装包可能有几十 MB，不一次读进内存。
fn file_sha256_hex(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// 只用于计划展示：真实启动由 G12 的 `ShellExecuteW` 负责。
pub fn command_line(plan: &InstallPlan) -> String {
    let mut line = format!("\"{}\"", plan.program.display());
    for arg in &plan.args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_cache(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("zssh-update-plan-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("创建临时缓存目录");
        root
    }

    fn request(root: &Path, restart: bool) -> InstallRequest {
        let installer = root.join("Z-SSH_0.3.0_x64-setup.exe");
        fs::write(&installer, b"fake installer").expect("写安装包");
        InstallRequest {
            installer,
            cache_root: root.to_path_buf(),
            version: "0.3.0".to_owned(),
            restart,
            current_exe_args: vec!["--minimized".to_owned()],
            sha256: sha256_hex(b"fake installer"),
        }
    }

    #[test]
    fn passive_update_without_restart() {
        let root = temp_cache("no-restart");
        let plan = plan_install(&request(&root, false)).expect("计划应通过");
        assert_eq!(plan.args, ["/P", "/UPDATE"]);
        assert!(!plan.restart);
        assert!(command_line(&plan).contains("setup.exe"));
    }

    #[test]
    fn restart_passes_current_arguments() {
        let root = temp_cache("restart");
        let plan = plan_install(&request(&root, true)).expect("计划应通过");
        assert_eq!(plan.args, ["/P", "/UPDATE", "/R", "/ARGS", "--minimized"]);
    }

    #[test]
    fn installer_outside_cache_is_rejected() {
        let root = temp_cache("outside");
        let other = temp_cache("outside-other");
        let mut req = request(&root, false);
        let outside = other.join("evil.exe");
        fs::write(&outside, b"evil").unwrap();
        req.installer = outside;
        assert_eq!(plan_install(&req), Err(InstallError::OutsideCache));
    }

    #[test]
    fn non_exe_is_rejected() {
        let root = temp_cache("non-exe");
        let mut req = request(&root, false);
        let zip = root.join("payload.zip");
        fs::write(&zip, b"zip").unwrap();
        req.installer = zip;
        assert_eq!(plan_install(&req), Err(InstallError::NotExe));
    }

    #[test]
    fn missing_and_empty_are_rejected() {
        let root = temp_cache("missing");
        let mut req = request(&root, false);
        req.installer = root.join("missing.exe");
        assert_eq!(plan_install(&req), Err(InstallError::MissingInstaller));

        let mut req = request(&root, false);
        req.version = "  ".to_owned();
        assert_eq!(plan_install(&req), Err(InstallError::EmptyVersion));
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn plan_carries_the_hash_and_rechecks_clean() {
        let root = temp_cache("hash-ok");
        let plan = plan_install(&request(&root, false)).expect("计划应通过");
        assert_eq!(plan.sha256, sha256_hex(b"fake installer"));
        assert_eq!(verify_plan_file(&plan), Ok(()));
    }

    #[test]
    fn plan_is_refused_when_the_file_differs_from_the_verified_bytes() {
        let root = temp_cache("hash-differs");
        let mut req = request(&root, false);
        req.sha256 = sha256_hex(b"some other bytes");
        assert_eq!(plan_install(&req), Err(InstallError::HashMismatch));
    }

    #[test]
    fn recheck_rejects_a_file_swapped_after_planning() {
        let root = temp_cache("hash-swapped");
        let req = request(&root, false);
        let plan = plan_install(&req).expect("计划应通过");
        fs::write(&req.installer, b"swapped after verification").unwrap();
        assert_eq!(verify_plan_file(&plan), Err(InstallError::HashMismatch));
        fs::remove_file(&req.installer).unwrap();
        assert!(matches!(
            verify_plan_file(&plan),
            Err(InstallError::Unreadable(_))
        ));
    }
}
