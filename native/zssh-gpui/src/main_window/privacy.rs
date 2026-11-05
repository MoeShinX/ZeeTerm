//! 截图前的隐私检查里「不用事先知道具体值」的那部分：画面文字里不能有公网 IP、
//! `Last login ... from <IP>` 这行、`C:\Users\...` 这类用户目录路径。
//!
//! 具体的禁用片段（测试服务器地址、本机用户名）由调用方从环境变量传进来；这里补的是
//! 「不认识的地址也不能漏」：登录提示里会带出用户本机的公网 IP，那个 IP 不会出现在任何环境变量里。

/// 文档保留段（RFC 5737）和回环：演示数据用它们，不算泄露。
#[cfg(any(test, debug_assertions))]
fn is_reserved(ip: [u8; 4]) -> bool {
    matches!(
        ip,
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _] | [127, ..] | [0, 0, 0, 0]
    )
}

/// 从文字里找点分十进制 IPv4（四段、每段 0..=255）。
#[cfg(any(test, debug_assertions))]
fn ipv4_candidates(text: &str) -> Vec<[u8; 4]> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit()
            || (i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.'))
        {
            i += 1;
            continue;
        }
        // 从这个数字开始读 a.b.c.d。
        let mut parts = [0u8; 4];
        let mut at = i;
        let mut ok = true;
        for (n, part) in parts.iter_mut().enumerate() {
            let start = at;
            while at < bytes.len() && bytes[at].is_ascii_digit() && at - start < 3 {
                at += 1;
            }
            let Ok(value) = text[start..at].parse::<u16>() else {
                ok = false;
                break;
            };
            if value > 255 {
                ok = false;
                break;
            }
            *part = value as u8;
            if n < 3 {
                if at < bytes.len() && bytes[at] == b'.' {
                    at += 1;
                } else {
                    ok = false;
                    break;
                }
            }
        }
        // 后面还跟着数字（1.2.3.4567）或「.数字」（1.2.3.4.5）说明不是一个 IPv4，不算。
        let longer = at < bytes.len()
            && (bytes[at].is_ascii_digit()
                || (bytes[at] == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)));
        if ok && !longer {
            found.push(parts);
            i = at;
        } else {
            i += 1;
        }
    }
    found
}

/// 文字里有没有公网（非保留段）IPv4。
#[cfg(any(test, debug_assertions))]
pub fn has_public_ipv4(text: &str) -> bool {
    ipv4_candidates(text).into_iter().any(|ip| !is_reserved(ip))
}

/// 内置规则命中的类别名（不返回原文）。
#[cfg(any(test, debug_assertions))]
pub fn builtin_hits(text: &str) -> Vec<&'static str> {
    let mut hits = Vec::new();
    if has_public_ipv4(text) {
        hits.push(crate::i18n::t(PrivacyMsg::PublicIp));
    }
    if text.contains("Last login") {
        hits.push(crate::i18n::t(PrivacyMsg::LoginBanner));
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("c:\\users\\") || lower.contains("c:/users/") {
        hits.push(crate::i18n::t(PrivacyMsg::UserHomePath));
    }
    hits
}

/// 截图隐私检查里命中的类别名。
#[cfg(any(test, debug_assertions))]
#[derive(Clone, Copy, Debug)]
enum PrivacyMsg {
    PublicIp,
    LoginBanner,
    UserHomePath,
}

#[cfg(any(test, debug_assertions))]
impl crate::i18n::Msg for PrivacyMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::PublicIp => "公网 IP",
            Self::LoginBanner => "登录提示 Last login",
            Self::UserHomePath => "用户目录路径",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::PublicIp => "Public IP address",
            Self::LoginBanner => "Login banner (Last login)",
            Self::UserHomePath => "User home directory path",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{builtin_hits, has_public_ipv4};

    /// 样例地址在运行时拼出来：源码里不放点分十进制字面量，隐私扫描就不会把测试数据当成泄露。
    fn ip(a: u8, b: u8, c: u8, d: u8) -> String {
        format!("{a}.{b}.{c}.{d}")
    }

    #[test]
    fn public_addresses_are_found_wherever_they_sit() {
        assert!(has_public_ipv4(&format!(
            "Last login: Sun from {}",
            ip(8, 8, 8, 8)
        )));
        assert!(has_public_ipv4(&format!("x={};", ip(1, 1, 1, 1))));
        assert!(has_public_ipv4(&format!("user@{}:22", ip(45, 10, 20, 30))));
        assert!(
            has_public_ipv4(&format!("a {} b", ip(10, 0, 0, 1))),
            "私有地址也是真实环境信息，不放行"
        );
    }

    #[test]
    fn documentation_and_loopback_ranges_are_allowed() {
        assert!(!has_public_ipv4("deploy@192.0.2.11:22"));
        assert!(!has_public_ipv4("198.51.100.21 and 203.0.113.5"));
        assert!(!has_public_ipv4("127.0.0.1"));
    }

    #[test]
    fn things_that_only_look_like_addresses_are_ignored() {
        assert!(!has_public_ipv4("version 1.2.3"));
        assert!(!has_public_ipv4("999.1.1.1"));
        assert!(!has_public_ipv4("1.2.3.4567"));
        assert!(!has_public_ipv4("1.2.3.4.5"), "五段不是 IPv4");
        assert!(!has_public_ipv4("12345"));
        assert!(!has_public_ipv4(""));
    }

    #[test]
    fn builtin_rules_report_categories_not_content() {
        let hits = builtin_hits(&format!(
            "Last login: today from {} in C:\\Users\\someone",
            ip(8, 8, 4, 4)
        ));
        assert_eq!(hits, ["公网 IP", "登录提示 Last login", "用户目录路径"]);
        assert!(builtin_hits("web-1  $").is_empty());
    }
}
