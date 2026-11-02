//! Xauthority 文件（`-auth` 参数用的授权文件）的读写。
//!
//! 格式见 libXau：每条记录依次是 family（u16 大端）、address / number / name / data 四个
//! "长度（u16 大端）+ 字节"。X 服务器启动时读这个文件，把每条记录的 name + data 登记成
//! 允许的认证。我们只写一条 `MIT-MAGIC-COOKIE-1`，family 用 FamilyWild（0xFFFF），
//! 地址留空：服务器不看这两项，本机其他程序也不会读这个文件。

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
}
