//! 窗格的输入跟踪接线：把 shell 标记、按键、粘贴、广播 / 锁屏等限制、屏幕读数喂给核心库里的
//! [`InputTracker`]（`zssh_core::terminal_assist`）。
//!
//! 这一层**只旁路观察，不改变任何按键行为**：按键该发什么还是发什么，Tab、Enter、方向键原样交给 shell；
//! 这里只是在发送之前把"这是什么输入"告诉状态机、发送之后把结果告诉它。
//! 状态机在拿不准时会失同步，失同步后绝不会往远端补发任何东西。
//!
//! 代次失效复用 `highlight_ops` 的做法：窗格编号 + 连接代次 + 目标身份变了，整个状态机作废
//! （见 [`PaneAssist::sync_context`]）。

use std::time::Instant;

use zssh_core::terminal_assist::{
    HandoffKey, InputCell, InputTracker, IntentOrigin, KeyClass, MarkAnchor, MarkKind, Phase,
    RestrictionKind, ScreenSource, ScreenView, TrackerContext, WriteOutcome, WriteSource,
};
use zssh_session::{CoreFilter, SessionPump};
use zssh_terminal::{LineCellKind, ShellMarkKind};

/// 一次写入是怎么来的。输入跟踪据此决定"这是用户在输入行上打的字"还是"别的东西"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteOrigin {
    /// 键盘上敲出来的可打印字符（字节就是它的 UTF-8 文本）。
    TypedText,
    /// 输入法的最终提交（字节就是它的 UTF-8 文本）。
    ImeText,
    /// 非字符按键：Enter、退格，以及一切"交给 shell 并失同步"的键。
    Key(KeyClass),
    /// 不是逐键输入：粘贴、快捷命令、撰写栏、凭据回答……
    Foreign(WriteSource),
    /// 不经输入跟踪：鼠标上报、调试钩子、演示窗格的自写输出。
    Untracked,
}

/// 把一次按键分成"字符"还是"交给 shell 的键"。`bytes` 是 `encode_key` 编出来的、真正要发的字节。
pub(crate) fn classify_key(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    bytes: &[u8],
) -> WriteOrigin {
    let handoff = |kind| WriteOrigin::Key(KeyClass::Handoff(kind));
    match key {
        "enter" | "\n" | "\r" => return WriteOrigin::Key(KeyClass::Enter),
        "backspace" => return WriteOrigin::Key(KeyClass::Backspace),
        "delete" => return WriteOrigin::Key(KeyClass::Delete),
        "tab" => return handoff(HandoffKey::Tab),
        "up" | "down" | "left" | "right" | "home" | "end" | "pageup" | "pagedown" => {
            return handoff(HandoffKey::Navigation);
        }
        "escape" => return handoff(HandoffKey::Escape),
        _ => {}
    }
    if ctrl {
        return if matches!(key, "r" | "R" | "s" | "S") {
            handoff(HandoffKey::HistorySearch)
        } else {
            handoff(HandoffKey::Control)
        };
    }
    if alt {
        // Alt+字符发的是 ESC + 字符（readline 的 Meta 前缀）。
        return handoff(HandoffKey::Control);
    }
    match key_char {
        // 发出去的字节必须就是这个字符本身，否则不是"手动字符"。
        Some(text)
            if !text.is_empty()
                && text.as_bytes() == bytes
                && !text.chars().any(char::is_control) =>
        {
            WriteOrigin::TypedText
        }
        _ => handoff(HandoffKey::Other),
    }
}

/// 屏幕来源适配：状态机只看轻量读数，需要时才取一行单元格。
struct PumpScreen<'a>(&'a SessionPump<CoreFilter>);

impl ScreenSource for PumpScreen<'_> {
    fn view(&self) -> ScreenView {
        let probe = self.0.screen_probe();
        ScreenView {
            rows: probe.rows,
            cols: probe.cols,
            cursor_row: probe.cursor_row,
            cursor_col: probe.cursor_col,
            alt_screen: probe.alt_screen,
            display_offset: probe.display_offset,
            content_revision: probe.content_revision,
        }
    }

    fn line(&self, row: usize) -> Option<Vec<InputCell>> {
        let cells = self.0.line_cells(row)?;
        Some(
            cells
                .into_iter()
                .map(|cell| InputCell {
                    ch: cell.ch,
                    extra: cell.zerowidth,
                    kind: match cell.kind {
                        LineCellKind::Normal => zssh_core::terminal_assist::CellKind::Normal,
                        LineCellKind::Wide => zssh_core::terminal_assist::CellKind::Wide,
                        LineCellKind::WideSpacer => {
                            zssh_core::terminal_assist::CellKind::WideSpacer
                        }
                        LineCellKind::LeadingSpacer => {
                            zssh_core::terminal_assist::CellKind::LeadingSpacer
                        }
                    },
                })
                .collect(),
        )
    }
}

fn mark_kind(kind: ShellMarkKind) -> MarkKind {
    match kind {
        ShellMarkKind::PromptStart => MarkKind::PromptStart,
        ShellMarkKind::PromptEnd => MarkKind::PromptEnd,
        ShellMarkKind::CommandStart => MarkKind::CommandStart,
        ShellMarkKind::CommandEnd { exit_code } => MarkKind::CommandEnd { exit_code },
        ShellMarkKind::CommandLine(line) => MarkKind::CommandLine(line),
        ShellMarkKind::Cwd { host, path } => MarkKind::Cwd { host, path },
    }
}

/// 每个窗格一份：输入跟踪 + 它依赖的少量窗格级状态。
pub(crate) struct PaneAssist {
    tracker: InputTracker,
    /// 设置里的 `terminal_assist.enabled`。关着时不喂、不观察，状态机停在 Unknown。
    enabled: bool,
    /// 已经处理过的 AI 注入次数（泵里的计数追上它就说明又有 AI 命令写进了终端）。
    ai_arms_seen: u64,
}

impl PaneAssist {
    pub(crate) fn new(ctx: TrackerContext) -> Self {
        Self {
            tracker: InputTracker::new(ctx),
            enabled: true,
            ai_arms_seen: 0,
        }
    }

    pub(crate) fn tracker(&self) -> &InputTracker {
        &self.tracker
    }

    /// 测试里直接摆弄状态机（取候选请求身份、取事件、开关限制）。
    #[cfg(test)]
    pub(crate) fn tracker_mut(&mut self) -> &mut InputTracker {
        &mut self.tracker
    }

    pub(crate) fn phase(&self) -> Phase {
        self.tracker.phase()
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool, now: Instant) {
        if self.enabled != enabled {
            self.enabled = enabled;
            // 开关切换等于换了一个世代：关掉时立刻取消一切，打开时从 Unknown 重新来。
            self.tracker.on_integration_reset(now);
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// 窗格所属的连接 / 目标变了就整个重置。没变就什么也不做（不分配）。
    pub(crate) fn sync_context(
        &mut self,
        pane_id: u64,
        generation: u64,
        target: &str,
        now: Instant,
    ) {
        let current = self.tracker.context();
        if current.pane_id == pane_id
            && current.connection_generation == generation
            && current.target == target
        {
            return;
        }
        let ctx = TrackerContext {
            pane_id,
            connection_generation: generation,
            target: target.to_owned(),
            broadcast_revision: current.broadcast_revision,
        };
        self.tracker.set_context(ctx, now);
    }

    /// 标签的广播开关翻转过几次：变了只让在途的候选请求过期。
    pub(crate) fn set_broadcast_revision(&mut self, revision: u64, now: Instant) {
        if self.tracker.context().broadcast_revision != revision {
            let mut ctx = self.tracker.context().clone();
            ctx.broadcast_revision = revision;
            self.tracker.set_context(ctx, now);
        }
    }

    /// 写入时发现已锁屏：马上暂停，不等下一帧的限制同步。
    pub(crate) fn note_locked(&mut self, now: Instant) {
        self.tracker
            .set_restriction(RestrictionKind::Locked, true, now);
    }

    /// 断线 / 重连 / 连上：旧会话的状态全部作废。
    pub(crate) fn on_connection_reset(&mut self, now: Instant) {
        self.tracker.on_connection_reset(now);
    }

    pub(crate) fn on_geometry_changed(&mut self, now: Instant) {
        self.tracker.on_geometry_changed(now);
    }

    /// 外部限制（锁屏 / 广播 / 模态 / 鼠标上报）。
    pub(crate) fn set_restrictions(
        &mut self,
        locked: bool,
        broadcast: bool,
        modal: bool,
        mouse_report: bool,
        now: Instant,
    ) {
        for (kind, on) in [
            (RestrictionKind::Locked, locked),
            (RestrictionKind::Broadcast, broadcast),
            (RestrictionKind::Modal, modal),
            (RestrictionKind::MouseReport, mouse_report),
        ] {
            self.tracker.set_restriction(kind, on, now);
        }
    }

    pub(crate) fn on_ime_preedit(&mut self, active: bool, now: Instant) {
        if self.enabled {
            self.tracker.on_ime_preedit(active, now);
        }
    }

    /// 发送之前：登记这次写入。返回要在发送之后报告结果的本地序号。
    pub(crate) fn before_write(
        &mut self,
        origin: WriteOrigin,
        bytes: &[u8],
        now: Instant,
    ) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        match origin {
            WriteOrigin::TypedText | WriteOrigin::ImeText => {
                let intent = if origin == WriteOrigin::ImeText {
                    IntentOrigin::ImeCommit
                } else {
                    IntentOrigin::Manual
                };
                match std::str::from_utf8(bytes) {
                    Ok(text) => self.tracker.on_text(text, intent, now),
                    Err(_) => {
                        self.tracker
                            .on_key(KeyClass::Handoff(HandoffKey::Other), now);
                        None
                    }
                }
            }
            WriteOrigin::Key(class) => {
                self.tracker.on_key(class, now);
                None
            }
            WriteOrigin::Foreign(source) => {
                self.tracker.on_foreign_write(source, now);
                None
            }
            WriteOrigin::Untracked => None,
        }
    }

    /// 发送之后：报告结果。
    pub(crate) fn after_write(
        &mut self,
        origin: WriteOrigin,
        seq: Option<u64>,
        outcome: WriteOutcome,
        now: Instant,
    ) {
        if !self.enabled {
            return;
        }
        match seq {
            Some(seq) => self.tracker.on_write_outcome(seq, outcome, now),
            None => {
                if matches!(origin, WriteOrigin::Key(_)) {
                    self.tracker.on_unsent_write(outcome, now);
                }
            }
        }
    }

    /// 每次处理完输出（以及定时）：先处理新到的 shell 标记，再观察屏幕。
    pub(crate) fn after_output(&mut self, pump: &mut SessionPump<CoreFilter>, now: Instant) {
        let events = pump.take_shell_events();
        if !self.enabled {
            return;
        }
        let arms = pump.ai_arm_count();
        if arms != self.ai_arms_seen {
            self.ai_arms_seen = arms;
            self.tracker.on_foreign_write(WriteSource::AiInjection, now);
        }
        let screen = PumpScreen(pump);
        for event in events {
            let anchor = MarkAnchor {
                row: event.anchor.row,
                col: event.anchor.col,
                alt_screen: event.anchor.alt_screen,
            };
            self.tracker
                .on_mark(mark_kind(event.mark.kind), anchor, &screen, now);
        }
        self.tracker.observe(&screen, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(key: &str, key_char: Option<&str>, ctrl: bool, alt: bool) -> WriteOrigin {
        let bytes = key_char.unwrap_or("").as_bytes().to_vec();
        classify_key(key, key_char, ctrl, alt, &bytes)
    }

    #[test]
    fn printable_characters_are_typed_text_and_everything_the_shell_edits_is_handed_over() {
        assert_eq!(
            classify("a", Some("a"), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("a", Some("A"), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("space", Some(" "), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("1", Some("!"), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("enter", None, false, false),
            WriteOrigin::Key(KeyClass::Enter)
        );
        assert_eq!(
            classify("\n", Some("\n"), false, false),
            WriteOrigin::Key(KeyClass::Enter),
            "输入法单独提交的换行是 Enter，不是字符"
        );
        assert_eq!(
            classify("backspace", None, false, false),
            WriteOrigin::Key(KeyClass::Backspace)
        );
        assert_eq!(
            classify("delete", None, false, false),
            WriteOrigin::Key(KeyClass::Delete)
        );
        for key in [
            "tab", "up", "down", "left", "right", "home", "end", "pageup", "pagedown",
        ] {
            let expected = if key == "tab" {
                HandoffKey::Tab
            } else {
                HandoffKey::Navigation
            };
            assert_eq!(
                classify(key, None, false, false),
                WriteOrigin::Key(KeyClass::Handoff(expected)),
                "{key}"
            );
        }
        assert_eq!(
            classify("r", Some("r"), true, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::HistorySearch))
        );
        assert_eq!(
            classify("c", Some("c"), true, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Control))
        );
        assert_eq!(
            classify("b", Some("b"), false, true),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Control)),
            "Alt+字符是 ESC 前缀"
        );
        assert_eq!(
            classify("escape", None, false, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Escape))
        );
        assert_eq!(
            classify("f5", None, false, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Other))
        );
    }

    #[test]
    fn a_character_whose_bytes_differ_from_its_text_is_not_manual_text() {
        assert_eq!(
            classify_key("a", Some("a"), false, false, b"\x1ba"),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Other))
        );
        assert_eq!(
            classify_key("x", Some("\u{7}"), false, false, b"\x07"),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Other))
        );
    }
}
