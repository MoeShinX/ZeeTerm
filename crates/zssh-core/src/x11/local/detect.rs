//! 检测本机 X 服务器：已安装的 VcXsrv、正在运行的 X 服务器。
//!
//! 安装位置不照某台机器写：先看注册表记的位置（安装目录项、卸载项），再看各种常见目录
//! （`Program Files` 的几个变体、用户目录下的 `Programs`），每个候选都要真有 `vcxsrv.exe` 才算。
//! 读不到的候选按"没有"处理，不报错。
//!
//! **只信任别的用户写不进去的位置**：这里找到的 exe 会被直接拉起来。系统盘根目录（`C:\VcXsrv`）
//! 不在其内——Windows 默认让任何已登录用户在盘根建子目录，并且对建出来的子目录有修改权限，
//! 多用户机器上别人可以预先放一个同名的 `vcxsrv.exe`，等本机用户一点连接就以他的身份运行。
//! 选“看所有者 / ACL”也不够（目录的所有者是本人，别人照样能改里面的文件），所以不自动认它；
//! 装在那里的人，安装程序写的注册表项（HKLM / HKCU，别的用户改不了）会把它带进候选。

use std::path::PathBuf;

use super::system::LocalSystem;

/// X 服务器监听的 TCP 端口范围：显示号 0～10。
pub const PROBE_FIRST_DISPLAY: u16 = 0;
pub const PROBE_LAST_DISPLAY: u16 = 10;

/// 一处找到的 VcXsrv 安装。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcXsrvInstall {
    pub exe: PathBuf,
    /// 注册表里记的版本；目录里找到的没有版本信息。
    pub version: Option<String>,
}

/// 候选安装目录（含注册表给的版本），按优先级排好、去重。
fn candidate_dirs(sys: &dyn LocalSystem) -> Vec<(PathBuf, Option<String>)> {
    let mut candidates: Vec<(PathBuf, Option<String>)> = sys
        .registry_installs()
        .into_iter()
        .map(|install| (install.dir, install.version))
        .collect();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(base) = sys.env_var(var) {
            candidates.push((PathBuf::from(base).join("VcXsrv"), None));
        }
    }
    // 系统盘根下的 `VcXsrv` 目录故意不在这里：见文件头的说明。
    if let Some(local) = sys.env_var("LOCALAPPDATA") {
        candidates.push((PathBuf::from(local).join("Programs").join("VcXsrv"), None));
    }

    let mut seen: Vec<String> = Vec::new();
    candidates.retain(|(dir, _)| {
        let key = dir.to_string_lossy().to_ascii_lowercase();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    candidates
}

/// 找已安装的 VcXsrv。
pub fn find_vcxsrv(sys: &dyn LocalSystem) -> Option<VcXsrvInstall> {
    candidate_dirs(sys).into_iter().find_map(|(dir, version)| {
        let exe = dir.join("vcxsrv.exe");
        sys.path_is_file(&exe)
            .then_some(VcXsrvInstall { exe, version })
    })
}

/// 正在运行的 X 服务器占着的显示号（探测回环上 6000～6010）。
pub fn running_displays(sys: &dyn LocalSystem) -> Vec<u16> {
    (PROBE_FIRST_DISPLAY..=PROBE_LAST_DISPLAY)
        .filter(|display| sys.tcp_listening(6000 + display))
        .collect()
}

#[cfg(test)]
pub(crate) mod fake {
    //! 单测用的内存假环境（也给 launch / mod 的测试用）。

    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::super::system::{LocalSystem, OwnedProcess, RegistryInstall};

    #[derive(Default)]
    pub struct FakeState {
        pub listening: HashSet<u16>,
        pub env: HashMap<String, String>,
        pub files: HashSet<PathBuf>,
        /// 能读出内容的文件（`read_file`）。
        pub contents: HashMap<PathBuf, Vec<u8>>,
        pub hostname: Option<String>,
        pub home: Option<PathBuf>,
        pub registry: Vec<RegistryInstall>,
        pub short_paths: HashMap<PathBuf, PathBuf>,
        pub spawned: Vec<(PathBuf, Vec<String>)>,
        pub killed: usize,
        /// 起进程后让这个端口开始监听（模拟 X 服务器起来了）；`None` = 起了但永远不监听。
        pub listen_after_spawn: Option<u16>,
        pub spawn_error: Option<String>,
        pub exit_immediately: bool,
        pub slept: Duration,
    }

    #[derive(Clone, Default)]
    pub struct FakeSystem(pub Arc<Mutex<FakeState>>);

    impl FakeSystem {
        pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }
    }

    struct FakeProcess {
        state: Arc<Mutex<FakeState>>,
        exited: bool,
    }

    impl OwnedProcess for FakeProcess {
        fn kill(&mut self) {
            self.state.lock().unwrap().killed += 1;
            self.exited = true;
        }

        fn has_exited(&mut self) -> bool {
            self.exited
        }
    }

    impl LocalSystem for FakeSystem {
        fn tcp_listening(&self, port: u16) -> bool {
            self.with(|s| s.listening.contains(&port))
        }

        fn env_var(&self, name: &str) -> Option<String> {
            self.with(|s| s.env.get(name).cloned())
        }

        fn path_is_file(&self, path: &Path) -> bool {
            self.with(|s| s.files.contains(path))
        }

        fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
            self.with(|s| s.contents.get(path).cloned())
        }

        fn hostname(&self) -> Option<String> {
            self.with(|s| s.hostname.clone())
        }

        fn home_dir(&self) -> Option<PathBuf> {
            self.with(|s| s.home.clone())
        }

        fn registry_installs(&self) -> Vec<RegistryInstall> {
            self.with(|s| s.registry.clone())
        }

        fn short_path(&self, path: &Path) -> Option<PathBuf> {
            self.with(|s| s.short_paths.get(path).cloned())
        }

        fn spawn(&self, exe: &Path, args: &[String]) -> Result<Box<dyn OwnedProcess>, String> {
            let exited = self.with(|s| {
                if let Some(err) = s.spawn_error.clone() {
                    return Err(err);
                }
                s.spawned.push((exe.to_path_buf(), args.to_vec()));
                if let Some(port) = s.listen_after_spawn {
                    s.listening.insert(port);
                }
                Ok(s.exit_immediately)
            })?;
            Ok(Box::new(FakeProcess {
                state: Arc::clone(&self.0),
                exited,
            }))
        }

        fn sleep(&self, duration: Duration) {
            self.with(|s| s.slept += duration);
        }
    }
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::fake::FakeSystem;
    use super::*;
    use crate::x11::local::system::RegistryInstall;

    #[test]
    fn nothing_installed_finds_nothing() {
        let sys = FakeSystem::default();
        assert_eq!(find_vcxsrv(&sys), None);
    }

    #[test]
    fn the_registry_location_wins_and_carries_its_version() {
        let sys = FakeSystem::default();
        sys.with(|s| {
            s.registry.push(RegistryInstall {
                dir: PathBuf::from(r"E:\Apps\VcXsrv"),
                version: Some("1.2.3".into()),
            });
            s.env
                .insert("ProgramFiles".into(), r"C:\Program Files".into());
            s.files.insert(PathBuf::from(r"E:\Apps\VcXsrv\vcxsrv.exe"));
            s.files
                .insert(PathBuf::from(r"C:\Program Files\VcXsrv\vcxsrv.exe"));
        });
        let found = find_vcxsrv(&sys).expect("找到");
        assert_eq!(found.exe, PathBuf::from(r"E:\Apps\VcXsrv\vcxsrv.exe"));
        assert_eq!(found.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn a_stale_registry_entry_falls_back_to_common_directories() {
        let sys = FakeSystem::default();
        sys.with(|s| {
            // 注册表指向一个已经被删掉的目录。
            s.registry.push(RegistryInstall {
                dir: PathBuf::from(r"E:\Gone\VcXsrv"),
                version: None,
            });
            s.env
                .insert("ProgramFiles".into(), r"C:\Program Files".into());
            s.files
                .insert(PathBuf::from(r"C:\Program Files\VcXsrv\vcxsrv.exe"));
        });
        let found = find_vcxsrv(&sys).expect("退回常见目录");
        assert_eq!(
            found.exe,
            PathBuf::from(r"C:\Program Files\VcXsrv\vcxsrv.exe")
        );
        assert_eq!(found.version, None);
    }

    #[test]
    fn every_common_location_is_checked() {
        for (var, base, expected) in [
            ("ProgramW6432", r"D:\PF", r"D:\PF\VcXsrv\vcxsrv.exe"),
            (
                "ProgramFiles(x86)",
                r"D:\PF86",
                r"D:\PF86\VcXsrv\vcxsrv.exe",
            ),
            (
                "LOCALAPPDATA",
                r"C:\Users\<用户>\AppData\Local",
                r"C:\Users\<用户>\AppData\Local\Programs\VcXsrv\vcxsrv.exe",
            ),
        ] {
            let sys = FakeSystem::default();
            sys.with(|s| {
                s.env.insert(var.into(), base.into());
                s.files.insert(PathBuf::from(expected));
            });
            assert_eq!(
                find_vcxsrv(&sys).map(|i| i.exe),
                Some(PathBuf::from(expected)),
                "{var}"
            );
        }
    }

    /// 盘根目录下的 `vcxsrv.exe` 别的用户也放得进去：就算文件在、`SystemDrive` 也设了，也不能自动认。
    #[test]
    fn a_vcxsrv_in_the_drive_root_is_never_trusted_on_its_own() {
        let sys = FakeSystem::default();
        sys.with(|s| {
            s.env.insert("SystemDrive".into(), "C:".into());
            s.files.insert(PathBuf::from(r"C:\VcXsrv\vcxsrv.exe"));
        });
        assert_eq!(find_vcxsrv(&sys), None);
        assert!(
            candidate_dirs(&sys)
                .iter()
                .all(|(dir, _)| dir != &PathBuf::from(r"C:\VcXsrv")),
            "{:?}",
            candidate_dirs(&sys)
        );
    }

    /// 真的装在盘根（免管理员装法）的人：安装程序写的注册表项会指过去，这样照样找得到。
    #[test]
    fn a_drive_root_install_recorded_in_the_registry_is_still_found() {
        let sys = FakeSystem::default();
        sys.with(|s| {
            s.registry.push(RegistryInstall {
                dir: PathBuf::from(r"C:\VcXsrv"),
                version: Some("1.20".into()),
            });
            s.files.insert(PathBuf::from(r"C:\VcXsrv\vcxsrv.exe"));
        });
        let found = find_vcxsrv(&sys).expect("注册表记着的位置");
        assert_eq!(found.exe, PathBuf::from(r"C:\VcXsrv\vcxsrv.exe"));
    }

    #[test]
    fn duplicate_candidates_are_probed_once() {
        let sys = FakeSystem::default();
        sys.with(|s| {
            s.registry.push(RegistryInstall {
                dir: PathBuf::from(r"C:\Program Files\VcXsrv"),
                version: None,
            });
            s.env
                .insert("ProgramFiles".into(), r"C:\PROGRAM FILES".into());
        });
        assert_eq!(candidate_dirs(&sys).len(), 1);
    }

    #[test]
    fn running_servers_are_found_in_the_6000_to_6010_range_only() {
        let sys = FakeSystem::default();
        sys.with(|s| {
            s.listening.extend([6000, 6003, 6010, 6011, 6020, 5999]);
        });
        assert_eq!(running_displays(&sys), vec![0, 3, 10]);
    }
}
