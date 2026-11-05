//! 打开 / 保存的内容契约：能不能打开、BOM 和换行怎么记、版本怎么算。
//!
//! 全部复用 0.2.14 的做法（`zssh_core::sftp::edit::text`，Tauri 版内置编辑器也是它）：
//! - 只接受合法 UTF-8（可以带 BOM）；UTF-16、含 NUL（二进制）、LF 和 CRLF 混用或单独的 CR 一律不打开，
//!   因为硬打开再保存会损坏内容，或者悄悄把整个文件的换行改掉；
//! - 编辑器里的文本一律是 `\n`，打开时记下 BOM / 换行风格，保存时原样还原，没改的部分逐字节不变；
//! - 大小上限是核心的 [`MAX_EDIT_BYTES`]（5 MiB），不是原型阶段自己定的 4 MiB。

use zssh_core::sftp::edit::MAX_EDIT_BYTES;
use zssh_core::sftp::edit::recovery::sha256_hex;
use zssh_core::sftp::edit::text::{self, Eol, TextFormat};

/// 单文件上限（字节）。
pub(super) const MAX_BYTES: usize = MAX_EDIT_BYTES as usize;

/// 远端内容版本：整份字节的 SHA-256 取前 8 字节，只用来比较两次读到的是不是同一份内容。
///
/// 以前用 `DefaultHasher`：它不保证跨 Rust 版本稳定，而草稿里会把这个值存到磁盘、下次启动再比，
/// 工具链一升级旧草稿就全部变成「基于旧版本」。SHA-256 没有这个问题，也不会撞车。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct RemoteVersion(pub(super) u64);

pub(super) fn version_of(bytes: &[u8]) -> RemoteVersion {
    let hex = sha256_hex(bytes);
    RemoteVersion(u64::from_str_radix(&hex[..16], 16).unwrap_or(0))
}

/// 打开成功的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Opened {
    /// 编辑器里的文本（换行统一成 `\n`，没有 BOM）。
    pub(super) text: String,
    pub(super) format: TextFormat,
    /// 文件原始字节的版本（冲突判断认它）。
    pub(super) version: RemoteVersion,
    pub(super) size: usize,
}

/// 把远端读回来的字节解成可编辑的文本；不适合内置编辑器的给出原因（界面直接显示）。
pub(super) fn open_bytes(bytes: &[u8]) -> Result<Opened, String> {
    if bytes.len() > MAX_BYTES {
        return Err(too_large(bytes.len()));
    }
    let decoded = text::decode(bytes).map_err(|refusal| refusal.message())?;
    Ok(Opened {
        text: decoded.text,
        format: decoded.format,
        version: version_of(bytes),
        size: bytes.len(),
    })
}

/// 打开 / 保存时给用户看的两句话。
#[derive(Clone, Copy, Debug)]
enum ContractMsg {
    TooLargePrefix,
    TooLargeMid,
    TooLargeSuffix,
    ByteSuffix,
}

impl crate::i18n::Msg for ContractMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::TooLargePrefix => "文件有 ",
            Self::TooLargeMid => "，超过内置编辑器的上限（",
            Self::TooLargeSuffix => "）。请下载后再看",
            Self::ByteSuffix => " 字节",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::TooLargePrefix => "The file is ",
            Self::TooLargeMid => ", over the built-in editor limit (",
            Self::TooLargeSuffix => "). Download it to view",
            Self::ByteSuffix => " bytes",
        }
    }
}

/// 超过上限的说法（读文件时发现、保存时发现共用）。
pub(super) fn too_large(size: usize) -> String {
    format!(
        "{}{}{}{}{}",
        crate::i18n::t(ContractMsg::TooLargePrefix),
        crate::i18n::count(
            size as u64,
            crate::i18n::t(ContractMsg::ByteSuffix),
            "byte",
            "bytes"
        ),
        crate::i18n::t(ContractMsg::TooLargeMid),
        crate::i18n::count(
            MAX_BYTES as u64,
            crate::i18n::t(ContractMsg::ByteSuffix),
            "byte",
            "bytes"
        ),
        crate::i18n::t(ContractMsg::TooLargeSuffix)
    )
}

/// 保存时把编辑器里的文本还原成文件字节（BOM、换行按打开时的样子）。
pub(super) fn encode(text: &str, format: &TextFormat) -> Vec<u8> {
    text::encode(text, format)
}

/// 状态栏上的编码（同 0.2.14 `encodingLabel`）。
pub(super) fn encoding_label(format: &TextFormat) -> &'static str {
    if format.bom { "UTF-8 (BOM)" } else { "UTF-8" }
}

pub(super) fn eol_label(format: &TextFormat) -> &'static str {
    match format.eol {
        Eol::Lf => "LF",
        Eol::Crlf => "CRLF",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_utf8_opens_and_round_trips() {
        let bytes = "第一行\nsecond line\n".as_bytes();
        let opened = open_bytes(bytes).expect("UTF-8 文本");
        assert_eq!(opened.text.as_bytes(), bytes);
        assert_eq!(encode(&opened.text, &opened.format), bytes);
        assert_eq!(encoding_label(&opened.format), "UTF-8");
        assert_eq!(eol_label(&opened.format), "LF");
        assert_eq!(open_bytes(b"").expect("空文件").text, "");
    }

    /// 卡里要求的：CRLF / BOM 文件原样往返，编辑器里看到的只有 `\n`。
    #[test]
    fn bom_and_crlf_survive_a_round_trip_byte_for_byte() {
        let bytes = b"\xEF\xBB\xBFone\r\ntwo\r\n\xE4\xB8\xAD\r\n";
        let opened = open_bytes(bytes).expect("带 BOM 的 CRLF 文本");
        assert_eq!(opened.text, "one\ntwo\n中\n");
        assert_eq!(encoding_label(&opened.format), "UTF-8 (BOM)");
        assert_eq!(eol_label(&opened.format), "CRLF");
        assert_eq!(encode(&opened.text, &opened.format), bytes);
        // 改一个字再还原：只有改的那个字变，BOM 和 CRLF 照旧。
        let edited = opened.text.replace("two", "2");
        assert_eq!(
            encode(&edited, &opened.format),
            b"\xEF\xBB\xBFone\r\n2\r\n\xE4\xB8\xAD\r\n"
        );
    }

    #[test]
    fn lf_file_without_a_final_newline_stays_that_way() {
        let bytes = b"no newline at end";
        let opened = open_bytes(bytes).expect("文本");
        assert_eq!(encode(&opened.text, &opened.format), bytes);
    }

    #[test]
    fn files_that_would_be_damaged_by_saving_are_refused() {
        // 二进制（NUL）。
        assert!(open_bytes(b"abc\0def").is_err());
        // 不是 UTF-8。
        assert!(open_bytes(&[0xff, 0xfe, 0xfa]).is_err());
        // UTF-16（带 BOM）。
        assert!(open_bytes(b"\xFF\xFEa\0b\0").is_err());
        // 换行混用：保存会统一换行、改动整个文件。
        let mixed = open_bytes(b"a\r\nb\nc\r\n").expect_err("混用换行不打开");
        assert!(mixed.contains("换行"), "{mixed}");
        // 单独的 CR。
        assert!(open_bytes(b"a\rb\n").is_err());
    }

    #[test]
    fn the_size_limit_is_the_core_limit() {
        assert!(open_bytes(&vec![b'a'; MAX_BYTES]).is_ok());
        let message = open_bytes(&vec![b'a'; MAX_BYTES + 1]).expect_err("超限不打开");
        assert!(message.contains("超过内置编辑器的上限"), "{message}");
        assert_eq!(MAX_BYTES, 5 * 1024 * 1024);
    }

    #[test]
    fn version_depends_on_every_byte_and_is_stable() {
        assert_eq!(version_of(b"same"), version_of(b"same"));
        assert_ne!(version_of(b"same"), version_of(b"same\n"));
        assert_ne!(version_of(b""), version_of(b" "));
        // 稳定性：值固定（SHA-256 前 8 字节），不随工具链变；草稿里存的就是它。
        assert_eq!(version_of(b"").0, 0xe3b0_c442_98fc_1c14);
    }

    /// BOM 和 CRLF 不同的两份内容版本不同（保存后才不会被当成「没变」）。
    #[test]
    fn eol_style_changes_the_version() {
        assert_ne!(version_of(b"a\nb\n"), version_of(b"a\r\nb\r\n"));
    }
}
