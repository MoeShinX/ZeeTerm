//! 终端里标出 AI 执行的命令（V2-U50）：界面这一侧只管两件事——设置开着时给每个窗格的显示过滤器配上
//! 当前语言的文案，关着时撤掉；颜色和标记行本身由核心库的 `DisplayFilter` 生成（见
//! `zssh_core::ai::display`），所以标记和命令输出严格按终端里的顺序出现。
//!
//! 文案在这里按语言备好、整套交给过滤器，核心库不认识语言；语言切换后下一次轮询换成新的一套，
//! 只影响之后开始的 AI 命令。

use std::sync::Arc;

use super::*;
use crate::i18n::{Lang, Msg};
use zssh_core::ai::display::AiMarkLabels;

/// 标记行的文案。`{agent}` `{code}` `{time}` `{n}` `{m}` `{s}` 是占位，由核心库填。
#[derive(Clone, Copy, Debug)]
enum AiMarkMsg {
    BeginNamed,
    BeginPlain,
    Success,
    Failure,
    Unknown,
    Timeout,
    Millis,
    Seconds,
    Minutes,
}

impl Msg for AiMarkMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::BeginNamed => "▍AI（{agent}）",
            Self::BeginPlain => "▍AI",
            Self::Success => "✓ 退出码 {code} · {time}",
            Self::Failure => "✗ 退出码 {code} · {time}",
            Self::Unknown => "结束 · {time}",
            Self::Timeout => "⧗ 超时，命令可能仍在运行",
            Self::Millis => "{n} 毫秒",
            Self::Seconds => "{n} 秒",
            Self::Minutes => "{m} 分 {s} 秒",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::BeginNamed => "▍AI ({agent})",
            Self::BeginPlain => "▍AI",
            Self::Success => "✓ exit {code} · {time}",
            Self::Failure => "✗ exit {code} · {time}",
            Self::Unknown => "Finished · {time}",
            Self::Timeout => "⧗ Timed out, the command may still be running",
            Self::Millis => "{n} ms",
            Self::Seconds => "{n} s",
            Self::Minutes => "{m} min {s} s",
        }
    }
}

/// 某一种语言的整套标记文案。
pub(super) fn labels_for(lang: Lang) -> AiMarkLabels {
    let text = |msg: AiMarkMsg| match lang {
        Lang::Zh => msg.zh().to_owned(),
        Lang::En => msg.en().to_owned(),
    };
    AiMarkLabels {
        begin_named: text(AiMarkMsg::BeginNamed),
        begin_plain: text(AiMarkMsg::BeginPlain),
        success: text(AiMarkMsg::Success),
        failure: text(AiMarkMsg::Failure),
        unknown: text(AiMarkMsg::Unknown),
        timeout: text(AiMarkMsg::Timeout),
        millis: text(AiMarkMsg::Millis),
        seconds: text(AiMarkMsg::Seconds),
        minutes: text(AiMarkMsg::Minutes),
    }
}

impl TerminalWorkspace {
    /// 把「标出 AI 命令」的设置同步给每个窗格的显示过滤器：开着配当前语言的文案，关着撤掉。
    /// 每次轮询都做（和 `sync_assist_restrictions` 一样）：新开的窗格、重连换出来的窗格、改了设置或语言，
    /// 下一次轮询就生效，不用在每个建窗格的地方各接一遍。
    pub(super) fn sync_ai_marks(&mut self) {
        let want: Option<Arc<AiMarkLabels>> = if self.settings.mark_ai_commands {
            let lang = crate::i18n::current();
            let fresh = !matches!(&self.ai_marks_cache, Some((cached, _)) if *cached == lang);
            if fresh {
                self.ai_marks_cache = Some((lang, Arc::new(labels_for(lang))));
            }
            self.ai_marks_cache
                .as_ref()
                .map(|(_, labels)| Arc::clone(labels))
        } else {
            None
        };
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.pump_mut().set_ai_marks(want.clone());
            }
        }
    }
}

/// 缓存的那一套文案对应哪种语言。
pub(super) type MarksCache = Option<(Lang, Arc<AiMarkLabels>)>;
