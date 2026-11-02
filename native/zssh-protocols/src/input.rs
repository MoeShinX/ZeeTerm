// Adapted from nyaterm (https://github.com/nyakang/nyaterm, Apache-2.0, Copyright 2026 Kang and
// contributors; see native/NOTICE), crates/nyaterm-transport/src/telnet_input.rs: the Enter
// normalization, the local-echo rules and the escape-sequence tracker that keeps the tail of a
// keyboard sequence out of echo even when it spans several writes. The local line editor of the
// original is not ported (ZeeTerm has no such option); the charset conversion and Telnet IAC
// escaping happen in the callers, after this step.

//! 用户键入的处理：回车模式、本地回显，以及"转义序列不当成文本"。
//!
//! Telnet 和串口共用。输入是终端给的 UTF-8 字节；输出的 `send` 还没转成远端编码、也没做 Telnet 转义。

use zssh_core::model::{SerialNewline, TelnetNewline};

/// 回车键发送的字节。
pub fn telnet_enter_bytes(newline: TelnetNewline) -> &'static [u8] {
    match newline {
        TelnetNewline::Cr => b"\r",
        TelnetNewline::CrLf => b"\r\n",
        TelnetNewline::Lf => b"\n",
    }
}

pub fn serial_enter_bytes(newline: SerialNewline) -> &'static [u8] {
    match newline {
        SerialNewline::Cr => b"\r",
        SerialNewline::CrLf => b"\r\n",
        SerialNewline::Lf => b"\n",
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum EscapeState {
    #[default]
    Ground,
    Escape,
    Csi,
    Ss3,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

impl EscapeState {
    /// 返回这个字节是不是转义序列的一部分。转义序列的字节立刻原样转发，但不回显、
    /// 也不当文本处理；序列被切在两次写入之间时靠这个状态接着认。
    fn consume(&mut self, byte: u8) -> bool {
        if *self == Self::Ground {
            if byte == 0x1b {
                *self = Self::Escape;
                return true;
            }
            return false;
        }
        // 单独按下 Esc 之后的回车 / 控制键，不能被当成没结束的序列的一部分吞掉。
        // 字符串类序列（OSC、DCS……）的内容里则可以合法地含有这些字节。
        if matches!(*self, Self::Escape | Self::Csi | Self::Ss3) && byte < 0x20 && byte != 0x1b {
            *self = Self::Ground;
            return false;
        }
        *self = match (*self, byte) {
            (Self::Escape, b'[') => Self::Csi,
            (Self::Escape, b'O') => Self::Ss3,
            (Self::Escape, b']') => Self::Osc,
            (Self::Escape, b'P' | b'X' | b'^' | b'_') => Self::ControlString,
            (Self::Escape, 0x20..=0x2f) => Self::Escape,
            (Self::Csi | Self::Ss3, 0x40..=0x7e) => Self::Ground,
            (Self::Osc, 0x07) => Self::Ground,
            (Self::Osc, 0x1b) => Self::OscEscape,
            (Self::OscEscape | Self::ControlStringEscape, b'\\') => Self::Ground,
            (Self::OscEscape, 0x1b) => Self::OscEscape,
            (Self::OscEscape, _) => Self::Osc,
            (Self::ControlString, 0x1b) => Self::ControlStringEscape,
            (Self::ControlStringEscape, 0x1b) => Self::ControlStringEscape,
            (Self::ControlStringEscape, _) => Self::ControlString,
            (_, 0x1b) => Self::Escape,
            (Self::Escape, _) => Self::Ground,
            (state, _) => state,
        };
        true
    }
}

/// 键入处理的状态（只有转义序列的识别状态）。
#[derive(Clone, Default)]
pub struct InputFilter {
    escape: EscapeState,
}

impl InputFilter {
    /// 处理一次键入。返回 `(要发给远端的字节, 要本地回显的字节)`。
    ///
    /// - 回车 / 换行（`\r`、`\n`、`\r\n` 都算一次）换成 `enter`；
    /// - 其他控制字符原样发送（Ctrl+C 之类），只有 Tab 会回显；
    /// - 可打印字符（含多字节字符的各个字节）发送，开了本地回显就原样回显。
    pub fn prepare(&mut self, data: &[u8], enter: &[u8], local_echo: bool) -> (Vec<u8>, Vec<u8>) {
        let mut send = Vec::with_capacity(data.len());
        let mut echo = Vec::new();
        let mut index = 0;
        while index < data.len() {
            let byte = data[index];
            if self.escape.consume(byte) {
                send.push(byte);
                index += 1;
                continue;
            }
            match byte {
                b'\r' | b'\n' => {
                    send.extend_from_slice(enter);
                    if local_echo {
                        echo.extend_from_slice(b"\r\n");
                    }
                    if byte == b'\r' && data.get(index + 1) == Some(&b'\n') {
                        index += 1;
                    }
                }
                0x00..=0x1f | 0x7f => {
                    send.push(byte);
                    if local_echo && byte == b'\t' {
                        echo.push(byte);
                    }
                }
                _ => {
                    send.push(byte);
                    if local_echo {
                        echo.push(byte);
                    }
                }
            }
            index += 1;
        }
        (send, echo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_lines_multiline_paste_and_consecutive_enters_submit_once_each() {
        for local_echo in [false, true] {
            let (send, echo) =
                InputFilter::default().prepare(b"\r\na\nb\r\n\r\r", b"\r\n", local_echo);
            assert_eq!(send, b"\r\na\r\nb\r\n\r\n\r\n");
            assert_eq!(echo, if local_echo { send } else { Vec::new() });
        }
    }

    #[test]
    fn each_enter_mode_sends_its_own_bytes() {
        for (newline, wire) in [
            (TelnetNewline::Cr, b"\r".as_slice()),
            (TelnetNewline::Lf, b"\n"),
            (TelnetNewline::CrLf, b"\r\n"),
        ] {
            let (send, _) =
                InputFilter::default().prepare(b"x\r", telnet_enter_bytes(newline), false);
            let mut expected = b"x".to_vec();
            expected.extend_from_slice(wire);
            assert_eq!(send, expected);
        }
        for (newline, wire) in [
            (SerialNewline::Cr, b"\r".as_slice()),
            (SerialNewline::Lf, b"\n"),
            (SerialNewline::CrLf, b"\r\n"),
        ] {
            assert_eq!(serial_enter_bytes(newline), wire);
        }
    }

    #[test]
    fn controls_and_escape_sequences_pass_through_but_are_not_echoed() {
        let mut filter = InputFilter::default();
        let (send, echo) = filter.prepare(b"a\x03\x04\t\x1b[A\x1bOB", b"\r", true);
        assert_eq!(send, b"a\x03\x04\t\x1b[A\x1bOB");
        assert_eq!(echo, b"a\t");
    }

    #[test]
    fn escape_tails_across_writes_are_forwarded_and_never_echoed() {
        let mut filter = InputFilter::default();
        for packet in [
            b"\x1b".as_slice(),
            b"[",
            b"1;5D",
            b"\x1b]title",
            b"\r\n",
            b"\x1b",
            b"\\",
            b"\x1bPignored",
            b"\x1b\\",
        ] {
            let (send, echo) = filter.prepare(packet, b"\r", true);
            assert_eq!(send, packet);
            assert!(echo.is_empty());
        }
        assert_eq!(
            filter.prepare(b"ok\r", b"\r", true),
            (b"ok\r".to_vec(), b"ok\r\n".to_vec())
        );
    }

    #[test]
    fn a_lone_escape_does_not_swallow_the_following_enter() {
        let mut filter = InputFilter::default();
        assert_eq!(
            filter.prepare(b"\x1b", b"\r\n", true),
            (b"\x1b".to_vec(), vec![])
        );
        assert_eq!(
            filter.prepare(b"\r", b"\r\n", true),
            (b"\r\n".to_vec(), b"\r\n".to_vec())
        );
    }

    #[test]
    fn multibyte_text_is_sent_and_echoed_byte_for_byte() {
        let (send, echo) = InputFilter::default().prepare("中a".as_bytes(), b"\r", true);
        assert_eq!(send, "中a".as_bytes());
        assert_eq!(echo, "中a".as_bytes());
    }

    #[test]
    fn local_echo_off_echoes_nothing() {
        let (send, echo) = InputFilter::default().prepare(b"abc\t\r", b"\r", false);
        assert_eq!(send, b"abc\t\r");
        assert!(echo.is_empty());
    }
}
