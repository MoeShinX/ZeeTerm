//! 本机 X 服务器：检测、一键安装、自动启动。
//!
//! 连接一个开了 X11 的主机前，调用 [`prepare`] 决定"转发到哪个本机 X 服务器、用哪个真 cookie"：
//!
//! 1. 设置里指定了显示（用户自己管理的 X 服务器，如 X410、MobaXterm）→ 直接用，cookie 透传；
//! 2. 自动：先沿用 ZeeTerm 上次留下的实例（有 cookie）；
//! 3. 再看 6000～6010 上有没有别的 X 服务器在运行（用户自己开的，cookie 透传）；
//! 4. 再看有没有装 VcXsrv，有就由 ZeeTerm 拉起（带随机 cookie，不用 `-ac`）；
//! 5. 都没有：返回 [`X11Failure::NoLocalXServer`]（`installable` 告诉界面能不能一键安装）。
//!
//! 本模块不碰界面。界面入口（一键安装按钮、设置页）在后续卡，调用 [`install::install_vcxsrv`]、
//! [`status`]、[`prefs`] 里的函数即可。

pub mod detect;
pub mod install;
pub mod launch;
pub mod prefs;
pub mod system;
pub mod xauth;

use std::path::Path;

pub use detect::VcXsrvInstall;
pub use launch::OwnedRegistry;
pub use prefs::{DisplayMode, X11Prefs};
pub use system::{LocalSystem, RealSystem};

use super::display::{parse_display, XEndpoint};
use super::failure::X11Failure;
use super::registry::X11Route;

/// 转发目标是怎么来的，界面可以据此提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XSource {
    /// 设置里指定的显示。
    Custom,
    /// 用户自己已经在运行的 X 服务器（ZeeTerm 拿不到它的 cookie，握手包透传）。
    UserRunning { display: u16 },
    /// 沿用 ZeeTerm 上次拉起、仍在运行的 VcXsrv。
    OwnedReused,
    /// 这次由 ZeeTerm 拉起的 VcXsrv。
    OwnedStarted,
}

/// [`prepare`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub route: X11Route,
    pub source: XSource,
}

/// 界面用的本机 X 服务器状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalXStatus {
    pub vcxsrv: Option<VcXsrvInstall>,
    /// 6000～6010 上正在监听的显示号。
    pub running_displays: Vec<u16>,
    /// ZeeTerm 自己拉起（或沿用）的实例的显示号。
    pub owned_display: Option<u16>,
    /// 能不能由 ZeeTerm 一键安装（目前只有 Windows）。
    pub can_install: bool,
}

pub fn status(sys: &dyn LocalSystem, owned: &OwnedRegistry) -> LocalXStatus {
    LocalXStatus {
        vcxsrv: detect::find_vcxsrv(sys),
        running_displays: detect::running_displays(sys),
        owned_display: owned.current_display(),
        can_install: cfg!(windows),
    }
}

/// 同步版（会探端口、起进程、睡眠等待）。异步环境用 [`prepare`]。
pub fn prepare_blocking(
    prefs: &X11Prefs,
    x11_dir: &Path,
    sys: &dyn LocalSystem,
    owned: &OwnedRegistry,
) -> Result<Prepared, X11Failure> {
    if let DisplayMode::Custom(spec) = &prefs.mode {
        let endpoint = parse_display(spec).map_err(|reason| X11Failure::LaunchFailed {
            reason: format!("设置里指定的显示地址不可用：{reason}"),
        })?;
        return Ok(Prepared {
            route: X11Route {
                endpoint,
                real_cookie: None,
            },
            source: XSource::Custom,
        });
    }

    if let Some(server) = launch::reuse_owned(sys, owned, x11_dir) {
        return Ok(Prepared {
            route: X11Route {
                endpoint: server.endpoint,
                real_cookie: Some(server.cookie),
            },
            source: XSource::OwnedReused,
        });
    }

    if let Some(&display) = detect::running_displays(sys).first() {
        return Ok(Prepared {
            route: X11Route {
                endpoint: XEndpoint::loopback(display),
                real_cookie: None,
            },
            source: XSource::UserRunning { display },
        });
    }

    match detect::find_vcxsrv(sys) {
        Some(install) => {
            let server = launch::start_owned(sys, owned, x11_dir, &install)?;
            Ok(Prepared {
                route: X11Route {
                    endpoint: server.endpoint,
                    real_cookie: Some(server.cookie),
                },
                source: XSource::OwnedStarted,
            })
        }
        None => Err(X11Failure::NoLocalXServer {
            display: XEndpoint::loopback(0).describe(),
            installable: cfg!(windows),
        }),
    }
}

/// 在阻塞线程里跑 [`prepare_blocking`]（真实本机环境、进程级登记表）。
pub async fn prepare(prefs: X11Prefs, x11_dir: std::path::PathBuf) -> Result<Prepared, X11Failure> {
    tokio::task::spawn_blocking(move || {
        prepare_blocking(&prefs, &x11_dir, &RealSystem, OwnedRegistry::global())
    })
    .await
    .map_err(|err| X11Failure::LaunchFailed {
        reason: format!("准备本机 X 服务器的任务异常结束：{err}"),
    })?
}

/// ZeeTerm 退出时调用：设置里要求关闭时，关掉本次运行亲手拉起的 X 服务器。
/// 返回是否真的关了一个。用户自己的、沿用上次留下的实例都不动。
pub fn shutdown_owned_if_configured(prefs: &X11Prefs, x11_dir: &Path) -> bool {
    prefs.close_owned_on_exit && OwnedRegistry::global().shutdown(x11_dir)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::detect::fake::FakeSystem;
    use super::system::RegistryInstall;
    use super::*;
    use crate::test_util::TestDir;

    fn with_vcxsrv(sys: &FakeSystem) {
        sys.with(|s| {
            s.registry.push(RegistryInstall {
                dir: PathBuf::from(r"C:\Program Files\VcXsrv"),
                version: Some("1.2.3".into()),
            });
            s.files
                .insert(PathBuf::from(r"C:\Program Files\VcXsrv\vcxsrv.exe"));
            s.listen_after_spawn = Some(6020);
        });
    }

    #[test]
    fn a_custom_display_is_used_as_is_without_touching_anything() {
        let dir = TestDir::new("x11-prepare-custom");
        let sys = FakeSystem::default();
        with_vcxsrv(&sys);
        let prefs = X11Prefs {
            mode: DisplayMode::Custom("localhost:3".into()),
            close_owned_on_exit: true,
        };
        let prepared =
            prepare_blocking(&prefs, &dir, &sys, &OwnedRegistry::default()).expect("指定显示");
        assert_eq!(prepared.source, XSource::Custom);
        assert_eq!(prepared.route.endpoint.port, 6003);
        assert_eq!(prepared.route.real_cookie, None, "用户自己的 X 服务器透传");
        assert!(sys.with(|s| s.spawned.is_empty()), "不拉起任何东西");
    }

    #[test]
    fn a_bad_custom_display_is_a_clear_failure() {
        let dir = TestDir::new("x11-prepare-bad");
        let prefs = X11Prefs {
            mode: DisplayMode::Custom("/tmp/x".into()),
            close_owned_on_exit: true,
        };
        let err = prepare_blocking(
            &prefs,
            &dir,
            &FakeSystem::default(),
            &OwnedRegistry::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("不可用"), "{err}");
    }

    #[test]
    fn auto_prefers_a_server_the_user_already_runs_and_does_not_launch_ours() {
        let dir = TestDir::new("x11-prepare-running");
        let sys = FakeSystem::default();
        with_vcxsrv(&sys);
        sys.with(|s| {
            s.listening.insert(6001);
        });
        let prepared =
            prepare_blocking(&X11Prefs::default(), &dir, &sys, &OwnedRegistry::default())
                .expect("用现成的");
        assert_eq!(prepared.source, XSource::UserRunning { display: 1 });
        assert_eq!(prepared.route.endpoint.port, 6001);
        assert_eq!(prepared.route.real_cookie, None);
        assert!(sys.with(|s| s.spawned.is_empty()));
    }

    #[test]
    fn auto_launches_vcxsrv_when_installed_and_nothing_runs() {
        let dir = TestDir::new("x11-prepare-launch");
        let sys = FakeSystem::default();
        with_vcxsrv(&sys);
        let owned = OwnedRegistry::default();
        let prepared = prepare_blocking(&X11Prefs::default(), &dir, &sys, &owned).expect("拉起");
        assert_eq!(prepared.source, XSource::OwnedStarted);
        assert_eq!(prepared.route.endpoint.port, 6020);
        let cookie = prepared.route.real_cookie.clone().expect("有真 cookie");
        assert_eq!(
            xauth::read_cookie(&launch::auth_file_path(&dir, 20)),
            Some(cookie)
        );
        assert_eq!(owned.current_display(), Some(20));

        // 第二次连接：直接沿用，不再起第二个进程。
        let again = prepare_blocking(&X11Prefs::default(), &dir, &sys, &owned).expect("沿用");
        assert_eq!(again.source, XSource::OwnedReused);
        assert_eq!(again.route, prepared.route);
        assert_eq!(sys.with(|s| s.spawned.len()), 1);
    }

    #[test]
    fn auto_with_nothing_available_says_it_can_be_installed() {
        let dir = TestDir::new("x11-prepare-none");
        let err = prepare_blocking(
            &X11Prefs::default(),
            &dir,
            &FakeSystem::default(),
            &OwnedRegistry::default(),
        )
        .unwrap_err();
        match err {
            X11Failure::NoLocalXServer { installable, .. } => {
                assert_eq!(installable, cfg!(windows));
            }
            other => panic!("预期 NoLocalXServer：{other:?}"),
        }
    }

    #[test]
    fn a_launch_failure_surfaces_as_a_structured_error() {
        let dir = TestDir::new("x11-prepare-launch-fail");
        let sys = FakeSystem::default();
        with_vcxsrv(&sys);
        sys.with(|s| s.spawn_error = Some("拒绝访问".into()));
        let err = prepare_blocking(&X11Prefs::default(), &dir, &sys, &OwnedRegistry::default())
            .unwrap_err();
        assert_eq!(err.code(), "launch_failed");
    }

    #[test]
    fn status_reports_install_running_and_owned_state() {
        let dir = TestDir::new("x11-status");
        let sys = FakeSystem::default();
        let owned = OwnedRegistry::default();
        let empty = status(&sys, &owned);
        assert_eq!(empty.vcxsrv, None);
        assert!(empty.running_displays.is_empty());
        assert_eq!(empty.owned_display, None);

        with_vcxsrv(&sys);
        prepare_blocking(&X11Prefs::default(), &dir, &sys, &owned).unwrap();
        let after = status(&sys, &owned);
        assert!(after.vcxsrv.is_some());
        assert_eq!(after.owned_display, Some(20));
        assert_eq!(after.can_install, cfg!(windows));
    }
}
