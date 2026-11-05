//! 由 ZeeTerm 拉起 VcXsrv。
//!
//! 安全上的选择：**不用 `-ac`**（关闭访问控制会让同网段的人也能连上这个 X 服务器）。改为
//! ZeeTerm 生成随机 MIT-MAGIC-COOKIE，写进数据目录下的授权文件，用 `-auth <文件>` 启动；转发时
//! 这个 cookie 就是"真 cookie"，本机不需要有 `xauth.exe`。
//!
//! 显示号从 20 起找空闲的，避开用户自己常用的 `:0`，也在 6000～6010 的"用户自己的 X 服务器"
//! 探测范围之外，两边不会互相当成对方。
//!
//! 记录：拉起的实例写进数据目录的 `owned.json`（只有显示号）。ZeeTerm 下次启动时若那个端口
//! 还在监听、授权文件还在，就直接沿用，不重复拉起。只有**本次运行亲手拉起**的进程才会被关闭；
//! 沿用的、用户自己开的 X 服务器 ZeeTerm 都不碰。

use crate::error::{reason_keys, Reason};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::detect::VcXsrvInstall;
use super::system::{LocalSystem, OwnedProcess};
use super::xauth;
use crate::x11::cookie::random_cookie;
use crate::x11::display::XEndpoint;
use crate::x11::failure::X11Failure;

/// 自己拉起的实例用的显示号范围。
pub const OWNED_FIRST_DISPLAY: u16 = 20;
pub const OWNED_LAST_DISPLAY: u16 = 39;

/// 等 X 服务器开始监听：最多 `READY_POLLS * READY_POLL_INTERVAL` ≈ 10 秒。
const READY_POLLS: u32 = 100;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 传给 VcXsrv 的命令行：多窗口、共享剪贴板、`-auth` 授权文件。
/// 不带 `-ac`；测试钉死这一点。
pub fn launch_args(display: u16, auth_file: &Path) -> Vec<String> {
    vec![
        format!(":{display}"),
        "-multiwindow".to_owned(),
        "-clipboard".to_owned(),
        "-auth".to_owned(),
        auth_file.to_string_lossy().into_owned(),
    ]
}

pub fn auth_file_path(x11_dir: &Path, display: u16) -> PathBuf {
    x11_dir.join(format!("xauth-{display}"))
}

fn record_path(x11_dir: &Path) -> PathBuf {
    x11_dir.join("owned.json")
}

#[derive(Serialize, Deserialize)]
struct OwnedRecord {
    display: u16,
}

fn read_record(x11_dir: &Path) -> Option<OwnedRecord> {
    let record: OwnedRecord =
        serde_json::from_slice(&std::fs::read(record_path(x11_dir)).ok()?).ok()?;
    (OWNED_FIRST_DISPLAY..=OWNED_LAST_DISPLAY)
        .contains(&record.display)
        .then_some(record)
}

struct Owned {
    display: u16,
    /// 本次运行亲手拉起的进程；沿用上次留下的实例时是 `None`（不关）。
    process: Option<Box<dyn OwnedProcess>>,
}

/// 记录"ZeeTerm 拉起过哪个 X 服务器实例"。进程里一份（[`OwnedRegistry::global`]），测试各自新建。
#[derive(Default)]
pub struct OwnedRegistry {
    inner: Mutex<Option<Owned>>,
}

impl OwnedRegistry {
    pub fn global() -> &'static OwnedRegistry {
        static GLOBAL: OnceLock<OwnedRegistry> = OnceLock::new();
        GLOBAL.get_or_init(OwnedRegistry::default)
    }

    fn lock(&self) -> MutexGuard<'_, Option<Owned>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前记着的自己拉起（或沿用）的实例的显示号。
    pub fn current_display(&self) -> Option<u16> {
        self.lock().as_ref().map(|owned| owned.display)
    }

    /// 关闭本次运行亲手拉起的实例，并清掉记录和授权文件。返回是否真的关了一个。
    /// 沿用来的实例不动，记录也留着，下次还能沿用。
    pub fn shutdown(&self, x11_dir: &Path) -> bool {
        let mut slot = self.lock();
        let Some(mut owned) = slot.take() else {
            return false;
        };
        let Some(process) = owned.process.as_mut() else {
            // 不是这次拉起的：放回去，什么都不动。
            *slot = Some(owned);
            return false;
        };
        process.kill();
        let _ = std::fs::remove_file(record_path(x11_dir));
        let _ = std::fs::remove_file(auth_file_path(x11_dir, owned.display));
        true
    }
}

/// 自己拉起（或沿用）的 X 服务器，转发用它的端点和 cookie。
///
/// `Debug` 是手写的：cookie 是本机 X 服务器的口令（同 `X11Route`），不能进日志。
#[derive(Clone, PartialEq, Eq)]
pub struct OwnedServer {
    pub endpoint: XEndpoint,
    pub cookie: Vec<u8>,
    /// true = 沿用上次运行留下的实例。
    pub reused: bool,
}

impl std::fmt::Debug for OwnedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedServer")
            .field("endpoint", &self.endpoint)
            .field(
                "cookie",
                &format_args!("<{} 字节，已隐藏>", self.cookie.len()),
            )
            .field("reused", &self.reused)
            .finish()
    }
}

/// 沿用上次留下的实例：记录在、端口还在监听、授权文件读得出 cookie。
pub fn reuse_owned(
    sys: &dyn LocalSystem,
    owned: &OwnedRegistry,
    x11_dir: &Path,
) -> Option<OwnedServer> {
    let display = match owned.current_display() {
        Some(display) => display,
        None => read_record(x11_dir)?.display,
    };
    if !sys.tcp_listening(6000 + display) {
        return None;
    }
    let cookie = xauth::read_cookie(&auth_file_path(x11_dir, display))?;
    let mut slot = owned.lock();
    if slot.is_none() {
        *slot = Some(Owned {
            display,
            process: None,
        });
    }
    Some(OwnedServer {
        endpoint: XEndpoint::loopback(display),
        cookie,
        reused: true,
    })
}

/// 授权文件路径要交给 VcXsrv（ANSI 命令行程序）：含非 ASCII 字符的用户目录会被截坏，
/// 这时改用 8.3 短路径；两者都不行就报错而不是让它悄悄读不到文件。
fn ascii_safe_path(sys: &dyn LocalSystem, path: &Path) -> Result<PathBuf, Reason> {
    if path.to_string_lossy().is_ascii() {
        return Ok(path.to_path_buf());
    }
    sys.short_path(path)
        .filter(|short| short.to_string_lossy().is_ascii())
        .ok_or_else(|| {
            Reason::new(
                reason_keys::X11_NON_ASCII_PATH,
                "数据目录的路径含有非英文字符，VcXsrv 读不到授权文件，且系统没有提供短路径名。\
                 可以改用 ZSSH_DATA_DIR 指向纯英文路径，或在设置里改成指定显示",
            )
        })
}

/// 拉起一个新的 VcXsrv 实例。
pub fn start_owned(
    sys: &dyn LocalSystem,
    owned: &OwnedRegistry,
    x11_dir: &Path,
    install: &VcXsrvInstall,
) -> Result<OwnedServer, X11Failure> {
    let fail = |reason: Reason| X11Failure::LaunchFailed { reason };

    let display = (OWNED_FIRST_DISPLAY..=OWNED_LAST_DISPLAY)
        .find(|display| !sys.tcp_listening(6000 + display))
        .ok_or_else(|| {
            fail(Reason::new(
                reason_keys::X11_NO_FREE_DISPLAY,
                "找不到空闲的显示号",
            ))
        })?;
    let cookie = random_cookie()
        .ok_or_else(|| fail(Reason::new(reason_keys::X11_RANDOM, "系统随机数不可用")))?;

    let auth_file = auth_file_path(x11_dir, display);
    xauth::write_file(&auth_file, display, &cookie).map_err(|err| {
        fail(
            Reason::new(
                reason_keys::X11_WRITE_AUTH,
                format!("写授权文件失败：{err}"),
            )
            .with("cause", &err),
        )
    })?;
    let auth_arg = ascii_safe_path(sys, &auth_file).map_err(fail)?;

    let mut process = sys
        .spawn(&install.exe, &launch_args(display, &auth_arg))
        .map_err(|err| fail(Reason::text(err)))?;

    let mut ready = false;
    for _ in 0..READY_POLLS {
        if sys.tcp_listening(6000 + display) {
            ready = true;
            break;
        }
        if process.has_exited() {
            let _ = std::fs::remove_file(&auth_file);
            return Err(fail(Reason::new(
                reason_keys::X11_EXITED,
                "VcXsrv 启动后立刻退出了",
            )));
        }
        sys.sleep(READY_POLL_INTERVAL);
    }
    if !ready {
        process.kill();
        let _ = std::fs::remove_file(&auth_file);
        return Err(fail(Reason::new(
            reason_keys::X11_NO_LISTEN,
            "VcXsrv 启动后 10 秒内没有开始监听",
        )));
    }

    // 记录写不进去不致命：只是下次运行没法沿用，会再拉起一个。
    if let Ok(json) = serde_json::to_vec(&OwnedRecord { display }) {
        let _ = std::fs::write(record_path(x11_dir), json);
    }
    *owned.lock() = Some(Owned {
        display,
        process: Some(process),
    });
    Ok(OwnedServer {
        endpoint: XEndpoint::loopback(display),
        cookie: cookie.to_vec(),
        reused: false,
    })
}

#[cfg(test)]
mod tests {
    use super::super::detect::fake::FakeSystem;
    use super::*;

    #[test]
    fn the_owned_servers_debug_output_hides_the_cookie() {
        let server = OwnedServer {
            endpoint: XEndpoint {
                host: "127.0.0.1".to_owned(),
                port: 6000,
                screen: 0,
            },
            cookie: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02],
            reused: false,
        };
        for text in [format!("{server:?}"), format!("{server:#?}")] {
            assert!(!text.contains("222") && !text.contains("173"), "{text}");
            assert!(!text.to_lowercase().contains("dead"), "{text}");
            assert!(text.contains("6 字节") && text.contains("6000"), "{text}");
        }
    }
    use crate::test_util::TestDir;

    fn install() -> VcXsrvInstall {
        VcXsrvInstall {
            exe: PathBuf::from(r"C:\Program Files\VcXsrv\vcxsrv.exe"),
            version: None,
        }
    }

    #[test]
    fn launch_args_use_auth_and_never_disable_access_control() {
        let args = launch_args(20, Path::new(r"C:\data\x11\xauth-20"));
        assert_eq!(args[0], ":20");
        assert!(args.contains(&"-multiwindow".to_owned()));
        assert!(args.contains(&"-clipboard".to_owned()));
        let auth = args.iter().position(|a| a == "-auth").expect("有 -auth");
        assert_eq!(args[auth + 1], r"C:\data\x11\xauth-20");
        assert!(!args.iter().any(|a| a == "-ac"), "绝不能关闭访问控制");
    }

    #[test]
    fn starting_picks_a_free_display_writes_the_cookie_and_records_ownership() {
        let dir = TestDir::new("x11-launch");
        let sys = FakeSystem::default();
        sys.with(|s| {
            // 20 号已经被别人占了，应当选 21。
            s.listening.insert(6020);
            s.listen_after_spawn = Some(6021);
        });
        let owned = OwnedRegistry::default();
        let server = start_owned(&sys, &owned, &dir, &install()).expect("拉起");

        assert_eq!(server.endpoint.port, 6021);
        assert!(!server.reused);
        assert_eq!(server.cookie.len(), 16);
        // 授权文件里的 cookie 就是转发用的真 cookie。
        assert_eq!(
            xauth::read_cookie(&auth_file_path(&dir, 21)),
            Some(server.cookie.clone())
        );
        assert_eq!(owned.current_display(), Some(21));
        let spawned = sys.with(|s| s.spawned.clone());
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].0, install().exe);
        assert_eq!(spawned[0].1[0], ":21");
        assert!(!spawned[0].1.iter().any(|a| a == "-ac"));
        assert!(record_path(&dir).exists());
    }

    #[test]
    fn each_launch_uses_a_fresh_random_cookie() {
        let dir = TestDir::new("x11-launch-cookies");
        let sys = FakeSystem::default();
        sys.with(|s| s.listen_after_spawn = Some(6020));
        let a = start_owned(&sys, &OwnedRegistry::default(), &dir, &install()).unwrap();
        sys.with(|s| s.listening.clear());
        let b = start_owned(&sys, &OwnedRegistry::default(), &dir, &install()).unwrap();
        assert_ne!(a.cookie, b.cookie);
    }

    #[test]
    fn a_server_that_never_listens_is_killed_and_reported() {
        let dir = TestDir::new("x11-launch-timeout");
        let sys = FakeSystem::default();
        let owned = OwnedRegistry::default();
        let err = start_owned(&sys, &owned, &dir, &install()).unwrap_err();
        assert_eq!(err.code(), "launch_failed");
        assert_eq!(sys.with(|s| s.killed), 1, "没起来的进程要杀掉");
        assert_eq!(owned.current_display(), None);
        assert!(!auth_file_path(&dir, 20).exists(), "不留授权文件");
    }

    #[test]
    fn a_server_that_exits_at_once_and_a_spawn_error_are_launch_failures() {
        let dir = TestDir::new("x11-launch-exit");
        let sys = FakeSystem::default();
        sys.with(|s| s.exit_immediately = true);
        let err = start_owned(&sys, &OwnedRegistry::default(), &dir, &install()).unwrap_err();
        assert!(err.to_string().contains("立刻退出"), "{err}");

        let sys = FakeSystem::default();
        sys.with(|s| s.spawn_error = Some("找不到程序".into()));
        let err = start_owned(&sys, &OwnedRegistry::default(), &dir, &install()).unwrap_err();
        assert!(err.to_string().contains("找不到程序"), "{err}");
    }

    #[test]
    fn every_display_taken_is_a_launch_failure() {
        let dir = TestDir::new("x11-launch-full");
        let sys = FakeSystem::default();
        sys.with(|s| {
            s.listening
                .extend((OWNED_FIRST_DISPLAY..=OWNED_LAST_DISPLAY).map(|d| 6000 + d));
        });
        let err = start_owned(&sys, &OwnedRegistry::default(), &dir, &install()).unwrap_err();
        assert!(err.to_string().contains("空闲"), "{err}");
    }

    #[test]
    fn shutdown_kills_only_what_this_run_started() {
        let dir = TestDir::new("x11-launch-shutdown");
        let sys = FakeSystem::default();
        sys.with(|s| s.listen_after_spawn = Some(6020));
        let owned = OwnedRegistry::default();
        start_owned(&sys, &owned, &dir, &install()).unwrap();
        assert!(owned.shutdown(&dir));
        assert_eq!(sys.with(|s| s.killed), 1);
        assert_eq!(owned.current_display(), None);
        assert!(!record_path(&dir).exists());
        assert!(!auth_file_path(&dir, 20).exists());
        assert!(!owned.shutdown(&dir), "第二次没有东西可关");
    }

    #[test]
    fn a_reused_instance_is_adopted_but_never_killed() {
        let dir = TestDir::new("x11-launch-reuse");
        let sys = FakeSystem::default();
        sys.with(|s| s.listen_after_spawn = Some(6020));
        let first = OwnedRegistry::default();
        let started = start_owned(&sys, &first, &dir, &install()).unwrap();

        // 模拟 ZeeTerm 重启：新的登记表，进程还在监听。
        let second = OwnedRegistry::default();
        let reused = reuse_owned(&sys, &second, &dir).expect("沿用");
        assert!(reused.reused);
        assert_eq!(reused.cookie, started.cookie);
        assert_eq!(reused.endpoint, started.endpoint);
        assert_eq!(second.current_display(), Some(20));
        assert!(!second.shutdown(&dir), "沿用来的不关");
        assert_eq!(sys.with(|s| s.killed), 0);
        assert!(record_path(&dir).exists(), "记录留着，下次还能沿用");
    }

    #[test]
    fn nothing_is_reused_when_the_port_is_dead_or_the_auth_file_is_gone() {
        let dir = TestDir::new("x11-launch-no-reuse");
        let sys = FakeSystem::default();
        assert!(
            reuse_owned(&sys, &OwnedRegistry::default(), &dir).is_none(),
            "没有记录"
        );

        sys.with(|s| s.listen_after_spawn = Some(6020));
        start_owned(&sys, &OwnedRegistry::default(), &dir, &install()).unwrap();
        sys.with(|s| s.listening.clear());
        assert!(
            reuse_owned(&sys, &OwnedRegistry::default(), &dir).is_none(),
            "端口没人监听"
        );

        sys.with(|s| s.listening.insert(6020));
        std::fs::remove_file(auth_file_path(&dir, 20)).unwrap();
        assert!(
            reuse_owned(&sys, &OwnedRegistry::default(), &dir).is_none(),
            "授权文件没了"
        );

        // 记录被改成范围外的显示号（例如被篡改成用户自己的 :0）一律不认。
        std::fs::write(record_path(&dir), br#"{"display":0}"#).unwrap();
        sys.with(|s| s.listening.insert(6000));
        assert!(reuse_owned(&sys, &OwnedRegistry::default(), &dir).is_none());
    }

    #[test]
    fn non_ascii_data_dirs_use_the_short_path_or_fail_clearly() {
        let sys = FakeSystem::default();
        let long = PathBuf::from(r"C:\Users\张 三\AppData\Roaming\ZeeTerm\x11\xauth-20");
        let short = PathBuf::from(r"C:\Users\ZHANGS~1\APPDATA~1\ZeeTerm\x11\xauth-20");
        assert_eq!(
            ascii_safe_path(&sys, Path::new(r"C:\data\xauth-20")).unwrap(),
            PathBuf::from(r"C:\data\xauth-20")
        );
        assert!(ascii_safe_path(&sys, &long).is_err());
        sys.with(|s| {
            s.short_paths.insert(long.clone(), short.clone());
        });
        assert_eq!(ascii_safe_path(&sys, &long).unwrap(), short);
        // 短路径本身还是非 ASCII，等于没用。
        sys.with(|s| {
            s.short_paths.insert(long.clone(), long.clone());
        });
        assert!(ascii_safe_path(&sys, &long).is_err());
    }
}
