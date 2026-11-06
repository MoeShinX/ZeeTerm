//! 本地目录读取的后台化（V2-C3 / R09）。
//!
//! 本地栏、「移动到…」对话框浏览本地文件夹时，路径校验（`is_dir`）、`read_dir` 和逐项读属性都可能
//! 很慢：慢盘、杀毒软件正在扫描、几十万项的大目录、断开的映射盘 / 网络路径。这些调用一律不能在界面
//! 线程上做，这里提供：
//!
//! - [`LocalPool`]：有界的工作线程池（固定线程数 + 有界队列）。界面线程只负责 `submit`，队列满了立刻
//!   返回错误，绝不阻塞，也不会因为连点刷新而无限开线程。
//! - [`SeqGate`]：请求序号。每发一次读取取一个新号，**排在队里还没开始的旧请求直接跳过**，已经在读的
//!   旧请求读完后结果由界面按序号丢弃——A 慢、立刻切到 B，A 的迟到结果盖不掉 B。
//! - [`LocalFs`]：文件系统的最小接口。生产用 [`RealFs`]；测试注入带延迟的桩，稳定复现“慢目录”。

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use super::local_ops;
use super::model::Entry;
use crate::i18n::t;

/// 工作线程数。不同请求互不排队的前提是总有空闲线程：一个线程卡在失联的网络路径上时，其余线程
/// 还能服务别的目录。不设太大，免得连点时把系统线程耗光。
const WORKERS: usize = 4;
/// 排队上限。排队的旧请求会被序号作废后跳过，正常用法远到不了这个数。
const QUEUE_CAPACITY: usize = 64;

/// 一个排队的任务。`stale` 在工作线程取到它时检查，返回真就整个跳过（用户已经去了别处）。
struct Task {
    stale: Box<dyn Fn() -> bool + Send>,
    run: Box<dyn FnOnce() + Send>,
}

/// 有界线程池：固定的 `workers` 个线程共用一个有界队列。
pub(super) struct LocalPool {
    sender: SyncSender<Task>,
}

/// 队列满了（或线程起不来），没能接下这个任务。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Busy;

impl LocalPool {
    pub(super) fn new(workers: usize, capacity: usize) -> Self {
        let (sender, receiver) = sync_channel::<Task>(capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers.max(1) {
            let receiver = Arc::clone(&receiver);
            let spawned = std::thread::Builder::new()
                .name(format!("zssh-local-fs-{index}"))
                .spawn(move || worker_loop(&receiver));
            if let Err(err) = spawned {
                tracing::warn!(%err, "本地目录读取线程起不来");
            }
        }
        Self { sender }
    }

    /// 进程里共用的那一个池（懒创建）。
    pub(super) fn global() -> &'static Arc<LocalPool> {
        static GLOBAL: OnceLock<Arc<LocalPool>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(LocalPool::new(WORKERS, QUEUE_CAPACITY)))
    }

    fn submit(&self, task: Task) -> Result<(), Busy> {
        match self.sender.try_send(task) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Busy),
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<Task>>) {
    loop {
        // 取任务时才拿锁，执行时放开：别的线程能同时取下一个。
        let task = match receiver.lock() {
            Ok(guard) => guard.recv(),
            Err(_) => return,
        };
        let Ok(task) = task else {
            return;
        };
        if (task.stale)() {
            continue;
        }
        // 文件系统调用 panic 不能带走工作线程（池是固定大小的）；`run` 里的 `deliver` 负责把失败回报给界面。
        let _ = catch_unwind(AssertUnwindSafe(task.run));
    }
}

/// 请求序号：`next()` 之后，更早发出的请求全部作废。可克隆，克隆体共享同一个计数。
#[derive(Clone, Debug, Default)]
pub(super) struct SeqGate(Arc<AtomicU64>);

impl SeqGate {
    pub(super) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(super) fn is_latest(&self, seq: u64) -> bool {
        self.0.load(Ordering::SeqCst) == seq
    }
}

/// 本地文件系统的最小接口（读目录用）。方法都可能阻塞，只能在工作线程里调。
pub(super) trait LocalFs: Send + Sync + 'static {
    /// 路径是不是（跟随链接后的）目录。
    fn is_dir(&self, path: &str) -> bool;
    /// 列目录；空路径是“此电脑”（盘符）。
    fn list_dir(&self, path: &str) -> Result<Vec<Entry>, String>;
    /// 本地栏一开始落在哪：`wanted` 是复制标签带来的目录，必须还是个目录才用，否则按默认处理。
    fn start_dir(&self, wanted: Option<&str>) -> String;
}

/// 生产用的实现：直接走系统文件接口。`plan` 决定本地栏一开始落在哪。
pub(super) struct RealFs {
    pub(super) plan: super::local_start::StartPlan,
}

impl LocalFs for RealFs {
    fn is_dir(&self, path: &str) -> bool {
        debug_delay(path);
        std::path::Path::new(path).is_dir()
    }

    fn list_dir(&self, path: &str) -> Result<Vec<Entry>, String> {
        debug_delay(path);
        local_ops::list_dir(path)
    }

    fn start_dir(&self, wanted: Option<&str>) -> String {
        let wanted = wanted
            .filter(|dir| !dir.is_empty() && std::path::Path::new(dir).is_dir())
            .map(str::to_owned);
        wanted.unwrap_or_else(|| {
            super::engine::absolute_local(&super::engine::initial_local_dir(&self.plan))
                .display()
                .to_string()
        })
    }
}

/// 调试构建里的“慢盘”钩子：`ZSSH_SFTP_LOCAL_DELAY=<路径里含有的片段>:<毫秒>`（可写多条，用 `;` 分隔）。
/// 路径里含有片段（不区分大小写）的读取先睡这么久，用来在真窗口里看“正在读取…”状态。
/// release 构建里读不到环境变量，恒为不延迟。
fn debug_delay(path: &str) {
    let Some(spec) = super::engine::env_value("ZSSH_SFTP_LOCAL_DELAY") else {
        return;
    };
    let lowered = path.to_lowercase();
    for item in spec.split(';') {
        let Some((fragment, millis)) = item.rsplit_once(':') else {
            continue;
        };
        let (fragment, millis) = (fragment.trim().to_lowercase(), millis.trim());
        if let Ok(millis) = millis.parse::<u64>()
            && !fragment.is_empty()
            && lowered.contains(&fragment)
        {
            std::thread::sleep(std::time::Duration::from_millis(millis));
        }
    }
}

/// 读一个目录的失败原因：区分“不是目录”和“读不出来”，界面的提示不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScanError {
    NotDir,
    Failed(String),
}

/// 路径校验 + 读目录，整体在工作线程里做（`is_dir` 也是文件系统调用，不能留在界面线程）。
pub(super) fn scan(fs: &dyn LocalFs, target: &str) -> Result<Vec<Entry>, ScanError> {
    if !target.is_empty() && !fs.is_dir(target) {
        return Err(ScanError::NotDir);
    }
    fs.list_dir(target).map_err(ScanError::Failed)
}

/// 调度本地读取的入口：文件系统实现 + 线程池。每个文件视图一个，线程池是进程共用的。
pub(super) struct LocalLister {
    fs: Arc<dyn LocalFs>,
    pool: Arc<LocalPool>,
}

impl LocalLister {
    pub(super) fn new(fs: Arc<dyn LocalFs>, pool: Arc<LocalPool>) -> Self {
        Self { fs, pool }
    }

    /// 生产配置：真文件系统 + 共用线程池。
    pub(super) fn real(plan: super::local_start::StartPlan) -> Self {
        Self::new(Arc::new(RealFs { plan }), Arc::clone(LocalPool::global()))
    }

    /// 在后台线程里跑 `work`，结果交给 `deliver`（也在后台线程里调，通常是往界面的事件通道里送）。
    ///
    /// 立刻返回新请求的序号；队列满了返回 [`Busy`]。`gate.next()` 之后，排在队里还没开始的更早请求
    /// 会被跳过，已经在跑的读完后照样调 `deliver`，由接收方用 `gate.is_latest(seq)` 丢弃。
    /// `work` panic 时 `deliver` 收到 `Err`，界面不会永远停在“正在读取”。
    pub(super) fn spawn<T: Send + 'static>(
        &self,
        gate: &SeqGate,
        work: impl FnOnce(&dyn LocalFs) -> T + Send + 'static,
        deliver: impl FnOnce(u64, Result<T, String>) + Send + 'static,
    ) -> Result<u64, Busy> {
        let seq = gate.next();
        let fs = Arc::clone(&self.fs);
        let stale_gate = gate.clone();
        let task = Task {
            stale: Box::new(move || !stale_gate.is_latest(seq)),
            run: Box::new(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| work(&*fs)))
                    .map_err(|_| t(super::SftpMsg::LocalReadInternalError).to_owned());
                deliver(seq, outcome);
            }),
        };
        self.pool.submit(task).map(|()| seq)
    }
}

/// 界面发给后台的一次本地列表请求。
#[derive(Debug, Clone)]
pub(super) enum LocalRequest {
    /// 去 `target`（已去掉 `\\?\` 前缀；空 = “此电脑”）。
    Dir(String),
    /// 视图刚创建：先定下落点（复制标签带来的目录必须还是目录才用，否则默认临时目录），再读。
    Start(Option<String>),
}

/// 后台读完的结果。
#[derive(Debug)]
pub(super) struct LocalReply {
    pub(super) seq: u64,
    /// 实际读的目录（`Start` 时是后台定下来的那个）。
    pub(super) target: String,
    pub(super) result: Result<Vec<Entry>, ScanError>,
}

/// 把请求交给后台；读完以 [`LocalReply`] 交给 `deliver`。
pub(super) fn request_listing(
    lister: &LocalLister,
    gate: &SeqGate,
    request: LocalRequest,
    deliver: impl FnOnce(LocalReply) + Send + 'static,
) -> Result<u64, String> {
    // 工作线程 panic 时拿不到 `target`：先备一份，回报失败时用。
    let fallback_target = match &request {
        LocalRequest::Dir(target) => target.clone(),
        LocalRequest::Start(wanted) => wanted.clone().unwrap_or_default(),
    };
    lister
        .spawn(
            gate,
            move |fs| {
                let target = match &request {
                    LocalRequest::Dir(target) => target.clone(),
                    LocalRequest::Start(wanted) => fs.start_dir(wanted.as_deref()),
                };
                let result = scan(fs, &target);
                (target, result)
            },
            move |seq, outcome| {
                let reply = match outcome {
                    Ok((target, result)) => LocalReply {
                        seq,
                        target,
                        result,
                    },
                    Err(message) => LocalReply {
                        seq,
                        target: fallback_target,
                        result: Err(ScanError::Failed(message)),
                    },
                };
                deliver(reply);
            },
        )
        .map_err(|Busy| t(super::SftpMsg::LocalReadQueueFull).to_owned())
}

/// 界面一侧决定“这个回复算不算数”：只有最新一次请求的结果才用。
pub(super) fn accepts(gate: &SeqGate, reply: &LocalReply) -> bool {
    gate.is_latest(reply.seq)
}
