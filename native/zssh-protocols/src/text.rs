//! Telnet / 串口字节流和终端之间的字符编码转换。
//!
//! 终端模型只认 UTF-8。远端用 GBK 等编码时：收到的字节要先转成 UTF-8 再交给终端，
//! 用户键入的 UTF-8 要先转成远端编码再发出去。UTF-8 本身不转换、原样透传：终端的
//! 解析器自己能处理跨包被切开的多字节字符。

use encoding_rs::{Decoder, Encoder, Encoding};
use zssh_core::model::TextEncoding;

/// 选项里的编码对应的 `encoding_rs` 编码。
pub fn encoding_for(encoding: TextEncoding) -> &'static Encoding {
    match encoding {
        TextEncoding::Utf8 => encoding_rs::UTF_8,
        TextEncoding::Gbk => encoding_rs::GBK,
        TextEncoding::Big5 => encoding_rs::BIG5,
        TextEncoding::ShiftJis => encoding_rs::SHIFT_JIS,
        TextEncoding::EucKr => encoding_rs::EUC_KR,
        // WHATWG 把 "latin1" 标签映射到 windows-1252（0x80..0x9F 是可打印字符而不是控制码）。
        TextEncoding::Latin1 => encoding_rs::WINDOWS_1252,
    }
}

/// 远端字节 → 终端 UTF-8。解码器带状态：多字节字符被切在两次读取之间也不会出乱码。
pub struct TextDecoder {
    inner: Option<Decoder>,
}

impl TextDecoder {
    pub fn new(encoding: TextEncoding) -> Self {
        let inner = (encoding != TextEncoding::Utf8).then(|| encoding_for(encoding).new_decoder());
        Self { inner }
    }

    pub fn decode(&mut self, bytes: &[u8]) -> Vec<u8> {
        let Some(decoder) = self.inner.as_mut() else {
            return bytes.to_vec();
        };
        let mut out = String::with_capacity(
            decoder
                .max_utf8_buffer_length(bytes.len())
                .unwrap_or(bytes.len().saturating_mul(4)),
        );
        let mut rest = bytes;
        loop {
            let (result, read, _) = decoder.decode_to_string(rest, &mut out, false);
            rest = &rest[read..];
            match result {
                encoding_rs::CoderResult::InputEmpty => break,
                // 预留的空间按理够用；万一不够就多给一点再来，不丢字节。
                encoding_rs::CoderResult::OutputFull => out.reserve(rest.len().max(16) * 3),
            }
        }
        out.into_bytes()
    }
}

/// 用户键入的 UTF-8 → 远端字节。
pub struct TextEncoder {
    encoding: Option<&'static Encoding>,
    /// 上一次末尾被切开的 UTF-8 字符（最多 3 字节），等下一批补全。
    pending: Vec<u8>,
}

impl TextEncoder {
    pub fn new(encoding: TextEncoding) -> Self {
        Self {
            encoding: (encoding != TextEncoding::Utf8).then(|| encoding_for(encoding)),
            pending: Vec::new(),
        }
    }

    /// 转成远端编码。远端编码里没有的字符换成 `?`（别让一个字符害得整行发不出去）。
    pub fn encode(&mut self, bytes: &[u8]) -> Vec<u8> {
        let Some(encoding) = self.encoding else {
            return bytes.to_vec();
        };
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);

        let mut out = Vec::with_capacity(input.len());
        let mut encoder = encoding.new_encoder();
        let mut rest = input.as_slice();
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    encode_str(&mut encoder, text, &mut out);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // 前缀已经验证过是合法 UTF-8。
                        let text = String::from_utf8_lossy(&rest[..valid]);
                        encode_str(&mut encoder, &text, &mut out);
                    }
                    match error.error_len() {
                        // 末尾是半个字符：留到下一批。
                        None => {
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                        // 中间有非法字节：原样放过（不是文本，多半是用户粘贴的二进制）。
                        Some(len) => {
                            out.extend_from_slice(&rest[valid..valid + len]);
                            rest = &rest[valid + len..];
                        }
                    }
                }
            }
        }
        out
    }
}

fn encode_str(encoder: &mut Encoder, mut text: &str, out: &mut Vec<u8>) {
    loop {
        let (result, read) = encoder.encode_from_utf8_to_vec_without_replacement(text, out, false);
        text = &text[read..];
        match result {
            encoding_rs::EncoderResult::InputEmpty => return,
            encoding_rs::EncoderResult::OutputFull => out.reserve(text.len().max(16) * 2),
            encoding_rs::EncoderResult::Unmappable(_) => out.push(b'?'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through_untouched_even_when_invalid() {
        let mut decoder = TextDecoder::new(TextEncoding::Utf8);
        assert_eq!(decoder.decode(b"a\xe4\xb8"), b"a\xe4\xb8");
        let mut encoder = TextEncoder::new(TextEncoding::Utf8);
        assert_eq!(encoder.encode(b"\xff"), b"\xff");
    }

    #[test]
    fn gbk_decoding_survives_a_character_split_across_reads() {
        // “中” 在 GBK 里是 D6 D0。
        let mut decoder = TextDecoder::new(TextEncoding::Gbk);
        let mut out = decoder.decode(b"a\xd6");
        out.extend(decoder.decode(b"\xd0b"));
        assert_eq!(String::from_utf8(out).unwrap(), "a中b");
    }

    #[test]
    fn every_supported_encoding_round_trips_its_own_text() {
        for (encoding, sample) in [
            (TextEncoding::Gbk, "中文abc"),
            (TextEncoding::Big5, "中文abc"),
            (TextEncoding::ShiftJis, "日本語abc"),
            (TextEncoding::EucKr, "한국어abc"),
            (TextEncoding::Latin1, "café"),
        ] {
            let wire = TextEncoder::new(encoding).encode(sample.as_bytes());
            assert_ne!(wire, sample.as_bytes(), "{encoding:?} 应当不是 UTF-8 字节");
            let back = TextDecoder::new(encoding).decode(&wire);
            assert_eq!(String::from_utf8(back).unwrap(), sample, "{encoding:?}");
        }
    }

    #[test]
    fn encoder_waits_for_the_rest_of_a_split_utf8_character() {
        let bytes = "中".as_bytes();
        let mut encoder = TextEncoder::new(TextEncoding::Gbk);
        assert!(encoder.encode(&bytes[..2]).is_empty());
        assert_eq!(encoder.encode(&bytes[2..]), vec![0xd6, 0xd0]);
    }

    #[test]
    fn unmappable_characters_become_question_marks() {
        let mut encoder = TextEncoder::new(TextEncoding::Latin1);
        assert_eq!(encoder.encode("a中b".as_bytes()), b"a?b");
    }

    #[test]
    fn escape_sequences_and_controls_are_unchanged_by_transcoding() {
        let mut encoder = TextEncoder::new(TextEncoding::Gbk);
        assert_eq!(encoder.encode(b"\x1b[A\x03\r"), b"\x1b[A\x03\r");
    }
}
