//! 候选检索（纯逻辑）：给定输入行里已经确认的文字，从同目标内存历史和快捷命令里挑出严格前缀扩展。
//!
//! 规则（方案 6.2）：
//! - 输入至少 [`MIN_INPUT_CHARS`] 个字符，`input_revision` 稳定 [`SETTLE`]（约 80 ms）之后才检索（[`SettleTimer`]）；
//! - 只做**严格前缀**匹配（区分大小写，候选必须比输入长）；模糊匹配不在终端内候选里；
//! - 来源 = 同目标历史（按使用频次、最近时间排序）+ 快捷命令（排在历史后面，按用户列表顺序），合并、去重，
//!   最多 [`MAX_CANDIDATES`] 条；
//! - **不预选**：这里只产生列表，选中是界面显式动作；
//! - 展示层再过一遍敏感规则（[`super::sensitive`]）：历史里就算有旧规则放进来的条目，快捷命令就算没勾"排除"，
//!   命中的也不出现；快捷命令被显式排除的整条不出现，不是只遮住标签。
//!
//! 结果只是候选；能不能填入由 [`super::InputTracker::is_current`] / `begin_accept` 在接受时再核对一遍。

use std::fmt;
use std::time::{Duration, Instant};

use super::cells::MAX_TRACKED_BYTES;
use super::history::HistoryHit;
use super::sensitive::is_sensitive;

/// 输入至少这么多字符才检索。
pub const MIN_INPUT_CHARS: usize = 2;
/// 输入稳定这么久才检索。
pub const SETTLE: Duration = Duration::from_millis(80);
/// 最多给这么多条。
pub const MAX_CANDIDATES: usize = 8;

/// 候选来自哪里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateSource {
    History,
    QuickCommand,
}

/// 一条候选。`text` 是完整命令，必须以输入为严格前缀。
#[derive(Clone, PartialEq, Eq)]
pub struct Candidate {
    pub source: CandidateSource,
    pub text: String,
}

impl fmt::Debug for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 命令原文不进调试输出（日志 / 诊断里不出现命令）。
        f.debug_struct("Candidate")
            .field("source", &self.source)
            .field("len", &self.text.len())
            .finish()
    }
}

impl Candidate {
    /// 接受时要追加的后缀：`text` 去掉输入前缀之后剩下的部分。不是严格前缀扩展就是 `None`。
    pub fn suffix_after(&self, typed: &str) -> Option<&str> {
        let suffix = self.text.strip_prefix(typed)?;
        (!suffix.is_empty()).then_some(suffix)
    }
}

/// 快捷命令的只读快照（只含候选需要的东西，不把设置库整个交进来）。
#[derive(Clone, PartialEq, Eq)]
pub struct QuickSource {
    pub text: String,
    /// 用户勾了"排除自动建议"。
    pub excluded: bool,
}

impl fmt::Debug for QuickSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuickSource")
            .field("len", &self.text.len())
            .field("excluded", &self.excluded)
            .finish()
    }
}

/// 这行输入值不值得去检索：字符数够、不是以空白开头（用户用前导空格表示"别记"，也就不提示）、
/// 没有控制字符、没超 tracker 上限。
pub fn eligible_input(typed: &str) -> bool {
    typed.chars().count() >= MIN_INPUT_CHARS
        && !typed.starts_with(char::is_whitespace)
        && !typed.chars().any(char::is_control)
        && typed.len() <= MAX_TRACKED_BYTES
}

/// 一条快捷命令能不能当自动候选：没被排除、不是疑似敏感的（即使没勾也整条排除）、单行。
pub fn quick_command_suggestible(text: &str, excluded: bool) -> bool {
    !excluded && !text.trim().is_empty() && !is_sensitive(text)
}

/// 检索：`history` 是同目标历史里以 `typed` 为前缀的命中（[`super::HistoryStore::matches`] 的结果），
/// `quick` 是快捷命令快照。返回至多 [`MAX_CANDIDATES`] 条，不预选。
pub fn search(typed: &str, history: Vec<HistoryHit>, quick: &[QuickSource]) -> Vec<Candidate> {
    if !eligible_input(typed) {
        return Vec::new();
    }
    let extends = |text: &str| text.len() > typed.len() && text.starts_with(typed);

    let mut history = history;
    // 使用次数多的在前，同次数里最近用过的在前，再按文字排序保证结果稳定。
    history.sort_by(|a, b| {
        b.uses
            .cmp(&a.uses)
            .then(b.last_used.cmp(&a.last_used))
            .then_with(|| a.text.cmp(&b.text))
    });

    let mut out: Vec<Candidate> = Vec::new();
    let push = |out: &mut Vec<Candidate>, source: CandidateSource, text: &str| {
        if out.len() < MAX_CANDIDATES && !out.iter().any(|existing| existing.text == text) {
            out.push(Candidate {
                source,
                text: text.to_owned(),
            });
        }
    };
    for hit in &history {
        if extends(&hit.text) && !is_sensitive(&hit.text) {
            push(&mut out, CandidateSource::History, &hit.text);
        }
    }
    for source in quick {
        if extends(&source.text) && quick_command_suggestible(&source.text, source.excluded) {
            push(&mut out, CandidateSource::QuickCommand, &source.text);
        }
    }
    out
}

/// "输入稳定约 80 ms"计时：`input_revision` 变了重新计，稳定够久只放行一次。
#[derive(Clone, Copy, Debug, Default)]
pub struct SettleTimer {
    tracked: Option<(u64, Instant)>,
    fired: bool,
}

impl SettleTimer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 每次观察输入时调用。这个修订号第一次稳定满 [`SETTLE`] 时返回 `true`（之后同一修订号返回 `false`）。
    pub fn poll(&mut self, revision: u64, now: Instant) -> bool {
        match self.tracked {
            Some((seen, since)) if seen == revision => {
                if !self.fired && now.saturating_duration_since(since) >= SETTLE {
                    self.fired = true;
                    return true;
                }
                false
            }
            _ => {
                self.tracked = Some((revision, now));
                self.fired = false;
                false
            }
        }
    }

    /// 输入条件不满足了（离开输入行、受限……）：忘掉计时，下次从头算。
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 这个修订号是否已经放行过一次检索。
    pub fn has_fired_for(&self, revision: u64) -> bool {
        self.fired && self.tracked.is_some_and(|(seen, _)| seen == revision)
    }
}

#[cfg(test)]
mod tests;
