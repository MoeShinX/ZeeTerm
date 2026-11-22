// Adapted from nyaterm (https://github.com/nyakang/nyaterm, Apache-2.0,
// Copyright 2026 Kang and contributors): the X11 connection-setup packet
// handling below (packet length, byte-order aware field reads, cookie
// replacement, hex helpers) follows crates/nyaterm-transport/src/x11.rs
// (X11AuthRewriter, setup_packet_len, rewrite_x11_auth_setup_packet).
// Changes: the cookie is looked up in a distribution table before it is
// replaced (see registry.rs), parsing is bounds-checked and returns the
// protocol name and data instead of rewriting in place, and the fake cookie
// comes from the OS random source.

//! 假 cookie 与 X11 握手包。
//!
//! 为什么要假 cookie：sshd 会把我们给的 cookie 写进远端的 `.Xauthority`，远端程序带着它
//! 连 X11 通道。真 cookie 是本机 X 服务器的口令，不该交给远端；所以只给远端一个随机的假值，
//! 通道进来时在握手包里换成真值再交给本机 X 服务器。

/// 唯一支持的 X11 认证协议名。
pub const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

/// cookie 长度（字节）。X 服务器自己生成的也是 16 字节。
pub const COOKIE_LEN: usize = 16;

/// 生成一个随机假 cookie。随机源失败时返回 `None`（调用方当作无法开 X11，不能退回固定值）。
pub fn random_cookie() -> Option<[u8; COOKIE_LEN]> {
    let mut cookie = [0u8; COOKIE_LEN];
    getrandom::fill(&mut cookie).ok()?;
    Some(cookie)
}

/// 小写十六进制。
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 十六进制解码；长度是奇数或含非法字符返回 `None`。
pub fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some((hex_value(pair[0])? << 4) | hex_value(pair[1])?))
        .collect()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// 握手包固定头的长度（字节序、版本、两个长度字段）。
const SETUP_HEADER: usize = 12;

fn read_u16(buffer: &[u8], offset: usize) -> Option<u16> {
    let bytes = [*buffer.get(offset)?, *buffer.get(offset + 1)?];
    match *buffer.first()? {
        b'l' => Some(u16::from_le_bytes(bytes)),
        b'B' => Some(u16::from_be_bytes(bytes)),
        _ => None,
    }
}

/// [`setup_packet_len`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupLen {
    /// 数据还不够读出长度字段，继续等。
    NeedMore,
    /// 握手包的总长度（字节）。
    Total(usize),
    /// 字节序标记不是 `l`/`B`：这不是 X11 客户端。
    Invalid,
}

/// 握手包的总长度。
pub fn setup_packet_len(buffer: &[u8]) -> SetupLen {
    let Some(&order) = buffer.first() else {
        return SetupLen::NeedMore;
    };
    if order != b'l' && order != b'B' {
        return SetupLen::Invalid;
    }
    if buffer.len() < SETUP_HEADER {
        return SetupLen::NeedMore;
    }
    let (Some(protocol_len), Some(data_len)) = (read_u16(buffer, 6), read_u16(buffer, 8)) else {
        return SetupLen::Invalid;
    };
    SetupLen::Total(SETUP_HEADER + pad4(protocol_len as usize) + pad4(data_len as usize))
}

/// 完整握手包里的认证协议名和认证数据。包不完整或长度字段对不上返回 `None`。
pub fn setup_auth(packet: &[u8]) -> Option<(&[u8], &[u8])> {
    let protocol_len = read_u16(packet, 6)? as usize;
    let data_len = read_u16(packet, 8)? as usize;
    let protocol_start = SETUP_HEADER;
    let data_start = protocol_start + pad4(protocol_len);
    let protocol = packet.get(protocol_start..protocol_start + protocol_len)?;
    let data = packet.get(data_start..data_start + data_len)?;
    Some((protocol, data))
}

/// 把握手包里的认证数据换成 `real`（长度必须和原来一致，包长度才不变）。
/// 协议名不是 `MIT-MAGIC-COOKIE-1` 或长度不一致时不改，返回 `false`。
pub fn replace_cookie(packet: &mut [u8], real: &[u8]) -> bool {
    let (Some(protocol_len), Some(data_len)) = (read_u16(packet, 6), read_u16(packet, 8)) else {
        return false;
    };
    let protocol_len = protocol_len as usize;
    let data_len = data_len as usize;
    let data_start = SETUP_HEADER + pad4(protocol_len);
    if data_len != real.len() || data_start + data_len > packet.len() {
        return false;
    }
    if packet.get(SETUP_HEADER..SETUP_HEADER + protocol_len) != Some(MIT_MAGIC_COOKIE.as_bytes()) {
        return false;
    }
    packet[data_start..data_start + data_len].copy_from_slice(real);
    true
}

#[cfg(test)]
pub(crate) fn test_setup_packet(order: u8, protocol: &[u8], cookie: &[u8]) -> Vec<u8> {
    let mut packet = vec![order, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let protocol_len = protocol.len() as u16;
    let cookie_len = cookie.len() as u16;
    let (protocol_bytes, cookie_bytes) = if order == b'l' {
        (protocol_len.to_le_bytes(), cookie_len.to_le_bytes())
    } else {
        (protocol_len.to_be_bytes(), cookie_len.to_be_bytes())
    };
    packet[6..8].copy_from_slice(&protocol_bytes);
    packet[8..10].copy_from_slice(&cookie_bytes);
    packet.extend_from_slice(protocol);
    packet.resize(SETUP_HEADER + pad4(protocol.len()), 0);
    packet.extend_from_slice(cookie);
    packet.resize(SETUP_HEADER + pad4(protocol.len()) + pad4(cookie.len()), 0);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_cookies_are_16_bytes_and_differ() {
        let a = random_cookie().expect("随机源");
        let b = random_cookie().expect("随机源");
        assert_eq!(a.len(), COOKIE_LEN);
        assert_ne!(a, b, "两次生成撞车说明随机源有问题");
        assert_ne!(a, [0u8; COOKIE_LEN]);
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        let bytes = [0x00, 0x7f, 0xab, 0xff];
        assert_eq!(encode_hex(&bytes), "007fabff");
        assert_eq!(decode_hex("007FABff").as_deref(), Some(&bytes[..]));
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("").as_deref(), Some(&[][..]));
    }

    #[test]
    fn packet_length_waits_for_the_header_and_follows_byte_order() {
        let fake = [1u8; 16];
        for order in *b"lB" {
            let packet = test_setup_packet(order, MIT_MAGIC_COOKIE.as_bytes(), &fake);
            assert_eq!(setup_packet_len(&packet[..5]), SetupLen::NeedMore);
            assert_eq!(setup_packet_len(&packet[..11]), SetupLen::NeedMore);
            assert_eq!(setup_packet_len(&packet), SetupLen::Total(packet.len()));
            // 后面跟着别的请求数据时，长度仍是握手包本身的长度。
            let mut longer = packet.clone();
            longer.extend_from_slice(&[9; 40]);
            assert_eq!(setup_packet_len(&longer), SetupLen::Total(packet.len()));
        }
        assert_eq!(setup_packet_len(b"G"), SetupLen::Invalid);
        assert_eq!(setup_packet_len(b""), SetupLen::NeedMore);
    }

    #[test]
    fn auth_is_extracted_and_cookie_replaced_in_both_byte_orders() {
        let fake = [1u8; 16];
        let real = [2u8; 16];
        for order in *b"lB" {
            let mut packet = test_setup_packet(order, MIT_MAGIC_COOKIE.as_bytes(), &fake);
            let (protocol, data) = setup_auth(&packet).expect("解析认证");
            assert_eq!(protocol, MIT_MAGIC_COOKIE.as_bytes());
            assert_eq!(data, fake);
            assert!(replace_cookie(&mut packet, &real));
            assert_eq!(setup_auth(&packet).map(|(_, d)| d), Some(&real[..]));
            assert!(!packet.windows(16).any(|w| w == fake));
        }
    }

    #[test]
    fn replacement_refuses_foreign_protocols_and_wrong_lengths() {
        let fake = [1u8; 16];
        let mut packet = test_setup_packet(b'l', b"OTHER", &fake);
        assert!(!replace_cookie(&mut packet, &[2u8; 16]));
        let mut packet = test_setup_packet(b'l', MIT_MAGIC_COOKIE.as_bytes(), &fake);
        assert!(
            !replace_cookie(&mut packet, &[2u8; 8]),
            "长度不同会改变包长"
        );
        let before = packet.clone();
        assert!(!replace_cookie(&mut packet, &[2u8; 15]));
        assert_eq!(packet, before, "失败时不能改动包");
        // 截断的包不会 panic。
        let mut cut = before[..20].to_vec();
        assert!(!replace_cookie(&mut cut, &[2u8; 16]));
        assert_eq!(setup_auth(&before[..20]), None);
    }

    #[test]
    fn a_setup_packet_without_auth_has_empty_fields() {
        let packet = test_setup_packet(b'l', b"", b"");
        assert_eq!(packet.len(), 12);
        assert_eq!(setup_auth(&packet), Some((&[][..], &[][..])));
    }
}
