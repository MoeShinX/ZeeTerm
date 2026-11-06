//! Unix（Linux）上的本机 X 服务器：就是当前桌面会话的 `$DISPLAY`。
//!
//! Xorg / Xwayland 默认只开 Unix 套接字，不开 TCP，也没有 VcXsrv 那种由 ZeeTerm 自己拉起的
//! 服务器，所以这里没有检测 / 安装 / 启动，只做两件事：
//!
//! 1. 决定连哪个显示：自动 = 环境里的 `DISPLAY`，指定显示 = 设置里写的那个；
//! 2. 从用户的授权文件（`$XAUTHORITY`，没有就 `~/.Xauthority`）里找出这个显示的 MIT-MAGIC-COOKIE-1，
//!    作为真 cookie 交给转发任务去替换远端送来的假 cookie。找不到不算失败：服务器可能根本不要求认证
//!    （`xhost +local:` 一类），照常尝试，被拒绝时由 [`X11Failure::LocalXServerRejected`] 说清原因。
//!
//! 本模块的函数不判断当前系统，两种平台上都能编译、都有测试；只有 [`super::prepare_blocking`]
//! 决定在 Unix 上走这里。

use std::net::IpAddr;
use std::path::PathBuf;

use crate::error::{reason_keys, Reason};
use crate::x11::display::{parse_display_as, XEndpoint, X_TCP_BASE};
use crate::x11::failure::X11Failure;
use crate::x11::registry::X11Route;

use super::prefs::{DisplayMode, X11Prefs};
use super::system::LocalSystem;
use super::xauth::{self, AuthTarget};
use super::{Prepared, XSource};

/// 当前会话的显示：环境变量 `DISPLAY`（空串当作没有）。
pub fn session_display(sys: &dyn LocalSystem) -> Option<String> {
    sys.env_var("DISPLAY")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 授权文件的位置：`$XAUTHORITY`，没有就 `~/.Xauthority`。
pub fn authority_path(sys: &dyn LocalSystem) -> Option<PathBuf> {
    match sys.env_var("XAUTHORITY") {
        Some(path) => Some(PathBuf::from(path)),
        None => sys.home_dir().map(|home| home.join(".Xauthority")),
    }
}

/// 在授权文件里找这个端点的真 cookie。文件读不了、没有对应记录都是 `None`。
pub fn find_real_cookie(sys: &dyn LocalSystem, endpoint: &XEndpoint) -> Option<Vec<u8>> {
    let bytes = sys.read_file(&authority_path(sys)?)?;
    xauth::find_cookie(&bytes, &auth_target(sys, endpoint))
}

/// 端点对应的授权记录查找条件。
fn auth_target(sys: &dyn LocalSystem, endpoint: &XEndpoint) -> AuthTarget {
    let hostname = sys.hostname();
    if let Some(unix) = &endpoint.unix {
        return AuthTarget::local(Some(unix.display), hostname);
    }
    // TCP：端口 6000 + 显示号。显式写了 ≥ 100 的端口时，6000 以外的端口没有"显示号"可言。
    let display = endpoint
        .port
        .checked_sub(X_TCP_BASE)
        .filter(|display| *display < 100)
        .map(u32::from);
    if endpoint.is_loopback() {
        return AuthTarget::local(display, hostname);
    }
    let ip = endpoint.host.parse::<IpAddr>().ok();
    // 主机名写成了本机自己的名字：授权文件里记的是 FamilyLocal。
    let local_hostname = hostname.filter(|name| name.eq_ignore_ascii_case(&endpoint.host));
    AuthTarget {
        display,
        local_hostname,
        ip,
    }
}

/// 决定这次转发连哪个本机 X 服务器、用哪个真 cookie。
pub fn prepare(prefs: &X11Prefs, sys: &dyn LocalSystem) -> Result<Prepared, X11Failure> {
    let (endpoint, source) = match &prefs.mode {
        DisplayMode::Custom(spec) => {
            let endpoint =
                parse_display_as(spec, true).map_err(|reason| X11Failure::LaunchFailed {
                    reason: Reason::new(
                        reason_keys::X11_CUSTOM_BAD,
                        format!("设置里指定的显示地址不可用：{}", reason.zh()),
                    )
                    .caused_by(&reason),
                })?;
            (endpoint, XSource::Custom)
        }
        DisplayMode::Auto => {
            let spec = session_display(sys).ok_or(X11Failure::NoXDisplay)?;
            let endpoint =
                parse_display_as(&spec, true).map_err(|reason| X11Failure::LaunchFailed {
                    reason: Reason::new(
                        reason_keys::X11_SESSION_DISPLAY_BAD,
                        format!("环境变量 DISPLAY 的值不可用：{}", reason.zh()),
                    )
                    .caused_by(&reason),
                })?;
            (endpoint, XSource::Session)
        }
    };
    let real_cookie = find_real_cookie(sys, &endpoint);
    Ok(Prepared {
        route: X11Route {
            endpoint,
            real_cookie,
        },
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::detect::fake::FakeSystem;
    use super::*;
    use crate::x11::display::UnixSocket;

    const MIT: &str = "MIT-MAGIC-COOKIE-1";
    const LOCAL: u16 = 256;
    const WILD: u16 = 0xFFFF;
    const INTERNET: u16 = 0;

    /// 构造授权文件：（family，地址原始字节，显示号文本，cookie）。
    fn authority(entries: &[(u16, &[u8], &str, [u8; 16])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (family, address, number, cookie) in entries {
            out.extend_from_slice(&family.to_be_bytes());
            for field in [*address, number.as_bytes(), MIT.as_bytes(), &cookie[..]] {
                out.extend_from_slice(&(field.len() as u16).to_be_bytes());
                out.extend_from_slice(field);
            }
        }
        out
    }

    fn system(display: Option<&str>) -> FakeSystem {
        let sys = FakeSystem::default();
        sys.with(|s| {
            if let Some(display) = display {
                s.env.insert("DISPLAY".into(), display.into());
            }
            s.hostname = Some("devbox".into());
            s.home = Some(PathBuf::from("/home/someone"));
        });
        sys
    }

    fn add_file(sys: &FakeSystem, path: &str, bytes: Vec<u8>) {
        sys.with(|s| {
            s.contents.insert(PathBuf::from(path), bytes);
        });
    }

    #[test]
    fn auto_follows_display_and_reads_the_cookie_from_the_home_authority_file() {
        let sys = system(Some(":1"));
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[
                (LOCAL, b"devbox", "0", [1; 16]),
                (LOCAL, b"devbox", "1", [2; 16]),
            ]),
        );
        let prepared = prepare(&X11Prefs::default(), &sys).expect("准备");
        assert_eq!(prepared.source, XSource::Session);
        assert_eq!(prepared.route.endpoint.unix, Some(UnixSocket::standard(1)));
        assert_eq!(prepared.route.real_cookie, Some(vec![2; 16]));
    }

    #[test]
    fn xauthority_overrides_the_home_file() {
        let sys = system(Some(":0"));
        sys.with(|s| {
            s.env
                .insert("XAUTHORITY".into(), "/run/user/1000/xauth".into());
        });
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[(LOCAL, b"devbox", "0", [1; 16])]),
        );
        add_file(
            &sys,
            "/run/user/1000/xauth",
            authority(&[(LOCAL, b"devbox", "0", [7; 16])]),
        );
        let prepared = prepare(&X11Prefs::default(), &sys).expect("准备");
        assert_eq!(prepared.route.real_cookie, Some(vec![7; 16]));
        assert_eq!(
            authority_path(&sys).as_deref(),
            Some(Path::new("/run/user/1000/xauth"))
        );
    }

    #[test]
    fn a_missing_cookie_is_not_a_failure_the_server_may_not_need_one() {
        // 没有授权文件。
        let sys = system(Some(":0"));
        let prepared = prepare(&X11Prefs::default(), &sys).expect("照常尝试");
        assert_eq!(prepared.route.real_cookie, None);
        // 有文件但没有这个显示的记录。
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[(LOCAL, b"devbox", "5", [1; 16])]),
        );
        let prepared = prepare(&X11Prefs::default(), &sys).expect("照常尝试");
        assert_eq!(prepared.route.real_cookie, None);
    }

    #[test]
    fn wild_records_are_used_whatever_the_host_name_is() {
        let sys = system(Some(":3"));
        sys.with(|s| s.hostname = None);
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[(WILD, b"", "3", [4; 16])]),
        );
        let prepared = prepare(&X11Prefs::default(), &sys).expect("准备");
        assert_eq!(prepared.route.real_cookie, Some(vec![4; 16]));
    }

    #[test]
    fn no_display_variable_is_a_clear_failure() {
        for sys in [system(None), system(Some("   "))] {
            assert_eq!(
                prepare(&X11Prefs::default(), &sys).unwrap_err(),
                X11Failure::NoXDisplay
            );
        }
        // 指定了显示就不看环境变量。
        let custom = X11Prefs {
            mode: DisplayMode::Custom(":2".into()),
            ..X11Prefs::default()
        };
        let prepared = prepare(&custom, &system(None)).expect("指定显示");
        assert_eq!(prepared.source, XSource::Custom);
        assert_eq!(prepared.route.endpoint.unix, Some(UnixSocket::standard(2)));
    }

    #[test]
    fn a_custom_display_also_looks_up_its_cookie() {
        let sys = system(Some(":0"));
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[(LOCAL, b"devbox", "2", [6; 16])]),
        );
        let custom = X11Prefs {
            mode: DisplayMode::Custom(":2".into()),
            ..X11Prefs::default()
        };
        let prepared = prepare(&custom, &sys).expect("准备");
        assert_eq!(prepared.route.real_cookie, Some(vec![6; 16]));
    }

    #[test]
    fn tcp_displays_find_their_cookie_by_port_and_host() {
        // 回环 TCP（例如 ssh 转发出来的 localhost:10）按本机主机名匹配。
        let sys = system(Some("localhost:10.0"));
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[(LOCAL, b"devbox", "10", [8; 16])]),
        );
        let prepared = prepare(&X11Prefs::default(), &sys).expect("准备");
        assert_eq!(prepared.route.endpoint.unix, None);
        assert_eq!(prepared.route.endpoint.port, 6010);
        assert_eq!(prepared.route.real_cookie, Some(vec![8; 16]));

        // 别的机器的 IP 字面量按 FamilyInternet 记录匹配。
        let sys = system(Some("192.168.1.9:2"));
        add_file(
            &sys,
            "/home/someone/.Xauthority",
            authority(&[(INTERNET, &[192, 168, 1, 9], "2", [9; 16])]),
        );
        let prepared = prepare(&X11Prefs::default(), &sys).expect("准备");
        assert_eq!(prepared.route.real_cookie, Some(vec![9; 16]));
    }

    #[test]
    fn unusable_displays_are_structured_failures() {
        let err = prepare(&X11Prefs::default(), &system(Some("/tmp/x:0"))).unwrap_err();
        match &err {
            X11Failure::LaunchFailed { reason } => {
                assert_eq!(reason.key(), reason_keys::X11_SESSION_DISPLAY_BAD)
            }
            other => panic!("预期 LaunchFailed：{other:?}"),
        }
        let custom = X11Prefs {
            mode: DisplayMode::Custom("/tmp/x".into()),
            ..X11Prefs::default()
        };
        match prepare(&custom, &system(None)).unwrap_err() {
            X11Failure::LaunchFailed { reason } => {
                assert_eq!(reason.key(), reason_keys::X11_CUSTOM_BAD)
            }
            other => panic!("预期 LaunchFailed：{other:?}"),
        }
    }
}
