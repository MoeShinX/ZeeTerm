// Adapted from nyaterm (https://github.com/nyakang/nyaterm, Apache-2.0, Copyright 2026 Kang and
// contributors; see native/NOTICE), crates/nyaterm-transport/src/telnet_codec.rs: the command/option
// constants, the NAWS frame layout and the IAC escaping of application data. The IAC parser itself was rewritten as a
// state machine that keeps its state across reads (the original splits a command or
// subnegotiation that straddles two reads and leaks or drops bytes), and a subnegotiation
// without an SE is capped instead of swallowing everything that follows.

//! Telnet 字节流编解码：IAC 状态机和帧的构造。

pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const NOP: u8 = 241;
pub const SE: u8 = 240;

pub const OPT_BINARY: u8 = 0;
pub const OPT_ECHO: u8 = 1;
pub const OPT_SGA: u8 = 3;
pub const OPT_TTYPE: u8 = 24;
pub const OPT_NAWS: u8 = 31;

/// TTYPE 子协商：`SEND` 请求 / `IS` 应答。
pub const TTYPE_IS: u8 = 0;
pub const TTYPE_SEND: u8 = 1;

/// 一次子协商（选项字节 + 载荷）最多收多少字节。超过就放弃这次子协商，后面的字节按正常数据处理：
/// 对端漏了 `IAC SE` 时，不能把之后的全部输出都吞掉、也不能无限攒内存。
pub const SUBNEGOTIATION_LIMIT: usize = 512;

/// 状态机识别出的控制事件（可见数据另外返回）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    /// `IAC WILL/WONT/DO/DONT <option>`
    Negotiate { verb: u8, option: u8 },
    /// `IAC SB <option> <payload> IAC SE`，载荷里的 `IAC IAC` 已还原成一个 255。
    Subnegotiation { option: u8, payload: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Data,
    Iac,
    Verb(u8),
    /// 子协商正文（第一个字节是选项号）。
    Sb,
    SbIac,
}

/// 接收方向的 IAC 状态机。一个连接一个，状态跨 `feed` 调用保留。
#[derive(Debug, Default)]
pub struct Decoder {
    state: State,
    sb: Vec<u8>,
    /// 上一个可见字节是 CR：非二进制模式下紧跟的 NUL 是 NVT 的"回车"写法，要吃掉。
    after_cr: bool,
    /// 对端方向已经协商成 BINARY：CR NUL 不再特殊处理。
    binary: bool,
    /// 因超长被放弃的子协商次数（诊断 / 测试用）。
    abandoned: u32,
}

impl Decoder {
    pub fn set_binary(&mut self, binary: bool) {
        self.binary = binary;
    }

    pub fn abandoned_subnegotiations(&self) -> u32 {
        self.abandoned
    }

    /// 喂一段刚读到的字节：可见数据追加到 `visible`，控制事件追加到 `events`。
    pub fn feed(&mut self, input: &[u8], visible: &mut Vec<u8>, events: &mut Vec<TelnetEvent>) {
        for &byte in input {
            self.step(byte, visible, events);
        }
    }

    fn step(&mut self, byte: u8, visible: &mut Vec<u8>, events: &mut Vec<TelnetEvent>) {
        // 一个字节最多重新解释一次（SB 里遇到 IAC 加非 SE 时，把后一个字节当成新命令）。
        let mut byte = byte;
        loop {
            match self.state {
                State::Data => {
                    if byte == IAC {
                        self.state = State::Iac;
                        self.after_cr = false;
                    } else if self.after_cr && byte == 0 && !self.binary {
                        self.after_cr = false;
                    } else {
                        self.after_cr = byte == b'\r';
                        visible.push(byte);
                    }
                    return;
                }
                State::Iac => {
                    self.state = State::Data;
                    match byte {
                        IAC => visible.push(IAC),
                        WILL | WONT | DO | DONT => self.state = State::Verb(byte),
                        SB => {
                            self.sb.clear();
                            self.state = State::Sb;
                        }
                        // NOP、GA、DM、BRK……以及游离的 SE：两个字节的命令，忽略。
                        _ => {}
                    }
                    return;
                }
                State::Verb(verb) => {
                    events.push(TelnetEvent::Negotiate { verb, option: byte });
                    self.state = State::Data;
                    return;
                }
                State::Sb => {
                    if byte == IAC {
                        self.state = State::SbIac;
                    } else {
                        self.push_sb(byte);
                    }
                    return;
                }
                State::SbIac => match byte {
                    SE => {
                        self.finish_sb(events);
                        return;
                    }
                    IAC => {
                        self.state = State::Sb;
                        self.push_sb(IAC);
                        return;
                    }
                    // 子协商里出现 IAC 加别的命令：这次子协商作废，那个字节是新命令的开头。
                    other => {
                        self.sb.clear();
                        self.state = State::Iac;
                        byte = other;
                    }
                },
            }
        }
    }

    fn push_sb(&mut self, byte: u8) {
        if self.sb.len() >= SUBNEGOTIATION_LIMIT {
            self.sb.clear();
            self.abandoned = self.abandoned.saturating_add(1);
            self.state = State::Data;
            return;
        }
        self.sb.push(byte);
    }

    fn finish_sb(&mut self, events: &mut Vec<TelnetEvent>) {
        self.state = State::Data;
        let mut body = std::mem::take(&mut self.sb);
        if body.is_empty() {
            return;
        }
        let option = body.remove(0);
        events.push(TelnetEvent::Subnegotiation {
            option,
            payload: body,
        });
    }
}

/// 发送方向：数据里的 255 要写成 `IAC IAC`。纯 TCP 模式不转义。
pub fn escape_iac(data: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(data.len());
    for &byte in data {
        escaped.push(byte);
        if byte == IAC {
            escaped.push(IAC);
        }
    }
    escaped
}

/// `IAC <verb> <option>`
pub fn command(verb: u8, option: u8) -> [u8; 3] {
    [IAC, verb, option]
}

/// 子协商帧：`IAC SB <option> <payload，255 转义> IAC SE`。
pub fn subnegotiation(option: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.extend_from_slice(&[IAC, SB, option]);
    frame.extend_from_slice(&escape_iac(payload));
    frame.extend_from_slice(&[IAC, SE]);
    frame
}

/// NAWS（窗口大小）帧：宽高各两字节、大端，值里的 255 转义。
pub fn naws_frame(cols: u16, rows: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&cols.to_be_bytes());
    payload.extend_from_slice(&rows.to_be_bytes());
    subnegotiation(OPT_NAWS, &payload)
}

/// TTYPE `IS` 应答帧。
pub fn ttype_is_frame(terminal_type: &str) -> Vec<u8> {
    let mut payload = vec![TTYPE_IS];
    payload.extend_from_slice(terminal_type.as_bytes());
    subnegotiation(OPT_TTYPE, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<TelnetEvent>, Decoder) {
        let mut decoder = Decoder::default();
        let mut visible = Vec::new();
        let mut events = Vec::new();
        for chunk in chunks {
            decoder.feed(chunk, &mut visible, &mut events);
        }
        (visible, events, decoder)
    }

    #[test]
    fn plain_text_passes_through() {
        let (visible, events, _) = run(&[b"hello\r\nworld"]);
        assert_eq!(visible, b"hello\r\nworld");
        assert!(events.is_empty());
    }

    #[test]
    fn iac_iac_is_a_literal_255() {
        let (visible, events, _) = run(&[b"a\xff\xffb"]);
        assert_eq!(visible, b"a\xffb");
        assert!(events.is_empty());
    }

    #[test]
    fn iac_iac_split_between_reads_is_still_one_255() {
        let (visible, _, _) = run(&[b"a\xff", b"\xffb"]);
        assert_eq!(visible, b"a\xffb");
    }

    #[test]
    fn a_negotiation_split_at_every_possible_point_is_recognized_once() {
        let wire = b"x\xff\xfb\x01y";
        for split in 0..=wire.len() {
            let (visible, events, _) = run(&[&wire[..split], &wire[split..]]);
            assert_eq!(visible, b"xy", "split {split}");
            assert_eq!(
                events,
                vec![TelnetEvent::Negotiate {
                    verb: WILL,
                    option: OPT_ECHO
                }],
                "split {split}"
            );
        }
    }

    #[test]
    fn byte_by_byte_feeding_gives_the_same_result_as_one_chunk() {
        let wire = b"ab\xff\xfd\x1f\xff\xfa\x18\x01\xff\xf0cd\xff\xff\xff\xf1e";
        let whole = run(&[wire]);
        let singles: Vec<&[u8]> = wire.chunks(1).collect();
        let piecewise = run(&singles);
        assert_eq!(whole.0, piecewise.0);
        assert_eq!(whole.1, piecewise.1);
        assert_eq!(whole.0, b"abcd\xffe");
    }

    #[test]
    fn subnegotiation_spanning_reads_is_delivered_whole() {
        let (visible, events, _) = run(&[b"\xff\xfa\x18", b"\x01\xff", b"\xf0tail"]);
        assert_eq!(visible, b"tail");
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: OPT_TTYPE,
                payload: vec![TTYPE_SEND]
            }]
        );
    }

    #[test]
    fn iac_iac_inside_a_subnegotiation_is_one_payload_byte() {
        let (_, events, _) = run(&[b"\xff\xfa\x1f\x00\xff\xff\x00\x18\xff\xf0"]);
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: OPT_NAWS,
                payload: vec![0, 255, 0, 24]
            }]
        );
    }

    #[test]
    fn a_missing_se_is_capped_and_later_output_is_not_swallowed() {
        // 子协商开了头但永远没有 IAC SE：超过上限之后，后面的字节必须作为数据出现。
        let mut wire = vec![IAC, SB, OPT_TTYPE];
        wire.extend(std::iter::repeat_n(b'x', SUBNEGOTIATION_LIMIT + 10));
        wire.extend_from_slice(b"VISIBLE");
        let (visible, events, decoder) = run(&[&wire]);
        assert!(events.is_empty());
        assert_eq!(decoder.abandoned_subnegotiations(), 1);
        let text = String::from_utf8_lossy(&visible);
        assert!(text.ends_with("VISIBLE"), "{text}");
    }

    #[test]
    fn an_unterminated_subnegotiation_never_grows_without_bound() {
        let mut decoder = Decoder::default();
        let mut visible = Vec::new();
        let mut events = Vec::new();
        decoder.feed(&[IAC, SB, OPT_TTYPE], &mut visible, &mut events);
        for _ in 0..100 {
            decoder.feed(&[b'z'; 1000], &mut visible, &mut events);
        }
        assert!(decoder.sb.len() <= SUBNEGOTIATION_LIMIT);
    }

    #[test]
    fn iac_plus_another_command_inside_a_subnegotiation_aborts_it() {
        // IAC SB 24 'a' IAC WILL 1：子协商作废，WILL 1 照常识别。
        let (visible, events, _) = run(&[b"\xff\xfa\x18a\xff\xfb\x01ok"]);
        assert_eq!(visible, b"ok");
        assert_eq!(
            events,
            vec![TelnetEvent::Negotiate {
                verb: WILL,
                option: OPT_ECHO
            }]
        );
    }

    #[test]
    fn two_byte_commands_are_ignored_and_do_not_eat_data() {
        let (visible, events, _) = run(&[b"a\xff\xf1b\xff\xf9c\xff\xf0d"]);
        assert_eq!(visible, b"abcd");
        assert!(events.is_empty());
    }

    #[test]
    fn cr_nul_is_a_carriage_return_unless_binary() {
        let (visible, _, _) = run(&[b"a\r\0b\r", b"\0c"]);
        assert_eq!(visible, b"a\rb\rc");

        let mut decoder = Decoder::default();
        decoder.set_binary(true);
        let mut visible = Vec::new();
        decoder.feed(b"a\r\0b", &mut visible, &mut Vec::new());
        assert_eq!(visible, b"a\r\0b");
    }

    #[test]
    fn escape_doubles_only_255() {
        assert_eq!(
            escape_iac(&[0, 13, 10, 127, 255, 128]),
            [0, 13, 10, 127, 255, 255, 128]
        );
    }

    #[test]
    fn naws_frame_has_big_endian_size_and_escapes_255() {
        assert_eq!(
            naws_frame(80, 24),
            [IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE]
        );
        // 宽 255 = 0x00FF；高 511 = 0x01FF：两个 255 字节都要转义。
        assert_eq!(
            naws_frame(255, 511),
            [IAC, SB, OPT_NAWS, 0, 255, 255, 1, 255, 255, IAC, SE]
        );
        // 解码回来是同一个载荷。
        let (_, events, _) = run(&[&naws_frame(255, 511)]);
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: OPT_NAWS,
                payload: vec![0, 255, 1, 255]
            }]
        );
    }

    #[test]
    fn ttype_is_frame_carries_the_name() {
        let frame = ttype_is_frame("xterm-256color");
        assert_eq!(&frame[..4], [IAC, SB, OPT_TTYPE, TTYPE_IS]);
        assert_eq!(&frame[4..frame.len() - 2], b"xterm-256color");
        assert_eq!(&frame[frame.len() - 2..], [IAC, SE]);
    }
}
