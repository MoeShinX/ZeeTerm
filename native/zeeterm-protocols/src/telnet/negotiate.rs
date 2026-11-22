// Option handling derived from nyaterm (Apache-2.0, see native/NOTICE),
// crates/nyaterm-transport/src/telnet_codec.rs `negotiate_response`: accept ECHO/SGA from the
// peer, answer NAWS. Extended here with per-option state (RFC 1143 style, so a repeated request
// is never answered twice and two ends can never loop), TTYPE, BINARY, and NAWS on resize.

//! Telnet 选项协商。
//!
//! 接受对端的 ECHO、SGA、BINARY；自己这边支持 NAWS（窗口大小）、TTYPE（终端类型）、SGA、BINARY。
//! 其余一律拒绝。每个选项在两个方向上各有一个状态：已经答应过的请求再来一次不重复答复，
//! 已经拒绝过的同一选项也只拒绝一次，所以恶意或有 bug 的对端不能靠反复发请求让我们无限回包。

use super::codec::{
    DO, DONT, OPT_BINARY, OPT_ECHO, OPT_NAWS, OPT_SGA, OPT_TTYPE, TTYPE_SEND, TelnetEvent, WILL,
    WONT, command, naws_frame, ttype_is_frame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Q {
    #[default]
    No,
    /// 我们已经发出请求，等对端答复。
    WantYes,
    Yes,
}

/// 协商要用到的配置。
#[derive(Debug, Clone)]
pub struct NegotiatorConfig {
    /// 发送窗口大小（NAWS）
    pub naws: bool,
    /// 回应 TTYPE 用的终端类型
    pub terminal_type: String,
    pub cols: u16,
    pub rows: u16,
}

pub struct Negotiator {
    config: NegotiatorConfig,
    /// 我们这一侧启用的选项（对应 WILL / WONT）。
    us: [Q; 256],
    /// 对端那一侧启用的选项（对应 DO / DONT 的对象，即对端 WILL）。
    him: [Q; 256],
    /// 已经拒绝过的选项：同一个请求不重复回拒绝。
    refused_us: [bool; 256],
    refused_him: [bool; 256],
}

impl Negotiator {
    /// 返回协商器和连接后要立刻发出的字节（主动提供 NAWS / TTYPE / SGA）。
    pub fn new(config: NegotiatorConfig) -> (Self, Vec<u8>) {
        let mut negotiator = Self {
            config,
            us: [Q::No; 256],
            him: [Q::No; 256],
            refused_us: [false; 256],
            refused_him: [false; 256],
        };
        let mut out = Vec::new();
        // 主动提供：窗口大小、终端类型、SGA。ECHO 和 BINARY 只应答、不主动要求。
        let mut offers = vec![OPT_TTYPE, OPT_SGA];
        if negotiator.config.naws {
            offers.insert(0, OPT_NAWS);
        }
        for option in offers {
            negotiator.us[usize::from(option)] = Q::WantYes;
            out.extend_from_slice(&command(WILL, option));
        }
        negotiator.him[usize::from(OPT_SGA)] = Q::WantYes;
        out.extend_from_slice(&command(DO, OPT_SGA));
        (negotiator, out)
    }

    fn supports_us(&self, option: u8) -> bool {
        match option {
            OPT_NAWS => self.config.naws,
            OPT_TTYPE | OPT_SGA | OPT_BINARY => true,
            _ => false,
        }
    }

    fn supports_him(option: u8) -> bool {
        matches!(option, OPT_ECHO | OPT_SGA | OPT_BINARY)
    }

    /// 对端 ECHO 选项是否已打开（对端负责回显，本地不该再回显）。
    pub fn remote_echo(&self) -> bool {
        self.him[usize::from(OPT_ECHO)] == Q::Yes
    }

    /// 对端到我们这个方向是否已协商成 BINARY（CR NUL 不再特殊处理）。
    pub fn binary_in(&self) -> bool {
        self.him[usize::from(OPT_BINARY)] == Q::Yes
    }

    /// 处理一个解析出来的事件，需要发出的字节追加到 `out`。
    pub fn handle(&mut self, event: &TelnetEvent, out: &mut Vec<u8>) {
        match event {
            TelnetEvent::Negotiate { verb, option } => self.negotiate(*verb, *option, out),
            TelnetEvent::Subnegotiation { option, payload } => {
                if *option == OPT_TTYPE
                    && payload.first() == Some(&TTYPE_SEND)
                    && self.us[usize::from(OPT_TTYPE)] == Q::Yes
                {
                    // 类型列表只有一项：每次被问都回同一个（RFC 1091 允许重复最后一项表示列表结束）。
                    out.extend_from_slice(&ttype_is_frame(&self.config.terminal_type));
                }
            }
        }
    }

    fn negotiate(&mut self, verb: u8, option: u8, out: &mut Vec<u8>) {
        let index = usize::from(option);
        match verb {
            DO => match self.us[index] {
                Q::Yes => {}
                Q::WantYes => self.enable_us(option, out),
                Q::No => {
                    if self.supports_us(option) {
                        out.extend_from_slice(&command(WILL, option));
                        self.enable_us(option, out);
                    } else if !self.refused_us[index] {
                        self.refused_us[index] = true;
                        out.extend_from_slice(&command(WONT, option));
                    }
                }
            },
            DONT => match self.us[index] {
                Q::Yes => {
                    self.us[index] = Q::No;
                    out.extend_from_slice(&command(WONT, option));
                }
                Q::WantYes => self.us[index] = Q::No,
                Q::No => {}
            },
            WILL => match self.him[index] {
                Q::Yes => {}
                Q::WantYes => self.him[index] = Q::Yes,
                Q::No => {
                    if Self::supports_him(option) {
                        self.him[index] = Q::Yes;
                        out.extend_from_slice(&command(DO, option));
                    } else if !self.refused_him[index] {
                        self.refused_him[index] = true;
                        out.extend_from_slice(&command(DONT, option));
                    }
                }
            },
            WONT => match self.him[index] {
                Q::Yes => {
                    self.him[index] = Q::No;
                    out.extend_from_slice(&command(DONT, option));
                }
                Q::WantYes => self.him[index] = Q::No,
                Q::No => {}
            },
            _ => {}
        }
    }

    /// 我们这一侧的选项刚生效：NAWS 要马上告诉对端当前窗口大小。
    fn enable_us(&mut self, option: u8, out: &mut Vec<u8>) {
        self.us[usize::from(option)] = Q::Yes;
        if option == OPT_NAWS {
            out.extend_from_slice(&naws_frame(self.config.cols, self.config.rows));
        }
    }

    /// 窗口大小变了。NAWS 已经协商成功才发；没成功或没开就什么都不发。
    pub fn resize(&mut self, cols: u16, rows: u16, out: &mut Vec<u8>) {
        self.config.cols = cols;
        self.config.rows = rows;
        if self.us[usize::from(OPT_NAWS)] == Q::Yes {
            out.extend_from_slice(&naws_frame(cols, rows));
        }
    }

    /// 我们这侧的选项是否已生效（测试 / 诊断用）。
    pub fn us_enabled(&self, option: u8) -> bool {
        self.us[usize::from(option)] == Q::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telnet::codec::{
        Decoder, IAC, OPT_BINARY, SB, SE, TTYPE_IS, TTYPE_SEND, subnegotiation,
    };

    fn config() -> NegotiatorConfig {
        NegotiatorConfig {
            naws: true,
            terminal_type: "xterm-256color".to_owned(),
            cols: 80,
            rows: 24,
        }
    }

    fn handle(negotiator: &mut Negotiator, wire: &[u8]) -> Vec<u8> {
        let mut decoder = Decoder::default();
        let (mut visible, mut events) = (Vec::new(), Vec::new());
        decoder.feed(wire, &mut visible, &mut events);
        let mut out = Vec::new();
        for event in &events {
            negotiator.handle(event, &mut out);
        }
        out
    }

    #[test]
    fn connection_start_offers_naws_ttype_and_sga() {
        let (_, out) = Negotiator::new(config());
        assert_eq!(
            out,
            [
                IAC, WILL, OPT_NAWS, IAC, WILL, OPT_TTYPE, IAC, WILL, OPT_SGA, IAC, DO, OPT_SGA
            ]
        );
        let (_, out) = Negotiator::new(NegotiatorConfig {
            naws: false,
            ..config()
        });
        assert!(!out.windows(3).any(|w| w == [IAC, WILL, OPT_NAWS]));
    }

    #[test]
    fn naws_is_sent_when_the_peer_accepts_our_offer_and_on_every_resize() {
        let (mut negotiator, _) = Negotiator::new(config());
        // 对端同意我们的 WILL NAWS（DO NAWS）：立刻发窗口大小，不重复发 WILL。
        let out = handle(&mut negotiator, &[IAC, DO, OPT_NAWS]);
        assert_eq!(out, naws_frame(80, 24));
        // 同一个 DO 再来一次：不再回包。
        assert!(handle(&mut negotiator, &[IAC, DO, OPT_NAWS]).is_empty());

        let mut out = Vec::new();
        negotiator.resize(132, 50, &mut out);
        assert_eq!(out, naws_frame(132, 50));
    }

    #[test]
    fn naws_disabled_refuses_the_option_and_never_sends_a_size() {
        let (mut negotiator, _) = Negotiator::new(NegotiatorConfig {
            naws: false,
            ..config()
        });
        let out = handle(&mut negotiator, &[IAC, DO, OPT_NAWS]);
        assert_eq!(out, [IAC, WONT, OPT_NAWS]);
        let mut out = Vec::new();
        negotiator.resize(100, 40, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn resize_before_naws_is_agreed_sends_nothing_but_the_agreement_uses_the_latest_size() {
        let (mut negotiator, _) = Negotiator::new(config());
        let mut out = Vec::new();
        negotiator.resize(120, 30, &mut out);
        assert!(out.is_empty());
        assert_eq!(
            handle(&mut negotiator, &[IAC, DO, OPT_NAWS]),
            naws_frame(120, 30)
        );
    }

    #[test]
    fn peer_echo_sga_and_binary_are_accepted_with_do() {
        let (mut negotiator, _) = Negotiator::new(config());
        assert_eq!(
            handle(&mut negotiator, &[IAC, WILL, OPT_ECHO]),
            [IAC, DO, OPT_ECHO]
        );
        assert!(negotiator.remote_echo());
        assert_eq!(
            handle(&mut negotiator, &[IAC, WILL, OPT_BINARY]),
            [IAC, DO, OPT_BINARY]
        );
        assert!(negotiator.binary_in());
        // SGA 是我们先发的 DO：对端的 WILL 是答复，不再回包。
        assert!(handle(&mut negotiator, &[IAC, WILL, OPT_SGA]).is_empty());
        // 对端关掉回显：本地回显恢复。
        assert_eq!(
            handle(&mut negotiator, &[IAC, WONT, OPT_ECHO]),
            [IAC, DONT, OPT_ECHO]
        );
        assert!(!negotiator.remote_echo());
    }

    #[test]
    fn do_binary_and_do_sga_get_will() {
        let (mut negotiator, _) = Negotiator::new(config());
        assert_eq!(
            handle(&mut negotiator, &[IAC, DO, OPT_BINARY]),
            [IAC, WILL, OPT_BINARY]
        );
        assert!(negotiator.us_enabled(OPT_BINARY));
        // SGA 我们先发了 WILL，对端的 DO 是答复。
        assert!(handle(&mut negotiator, &[IAC, DO, OPT_SGA]).is_empty());
        assert!(negotiator.us_enabled(OPT_SGA));
    }

    #[test]
    fn everything_else_is_refused_once() {
        let (mut negotiator, _) = Negotiator::new(config());
        // 我们不回显（ECHO 方向的 DO）、不支持 LINEMODE(34)、NEW-ENVIRON(39)。
        assert_eq!(
            handle(&mut negotiator, &[IAC, DO, OPT_ECHO]),
            [IAC, WONT, OPT_ECHO]
        );
        assert_eq!(handle(&mut negotiator, &[IAC, DO, 39]), [IAC, WONT, 39]);
        assert_eq!(handle(&mut negotiator, &[IAC, WILL, 34]), [IAC, DONT, 34]);
        // 对端坚持重复：不再回包，没有来回死循环。
        assert!(handle(&mut negotiator, &[IAC, DO, 39]).is_empty());
        assert!(handle(&mut negotiator, &[IAC, WILL, 34]).is_empty());
    }

    #[test]
    fn a_flood_of_unknown_requests_produces_a_bounded_reply() {
        let (mut negotiator, _) = Negotiator::new(config());
        let mut wire = Vec::new();
        for _ in 0..10_000 {
            for option in 100u8..120 {
                wire.extend_from_slice(&[IAC, DO, option, IAC, WILL, option]);
            }
        }
        let out = handle(&mut negotiator, &wire);
        // 每个选项在每个方向最多一次拒绝：20 × 2 × 3 字节。
        assert_eq!(out.len(), 20 * 2 * 3);
    }

    #[test]
    fn ttype_send_is_answered_with_the_configured_name_after_will_ttype_is_agreed() {
        let (mut negotiator, _) = Negotiator::new(config());
        // 还没协商成功就被问：不答。
        let ask = subnegotiation(OPT_TTYPE, &[TTYPE_SEND]);
        assert!(handle(&mut negotiator, &ask).is_empty());

        assert!(handle(&mut negotiator, &[IAC, DO, OPT_TTYPE]).is_empty());
        let reply = handle(&mut negotiator, &ask);
        let mut expected = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
        expected.extend_from_slice(b"xterm-256color");
        expected.extend_from_slice(&[IAC, SE]);
        assert_eq!(reply, expected);
        // 再问一次还是同一个。
        assert_eq!(handle(&mut negotiator, &ask), expected);
    }

    #[test]
    fn dont_turns_an_option_off_and_is_acknowledged_once() {
        let (mut negotiator, _) = Negotiator::new(config());
        handle(&mut negotiator, &[IAC, DO, OPT_NAWS]);
        assert_eq!(
            handle(&mut negotiator, &[IAC, DONT, OPT_NAWS]),
            [IAC, WONT, OPT_NAWS]
        );
        assert!(handle(&mut negotiator, &[IAC, DONT, OPT_NAWS]).is_empty());
        let mut out = Vec::new();
        negotiator.resize(90, 30, &mut out);
        assert!(out.is_empty(), "NAWS 关掉之后不再发窗口大小");
    }
}
