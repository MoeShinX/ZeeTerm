//! 用户终端的登记表（V2-G10b）：AI 要「在用户的终端里执行」「读终端」「Ctrl+C 中断」，就得从界面线程
//! 之外找到某个终端窗格背后的东西。
//!
//! 每个打开的 shell（一个窗格一个）在 [`crate::ssh_backend::SshSessionFactory::open_async`] 里登记一条
//! [`TermEntry`]：
//! - `ring`：核心转发路径旁路复制的最近输出（`read_terminal`、结束标记解析都读它，永远是**原始**字节，
//!   不受显示过滤影响）；
//! - `state`：核心的终端执行状态（备用屏幕跟踪 + 提示符尾部 + shell 类型 + 「有 AI 命令在跑」占位）；
//! - `commands`：会话任务的命令队列，AI 的包装脚本、超时通知、Ctrl+C 都走它；
//! - `connection`：这个终端所在的已认证连接（`exec` 通道、SFTP 通道开在它上面）。
//!
//! 登记表只放纯数据，不依赖 GPUI；界面线程（`TerminalWorkspace`）只做两件事：窗格连上后告诉它「这个终端
//! 属于哪台主机」，以及每个节拍把当前聚焦窗格标成「最近聚焦」。
//!
//! 「哪个终端是这台主机的焦点终端」的规则（0.2.14 `scopeTargets.ts` 的 `boundSession`）：
//! 当前聚焦窗格（它的聚焦序号最大）→ 这台主机最近一次获得焦点的还开着的窗格 → 开得最早的窗格。
//! 这里按「聚焦序号最大」一条规则统一：聚焦窗格每个节拍都会被重新标记，所以它的序号永远最大；
//! 从没聚焦过的窗格序号为 0，按打开顺序排在最后。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc;
use zssh_core::ai::bridge::terminal_write::AiWriteSlot;
use zssh_core::ai::pty::TerminalState;
use zssh_core::ai::runs::TerminalRuns;
use zssh_core::ai::terminal::SharedTerminalRing;
use zssh_core::ssh::Connection;
use zssh_session::SessionCommand;

/// 终端登记表自己的文案。
#[derive(Clone, Copy, Debug)]
enum TerminalsMsg {
    NoConnection,
}

impl crate::i18n::Msg for TerminalsMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::NoConnection => "这个终端没有可用的连接",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::NoConnection => "This terminal has no usable connection",
        }
    }
}

/// 终端编号的前缀。目标编号是 `host-<id>`，终端编号是 `term-<n>`，两者不会混。
pub const TERMINAL_PREFIX: &str = "term-";

/// 会话任务命令队列里的一条命令。键盘输入、改窗口大小还是 [`SessionCommand`]；AI 相关的几种另外带着
/// 凭证，不适合塞进界面共用的 `SessionCommand`（它要 `Clone + PartialEq`）。
pub enum TermCommand {
    Session(SessionCommand),
    /// 终端对远端查询的协议回复（DSR / DA 等）：会话任务照原样写给远端，和键盘输入走同一个有序的写出口，
    /// 但不是用户输入——丢了不提示"已丢弃按键"。
    Reply(Vec<u8>),
    /// AI 命令的整行包装脚本：会话任务在写之前先认领凭证（排队期间停止 / 移出目标就不写，审计 C04），
    /// 再通知显示过滤器记住这条命令。
    AiWrite {
        payload: Vec<u8>,
        marker: String,
        /// 用户要看到的命令原文（合成回显用）。
        command: String,
        slot: AiWriteSlot,
    },
    /// AI 命令超时返回，命令还在终端里跑：显示过滤器退出完整过滤，只留迟到的结束标记行。
    AiTimedOut {
        marker: String,
    },
}

impl std::fmt::Debug for TermCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印内容：键盘输入可能是密码，包装脚本里有命令全文。
        match self {
            TermCommand::Session(SessionCommand::Write(bytes)) => {
                write!(f, "Session::Write({} bytes)", bytes.len())
            }
            TermCommand::Session(SessionCommand::Resize(size)) => {
                write!(f, "Session::Resize({}x{})", size.cols, size.rows)
            }
            TermCommand::Session(SessionCommand::Close) => write!(f, "Session::Close"),
            TermCommand::Reply(bytes) => write!(f, "Reply({} bytes)", bytes.len()),
            TermCommand::AiWrite { marker, .. } => write!(f, "AiWrite({marker})"),
            TermCommand::AiTimedOut { marker } => write!(f, "AiTimedOut({marker})"),
        }
    }
}

/// 一个打开的终端。
pub struct TermEntry {
    pub id: String,
    pub ring: SharedTerminalRing,
    pub state: Arc<TerminalState>,
    pub commands: mpsc::Sender<TermCommand>,
    /// 终端所在的已认证连接。生产里一定有；登记表自己的单元测试不连网，传 `None`。
    connection: Option<Arc<Connection>>,
    /// 这个终端属于主机库里的哪台主机；快速连接 / 还没告诉登记表时为 `None`。
    host_id: Mutex<Option<i64>>,
    /// 最近一次获得焦点的序号（0 = 从没聚焦过）。
    focus: AtomicU64,
    /// 打开顺序：序号相同时早开的优先。
    seq: u64,
}

impl TermEntry {
    /// 终端所在的连接（`exec` 通道、SFTP 通道开在它上面，不重新握手）。
    pub fn connection(&self) -> Result<Arc<Connection>, String> {
        self.connection
            .clone()
            .ok_or_else(|| crate::i18n::t(TerminalsMsg::NoConnection).to_owned())
    }

    pub fn host_id(&self) -> Option<i64> {
        *self.host_id.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn focus(&self) -> u64 {
        self.focus.load(Ordering::SeqCst)
    }
}

/// 全进程的终端登记表。
#[derive(Default)]
pub struct Terminals {
    entries: Mutex<HashMap<String, Arc<TermEntry>>>,
    next_seq: AtomicU64,
    focus_clock: AtomicU64,
    runs: TerminalRuns,
    /// AI 此刻正在哪些终端里执行（对话编号 → 终端编号）：标签上的活动点用（0.2.14 的 `aiBadges`）。
    ai_active: Mutex<HashMap<u64, HashSet<String>>>,
}

impl Terminals {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TermEntry>>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 登记一个刚打开的终端，返回它的编号。
    pub fn register(
        &self,
        ring: SharedTerminalRing,
        state: Arc<TerminalState>,
        commands: mpsc::Sender<TermCommand>,
        connection: Option<Arc<Connection>>,
    ) -> Arc<TermEntry> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let entry = Arc::new(TermEntry {
            id: format!("{TERMINAL_PREFIX}{seq}"),
            ring,
            state,
            commands,
            connection,
            host_id: Mutex::new(None),
            focus: AtomicU64::new(0),
            seq,
        });
        self.entries().insert(entry.id.clone(), Arc::clone(&entry));
        entry
    }

    /// 终端关了 / 会话结束：摘掉登记，清掉它名下还挂着的「AI 命令在跑」记录。
    pub fn remove(&self, id: &str) {
        self.entries().remove(id);
        self.runs.clear_session(id);
    }

    pub fn get(&self, id: &str) -> Option<Arc<TermEntry>> {
        self.entries().get(id).cloned()
    }

    /// 告诉登记表这个终端属于哪台主机（窗格连上之后界面线程调）。
    pub fn describe(&self, id: &str, host_id: Option<i64>) {
        if let Some(entry) = self.get(id) {
            *entry.host_id.lock().unwrap_or_else(|p| p.into_inner()) = host_id;
        }
    }

    /// 这个终端现在是焦点（界面每个节拍给聚焦窗格调一次，幂等：已经是最近聚焦的就不再递增）。
    pub fn mark_focused(&self, id: &str) {
        let entries = self.entries();
        let Some(entry) = entries.get(id) else {
            return;
        };
        let newest = entries.values().map(|e| e.focus()).max().unwrap_or(0);
        if entry.focus() == 0 || entry.focus() < newest {
            let stamp = self.focus_clock.fetch_add(1, Ordering::SeqCst) + 1;
            entry.focus.store(stamp, Ordering::SeqCst);
        }
    }

    /// 这台主机此刻该用哪个终端（规则见文件头）；没有打开的终端返回 `None`。
    pub fn for_host(&self, host_id: i64) -> Option<Arc<TermEntry>> {
        self.entries()
            .values()
            .filter(|entry| entry.host_id() == Some(host_id))
            .max_by(|a, b| a.focus().cmp(&b.focus()).then(b.seq.cmp(&a.seq)))
            .cloned()
    }

    /// 这台主机有没有打开的终端。
    pub fn has_host(&self, host_id: i64) -> bool {
        self.entries()
            .values()
            .any(|entry| entry.host_id() == Some(host_id))
    }

    /// 全进程现在有终端开着的主机（别的窗口里的标签也算）：AI 对话锚定的主机还在不在，界面据此判断。
    pub fn open_host_ids(&self) -> HashSet<i64> {
        self.entries()
            .values()
            .filter_map(|entry| entry.host_id())
            .collect()
    }

    /// 全进程现在开着的终端编号（快速连接的对话锚定在终端上，终端关了对话就失效）。
    pub fn open_terminal_ids(&self) -> HashSet<String> {
        self.entries().keys().cloned().collect()
    }

    /// 某个对话（`owner`）此刻正在这些终端里执行；空集合 = 它没有在执行了。
    pub fn set_ai_active(&self, owner: u64, terminals: HashSet<String>) {
        let mut active = self.ai_active.lock().unwrap_or_else(|p| p.into_inner());
        if terminals.is_empty() {
            active.remove(&owner);
        } else {
            active.insert(owner, terminals);
        }
    }

    /// 有没有 AI 此刻正在这个终端里执行。
    pub fn is_ai_active(&self, id: &str) -> bool {
        self.ai_active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|set| set.contains(id))
    }

    /// 「AI 命令在终端里跑」的登记表（停止按钮据此决定要不要发 Ctrl+C）。
    pub fn runs(&self) -> &TerminalRuns {
        &self.runs
    }
}

static TERMINALS: OnceLock<Terminals> = OnceLock::new();

/// 全进程那一份登记表。
pub fn terminals() -> &'static Terminals {
    TERMINALS.get_or_init(Terminals::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试里不需要真连接：只测登记表的选择规则，所以单独造一个不带连接的条目。
    fn entry(table: &Terminals, host: i64) -> Arc<TermEntry> {
        let seq = table.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (commands, _rx) = mpsc::channel(1);
        let entry = Arc::new(TermEntry {
            id: format!("{TERMINAL_PREFIX}{seq}"),
            ring: zssh_core::ai::terminal::TerminalRing::shared(),
            state: Arc::new(TerminalState::new()),
            commands,
            connection: None,
            host_id: Mutex::new(Some(host)),
            focus: AtomicU64::new(0),
            seq,
        });
        table.entries().insert(entry.id.clone(), Arc::clone(&entry));
        entry
    }

    #[test]
    fn the_focused_terminal_wins_then_the_most_recent_then_the_oldest() {
        let table = Terminals::default();
        let first = entry(&table, 1);
        let second = entry(&table, 1);
        let other = entry(&table, 2);
        // 谁都没聚焦过：开得最早的优先。
        assert_eq!(
            table.for_host(1).map(|e| e.id.clone()),
            Some(first.id.clone())
        );
        table.mark_focused(&second.id);
        assert_eq!(
            table.for_host(1).map(|e| e.id.clone()),
            Some(second.id.clone())
        );
        // 别的主机的终端获得焦点：这台主机仍然用它自己最近聚焦的那个。
        table.mark_focused(&other.id);
        assert_eq!(
            table.for_host(1).map(|e| e.id.clone()),
            Some(second.id.clone())
        );
        assert_eq!(
            table.for_host(2).map(|e| e.id.clone()),
            Some(other.id.clone())
        );
        // 焦点回到第一个。
        table.mark_focused(&first.id);
        assert_eq!(
            table.for_host(1).map(|e| e.id.clone()),
            Some(first.id.clone())
        );
        // 反复标记同一个终端不无限递增序号，也不改变结果。
        table.mark_focused(&first.id);
        table.mark_focused(&first.id);
        assert_eq!(
            table.for_host(1).map(|e| e.id.clone()),
            Some(first.id.clone())
        );
        // 终端关了：回退到另一个，再关光就没有。
        table.remove(&first.id);
        assert_eq!(
            table.for_host(1).map(|e| e.id.clone()),
            Some(second.id.clone())
        );
        table.remove(&second.id);
        assert!(table.for_host(1).is_none());
        assert!(!table.has_host(1));
        assert!(table.has_host(2));
    }

    #[test]
    fn open_ids_follow_the_registry() {
        let table = Terminals::default();
        let first = entry(&table, 1);
        let second = entry(&table, 2);
        assert_eq!(table.open_host_ids(), HashSet::from([1, 2]));
        assert_eq!(
            table.open_terminal_ids(),
            HashSet::from([first.id.clone(), second.id.clone()])
        );
        table.remove(&first.id);
        assert_eq!(table.open_host_ids(), HashSet::from([2]));
        assert!(!table.open_terminal_ids().contains(&first.id));
    }

    #[test]
    fn ai_activity_is_tracked_per_conversation() {
        let table = Terminals::default();
        assert!(!table.is_ai_active("term-1"));
        table.set_ai_active(1, HashSet::from(["term-1".to_owned()]));
        table.set_ai_active(2, HashSet::from(["term-1".to_owned(), "term-2".to_owned()]));
        assert!(table.is_ai_active("term-1") && table.is_ai_active("term-2"));
        // 一个对话结束，另一个还在同一个终端里执行：点不能灭。
        table.set_ai_active(2, HashSet::new());
        assert!(table.is_ai_active("term-1"));
        assert!(!table.is_ai_active("term-2"));
        table.set_ai_active(1, HashSet::new());
        assert!(!table.is_ai_active("term-1"));
    }

    #[test]
    fn removing_a_terminal_clears_its_ai_run_record() {
        let table = Terminals::default();
        let term = entry(&table, 1);
        table.runs().register("scope", &term.id, "__M__");
        assert!(table.runs().running("scope").is_some());
        table.remove(&term.id);
        assert!(
            table.runs().running("scope").is_none(),
            "会话断开后停止按钮不能再对着它发 Ctrl+C"
        );
    }
}

/// 测试用的假终端：登记表里有一条真的条目，背后是个假 shell 任务，按「命令 → 输出」应答，
/// 把包装脚本的结束标记写进环形缓冲，Ctrl+C 能打断「挂着」的命令。
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use zssh_core::ai::shell_kind::ShellKind;

    pub struct FakeTerminal {
        pub entry: Arc<TermEntry>,
        /// 假 shell 收到的事：`write:<命令原文>`、`key:<字节>`、`timed-out:<标记>`。
        pub log: Arc<Mutex<Vec<String>>>,
        pub interrupts: Arc<AtomicUsize>,
    }

    impl FakeTerminal {
        pub fn log(&self) -> Vec<String> {
            self.log.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }

        /// 往终端里「打印」一段输出（用户自己的输出，不是 AI 命令的）。
        pub fn print(&self, text: &str) {
            self.entry
                .ring
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(text.as_bytes());
            self.entry.state.feed(text.as_bytes());
        }
    }

    /// 在登记表里开一个属于 `host_id` 的假终端，shell 空闲在提示符上。命令 `hang` 不会自己结束，
    /// 直到收到 Ctrl+C（0x03）才以 130 结束；`echo X` 输出 `X`；`exit N` 以 N 结束；别的输出 `ran:<命令>`。
    /// 必须在 tokio 运行时里调用。
    pub fn spawn(host_id: Option<i64>, hang: &'static str) -> FakeTerminal {
        let ring = zssh_core::ai::terminal::TerminalRing::shared();
        let state = Arc::new(TerminalState::new());
        state.set_shell_kind(ShellKind::Posix);
        state.feed(b"user@host:~$ ");
        let (commands, mut rx) = mpsc::channel::<TermCommand>(16);
        let entry = terminals().register(Arc::clone(&ring), Arc::clone(&state), commands, None);
        terminals().describe(&entry.id, host_id);
        let log = Arc::new(Mutex::new(Vec::new()));
        let interrupts = Arc::new(AtomicUsize::new(0));
        let shell = FakeTerminal {
            entry: Arc::clone(&entry),
            log: Arc::clone(&log),
            interrupts: Arc::clone(&interrupts),
        };
        tokio::spawn(async move {
            let print = |text: &str| {
                ring.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(text.as_bytes());
                state.feed(text.as_bytes());
            };
            // 正在「挂着」的命令的结束标记：收到 Ctrl+C 才打出来。
            let mut hanging: Option<String> = None;
            while let Some(command) = rx.recv().await {
                match command {
                    TermCommand::Session(SessionCommand::Write(bytes)) => {
                        if bytes == [0x03] {
                            interrupts.fetch_add(1, Ordering::SeqCst);
                            if let Some(marker) = hanging.take() {
                                print(&format!("^C\r\n{marker}_E:130\r\nuser@host:~$ "));
                            }
                        }
                        log.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(format!("key:{bytes:?}"));
                    }
                    TermCommand::Session(_) | TermCommand::Reply(_) => {}
                    TermCommand::AiTimedOut { marker } => {
                        log.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(format!("timed-out:{marker}"));
                    }
                    TermCommand::AiWrite {
                        marker,
                        command,
                        slot,
                        ..
                    } => {
                        let Some(claim) = slot.claim() else {
                            log.lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .push(format!("dropped:{command}"));
                            continue;
                        };
                        log.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(format!("write:{command}"));
                        claim.finish(Ok(()));
                        print(&format!("\r\n{marker}_S\r\n"));
                        if command == hang {
                            // 命令在跑：终端「忙」，直到 Ctrl+C。
                            hanging = Some(marker);
                            continue;
                        }
                        let (output, code) = if let Some(rest) = command.strip_prefix("echo ") {
                            (rest.to_owned(), 0)
                        } else if let Some(code) = command.strip_prefix("exit ") {
                            (String::new(), code.trim().parse().unwrap_or(1))
                        } else {
                            (format!("ran:{command}"), 0)
                        };
                        print(&format!("{output}\r\n{marker}_E:{code}\r\nuser@host:~$ "));
                    }
                }
            }
        });
        shell
    }
}
