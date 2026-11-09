//! 回显确认：本地发了字符，什么时候能说"屏幕已经显示了它们"。
//!
//! 做法是拿**屏幕上读到的光标前文本**去对"已确认文本 + 待确认后缀"：
//! - 屏幕文本必须以已确认文本开头，剩下的部分必须是待确认后缀的一个前缀（可以是空、可以是一部分、
//!   可以是全部）。所以远端分块回显、中间夹 ANSI 重绘都不影响，因为看的是最终落在格子里的字，
//!   不是网络上的原始 chunk；
//! - 确认只会单调向前：已经确认的字符不会因为后来的读数而"反悔"，读数如果不再包含它们就是不匹配；
//! - 屏幕出现了后缀之外的字符、少了已确认的字符，都是不匹配——调用方应当失同步，
//!   **绝不补发退格或补偿文本**。
//!
//! 输入只会追加在光标前文本的末尾（行中编辑发生时光标后面有字，但光标前文本照样是追加），
//! 所以这里不需要通用的文本比较。

/// 一次屏幕读数对回显模型的判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EchoVerdict {
    /// 屏幕还是上次确认的样子：回显还没到（或这次屏幕变化和输入行无关）。
    NoProgress,
    /// 确认向前推进了，后面还有待确认的字符。
    Advanced,
    /// 待确认的字符全部确认。
    Complete,
    /// 屏幕上的文字既不是已确认文本，也不是"已确认 + 待确认的前缀"。
    Mismatch,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EchoModel {
    confirmed: String,
    pending: String,
    /// 累计已确认的待确认字节数（用来判断每条写入意图是否确认完）。
    consumed: usize,
}

impl EchoModel {
    pub fn new(confirmed: &str) -> Self {
        Self {
            confirmed: confirmed.to_owned(),
            pending: String::new(),
            consumed: 0,
        }
    }

    pub fn confirmed(&self) -> &str {
        &self.confirmed
    }

    pub fn pending(&self) -> &str {
        &self.pending
    }

    /// 已确认 + 待确认，也就是"如果所有写入都回显了，屏幕上应该是什么"。
    pub fn predicted(&self) -> String {
        format!("{}{}", self.confirmed, self.pending)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 累计已确认的待确认字节数。
    pub fn consumed(&self) -> usize {
        self.consumed
    }

    /// 登记一段待确认的后缀，返回它在累计序列里的结束偏移。
    pub fn register(&mut self, suffix: &str) -> usize {
        self.pending.push_str(suffix);
        self.consumed + self.pending.len()
    }

    /// 清掉待确认的部分（失同步 / 重置时），已确认文本也换成屏幕上读到的。
    pub fn reset(&mut self, confirmed: &str) {
        self.confirmed.clear();
        self.confirmed.push_str(confirmed);
        self.pending.clear();
        self.consumed = 0;
    }

    /// 用屏幕上读到的光标前文本做一次判定。
    pub fn observe(&mut self, screen: &str) -> EchoVerdict {
        let Some(rest) = screen.strip_prefix(self.confirmed.as_str()) else {
            return EchoVerdict::Mismatch;
        };
        if !self.pending.starts_with(rest) {
            return EchoVerdict::Mismatch;
        }
        if rest.is_empty() {
            return if self.pending.is_empty() {
                EchoVerdict::Complete
            } else {
                EchoVerdict::NoProgress
            };
        }
        self.confirmed.push_str(rest);
        self.pending.drain(..rest.len());
        self.consumed += rest.len();
        if self.pending.is_empty() {
            EchoVerdict::Complete
        } else {
            EchoVerdict::Advanced
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_arriving_in_chunks_advances_monotonically_to_complete() {
        let mut echo = EchoModel::new("");
        echo.register("l");
        echo.register("s");
        echo.register(" -la");
        assert_eq!(echo.observe(""), EchoVerdict::NoProgress);
        assert_eq!(echo.observe("l"), EchoVerdict::Advanced);
        assert_eq!(echo.observe("l"), EchoVerdict::NoProgress);
        assert_eq!(echo.observe("ls -"), EchoVerdict::Advanced);
        assert_eq!(echo.confirmed(), "ls -");
        assert_eq!(echo.pending(), "la");
        assert_eq!(echo.observe("ls -la"), EchoVerdict::Complete);
        assert_eq!(echo.confirmed(), "ls -la");
        assert!(!echo.has_pending());
    }

    #[test]
    fn confirmation_never_goes_backwards() {
        let mut echo = EchoModel::new("");
        echo.register("abc");
        assert_eq!(echo.observe("ab"), EchoVerdict::Advanced);
        // 屏幕又退回到只有 "a"：已经确认的 "ab" 不能被收回，这是不匹配。
        assert_eq!(echo.observe("a"), EchoVerdict::Mismatch);
        assert_eq!(echo.confirmed(), "ab", "不匹配不改变已确认文本");
    }

    #[test]
    fn extra_or_different_characters_are_a_mismatch() {
        let mut echo = EchoModel::new("ls ");
        echo.register("-l");
        assert_eq!(echo.observe("ls -x"), EchoVerdict::Mismatch);
        assert_eq!(
            echo.observe("ls -lx"),
            EchoVerdict::Mismatch,
            "多出后缀之外的字符"
        );
        assert_eq!(echo.observe("l"), EchoVerdict::Mismatch, "少了已确认的前缀");
        assert_eq!(echo.observe("ls  -l"), EchoVerdict::Mismatch);
        assert_eq!(echo.observe("ls -l"), EchoVerdict::Complete);
    }

    #[test]
    fn wide_and_combining_characters_confirm_by_character_not_by_column() {
        let mut echo = EchoModel::new("echo ");
        echo.register("你好");
        echo.register("e\u{301}");
        assert_eq!(echo.observe("echo 你"), EchoVerdict::Advanced);
        assert_eq!(echo.observe("echo 你好e"), EchoVerdict::Advanced);
        assert_eq!(echo.observe("echo 你好e\u{301}"), EchoVerdict::Complete);
    }

    #[test]
    fn partial_utf8_prefixes_cannot_match_because_the_screen_text_is_whole_characters() {
        let mut echo = EchoModel::new("");
        echo.register("你");
        // 屏幕文本是 &str，不可能是半个字符；显示成别的字符就是不匹配。
        assert_eq!(echo.observe("您"), EchoVerdict::Mismatch);
    }

    #[test]
    fn identical_repeated_characters_are_told_apart_by_length() {
        let mut echo = EchoModel::new("a");
        echo.register("a");
        assert_eq!(echo.observe("a"), EchoVerdict::NoProgress);
        assert_eq!(echo.observe("aa"), EchoVerdict::Complete);
    }

    #[test]
    fn register_returns_cumulative_end_offsets_that_survive_partial_confirmation() {
        let mut echo = EchoModel::new("");
        assert_eq!(echo.register("ab"), 2);
        assert_eq!(echo.register("你"), 5);
        assert_eq!(echo.observe("ab"), EchoVerdict::Advanced);
        assert_eq!(echo.consumed(), 2);
        assert_eq!(echo.register("c"), 6, "已确认的不会让偏移倒退");
    }

    #[test]
    fn reset_replaces_the_confirmed_text_and_forgets_the_pending() {
        let mut echo = EchoModel::new("a");
        echo.register("bc");
        echo.reset("xyz");
        assert_eq!(echo.confirmed(), "xyz");
        assert!(!echo.has_pending());
        assert_eq!(echo.consumed(), 0);
        assert_eq!(echo.observe("xyz"), EchoVerdict::Complete);
    }
}
