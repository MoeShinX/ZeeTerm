//! 终端命令历史：只驻内存、按目标身份分域、有硬预算（方案 7.4）。
//!
//! - 来源只有一个：状态机的 `CommandStarted` 事件（用户在可靠输入行里提交、并且收到同一个 Prompt 的命令开始
//!   事件之后才有）。不导入远端 shell 历史、不读撰写历史、不写盘、不进同步、不跨目标；
//! - 每个目标最多 [`MAX_ENTRIES_PER_TARGET`] 条 / [`MAX_BYTES_PER_TARGET`] 字节，整个进程最多
//!   [`MAX_BYTES_TOTAL`] 字节。超限按"最旧访问"回收：先回收最久没被访问过的目标里最旧的条目，目标空了就整个丢掉；
//!   一个目标自己超限则回收它自己最旧的条目；
//! - 前导空格的命令一律不采集（保留用户用空格表示"别记"的习惯），带敏感内容的命令也不采集
//!   （见 [`super::sensitive`]）；
//! - `Debug` 输出里没有命令原文，日志 / 诊断里不会带出命令。
//!
//! 时间用单调递增的"访问序号"表示，不用墙上时钟：回收顺序因此是确定的，也不受系统时间调整影响。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use super::cells::MAX_TRACKED_BYTES;

/// 每个目标最多这么多条。
pub const MAX_ENTRIES_PER_TARGET: usize = 200;
/// 每个目标最多这么多字节（条目正文 + 每条的固定开销）。
pub const MAX_BYTES_PER_TARGET: usize = 1024 * 1024;
/// 整个进程里所有目标加起来最多这么多字节。
pub const MAX_BYTES_TOTAL: usize = 8 * 1024 * 1024;
/// 每条记录除了正文之外的固定开销（计数、序号、堆分配的零头），按它计入预算。
pub const ENTRY_OVERHEAD: usize = 48;

/// 命令是怎么来的：影响要不要采集。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// 普通：用户在输入行里敲出来并提交的。
    Typed,
    /// 来源显式标记为敏感（例如被用户勾了"排除自动建议"）。
    MarkedSensitive,
    /// 提交时处在密码输入阶段。
    PasswordPhase,
}

/// 一条命令没被采集的原因（诊断用，不含命令原文）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectedBecause {
    Empty,
    /// 前导空格：用户表示"别记"。
    LeadingSpace,
    /// 含控制字符 / 换行：不是单行命令。
    Control,
    TooLong,
    MarkedSensitive,
    PasswordPhase,
    /// 命中敏感规则（见 [`super::sensitive`]）。
    Sensitive(super::sensitive::SensitiveReason),
}

/// 采集结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// 新增了一条。
    New,
    /// 已有同样的命令，使用次数加一。
    Bumped,
    Rejected(RejectedBecause),
}

/// 一条历史命中（给候选检索用）。
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryHit {
    pub text: String,
    /// 使用次数。
    pub uses: u32,
    /// 最近一次使用的访问序号（越大越新）。
    pub last_used: u64,
}

impl fmt::Debug for HistoryHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryHit")
            .field("len", &self.text.len())
            .field("uses", &self.uses)
            .field("last_used", &self.last_used)
            .finish()
    }
}

struct Entry {
    text: String,
    uses: u32,
    last_used: u64,
}

impl Entry {
    fn cost(&self) -> usize {
        self.text.len() + ENTRY_OVERHEAD
    }
}

#[derive(Default)]
struct TargetHistory {
    entries: Vec<Entry>,
    bytes: usize,
    last_access: u64,
}

impl TargetHistory {
    /// 回收这个目标里最旧使用的一条，返回释放的字节数。
    fn evict_oldest(&mut self) -> usize {
        let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(index, _)| index)
        else {
            return 0;
        };
        let freed = self.entries.swap_remove(index).cost();
        self.bytes -= freed;
        freed
    }
}

/// 预算（测试里可以改小）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub entries_per_target: usize,
    pub bytes_per_target: usize,
    pub bytes_total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries_per_target: MAX_ENTRIES_PER_TARGET,
            bytes_per_target: MAX_BYTES_PER_TARGET,
            bytes_total: MAX_BYTES_TOTAL,
        }
    }
}

/// 当前占用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub targets: usize,
    pub entries: usize,
    pub bytes: usize,
}

/// 按目标分域的内存历史。
pub struct HistoryStore {
    limits: Limits,
    targets: HashMap<String, TargetHistory>,
    total_bytes: usize,
    tick: u64,
}

impl fmt::Debug for HistoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不打印目标名（里面有用户名 / 地址）和命令，只给数量。
        let stats = self.stats();
        f.debug_struct("HistoryStore")
            .field("targets", &stats.targets)
            .field("entries", &stats.entries)
            .field("bytes", &stats.bytes)
            .finish()
    }
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryStore {
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            targets: HashMap::new(),
            total_bytes: 0,
            tick: 0,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// 提交了一条命令。不满足采集条件的返回 [`Recorded::Rejected`]，什么也不存。
    pub fn record(&mut self, target: &str, command: &str, provenance: Provenance) -> Recorded {
        if let Some(reason) = self.refusal(command, provenance) {
            return Recorded::Rejected(reason);
        }
        self.tick += 1;
        let tick = self.tick;
        let history = self.targets.entry(target.to_owned()).or_default();
        history.last_access = tick;
        let outcome = if let Some(entry) = history.entries.iter_mut().find(|e| e.text == command) {
            entry.uses = entry.uses.saturating_add(1);
            entry.last_used = tick;
            Recorded::Bumped
        } else {
            let entry = Entry {
                text: command.to_owned(),
                uses: 1,
                last_used: tick,
            };
            let cost = entry.cost();
            history.bytes += cost;
            history.entries.push(entry);
            self.total_bytes += cost;
            Recorded::New
        };
        self.enforce(target);
        outcome
    }

    fn refusal(&self, command: &str, provenance: Provenance) -> Option<RejectedBecause> {
        match provenance {
            Provenance::Typed => {}
            Provenance::MarkedSensitive => return Some(RejectedBecause::MarkedSensitive),
            Provenance::PasswordPhase => return Some(RejectedBecause::PasswordPhase),
        }
        if command.trim().is_empty() {
            return Some(RejectedBecause::Empty);
        }
        if command.starts_with(char::is_whitespace) {
            return Some(RejectedBecause::LeadingSpace);
        }
        if command.chars().any(char::is_control) {
            return Some(RejectedBecause::Control);
        }
        if command.len() > MAX_TRACKED_BYTES {
            return Some(RejectedBecause::TooLong);
        }
        super::sensitive::sensitivity(command).map(RejectedBecause::Sensitive)
    }

    /// 超出预算就回收：先管这个目标自己的上限，再管进程总预算。
    fn enforce(&mut self, written: &str) {
        let limits = self.limits;
        if let Some(history) = self.targets.get_mut(written) {
            while history.entries.len() > limits.entries_per_target
                || history.bytes > limits.bytes_per_target
            {
                let freed = history.evict_oldest();
                if freed == 0 {
                    break;
                }
                self.total_bytes -= freed;
            }
        }
        while self.total_bytes > limits.bytes_total {
            // 最久没被访问的目标；刚写入的目标是最新访问的，只有它是唯一目标时才会轮到它。
            let Some(victim) = self
                .targets
                .iter()
                .min_by_key(|(_, history)| history.last_access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            let Some(history) = self.targets.get_mut(&victim) else {
                break;
            };
            let freed = history.evict_oldest();
            self.total_bytes -= freed;
            if history.entries.is_empty() {
                self.targets.remove(&victim);
            }
            if freed == 0 {
                break;
            }
        }
    }

    /// 这个目标里以 `prefix` 为严格前缀的命令（比 `prefix` 长）。读取算一次访问：
    /// 正在被用的目标不会被先回收。
    pub fn matches(&mut self, target: &str, prefix: &str) -> Vec<HistoryHit> {
        let Some(history) = self.targets.get_mut(target) else {
            return Vec::new();
        };
        self.tick += 1;
        history.last_access = self.tick;
        history
            .entries
            .iter()
            .filter(|entry| entry.text.len() > prefix.len() && entry.text.starts_with(prefix))
            .map(|entry| HistoryHit {
                text: entry.text.clone(),
                uses: entry.uses,
                last_used: entry.last_used,
            })
            .collect()
    }

    /// 清除一个目标的全部历史。返回清掉了几条。
    pub fn clear_target(&mut self, target: &str) -> usize {
        match self.targets.remove(target) {
            Some(history) => {
                self.total_bytes -= history.bytes;
                history.entries.len()
            }
            None => 0,
        }
    }

    /// 清除全部历史。返回清掉了几条。
    pub fn clear_all(&mut self) -> usize {
        let count = self.stats().entries;
        self.targets.clear();
        self.total_bytes = 0;
        count
    }

    pub fn stats(&self) -> Stats {
        Stats {
            targets: self.targets.len(),
            entries: self.targets.values().map(|h| h.entries.len()).sum(),
            bytes: self.total_bytes,
        }
    }

    /// 某个目标的条数 / 字节（测试和界面的"清除"按钮用）。
    pub fn target_stats(&self, target: &str) -> Stats {
        self.targets
            .get(target)
            .map_or_else(Stats::default, |h| Stats {
                targets: 1,
                entries: h.entries.len(),
                bytes: h.bytes,
            })
    }
}

/// 进程共用的那一份历史（"进程总预算"指的就是它）。
pub fn shared() -> Arc<Mutex<HistoryStore>> {
    static SHARED: OnceLock<Arc<Mutex<HistoryStore>>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(Mutex::new(HistoryStore::new()))))
}

/// 拿锁；别的线程持锁时 panic 了也照常用（里面只有普通数据）。
pub fn lock(store: &Mutex<HistoryStore>) -> std::sync::MutexGuard<'_, HistoryStore> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
