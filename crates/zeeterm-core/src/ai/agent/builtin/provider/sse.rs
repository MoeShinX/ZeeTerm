//! 增量 SSE 解析（Server-Sent Events，WHATWG HTML 规范第 9.2 节）。
//!
//! 字节流按任意大小的块喂进来：块边界可以落在行中间、`\r\n` 中间、甚至多字节 UTF-8
//! 字符中间，所以按字节攒完整的一行再解码。规则：
//! - 行结束符是 `\n`、`\r\n` 或单独的 `\r`；
//! - 空行 = 一条事件结束；`:` 开头的行是注释（心跳），忽略；
//! - `data:` 可以有多行，用 `\n` 连接；`event:` 是事件名；`id:` / `retry:` 用不到；
//! - 冒号后面如果跟一个空格，去掉这一个空格；
//! - 流开头的 UTF-8 BOM 去掉。

/// 一条完整的 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// 单行最长这么多字节，超了的整行丢掉（防止对端不发换行把内存吃光）。
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    line_overflow: bool,
    /// 上一个字节是 `\r`：紧跟的 `\n` 属于同一个行结束符。
    skip_lf: bool,
    started: bool,
    event: Option<String>,
    data: Vec<String>,
    /// 解析过至少一行 SSE 语法（字段行或注释）。
    saw_sse_syntax: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否见过 SSE 语法（`data:` / `event:` / 注释……）。纯 JSON / HTML 响应体不会。
    pub fn saw_sse_syntax(&self) -> bool {
        self.saw_sse_syntax
    }

    /// 喂一块字节，返回这一块里凑齐的事件。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut out),
                b'\r' => {
                    self.end_line(&mut out);
                    self.skip_lf = true;
                }
                _ => {
                    if self.line.len() >= MAX_LINE_BYTES {
                        self.line_overflow = true;
                    } else {
                        self.line.push(byte);
                    }
                }
            }
        }
        out
    }

    /// 流结束。规范说没有空行收尾的最后一条事件要丢掉；这里宽松一点：已经攒到 `data`
    /// 的就交出去（有的网关在 `data: [DONE]` 后面不补空行就关连接）。如果那条事件其实
    /// 被截断了，解码器解析 JSON 时会发现，并按「流中断」报错。
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) {
        let mut line = std::mem::take(&mut self.line);
        let overflow = std::mem::take(&mut self.line_overflow);
        if overflow {
            return;
        }
        if !self.started {
            self.started = true;
            if line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                line.drain(..3);
            }
        }
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line[0] == b':' {
            self.saw_sse_syntax = true;
            return;
        }
        let text = String::from_utf8_lossy(&line);
        let (field, value) = match text.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (text.as_ref(), ""),
        };
        match field {
            "data" => {
                self.saw_sse_syntax = true;
                self.data.push(value.to_string());
            }
            "event" => {
                self.saw_sse_syntax = true;
                self.event = Some(value.to_string());
            }
            "id" | "retry" => self.saw_sse_syntax = true,
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        let event = self.event.take();
        if self.data.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        out.push(SseEvent { event, data });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_whole(input: &[u8]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = parser.push(input);
        events.extend(parser.finish());
        events
    }

    fn data(events: &[SseEvent]) -> Vec<&str> {
        events.iter().map(|e| e.data.as_str()).collect()
    }

    #[test]
    fn basic_events_and_names() {
        let events = parse_whole(b"event: ping\ndata: {\"a\":1}\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].event, None);
        assert_eq!(events[1].data, "second");
    }

    #[test]
    fn crlf_cr_and_lf_line_endings() {
        for sep in ["\r\n", "\r", "\n"] {
            let input = format!("data: one{sep}{sep}data: two{sep}{sep}");
            let events = parse_whole(input.as_bytes());
            assert_eq!(data(&events), ["one", "two"], "{sep:?}");
        }
    }

    #[test]
    fn crlf_split_across_chunks() {
        let mut parser = SseParser::new();
        let mut events = parser.push(b"data: a\r");
        events.extend(parser.push(b"\n\r"));
        events.extend(parser.push(b"\ndata: b\r\n\r\n"));
        assert_eq!(data(&events), ["a", "b"]);
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let events = parse_whole(b": keep-alive\n\nid: 7\nretry: 100\nfoo: bar\ndata: x\n\n");
        assert_eq!(data(&events), ["x"]);
    }

    #[test]
    fn multi_line_data_is_joined() {
        let events = parse_whole(b"data: line1\ndata: line2\ndata:\ndata: line4\n\n");
        assert_eq!(data(&events), ["line1\nline2\n\nline4"]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let events = parse_whole(b"data:  two spaces\n\ndata:nospace\n\n");
        assert_eq!(data(&events), [" two spaces", "nospace"]);
    }

    #[test]
    fn bom_is_dropped() {
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(b"data: hi\n\n");
        assert_eq!(data(&parse_whole(&input)), ["hi"]);
    }

    #[test]
    fn event_without_data_is_not_dispatched() {
        assert!(parse_whole(b"event: ping\n\n").is_empty());
    }

    #[test]
    fn trailing_event_without_blank_line_is_kept_on_finish() {
        assert_eq!(data(&parse_whole(b"data: [DONE]")), ["[DONE]"]);
        assert_eq!(data(&parse_whole(b"data: [DONE]\n")), ["[DONE]"]);
    }

    #[test]
    fn multibyte_char_split_across_chunks() {
        let text = "data: 你好，世界\n\n".as_bytes();
        // 在每个字节处切一刀。
        for cut in 1..text.len() {
            let mut parser = SseParser::new();
            let mut events = parser.push(&text[..cut]);
            events.extend(parser.push(&text[cut..]));
            assert_eq!(data(&events), ["你好，世界"], "cut at {cut}");
        }
    }

    #[test]
    fn one_byte_chunks_match_whole_parse() {
        let input = b"event: a\r\ndata: 1\r\n\r\n: c\r\ndata: 2\r\ndata: 3\r\n\r\ndata: [DONE]\n\n";
        let whole = parse_whole(input);
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for byte in input {
            events.extend(parser.push(&[*byte]));
        }
        events.extend(parser.finish());
        assert_eq!(events, whole);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn plain_json_or_html_never_looks_like_sse() {
        let mut parser = SseParser::new();
        parser.push(b"{\"error\":{\"message\":\"nope\"}}");
        parser.finish();
        assert!(!parser.saw_sse_syntax());
        let mut parser = SseParser::new();
        parser.push(b"<html><body>hi</body></html>\n");
        parser.finish();
        assert!(!parser.saw_sse_syntax());
        let mut parser = SseParser::new();
        parser.push(b": ping\n\n");
        assert!(parser.saw_sse_syntax());
    }
}
