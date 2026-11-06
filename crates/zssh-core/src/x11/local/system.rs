//! 本机环境的抽象：端口探测、环境变量、文件、注册表、起进程。
//!
//! 检测和启动逻辑只通过 [`LocalSystem`] 看本机，所以单测可以用内存里的假环境，
//! 不读真实注册表、不探真实端口、不启动真实的 X 服务器。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 注册表里记着的一处 VcXsrv 安装。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryInstall {
    pub dir: PathBuf,
    pub version: Option<String>,
}

/// ZeeTerm 自己拉起的进程。
pub trait OwnedProcess: Send {
    /// 结束进程（已经退出就什么都不做）。
    fn kill(&mut self);
    /// 进程是不是已经退出了。
    fn has_exited(&mut self) -> bool;
}

pub trait LocalSystem: Send + Sync {
    /// 回环地址上这个 TCP 端口有没有程序在监听（连得上就算）。
    fn tcp_listening(&self, port: u16) -> bool;
    fn env_var(&self, name: &str) -> Option<String>;
    fn path_is_file(&self, path: &Path) -> bool;
    /// 注册表（安装目录项、卸载项）里记着的 VcXsrv 安装位置。读不到就是空。
    fn registry_installs(&self) -> Vec<RegistryInstall>;
    /// Windows 8.3 短路径（路径含非 ASCII 字符时给只认 ANSI 参数的程序用）。拿不到是 `None`。
    fn short_path(&self, path: &Path) -> Option<PathBuf>;
    fn spawn(&self, exe: &Path, args: &[String]) -> Result<Box<dyn OwnedProcess>, String>;
    fn sleep(&self, duration: Duration);
}

/// 真实的本机环境。
pub struct RealSystem;

struct ChildProcess(std::process::Child);

impl OwnedProcess for ChildProcess {
    fn kill(&mut self) {
        let _ = self.0.kill();
        // 回收子进程，避免留下僵尸句柄。
        let _ = self.0.wait();
    }

    fn has_exited(&mut self) -> bool {
        !matches!(self.0.try_wait(), Ok(None))
    }
}

impl LocalSystem for RealSystem {
    fn tcp_listening(&self, port: u16) -> bool {
        use std::net::{SocketAddr, TcpStream};
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        // 回环上连不上会立刻被拒绝；有防火墙丢包时才会走到超时，所以时限很短。
        TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }

    fn path_is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn registry_installs(&self) -> Vec<RegistryInstall> {
        #[cfg(windows)]
        {
            registry::installs()
        }
        #[cfg(not(windows))]
        {
            Vec::new()
        }
    }

    fn short_path(&self, path: &Path) -> Option<PathBuf> {
        #[cfg(windows)]
        {
            registry::short_path(path)
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            None
        }
    }

    fn spawn(&self, exe: &Path, args: &[String]) -> Result<Box<dyn OwnedProcess>, String> {
        let child = std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|err| format!("启动 {} 失败：{err}", exe.display()))?;
        Ok(Box::new(ChildProcess(child)))
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// 从注册表里记的命令行（`DisplayIcon`、`UninstallString`，可能带引号和 `,0` 图标序号）
/// 取出程序所在目录。
pub fn dir_from_registry_command(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    let path_text = if let Some(rest) = value.strip_prefix('"') {
        rest.split('"').next()?
    } else {
        // 无引号的写法：`C:\Dir\vcxsrv.exe,0`。去掉图标序号。
        let lower = value.to_ascii_lowercase();
        match lower.find(".exe") {
            Some(end) => &value[..end + 4],
            None => value,
        }
    };
    let path_text = path_text.split(',').next().unwrap_or(path_text).trim();
    Path::new(path_text)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

#[cfg(windows)]
mod registry {
    use std::path::{Path, PathBuf};

    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS,
        RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY,
    };

    use super::{dir_from_registry_command, RegistryInstall};

    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\VcXsrv";
    const INSTALL_KEY: &str = r"SOFTWARE\VcXsrv";

    fn read_string(root: HKEY, key: &str, name: &str, view: REG_ROUTINE_FLAGS) -> Option<String> {
        let key = HSTRING::from(key);
        let name = HSTRING::from(name);
        let mut size = 0u32;
        // SAFETY：第一次只取长度，不给缓冲。
        let status = unsafe {
            RegGetValueW(
                root,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | view,
                None,
                None,
                Some(&mut size),
            )
        };
        if status.is_err() || size < 2 {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY：缓冲按上一步返回的字节数分配，大小如实传入。
        let status = unsafe {
            RegGetValueW(
                root,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | view,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status.is_err() {
            return None;
        }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..end]);
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn installs() -> Vec<RegistryInstall> {
        let mut found: Vec<RegistryInstall> = Vec::new();
        let mut add = |dir: Option<PathBuf>, version: Option<String>| {
            if let Some(dir) = dir {
                if !found.iter().any(|known| known.dir == dir) {
                    found.push(RegistryInstall { dir, version });
                }
            }
        };
        for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            for view in [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY] {
                let version = read_string(root, UNINSTALL_KEY, "DisplayVersion", view);
                // 官方安装包写的安装目录项（64 位叫 Install_Dir_64，老的 32 位叫 Install_Dir）。
                for name in ["Install_Dir_64", "Install_Dir"] {
                    add(
                        read_string(root, INSTALL_KEY, name, view).map(PathBuf::from),
                        version.clone(),
                    );
                }
                // 卸载项：DisplayIcon 指向 vcxsrv.exe，UninstallString 指向 uninstall.exe。
                for name in ["DisplayIcon", "UninstallString"] {
                    add(
                        read_string(root, UNINSTALL_KEY, name, view)
                            .and_then(|value| dir_from_registry_command(&value)),
                        version.clone(),
                    );
                }
            }
        }
        found
    }

    pub fn short_path(path: &Path) -> Option<PathBuf> {
        let wide = HSTRING::from(path.as_os_str());
        // SAFETY：第一次传空缓冲取需要的长度（含结尾 0）。
        let needed = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), None) };
        if needed == 0 {
            return None;
        }
        let mut buffer = vec![0u16; needed as usize];
        // SAFETY：缓冲长度如实传入。
        let written = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buffer)) };
        if written == 0 || written as usize >= buffer.len() {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(
            &buffer[..written as usize],
        )))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn registry_commands_are_reduced_to_their_directory() {
        let dir = Some(PathBuf::from(r"C:\Tools\VcXsrv"));
        assert_eq!(
            dir_from_registry_command(r"C:\Tools\VcXsrv\vcxsrv.exe"),
            dir
        );
        assert_eq!(
            dir_from_registry_command(r"C:\Tools\VcXsrv\vcxsrv.exe,0"),
            dir
        );
        assert_eq!(
            dir_from_registry_command(r#""C:\Tools\VcXsrv\uninstall.exe""#),
            dir
        );
        assert_eq!(
            dir_from_registry_command(r#""C:\Tools\VcXsrv\uninstall.exe" /S"#),
            dir
        );
        assert_eq!(dir_from_registry_command(""), None);
        assert_eq!(dir_from_registry_command("vcxsrv.exe"), None);
    }

    #[test]
    fn paths_with_spaces_and_non_ascii_survive_the_reduction() {
        assert_eq!(
            dir_from_registry_command(r#""D:\Program Files\VcXsrv 新\vcxsrv.exe""#),
            Some(PathBuf::from(r"D:\Program Files\VcXsrv 新"))
        );
    }
}
