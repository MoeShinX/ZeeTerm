//! Xauthority 文件（`-auth` 参数用的授权文件）的读写。
//!
//! 格式见 libXau：每条记录依次是 family（u16 大端）、address / number / name / data 四个
//! "长度（u16 大端）+ 字节"。X 服务器启动时读这个文件，把每条记录的 name + data 登记成
//! 允许的认证。我们只写一条 `MIT-MAGIC-COOKIE-1`，family 用 FamilyWild（0xFFFF），
//! 地址留空：服务器不看这两项，本机其他程序也不会读这个文件。
//!
//! Linux 上还要读用户会话的授权文件（`$XAUTHORITY` 或 `~/.Xauthority`），按显示号和 family / 地址
//! 找出当前显示的 cookie（[`find_cookie`]）：Xorg / Xwayland 的记录通常是 FamilyLocal + 本机主机名，
//! 也可能是 FamilyWild 或不限显示号。

use std::path::Path;

use crate::x11::cookie::MIT_MAGIC_COOKIE;

/// libXau 的 FamilyWild。
const FAMILY_WILD: u16 = 0xFFFF;

fn push_counted(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// 序列化一条 MIT-MAGIC-COOKIE-1 记录。
pub fn encode_entry(display: u16, cookie: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&FAMILY_WILD.to_be_bytes());
    push_counted(&mut out, b"");
    push_counted(&mut out, display.to_string().as_bytes());
    push_counted(&mut out, MIT_MAGIC_COOKIE.as_bytes());
    push_counted(&mut out, cookie);
    out
}

/// 从授权文件内容里取出第一条 MIT-MAGIC-COOKIE-1 的数据。格式不对或没有就是 `None`。
pub fn first_mit_cookie(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut rest = bytes;
    let take_u16 = |rest: &mut &[u8]| -> Option<usize> {
        let (head, tail) = rest.split_first_chunk::<2>()?;
        *rest = tail;
        Some(u16::from_be_bytes(*head) as usize)
    };
    while !rest.is_empty() {
        let _family = take_u16(&mut rest)?;
        let mut fields: [&[u8]; 4] = [&[]; 4];
        for field in &mut fields {
            let len = take_u16(&mut rest)?;
            if rest.len() < len {
                return None;
            }
            let (value, tail) = rest.split_at(len);
            *field = value;
            rest = tail;
        }
        let [_address, _number, name, data] = fields;
        if name == MIT_MAGIC_COOKIE.as_bytes() {
            return Some(data.to_vec());
        }
    }
    None
}

/// 写授权文件（覆盖旧的）。文件放在用户自己的数据目录里，继承该目录的访问权限。
pub fn write_file(path: &Path, display: u16, cookie: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, encode_entry(display, cookie))
}

/// 读授权文件里的 cookie。读不了或格式不对是 `None`。
pub fn read_cookie(path: &Path) -> Option<Vec<u8>> {
    first_mit_cookie(&std::fs::read(path).ok()?)
}

/// libXau 的 FamilyInternet（IPv4，地址 4 字节）。
const FAMILY_INTERNET: u16 = 0;
/// libXau 的 FamilyInternet6（IPv6，地址 16 字节）。
const FAMILY_INTERNET6: u16 = 6;
/// libXau 的 FamilyLocal（本机的 Unix 套接字连接，地址是本机主机名）。
const FAMILY_LOCAL: u16 = 256;

/// 授权文件里的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub family: u16,
    pub address: Vec<u8>,
    /// 显示号的十进制文本；空 = 任意显示号。
    pub number: Vec<u8>,
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

/// 解析整个授权文件。读到格式不对（被截断）的地方就停，前面完整的记录照常返回。
pub fn parse_entries(bytes: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut rest = bytes;
    let take_u16 = |rest: &mut &[u8]| -> Option<usize> {
        let (head, tail) = rest.split_first_chunk::<2>()?;
        *rest = tail;
        Some(u16::from_be_bytes(*head) as usize)
    };
    'records: while !rest.is_empty() {
        let Some(family) = take_u16(&mut rest) else {
            break;
        };
        let mut fields: [Vec<u8>; 4] = Default::default();
        for field in &mut fields {
            let Some(len) = take_u16(&mut rest) else {
                break 'records;
            };
            if rest.len() < len {
                break 'records;
            }
            let (value, tail) = rest.split_at(len);
            *field = value.to_vec();
            rest = tail;
        }
        let [address, number, name, data] = fields;
        entries.push(Entry {
            family: family as u16,
            address,
            number,
            name,
            data,
        });
    }
    entries
}

/// 要连的 X 服务器，用来在授权文件里找对应的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTarget {
    /// 显示号；不知道（比如显式写了 TCP 端口）就是 `None`，这时只认不限显示号的记录。
    pub display: Option<u32>,
    /// 本机主机名。FamilyLocal 的记录按它匹配：Unix 套接字连接和回环 TCP 连接都算"本机"。
    pub local_hostname: Option<String>,
    /// 非本机的 TCP 主机是 IP 字面量时的地址，用来匹配 FamilyInternet / FamilyInternet6。
    pub ip: Option<std::net::IpAddr>,
}

impl AuthTarget {
    /// 本机的 X 服务器（Unix 套接字或回环 TCP）。
    pub fn local(display: Option<u32>, hostname: Option<String>) -> Self {
        Self {
            display,
            local_hostname: hostname,
            ip: None,
        }
    }
}

fn entry_matches(entry: &Entry, target: &AuthTarget) -> bool {
    if entry.name != MIT_MAGIC_COOKIE.as_bytes() {
        return false;
    }
    // 空显示号 = 任意显示号（libXau 同样处理）。
    if !entry.number.is_empty() {
        let Some(display) = target.display else {
            return false;
        };
        if entry.number != display.to_string().as_bytes() {
            return false;
        }
    }
    match entry.family {
        FAMILY_WILD => true,
        FAMILY_LOCAL => target
            .local_hostname
            .as_deref()
            .is_some_and(|host| entry.address == host.as_bytes()),
        FAMILY_INTERNET => matches!(
            target.ip,
            Some(std::net::IpAddr::V4(ip)) if entry.address == ip.octets()
        ),
        FAMILY_INTERNET6 => matches!(
            target.ip,
            Some(std::net::IpAddr::V6(ip)) if entry.address == ip.octets()
        ),
        _ => false,
    }
}

/// 在授权文件内容里找 `target` 对应的 MIT-MAGIC-COOKIE-1 数据（按文件里的顺序取第一条匹配的）。
///
/// 只认 16 字节的 cookie：发给远端的假 cookie 固定 16 字节，握手包里换 cookie 要求长度一致，
/// 别的长度换不进去，不如当作"没找到"，由调用方按"拿不到真 cookie"处理。
pub fn find_cookie(bytes: &[u8], target: &AuthTarget) -> Option<Vec<u8>> {
    parse_entries(bytes)
        .into_iter()
        .find(|entry| {
            entry.data.len() == crate::x11::cookie::COOKIE_LEN && entry_matches(entry, target)
        })
        .map(|entry| entry.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_has_the_libxau_layout_and_round_trips() {
        let cookie: Vec<u8> = (0u8..16).collect();
        let bytes = encode_entry(20, &cookie);
        // family(2) + address(2) + number(2+2) + name(2+18) + data(2+16)
        assert_eq!(bytes.len(), 2 + 2 + 4 + 20 + 18);
        assert_eq!(&bytes[..2], &[0xFF, 0xFF]);
        assert_eq!(&bytes[2..4], &[0, 0], "地址为空");
        assert_eq!(&bytes[4..8], &[0, 2, b'2', b'0'], "显示号");
        assert_eq!(&bytes[8..10], &[0, 18]);
        assert_eq!(&bytes[10..28], MIT_MAGIC_COOKIE.as_bytes());
        assert_eq!(&bytes[28..30], &[0, 16]);
        assert_eq!(&bytes[30..], &cookie[..]);
        assert_eq!(first_mit_cookie(&bytes), Some(cookie));
    }

    #[test]
    fn the_first_mit_entry_wins_and_other_protocols_are_skipped() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&FAMILY_WILD.to_be_bytes());
        push_counted(&mut bytes, b"");
        push_counted(&mut bytes, b"0");
        push_counted(&mut bytes, b"XDM-AUTHORIZATION-1");
        push_counted(&mut bytes, &[9; 8]);
        bytes.extend(encode_entry(1, &[7; 16]));
        bytes.extend(encode_entry(2, &[8; 16]));
        assert_eq!(first_mit_cookie(&bytes), Some(vec![7; 16]));
    }

    #[test]
    fn truncated_or_empty_files_yield_nothing() {
        let bytes = encode_entry(3, &[5; 16]);
        assert_eq!(first_mit_cookie(&[]), None);
        for cut in 1..bytes.len() {
            assert_eq!(first_mit_cookie(&bytes[..cut]), None, "截断到 {cut}");
        }
    }

    #[test]
    fn write_and_read_through_a_file() {
        let dir = crate::test_util::TestDir::new("xauth-file");
        let path = dir.join("nested").join("xauth-20");
        write_file(&path, 20, &[3; 16]).expect("写");
        assert_eq!(read_cookie(&path), Some(vec![3; 16]));
        write_file(&path, 20, &[4; 16]).expect("覆盖");
        assert_eq!(read_cookie(&path), Some(vec![4; 16]));
        assert_eq!(read_cookie(&dir.join("missing")), None);
    }

    /// 构造一条任意 family / 地址 / 显示号 / 协议名的记录。
    fn record(family: u16, address: &[u8], number: &str, name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&family.to_be_bytes());
        push_counted(&mut out, address);
        push_counted(&mut out, number.as_bytes());
        push_counted(&mut out, name.as_bytes());
        push_counted(&mut out, data);
        out
    }

    fn local(display: u32) -> AuthTarget {
        AuthTarget::local(Some(display), Some("devbox".to_owned()))
    }

    const MIT: &str = "MIT-MAGIC-COOKIE-1";

    #[test]
    fn a_local_record_matches_by_host_name_and_display_number() {
        let mut file = Vec::new();
        file.extend(record(FAMILY_LOCAL, b"devbox", "0", MIT, &[1; 16]));
        file.extend(record(FAMILY_LOCAL, b"devbox", "1", MIT, &[2; 16]));
        file.extend(record(FAMILY_LOCAL, b"otherhost", "1", MIT, &[3; 16]));
        assert_eq!(find_cookie(&file, &local(0)), Some(vec![1; 16]));
        assert_eq!(find_cookie(&file, &local(1)), Some(vec![2; 16]));
        assert_eq!(find_cookie(&file, &local(7)), None, "没有这个显示号的记录");
        // 本机主机名不一样（改过主机名、或拿了别的机器的文件）：不能用别人的记录。
        assert_eq!(
            find_cookie(
                &file,
                &AuthTarget::local(Some(1), Some("renamed".to_owned()))
            ),
            None
        );
        assert_eq!(find_cookie(&file, &AuthTarget::local(Some(1), None)), None);
    }

    #[test]
    fn wild_records_match_any_host_and_empty_numbers_match_any_display() {
        let wild = record(FAMILY_WILD, b"", "5", MIT, &[4; 16]);
        assert_eq!(find_cookie(&wild, &local(5)), Some(vec![4; 16]));
        assert_eq!(find_cookie(&wild, &local(6)), None);
        assert_eq!(
            find_cookie(&wild, &AuthTarget::local(Some(5), None)),
            Some(vec![4; 16]),
            "通配记录不看主机名"
        );

        let any_display = record(FAMILY_LOCAL, b"devbox", "", MIT, &[5; 16]);
        assert_eq!(find_cookie(&any_display, &local(42)), Some(vec![5; 16]));
        // 不知道显示号（显式写了 TCP 端口）时，只认不限显示号的记录。
        assert_eq!(
            find_cookie(
                &any_display,
                &AuthTarget::local(None, Some("devbox".to_owned()))
            ),
            Some(vec![5; 16])
        );
        let numbered = record(FAMILY_LOCAL, b"devbox", "0", MIT, &[6; 16]);
        assert_eq!(
            find_cookie(
                &numbered,
                &AuthTarget::local(None, Some("devbox".to_owned()))
            ),
            None
        );
    }

    #[test]
    fn the_first_matching_record_wins_in_file_order() {
        let mut file = Vec::new();
        file.extend(record(FAMILY_LOCAL, b"elsewhere", "0", MIT, &[9; 16]));
        file.extend(record(FAMILY_WILD, b"", "0", MIT, &[1; 16]));
        file.extend(record(FAMILY_LOCAL, b"devbox", "0", MIT, &[2; 16]));
        assert_eq!(find_cookie(&file, &local(0)), Some(vec![1; 16]));
    }

    #[test]
    fn other_protocols_and_odd_cookie_lengths_are_skipped() {
        let mut file = Vec::new();
        file.extend(record(
            FAMILY_LOCAL,
            b"devbox",
            "0",
            "XDM-AUTHORIZATION-1",
            &[7; 16],
        ));
        file.extend(record(FAMILY_LOCAL, b"devbox", "0", MIT, &[8; 20]));
        file.extend(record(FAMILY_LOCAL, b"devbox", "0", MIT, &[9; 16]));
        assert_eq!(find_cookie(&file, &local(0)), Some(vec![9; 16]));
        assert_eq!(find_cookie(&file[..file.len() - 1], &local(0)), None);
    }

    #[test]
    fn internet_records_match_ip_literals_only() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        let v6 = Ipv6Addr::LOCALHOST;
        let mut file = Vec::new();
        file.extend(record(
            FAMILY_INTERNET,
            &[192, 168, 1, 9],
            "2",
            MIT,
            &[1; 16],
        ));
        file.extend(record(FAMILY_INTERNET6, &v6.octets(), "2", MIT, &[2; 16]));
        let target = |ip: Option<IpAddr>| AuthTarget {
            display: Some(2),
            local_hostname: None,
            ip,
        };
        assert_eq!(
            find_cookie(
                &file,
                &target(Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9))))
            ),
            Some(vec![1; 16])
        );
        assert_eq!(
            find_cookie(&file, &target(Some(IpAddr::V6(v6)))),
            Some(vec![2; 16])
        );
        assert_eq!(
            find_cookie(&file, &target(Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))))),
            None
        );
        assert_eq!(find_cookie(&file, &target(None)), None);
    }

    #[test]
    fn a_truncated_file_keeps_the_complete_records_before_the_cut() {
        let mut file = record(FAMILY_LOCAL, b"devbox", "0", MIT, &[1; 16]);
        let first = file.len();
        file.extend(record(FAMILY_LOCAL, b"devbox", "1", MIT, &[2; 16]));
        for cut in first..file.len() {
            assert_eq!(parse_entries(&file[..cut]).len(), 1, "截断到 {cut}");
            assert_eq!(find_cookie(&file[..cut], &local(0)), Some(vec![1; 16]));
            assert_eq!(find_cookie(&file[..cut], &local(1)), None);
        }
        assert_eq!(parse_entries(&file).len(), 2);
        assert!(parse_entries(&[]).is_empty());
        assert!(parse_entries(&[0xff]).is_empty());
    }

    #[test]
    fn a_file_written_by_this_module_is_found_through_the_wild_record() {
        let bytes = encode_entry(20, &[3; 16]);
        assert_eq!(find_cookie(&bytes, &local(20)), Some(vec![3; 16]));
        assert_eq!(find_cookie(&bytes, &local(21)), None);
    }
}
