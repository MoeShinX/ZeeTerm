//! `DISPLAY` 字符串解析。
//!
//! 只面向 Windows：本机 X 服务器（VcXsrv、X410、MobaXterm 的 X 服务）都监听 TCP `6000 + 显示号`，
//! 所以 `:0`、`unix:0`、`localhost:0` 都落到 `localhost:6000`。
//! 写法和 nyaterm 的 `resolve_x11_display_spec` 保持一致（显示号 ≥ 100 直接当端口），
//! 但这里自己重写、加了范围检查，并且遇到解析不了的写法报错而不是悄悄当成 0 号。

/// X 服务器的 TCP 基准端口。
pub const X_TCP_BASE: u16 = 6000;

/// 本机 X 服务器的 TCP 端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XEndpoint {
    pub host: String,
    pub port: u16,
    /// 屏幕号（`:0.1` 里的 1）。`request_x11` 要带。
    pub screen: u32,
}

impl XEndpoint {
    /// 回环地址上的显示号。
    pub fn loopback(display: u16) -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: X_TCP_BASE.saturating_add(display),
            screen: 0,
        }
    }

    /// `主机:端口`，用在提示和日志里。
    pub fn describe(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// 主机是不是本机回环（决定要不要提醒"别人也能连这个 X 服务器"之类）。
    pub fn is_loopback(&self) -> bool {
        matches!(
            self.host.to_ascii_lowercase().as_str(),
            "localhost" | "127.0.0.1" | "::1"
        )
    }
}

/// 解析 `DISPLAY` 写法。空串按 `localhost:0`。
///
/// 支持：`:0`、`:0.1`、`unix:0`、`localhost:0`、`127.0.0.1:10.0`、`[::1]:0`、`主机:6000`
/// （冒号后 ≥ 100 的数字直接当 TCP 端口）。Unix 套接字路径在 Windows 上没有意义，报错。
pub fn parse_display(spec: &str) -> Result<XEndpoint, String> {
    let spec = spec.trim();
    let spec = if spec.is_empty() { "localhost:0" } else { spec };
    if spec.len() > 255 || spec.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("显示地址里有空白或控制字符，或者太长".to_owned());
    }
    if spec.starts_with('/') {
        return Err("Windows 上没有 Unix 套接字，请写成 localhost:0 这样的地址".to_owned());
    }

    let (host, tail) = split_host_and_tail(spec)
        .ok_or_else(|| format!("显示地址 {spec} 的写法不对，应当像 localhost:0 或 :0"))?;
    let host = match host {
        "" | "unix" => "localhost",
        other => other,
    };

    let (number, screen) = match tail.split_once('.') {
        Some((number, screen)) => (number, Some(screen)),
        None => (tail, None),
    };
    let number: u32 = number
        .parse()
        .map_err(|_| format!("显示地址 {spec} 里的显示号不是数字"))?;
    let screen: u32 = match screen {
        None => 0,
        Some(text) => text
            .parse()
            .map_err(|_| format!("显示地址 {spec} 里的屏幕号不是数字"))?,
    };
    let port = if number >= 100 {
        u16::try_from(number).map_err(|_| format!("显示地址 {spec} 里的端口超出范围"))?
    } else {
        X_TCP_BASE + number as u16
    };
    Ok(XEndpoint {
        host: host.to_owned(),
        port,
        screen,
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

    fn desc(spec: &str) -> String {
        parse_display(spec).expect(spec).describe()
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
        assert_eq!(parse_display("localhost:0.3").unwrap().screen, 3);
        assert_eq!(parse_display("localhost:0").unwrap().screen, 0);
    }

    #[test]
    fn unusable_spellings_are_errors_not_display_zero() {
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
            assert!(parse_display(bad).is_err(), "{bad} 应当报错");
        }
    }

    #[test]
    fn loopback_detection() {
        assert!(parse_display(":0").unwrap().is_loopback());
        assert!(parse_display("127.0.0.1:1").unwrap().is_loopback());
        assert!(!parse_display("xhost.example:1").unwrap().is_loopback());
        assert_eq!(XEndpoint::loopback(20).port, 6020);
    }
}
