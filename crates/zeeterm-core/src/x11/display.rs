//! `DISPLAY` 字符串解析。
//!
//! Windows：本机 X 服务器（VcXsrv、X410、MobaXterm 的 X 服务）都监听 TCP `6000 + 显示号`，
//! 所以 `:0`、`unix:0`、`localhost:0` 都落到 `localhost:6000`。
//! Linux 等 Unix：本机 X 服务器（Xorg、Xwayland）默认只开 Unix 套接字、不开 TCP，所以 `:N`、`unix:N`
//! 指向 `/tmp/.X11-unix/XN`（连接时 Linux 先试抽象命名空间里的同名套接字），带主机名的 `host:N` 仍是
//! TCP `6000 + N`。
//! 写法和 nyaterm 的 `resolve_x11_display_spec` 保持一致（TCP 主机的显示号 ≥ 100 直接当端口），
//! 但这里自己重写、加了范围检查，并且遇到解析不了的写法报错而不是悄悄当成 0 号。

use std::path::PathBuf;

use crate::error::{reason_keys as rk, Reason};

/// X 服务器的 TCP 基准端口。
pub const X_TCP_BASE: u16 = 6000;

/// Unix 域套接字所在目录（X11 约定，Xorg / Xwayland / Xvfb 都放这里）。
pub const X_UNIX_DIR: &str = "/tmp/.X11-unix";

/// 本机 X 服务器的 Unix 套接字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixSocket {
    /// 显示号（`:7` 里的 7），在提示里显示、按它找 Xauthority 记录。
    pub display: u32,
    /// 文件系统里的套接字路径；Linux 上抽象命名空间里的套接字名字就是这个路径（不带前导的 NUL）。
    pub path: PathBuf,
}

impl UnixSocket {
    /// 标准位置 `/tmp/.X11-unix/X<显示号>`。
    pub fn standard(display: u32) -> Self {
        Self {
            display,
            path: PathBuf::from(format!("{X_UNIX_DIR}/X{display}")),
        }
    }
}

/// 本机 X 服务器的端点：TCP，或者（`unix` 有值时）Unix 套接字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XEndpoint {
    /// TCP 主机；走 Unix 套接字时只是占位。
    pub host: String,
    /// TCP 端口；走 Unix 套接字时只是占位。
    pub port: u16,
    /// 屏幕号（`:0.1` 里的 1）。`request_x11` 要带。
    pub screen: u32,
    /// 有值 = 连这个 Unix 套接字，不碰 TCP。
    pub unix: Option<UnixSocket>,
}

impl XEndpoint {
    /// 回环地址上的显示号。
    pub fn loopback(display: u16) -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: X_TCP_BASE.saturating_add(display),
            screen: 0,
            unix: None,
        }
    }

    /// 本机 Unix 套接字上的显示号（标准路径）。
    pub fn local_unix(display: u32, screen: u32) -> Self {
        Self {
            host: "localhost".to_owned(),
            port: u16::try_from(display)
                .ok()
                .and_then(|display| X_TCP_BASE.checked_add(display))
                .unwrap_or(0),
            screen,
            unix: Some(UnixSocket::standard(display)),
        }
    }

    /// 用在提示和日志里：TCP 是 `主机:端口`，Unix 套接字是 DISPLAY 写法 `:N`。
    pub fn describe(&self) -> String {
        if let Some(unix) = &self.unix {
            return format!(":{}", unix.display);
        }
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// 主机是不是本机回环（决定要不要提醒"别人也能连这个 X 服务器"之类）。
    /// Unix 套接字天然只在本机。
    pub fn is_loopback(&self) -> bool {
        self.unix.is_some()
            || matches!(
                self.host.to_ascii_lowercase().as_str(),
                "localhost" | "127.0.0.1" | "::1"
            )
    }
}

/// 解析 `DISPLAY` 写法（按当前系统的习惯）。空串在 Windows 上按 `localhost:0`，在 Unix 上按 `:0`。
///
/// 支持：`:0`、`:0.1`、`unix:0`、`localhost:0`、`127.0.0.1:10.0`、`[::1]:0`、`主机:6000`
/// （TCP 主机冒号后 ≥ 100 的数字直接当 TCP 端口）。Unix 套接字路径的写法不支持，报错。
pub fn parse_display(spec: &str) -> Result<XEndpoint, Reason> {
    parse_display_as(spec, cfg!(unix))
}

/// [`parse_display`] 的实现。`local_sockets` 为真时 `:N` / `unix:N` 走本机 Unix 套接字，
/// 否则落到 `localhost` 的 TCP 端口。单独成函数是为了两种映射都能在任何系统上测试。
pub fn parse_display_as(spec: &str, local_sockets: bool) -> Result<XEndpoint, Reason> {
    let spec = spec.trim();
    let spec = match (spec.is_empty(), local_sockets) {
        (false, _) => spec,
        (true, true) => ":0",
        (true, false) => "localhost:0",
    };
    if spec.len() > 255 || spec.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(Reason::new(
            rk::X11_DISPLAY_BAD_TEXT,
            "显示地址里有空白或控制字符，或者太长",
        ));
    }
    if spec.starts_with('/') {
        return Err(if local_sockets {
            Reason::new(
                rk::X11_DISPLAY_SOCKET_PATH,
                "不支持直接写套接字路径，请写成 :0 或 localhost:0 这样的地址",
            )
        } else {
            Reason::new(
                rk::X11_DISPLAY_UNIX_SOCKET,
                "Windows 上没有 Unix 套接字，请写成 localhost:0 这样的地址",
            )
        });
    }

    let (host, tail) = split_host_and_tail(spec).ok_or_else(|| {
        Reason::new(
            rk::X11_DISPLAY_FORMAT,
            format!("显示地址 {spec} 的写法不对，应当像 localhost:0 或 :0"),
        )
        .with("spec", spec)
    })?;
    let local_socket = local_sockets && matches!(host, "" | "unix");
    let host = match host {
        "" | "unix" => "localhost",
        other => other,
    };

    let (number, screen) = match tail.split_once('.') {
        Some((number, screen)) => (number, Some(screen)),
        None => (tail, None),
    };
    let number: u32 = number.parse().map_err(|_| {
        Reason::new(
            rk::X11_DISPLAY_NUMBER,
            format!("显示地址 {spec} 里的显示号不是数字"),
        )
        .with("spec", spec)
    })?;
    let screen: u32 = match screen {
        None => 0,
        Some(text) => text.parse().map_err(|_| {
            Reason::new(
                rk::X11_SCREEN_NUMBER,
                format!("显示地址 {spec} 里的屏幕号不是数字"),
            )
            .with("spec", spec)
        })?,
    };
    if local_socket {
        // 套接字名是 X<显示号>，没有"≥ 100 当端口"这一说（Xvfb :600 之类是正常的显示号）。
        return Ok(XEndpoint::local_unix(number, screen));
    }
    let port = if number >= 100 {
        u16::try_from(number).map_err(|_| {
            Reason::new(
                rk::X11_PORT_RANGE,
                format!("显示地址 {spec} 里的端口超出范围"),
            )
            .with("spec", spec)
        })?
    } else {
        X_TCP_BASE + number as u16
    };
    Ok(XEndpoint {
        host: host.to_owned(),
        port,
        screen,
        unix: None,
    })
}

/// 把 `主机:显示号[.屏幕号]` 拆成主机和冒号后的部分。IPv6 要写方括号。
fn split_host_and_tail(spec: &str) -> Option<(&str, &str)> {
    if let Some(rest) = spec.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let tail = after.strip_prefix(':')?;
        return (!host.is_empty() && !tail.is_empty()).then_some((host, tail));
    }
    let (host, tail) = spec.rsplit_once(':')?;
    // 裸 IPv6（多个冒号）会在这里被拆错，要求用方括号。
    if host.contains(':') || tail.is_empty() {
        return None;
    }
    Some((host, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 的映射：`:0` 一类落到 localhost 的 TCP 端口。
    fn desc(spec: &str) -> String {
        parse_display_as(spec, false).expect(spec).describe()
    }

    /// Unix 的映射：`:0` 一类走本机 Unix 套接字。
    fn desc_unix(spec: &str) -> String {
        parse_display_as(spec, true).expect(spec).describe()
    }

    #[test]
    fn common_spellings_resolve_to_tcp_ports() {
        assert_eq!(desc("localhost:0"), "localhost:6000");
        assert_eq!(desc("localhost:1"), "localhost:6001");
        assert_eq!(desc("127.0.0.1:0"), "127.0.0.1:6000");
        assert_eq!(desc(":0"), "localhost:6000");
        assert_eq!(desc("unix:2"), "localhost:6002");
        assert_eq!(desc(""), "localhost:6000");
        assert_eq!(desc("  :10.0  "), "localhost:6010");
        assert_eq!(desc("xhost.example:3"), "xhost.example:6003");
        assert_eq!(desc("[::1]:0"), "[::1]:6000");
    }

    #[test]
    fn numbers_from_100_are_taken_as_ports_and_screens_are_kept() {
        assert_eq!(desc("localhost:6000"), "localhost:6000");
        assert_eq!(parse_display_as("localhost:0.3", false).unwrap().screen, 3);
        assert_eq!(parse_display_as("localhost:0", false).unwrap().screen, 0);
    }

    #[test]
    fn unusable_spellings_are_errors_not_display_zero() {
        for local_sockets in [false, true] {
            for bad in [
                "/tmp/.X11-unix/X0",
                "localhost:",
                "localhost:x",
                "localhost:0.x",
                "localhost",
                "::1:0",
                "[::1]",
                "local host:0",
                "host:70000",
            ] {
                assert!(
                    parse_display_as(bad, local_sockets).is_err(),
                    "{bad} 应当报错（local_sockets={local_sockets}）"
                );
            }
        }
    }

    #[test]
    fn loopback_detection() {
        assert!(parse_display_as(":0", false).unwrap().is_loopback());
        assert!(parse_display_as(":0", true).unwrap().is_loopback());
        assert!(parse_display_as("127.0.0.1:1", false)
            .unwrap()
            .is_loopback());
        assert!(!parse_display_as("xhost.example:1", false)
            .unwrap()
            .is_loopback());
        assert_eq!(XEndpoint::loopback(20).port, 6020);
    }

    #[test]
    fn local_forms_map_to_unix_sockets_where_the_system_has_them() {
        for spec in [":0", "unix:0", "", "  :0  "] {
            let endpoint = parse_display_as(spec, true).expect(spec);
            assert_eq!(endpoint.unix, Some(UnixSocket::standard(0)), "{spec:?}");
            assert_eq!(endpoint.describe(), ":0");
        }
        let endpoint = parse_display_as(":1.2", true).unwrap();
        assert_eq!(
            endpoint.unix.as_ref().map(|u| u.path.clone()),
            Some(PathBuf::from("/tmp/.X11-unix/X1"))
        );
        assert_eq!(endpoint.screen, 2);
        // 高位的显示号（Xvfb :600 一类）就是显示号，不会被当成 TCP 端口。
        let high = parse_display_as(":600", true).unwrap();
        assert_eq!(high.unix, Some(UnixSocket::standard(600)));
        assert_eq!(desc_unix("unix:7"), ":7");
    }

    #[test]
    fn explicit_hosts_stay_on_tcp_everywhere() {
        for local_sockets in [false, true] {
            for (spec, shown) in [
                ("localhost:0", "localhost:6000"),
                ("127.0.0.1:3", "127.0.0.1:6003"),
                ("[::1]:2", "[::1]:6002"),
                ("xhost.example:5", "xhost.example:6005"),
                ("localhost:6010", "localhost:6010"),
            ] {
                let endpoint = parse_display_as(spec, local_sockets).expect(spec);
                assert_eq!(endpoint.unix, None, "{spec}");
                assert_eq!(endpoint.describe(), shown);
            }
        }
    }

    #[test]
    fn socket_paths_are_refused_with_a_system_specific_reason() {
        let unix = parse_display_as("/tmp/.X11-unix/X0", true).unwrap_err();
        assert_eq!(unix.key(), rk::X11_DISPLAY_SOCKET_PATH);
        let windows = parse_display_as("/tmp/.X11-unix/X0", false).unwrap_err();
        assert_eq!(windows.key(), rk::X11_DISPLAY_UNIX_SOCKET);
    }

    #[test]
    fn parse_display_follows_the_current_system() {
        let endpoint = parse_display(":0").unwrap();
        assert_eq!(endpoint.unix.is_some(), cfg!(unix));
        assert_eq!(parse_display("localhost:0").unwrap().unix, None);
    }
}
