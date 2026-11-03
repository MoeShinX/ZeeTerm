//! 会话日志：把终端里显示的内容存成纯文本文件（去掉颜色、光标移动这些控制序列）。
//!
//! 写文件在单独的线程里做：会话任务只把字节扔进通道，磁盘慢也不会拖住终端输出。
//! 丢掉 [`SessionLog`]（关标签、重连换掉旧会话）时界面线程不等写线程，写线程自己写完退出；
//! 程序退出时 [`finish_all`] 有限等待，建不成文件 / 写失败 / 退出超时都记在 [`problems`] 里。
//! 日志里会有用户在终端里看到的一切（包括可能显示出来的敏感信息），所以默认关，
//! 只存在本机，目录由用户选。

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::warn;

/// 把终端输出流转成纯文本。跨块调用时状态会保留（控制序列、UTF-8 字符可能被切成两半）。
#[derive(Debug, Default)]
pub struct PlainText {
    state: EscState,
    /// 还没遇到换行的当前行（退格要能删掉前一个字符，所以先攒着）
    line: Vec<u8>,
    pending_cr: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum EscState {
    #[default]
    Text,
    /// 刚收到 ESC
    Escape,
    /// `ESC [` …… 直到 0x40-0x7E 的结束字节
    Csi,
    /// `ESC ]`、`ESC P` 之类的字符串序列，直到 BEL 或 `ESC \`
    Str,
    /// 字符串序列里收到了 ESC，下一个是 `\` 就结束
    StrEscape,
}

/// 一行最多攒这么多字节：有的程序整屏都不换行（进度条一直用 \r），别让内存无限涨。
const MAX_LINE_BYTES: usize = 64 * 1024;

impl PlainText {
    /// 喂一块输出，返回已经完整的若干行（每行以 `\n` 结尾）。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &byte in bytes {
            match self.state {
                EscState::Text => self.text_byte(byte, &mut out),
                EscState::Escape => {
                    self.state = match byte {
                        b'[' => EscState::Csi,
                        b']' | b'P' | b'X' | b'^' | b'_' => EscState::Str,
                        _ => EscState::Text,
                    };
                }
                EscState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.state = EscState::Text;
                    }
                }
                EscState::Str => match byte {
                    0x07 => self.state = EscState::Text,
                    0x1b => self.state = EscState::StrEscape,
                    _ => {}
                },
                EscState::StrEscape => {
                    self.state = if byte == b'\\' {
                        EscState::Text
                    } else {
                        EscState::Str
                    };
                }
            }
        }
        out
    }

    fn text_byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.pending_cr {
            self.pending_cr = false;
            // 单独的 \r（进度条、提示符重绘）也当换行：日志是按时间记的，不是屏幕快照
            self.end_line(out);
            if byte == b'\n' {
                return;
            }
        }
        match byte {
            0x1b => self.state = EscState::Escape,
            b'\r' => self.pending_cr = true,
            b'\n' => self.end_line(out),
            0x08 => self.backspace(),
            b'\t' => self.push(byte),
            byte if byte < 0x20 || byte == 0x7f => {}
            byte => self.push(byte),
        }
    }

    fn push(&mut self, byte: u8) {
        if self.line.len() < MAX_LINE_BYTES {
            self.line.push(byte);
        }
    }

    /// 删掉最后一个字符（UTF-8 多字节字符整个删）。
    fn backspace(&mut self) {
        while let Some(byte) = self.line.pop() {
            if byte & 0b1100_0000 != 0b1000_0000 {
                break;
            }
        }
    }

    fn end_line(&mut self, out: &mut Vec<u8>) {
        // 只剩空白的行（提示符重绘留下的）不写：日志里一堆空行没用
        if !self.line.iter().all(u8::is_ascii_whitespace) {
            out.extend_from_slice(&self.line);
            out.push(b'\n');
        }
        self.line.clear();
    }

    /// 会话结束：把没换行的最后一行也交出来。
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.pending_cr = false;
        self.end_line(&mut out);
        out
    }
}

/// 本地时间 `YYYYMMDD-HHMMSS`，日志文件名用（用户按自己的钟点找日志，UTC 会差几个小时）。
pub fn local_stamp() -> String {
    #[cfg(windows)]
    {
        // SAFETY: GetLocalTime 只往返回值里填系统时间，没有指针参数
        let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
        )
    }
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or_default();
        crate::ai::files::compact_timestamp(secs)
    }
}

/// 文件名里不能出现的字符换成 `_`，太长截断。
pub fn safe_file_stem(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(60)
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').to_owned();
    if trimmed.is_empty() {
        "session".to_owned()
    } else {
        trimmed
    }
}

/// 日志队列里最多积压这么多还没写进文件的字节（复审 F7）。磁盘慢（U 盘、网络共享、磁盘卡住）
/// 而终端输出很快时，超出的部分丢掉并记数，赶上来后在日志里写一行说明，内存不会一直涨。
pub const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;

/// 队列的字节预算：会话线程放进去前先占位，写线程写完再还回来。
#[derive(Debug, Default)]
pub struct QueueBudget {
    limit: usize,
    queued: std::sync::atomic::AtomicUsize,
    dropped: std::sync::atomic::AtomicU64,
}

impl QueueBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// 能放进队列就占位返回 true；放不下就记成丢弃返回 false。
    pub fn try_reserve(&self, len: usize) -> bool {
        use std::sync::atomic::Ordering;
        let reserved = self
            .queued
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |queued| {
                (queued.saturating_add(len) <= self.limit).then_some(queued + len)
            })
            .is_ok();
        if !reserved {
            self.dropped.fetch_add(len as u64, Ordering::SeqCst);
        }
        reserved
    }

    pub fn release(&self, len: usize) {
        self.queued
            .fetch_sub(len, std::sync::atomic::Ordering::SeqCst);
    }

    /// 取走到现在为止丢掉的字节数（取完清零）。
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, std::sync::atomic::Ordering::SeqCst)
    }
}

/// 写线程收到的消息。
enum Msg {
    Data(Vec<u8>),
    /// 会话结束 / 程序退出：把前面排着的写完就收尾。
    Close,
}

/// 一个日志出过的问题，已经合并成一句给人看的话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogProblem {
    /// 日志文件名（不带目录）。没能建出文件时是本来要用的名字。
    pub file: String,
    pub message: String,
}

/// 一个日志的健康状况：写线程记，退出时和设置页读。
#[derive(Debug, Default, Clone)]
struct Health {
    open_error: Option<String>,
    write_error: Option<String>,
    /// 文件没开成 / 写失败之后被丢掉的字节数。
    lost_bytes: u64,
    /// 磁盘跟不上、队列满了丢掉的字节数。
    backlog_bytes: u64,
}

impl Health {
    fn problem(&self, file: &str) -> Option<LogProblem> {
        let mut parts = Vec::new();
        if let Some(err) = &self.open_error {
            parts.push(format!(
                "建不了日志文件（{err}），这个会话没有记日志，已丢掉 {} 字节输出",
                self.lost_bytes
            ));
        } else if let Some(err) = &self.write_error {
            parts.push(format!(
                "写入失败（{err}），从这里起的输出没有记录，已丢掉约 {} 字节",
                self.lost_bytes
            ));
        }
        if self.backlog_bytes > 0 {
            parts.push(format!(
                "磁盘跟不上终端输出，丢掉了 {} 字节",
                self.backlog_bytes
            ));
        }
        (!parts.is_empty()).then(|| LogProblem {
            file: file.to_owned(),
            message: parts.join("；"),
        })
    }
}

/// 一个日志的共享部分：`SessionLog`（界面一侧）、写线程和 [`LogRegistry`] 三方各持一份。
struct Shared {
    /// 日志文件名（建文件之前按标签和时间戳先算出来，出错时也能说是哪一个）。
    file_hint: String,
    path: OnceLock<PathBuf>,
    sender: Sender<Msg>,
    health: Mutex<Health>,
    finished: Mutex<bool>,
    finished_changed: Condvar,
}

impl Shared {
    fn record(&self, change: impl FnOnce(&mut Health)) {
        if let Ok(mut health) = self.health.lock() {
            change(&mut health);
        }
    }

    fn problem(&self) -> Option<LogProblem> {
        self.health
            .lock()
            .ok()
            .and_then(|health| health.problem(&self.file_hint))
    }

    fn mark_finished(&self) {
        if let Ok(mut finished) = self.finished.lock() {
            *finished = true;
        }
        self.finished_changed.notify_all();
    }

    /// 等写线程收尾，最多等到 `deadline`；返回是否已收尾。
    fn wait_finished(&self, deadline: Instant) -> bool {
        let Ok(mut finished) = self.finished.lock() else {
            return false;
        };
        while !*finished {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.finished_changed.wait_timeout(finished, left) {
                Ok((guard, _)) => finished = guard,
                Err(_) => return false,
            }
        }
        true
    }
}

/// 最多记住这么多条已经收尾的日志的问题，免得长时间运行越攒越多。
const MAX_REMEMBERED_PROBLEMS: usize = 16;

/// 所有会话日志的生命周期管理者：界面丢掉一个 [`SessionLog`] 时不再等它的写线程，
/// 写线程自己把剩下的写完退出；程序退出时由 [`LogRegistry::finish_all`] 有限地等一下，
/// 超时、写失败、没建成文件都能在这里查到，不会悄悄丢日志。
#[derive(Default)]
pub struct LogRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    live: Vec<Arc<Shared>>,
    remembered: VecDeque<LogProblem>,
}

/// [`LogRegistry::finish_all`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishReport {
    /// 通知收尾时还在运行的日志数。
    pub closed: usize,
    /// 等到超时还没写完的日志数（这些日志的尾部可能不完整）。
    pub unfinished: usize,
    /// 所有已知的问题（含之前已经收尾的日志留下的）。
    pub problems: Vec<LogProblem>,
}

impl LogRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 进程里共用的那一个。
    pub fn global() -> &'static Arc<LogRegistry> {
        static GLOBAL: OnceLock<Arc<LogRegistry>> = OnceLock::new();
        GLOBAL.get_or_init(LogRegistry::new)
    }

    fn register(&self, shared: &Arc<Shared>) {
        if let Ok(mut state) = self.state.lock() {
            state.live.push(Arc::clone(shared));
        }
    }

    /// 写线程收尾：从在跑的名单里拿掉；出过问题的记下来。
    fn retire(&self, shared: &Arc<Shared>) {
        if let Ok(mut state) = self.state.lock() {
            state.live.retain(|live| !Arc::ptr_eq(live, shared));
            if let Some(problem) = shared.problem() {
                if state.remembered.len() >= MAX_REMEMBERED_PROBLEMS {
                    state.remembered.pop_front();
                }
                state.remembered.push_back(problem);
            }
        }
        shared.mark_finished();
    }

    /// 目前已知的问题：已收尾的日志留下的，加上还在运行的日志现在的状况。
    pub fn problems(&self) -> Vec<LogProblem> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let mut out: Vec<LogProblem> = state.remembered.iter().cloned().collect();
        out.extend(state.live.iter().filter_map(|shared| shared.problem()));
        out
    }

    /// 通知所有还在运行的日志收尾，最多等 `timeout`。程序退出时调用。
    pub fn finish_all(&self, timeout: Duration) -> FinishReport {
        let live: Vec<Arc<Shared>> = self
            .state
            .lock()
            .map(|state| state.live.clone())
            .unwrap_or_default();
        let deadline = Instant::now() + timeout;
        for shared in &live {
            let _ = shared.sender.send(Msg::Close);
        }
        let unfinished = live
            .iter()
            .filter(|shared| !shared.wait_finished(deadline))
            .count();
        let problems = self.problems();
        if unfinished > 0 {
            warn!(unfinished, "退出时还有会话日志没写完，尾部可能不完整");
        }
        for problem in &problems {
            warn!(file = %problem.file, message = %problem.message, "会话日志有问题");
        }
        FinishReport {
            closed: live.len(),
            unfinished,
            problems,
        }
    }
}

/// 程序退出时调用：让所有会话日志把排着的写完，最多等 `timeout`。
pub fn finish_all(timeout: Duration) -> FinishReport {
    LogRegistry::global().finish_all(timeout)
}

/// 本次运行里会话日志出过的问题（设置页显示用）。
pub fn problems() -> Vec<LogProblem> {
    LogRegistry::global().problems()
}

/// 写线程用的出口：文件没开成或写失败之后，只数丢了多少字节，不再碰文件。
struct Sink {
    writer: Option<BufWriter<Box<dyn Write + Send>>>,
    shared: Arc<Shared>,
    failed: bool,
}

impl Sink {
    fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(writer) = self.writer.as_mut().filter(|_| !self.failed) else {
            self.shared.record(|h| h.lost_bytes += bytes.len() as u64);
            return;
        };
        // 缓冲里还没落盘的也一并算：写失败时它们同样没了。
        let buffered = writer.buffer().len() as u64;
        if let Err(err) = writer.write_all(bytes) {
            self.fail(&err);
            self.shared
                .record(|h| h.lost_bytes += buffered + bytes.len() as u64);
        }
    }

    fn flush(&mut self) {
        if self.failed {
            return;
        }
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let buffered = writer.buffer().len() as u64;
        if let Err(err) = writer.flush() {
            self.fail(&err);
            self.shared.record(|h| h.lost_bytes += buffered);
        }
    }

    /// 磁盘满、U 盘拔了：记一次，之后不再写，不影响会话。
    fn fail(&mut self, err: &std::io::Error) {
        if self.failed {
            return;
        }
        self.failed = true;
        let reason = err.to_string();
        warn!(%err, file = %self.shared.file_hint, "会话日志写入失败，停止记录");
        self.shared
            .record(|h| h.write_error = h.write_error.take().or(Some(reason)));
    }
}

/// 丢过数据就在日志里补一行说明（经过 PlainText，接在当前行后面换行写）。
fn note_dropped(budget: &QueueBudget, sink: &mut Sink, text: &mut PlainText) {
    let dropped = budget.take_dropped();
    if dropped > 0 {
        sink.shared.record(|h| h.backlog_bytes += dropped);
        let note = format!("\r\n[Z-SSH] 日志写入跟不上终端输出，这里丢掉了 {dropped} 字节\r\n");
        sink.write(&text.feed(note.as_bytes()));
    }
}

/// 文件怎么打开：已经开好了（同步创建），还是交给写线程去建目录、开文件。
enum Opening {
    Ready(PathBuf, Box<dyn Write + Send>),
    InWorker {
        dir: PathBuf,
        stem: String,
        stamp: String,
    },
}

/// 在 `dir` 下建 `{stem}-{stamp}.log`（同名加序号）。
fn open_unique(dir: &Path, stem: &str, stamp: &str) -> std::io::Result<(PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    let mut sequence = 1u32;
    loop {
        let name = if sequence == 1 {
            format!("{stem}-{stamp}.log")
        } else {
            format!("{stem}-{stamp}-{sequence}.log")
        };
        let candidate = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && sequence < 1000 => {
                sequence += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn run_worker(
    shared: &Arc<Shared>,
    registry: &Arc<LogRegistry>,
    budget: &QueueBudget,
    receiver: &Receiver<Msg>,
    opening: Opening,
) {
    let writer: Option<Box<dyn Write + Send>> = match opening {
        Opening::Ready(_, writer) => Some(writer),
        Opening::InWorker { dir, stem, stamp } => match open_unique(&dir, &stem, &stamp) {
            Ok((path, file)) => {
                let _ = shared.path.set(path);
                Some(Box::new(file))
            }
            Err(err) => {
                warn!(%err, file = %shared.file_hint, "建不了会话日志文件，这个会话不记日志");
                let reason = err.to_string();
                shared.record(|h| h.open_error = Some(reason));
                None
            }
        },
    };
    let mut sink = Sink {
        writer: writer.map(BufWriter::new),
        shared: Arc::clone(shared),
        failed: false,
    };
    let mut text = PlainText::default();
    // 收到 `Close`、或发送端都没了就收尾。
    'serve: while let Ok(Msg::Data(first)) = receiver.recv() {
        budget.release(first.len());
        sink.write(&text.feed(&first));
        note_dropped(budget, &mut sink, &mut text);
        // 通道里已经排着的一并写掉，排空了再刷一次盘：程序崩了日志也基本是全的。
        loop {
            match receiver.try_recv() {
                Ok(Msg::Data(chunk)) => {
                    budget.release(chunk.len());
                    sink.write(&text.feed(&chunk));
                }
                Ok(Msg::Close) | Err(TryRecvError::Disconnected) => {
                    sink.flush();
                    break 'serve;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        sink.flush();
    }
    // 收尾：关闭之后才排进来的字节（不会有，发送端已被丢弃）也释放掉预算。
    note_dropped(budget, &mut sink, &mut text);
    sink.write(&text.finish());
    sink.flush();
    registry.retire(shared);
}

/// 一个会话的日志。丢掉（会话结束）时立刻返回：写线程自己把排着的写完、关文件后退出，
/// 界面线程不等磁盘。程序退出时用 [`finish_all`] 统一有限等待。
pub struct SessionLog {
    shared: Arc<Shared>,
    budget: Arc<QueueBudget>,
}

impl SessionLog {
    /// 在 `dir` 下新建 `{label}-{stamp}.log`（同名就加序号）。目录和文件在**调用线程**上创建，
    /// 出错立刻返回；界面线程请用 [`Self::start`]。
    pub fn create(dir: &Path, label: &str, stamp: &str) -> std::io::Result<Self> {
        Self::create_with_limit(dir, label, stamp, MAX_QUEUED_BYTES)
    }

    /// 同 [`Self::create`]，队列上限可调（测试用）。
    pub fn create_with_limit(
        dir: &Path,
        label: &str,
        stamp: &str,
        limit: usize,
    ) -> std::io::Result<Self> {
        let stem = safe_file_stem(label);
        let (path, file) = open_unique(dir, &stem, stamp)?;
        Self::launch(
            LogRegistry::global(),
            file_name_of(&path, &stem, stamp),
            limit,
            Opening::Ready(path, Box::new(file)),
        )
    }

    /// 立刻返回：建目录、开文件都交给写线程做，慢盘、网络目录、杀毒软件拦截都不会卡住调用者。
    /// 建不成时日志对象照常接收并丢弃输出，失败原因记在 [`problems`] 里（设置页可见）。
    /// 文件建成之前 [`Self::path`] 是 `None`。
    pub fn start(dir: &Path, label: &str, stamp: &str) -> std::io::Result<Self> {
        Self::start_in(LogRegistry::global(), dir, label, stamp, MAX_QUEUED_BYTES)
    }

    fn start_in(
        registry: &Arc<LogRegistry>,
        dir: &Path,
        label: &str,
        stamp: &str,
        limit: usize,
    ) -> std::io::Result<Self> {
        let stem = safe_file_stem(label);
        Self::launch(
            registry,
            format!("{stem}-{stamp}.log"),
            limit,
            Opening::InWorker {
                dir: dir.to_path_buf(),
                stem,
                stamp: stamp.to_owned(),
            },
        )
    }

    fn launch(
        registry: &Arc<LogRegistry>,
        file_hint: String,
        limit: usize,
        opening: Opening,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = channel::<Msg>();
        let shared = Arc::new(Shared {
            file_hint,
            path: OnceLock::new(),
            sender,
            health: Mutex::new(Health::default()),
            finished: Mutex::new(false),
            finished_changed: Condvar::new(),
        });
        if let Opening::Ready(path, _) = &opening {
            let _ = shared.path.set(path.clone());
        }
        let budget = Arc::new(QueueBudget::new(limit));
        registry.register(&shared);
        let (worker_shared, worker_registry, worker_budget) = (
            Arc::clone(&shared),
            Arc::clone(registry),
            Arc::clone(&budget),
        );
        let spawned = std::thread::Builder::new()
            .name("zssh-session-log".to_owned())
            .spawn(move || {
                run_worker(
                    &worker_shared,
                    &worker_registry,
                    &worker_budget,
                    &receiver,
                    opening,
                );
            });
        if let Err(err) = spawned {
            // 线程没起来：不能留一个永远不会收尾的登记。
            registry.retire(&shared);
            return Err(err);
        }
        Ok(Self { shared, budget })
    }

    /// 测试用：写到调用者给的写入器里（可以是慢的、会失败的），登记在传入的管理者下。
    #[cfg(test)]
    fn with_writer(
        registry: &Arc<LogRegistry>,
        file_hint: &str,
        limit: usize,
        writer: Box<dyn Write + Send>,
    ) -> Self {
        Self::launch(
            registry,
            file_hint.to_owned(),
            limit,
            Opening::Ready(PathBuf::from(file_hint), writer),
        )
        .expect("起写线程")
    }

    /// 日志文件的路径；用 [`Self::start`] 创建时，文件建成之前是 `None`。
    pub fn path(&self) -> Option<&Path> {
        self.shared.path.get().map(PathBuf::as_path)
    }

    /// 记一块输出。不阻塞。
    pub fn write(&self, bytes: &[u8]) {
        // 积压超过上限就丢掉这块（记数），不让内存跟着慢磁盘一直涨
        if !self.budget.try_reserve(bytes.len()) {
            return;
        }
        if self.shared.sender.send(Msg::Data(bytes.to_vec())).is_err() {
            self.budget.release(bytes.len());
        }
    }

    /// 通知写线程收尾并最多等 `timeout`，返回是否已写完（测试和需要确认落盘的场合用）。
    pub fn close_and_wait(self, timeout: Duration) -> bool {
        let _ = self.shared.sender.send(Msg::Close);
        self.shared.wait_finished(Instant::now() + timeout)
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // 只通知，不等：关标签 / 重连替换 driver 时在界面线程上，磁盘再慢也不能拖住窗口。
        // 写线程把排着的写完自己退出；还没写完的由程序退出时的 `finish_all` 有限等待并报告。
        let _ = self.shared.sender.send(Msg::Close);
    }
}

fn file_name_of(path: &Path, stem: &str, stamp: &str) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{stem}-{stamp}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(chunks: &[&[u8]]) -> String {
        let mut text = PlainText::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(text.feed(chunk));
        }
        out.extend(text.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn strips_colors_titles_and_cursor_moves() {
        let raw: &[u8] = b"\x1b]0;user@box: ~\x07\x1b[01;32muser@box\x1b[00m:~$ ls\r\n\x1b[0m\x1b[01;34mdir\x1b[0m  file\r\n";
        assert_eq!(plain(&[raw]), "user@box:~$ ls\ndir  file\n");
    }

    #[test]
    fn sequences_split_across_chunks_are_still_removed() {
        assert_eq!(
            plain(&[b"a\x1b[3", b"1mred\x1b", b"[0m\r", b"\n"]),
            "ared\n"
        );
    }

    #[test]
    fn backspace_removes_whole_utf8_characters() {
        assert_eq!(plain(&["错误\x08对\r\n".as_bytes()]), "错对\n");
        assert_eq!(plain(&[b"lss\x08 \x08\r\n"]), "ls\n");
    }

    #[test]
    fn lone_carriage_returns_become_lines_and_blank_lines_are_dropped() {
        assert_eq!(
            plain(&[b"10%\r50%\r100%\r\n\r\n  \r\ndone"]),
            "10%\n50%\n100%\ndone\n"
        );
    }

    #[test]
    fn local_stamp_looks_like_a_timestamp() {
        let stamp = local_stamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(&stamp[8..9], "-");
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_file_stem("web/01: 生产*"), "web_01_ 生产_");
        assert_eq!(safe_file_stem("  ..  "), "session");
    }

    #[test]
    fn log_file_gets_plain_text_and_a_unique_name() {
        let dir = crate::test_util::TestDir::new("session-log");
        let first = SessionLog::create(&dir, "测试机", "20261020-101500").unwrap();
        let second = SessionLog::create(&dir, "测试机", "20261020-101500").unwrap();
        assert_ne!(first.path(), second.path());
        first.write(b"\x1b[1mhello\x1b[0m\r\n");
        first.write(b"bye");
        let path = first.path().unwrap().to_path_buf();
        // 丢掉日志不再等写线程，要确认落盘就显式等一下。
        assert!(first.close_and_wait(Duration::from_secs(10)));
        assert!(second.close_and_wait(Duration::from_secs(10)));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello\nbye\n");
    }

    #[test]
    fn queue_budget_caps_the_backlog_and_counts_what_it_drops() {
        let budget = QueueBudget::new(10);
        assert!(budget.try_reserve(6));
        assert!(!budget.try_reserve(6), "超过上限就丢");
        assert!(budget.try_reserve(4));
        budget.release(6);
        assert!(budget.try_reserve(5), "写线程写掉一些之后又放得下");
        assert_eq!(budget.take_dropped(), 6);
        assert_eq!(budget.take_dropped(), 0);
    }

    #[test]
    fn dropped_output_is_noted_in_the_log() {
        let dir = crate::test_util::TestDir::new("session-log-full");
        // 上限比每一块都小：全部丢掉，日志里只留说明
        let log = SessionLog::create_with_limit(&dir, "测试机", "20261020-170000", 4).unwrap();
        log.write(b"0123456789\r\n");
        log.write(b"abcdefghij\r\n");
        let path = log.path().unwrap().to_path_buf();
        assert!(log.close_and_wait(Duration::from_secs(10)));
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("丢掉了 24 字节"), "{text}");
        assert!(!text.contains("0123456789"));
    }

    // ---- V2-C3 / R10：丢掉日志不等写线程；退出时有限等待；失败可观测 ----

    use std::sync::atomic::{AtomicBool, Ordering};

    /// 每次写入都睡一会儿的“慢磁盘”，写进去的字节存在共享缓冲里供断言。
    struct SlowWriter {
        delay: Duration,
        stored: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SlowWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.delay);
            if let Ok(mut stored) = self.stored.lock() {
                stored.extend_from_slice(buf);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 前 `ok_writes` 次写入成功，之后全部报“磁盘已满”。
    struct FullDisk {
        ok_writes: usize,
        stored: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.ok_writes == 0 {
                return Err(std::io::Error::other("磁盘已满"));
            }
            self.ok_writes -= 1;
            if let Ok(mut stored) = self.stored.lock() {
                stored.extend_from_slice(buf);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn stored_text(stored: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&stored.lock().map(|s| s.clone()).unwrap_or_default()).into_owned()
    }

    #[test]
    fn dropping_a_log_does_not_wait_for_a_slow_disk() {
        let registry = LogRegistry::new();
        let stored = Arc::new(Mutex::new(Vec::new()));
        let log = SessionLog::with_writer(
            &registry,
            "slow.log",
            MAX_QUEUED_BYTES,
            Box::new(SlowWriter {
                delay: Duration::from_millis(600),
                stored: Arc::clone(&stored),
            }),
        );
        log.write(b"first line\r\n");
        log.write(b"second line\r\n");

        let started = Instant::now();
        drop(log);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "关标签（丢掉日志）在界面线程上，不该等写线程；实际 {elapsed:?}"
        );

        // 写线程自己把剩下的写完，一行不少。
        let report = registry.finish_all(Duration::from_secs(20));
        assert_eq!(report.unfinished, 0);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(stored_text(&stored), "first line\nsecond line\n");
    }

    #[test]
    fn finish_all_waits_a_bounded_time_and_reports_what_is_still_running() {
        let registry = LogRegistry::new();
        let stored = Arc::new(Mutex::new(Vec::new()));
        let log = SessionLog::with_writer(
            &registry,
            "stuck.log",
            MAX_QUEUED_BYTES,
            Box::new(SlowWriter {
                delay: Duration::from_millis(1500),
                stored: Arc::clone(&stored),
            }),
        );
        log.write(b"tail that the slow disk is still taking\r\n");
        drop(log);

        let started = Instant::now();
        let report = registry.finish_all(Duration::from_millis(200));
        let waited = started.elapsed();
        assert_eq!(report.closed, 1);
        assert_eq!(report.unfinished, 1, "超时要如实说“还有一个没写完”");
        assert!(
            waited >= Duration::from_millis(150) && waited < Duration::from_millis(900),
            "有限等待，不是死等也不是不等：{waited:?}"
        );

        // 没有丢：再多给点时间，它最终还是写完了。
        let again = registry.finish_all(Duration::from_secs(20));
        assert_eq!(again.unfinished, 0);
        assert_eq!(
            stored_text(&stored),
            "tail that the slow disk is still taking\n"
        );
    }

    #[test]
    fn a_write_error_is_reported_with_how_much_was_lost_not_swallowed() {
        let registry = LogRegistry::new();
        let stored = Arc::new(Mutex::new(Vec::new()));
        let log = SessionLog::with_writer(
            &registry,
            "full.log",
            MAX_QUEUED_BYTES,
            Box::new(FullDisk {
                ok_writes: 1,
                stored: Arc::clone(&stored),
            }),
        );
        log.write(b"kept\r\n");
        // 让第一行先落盘（写线程每排空一次就刷一次盘），后面的写才会撞上“磁盘已满”。
        let began = Instant::now();
        while stored_text(&stored).is_empty() {
            assert!(began.elapsed() < Duration::from_secs(10), "第一行没落盘");
            std::thread::sleep(Duration::from_millis(5));
        }
        log.write(b"lost one\r\n");
        log.write(b"lost two\r\n");
        assert!(log.close_and_wait(Duration::from_secs(10)));

        assert_eq!(stored_text(&stored), "kept\n");
        let problems = registry.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].file, "full.log");
        assert!(problems[0].message.contains("磁盘已满"), "{problems:?}");
        assert!(
            problems[0].message.contains("丢掉"),
            "要说清丢了多少：{problems:?}"
        );
    }

    #[test]
    fn a_log_that_cannot_be_created_returns_at_once_and_says_why_later() {
        let registry = LogRegistry::new();
        let dir = crate::test_util::TestDir::new("session-log-blocked");
        // 目录路径上有一个同名的文件，建不出来。
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("sub");

        let started = Instant::now();
        let log = SessionLog::start_in(&registry, &target, "web-1", "20261103-100000", 1024)
            .expect("起线程");
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "建目录 / 开文件都在写线程里，调用者立刻拿到对象"
        );
        log.write(b"output nobody can store\r\n");
        assert!(log.close_and_wait(Duration::from_secs(10)));

        let problems = registry.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].file, "web-1-20261103-100000.log");
        assert!(
            problems[0].message.contains("建不了日志文件"),
            "{problems:?}"
        );
        assert!(problems[0].message.contains("丢掉"), "{problems:?}");
    }

    #[test]
    fn a_log_started_off_thread_creates_its_file_and_exposes_the_path_once_open() {
        let registry = LogRegistry::new();
        let dir = crate::test_util::TestDir::new("session-log-start");
        // 目录还不存在：也是写线程去建。
        let target = dir.join("nested").join("logs");
        let log = SessionLog::start_in(&registry, &target, "测试机", "20261103-110000", 1024)
            .expect("起线程");
        log.write(b"\x1b[1mhello\x1b[0m\r\nbye");
        let shared = Arc::clone(&log.shared);
        assert!(log.close_and_wait(Duration::from_secs(10)));

        let path = shared.path.get().expect("建成之后有路径").clone();
        assert!(path.starts_with(&target));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello\nbye\n");
        assert!(registry.problems().is_empty());
    }

    #[test]
    fn dropped_backlog_is_listed_as_a_problem_too() {
        let registry = LogRegistry::new();
        let stored = Arc::new(Mutex::new(Vec::new()));
        // 上限比每一块都小：全部丢掉，日志里留说明，同时登记成问题。
        let log = SessionLog::with_writer(
            &registry,
            "backlog.log",
            4,
            Box::new(SlowWriter {
                delay: Duration::ZERO,
                stored: Arc::clone(&stored),
            }),
        );
        log.write(b"0123456789\r\n");
        assert!(log.close_and_wait(Duration::from_secs(10)));
        assert!(stored_text(&stored).contains("丢掉了 12 字节"));
        let problems = registry.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].message.contains("丢掉了 12 字节"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_failure_after_the_exit_timeout_still_ends_up_in_the_report() {
        // 退出时等超时了，写线程之后才撞上写错误：问题照样登记，下一次查看能看到。
        let registry = LogRegistry::new();
        let released = Arc::new(AtomicBool::new(false));
        struct Gate(Arc<AtomicBool>);
        impl Write for Gate {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                while !self.0.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = buf;
                Err(std::io::Error::other("拔掉了"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = SessionLog::with_writer(
            &registry,
            "gate.log",
            MAX_QUEUED_BYTES,
            Box::new(Gate(Arc::clone(&released))),
        );
        log.write(b"x\r\n");
        drop(log);
        let early = registry.finish_all(Duration::from_millis(100));
        assert_eq!(early.unfinished, 1);
        released.store(true, Ordering::SeqCst);
        let done = registry.finish_all(Duration::from_secs(10));
        assert_eq!(done.unfinished, 0);
        assert!(
            done.problems.iter().any(|p| p.message.contains("拔掉了")),
            "{:?}",
            done.problems
        );
    }
}
