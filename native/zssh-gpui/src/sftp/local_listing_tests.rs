//! V2-C3 / R09：本地目录读取不占界面线程。用带延迟的文件系统桩复现慢盘，断言：
//! 提交立刻返回（界面回调耗时有上限）、迟到的旧结果盖不掉新目录、排队的旧请求不会被读、
//! 队列有界、文件系统 panic 不会让界面永远停在“正在读取”。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::local_listing::{
    LocalFs, LocalLister, LocalPool, LocalReply, LocalRequest, ScanError, SeqGate, accepts,
    request_listing, scan,
};
use super::model::{Entry, EntryKind};

/// 界面回调允许花的最长时间。桩的“慢目录”要睡几百毫秒，所以这个上限远低于它又给慢机器留足余量。
const UI_BUDGET: Duration = Duration::from_millis(150);
const WAIT: Duration = Duration::from_secs(10);

fn row(name: &str, kind: EntryKind) -> Entry {
    Entry {
        name: name.to_owned(),
        path: name.to_owned(),
        kind,
        target_kind: None,
        link_target: None,
        size: None,
        mtime: None,
        permissions: None,
        owner: None,
        hidden: false,
        name_is_utf8: true,
    }
}

/// 一次性的“放行”开关：被它拦住的读取要等测试代码 `open()` 才继续。
#[derive(Default)]
struct Latch {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Latch {
    fn open(&self) {
        if let Ok(mut open) = self.open.lock() {
            *open = true;
        }
        self.changed.notify_all();
    }

    fn wait(&self) {
        let Ok(mut open) = self.open.lock() else {
            return;
        };
        while !*open {
            match self.changed.wait_timeout(open, WAIT) {
                Ok((guard, timeout)) => {
                    open = guard;
                    if timeout.timed_out() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

/// 可控的文件系统桩：每个路径可以设延迟 / 拦截 / 失败 / panic，并记下读过哪些路径、在哪个线程。
#[derive(Default)]
struct StubFs {
    delays: HashMap<String, Duration>,
    blocked: HashMap<String, Arc<Latch>>,
    not_dirs: HashSet<String>,
    broken: HashSet<String>,
    panics: HashSet<String>,
    listed: Mutex<Vec<String>>,
    threads: Mutex<Vec<ThreadId>>,
}

impl StubFs {
    fn listed(&self) -> Vec<String> {
        self.listed.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl LocalFs for StubFs {
    fn is_dir(&self, path: &str) -> bool {
        if let Ok(mut threads) = self.threads.lock() {
            threads.push(std::thread::current().id());
        }
        !self.not_dirs.contains(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<Entry>, String> {
        if let Ok(mut listed) = self.listed.lock() {
            listed.push(path.to_owned());
        }
        if let Some(latch) = self.blocked.get(path) {
            latch.wait();
        }
        if let Some(delay) = self.delays.get(path) {
            std::thread::sleep(*delay);
        }
        assert!(!self.panics.contains(path), "桩：故意 panic");
        if self.broken.contains(path) {
            return Err("桩：读不了".to_owned());
        }
        Ok(vec![
            row(&format!("{path}-dir"), EntryKind::Dir),
            row(&format!("{path}-file"), EntryKind::File),
        ])
    }

    fn start_dir(&self, wanted: Option<&str>) -> String {
        if let Ok(mut threads) = self.threads.lock() {
            threads.push(std::thread::current().id());
        }
        match wanted {
            Some(dir) if !self.not_dirs.contains(dir) => dir.to_owned(),
            _ => "DEFAULT".to_owned(),
        }
    }
}

fn lister(fs: StubFs, workers: usize, queue: usize) -> (LocalLister, Arc<StubFs>) {
    let fs = Arc::new(fs);
    let pool = Arc::new(LocalPool::new(workers, queue));
    (LocalLister::new(fs.clone(), pool), fs)
}

fn ask(
    lister: &LocalLister,
    gate: &SeqGate,
    request: LocalRequest,
) -> (Result<u64, String>, Receiver<LocalReply>) {
    let (tx, rx) = channel();
    let seq = request_listing(lister, gate, request, move |reply| {
        let _ = tx.send(reply);
    });
    (seq, rx)
}

fn dir(path: &str) -> LocalRequest {
    LocalRequest::Dir(path.to_owned())
}

#[test]
fn a_slow_directory_does_not_hold_the_ui_callback() {
    let mut stub = StubFs::default();
    stub.delays
        .insert("slow".to_owned(), Duration::from_millis(800));
    let (lister, fs) = lister(stub, 2, 8);
    let gate = SeqGate::default();

    let started = Instant::now();
    let (seq, replies) = ask(&lister, &gate, dir("slow"));
    let elapsed = started.elapsed();
    assert!(seq.is_ok());
    assert!(
        elapsed < UI_BUDGET,
        "界面回调里只该提交请求，实际花了 {elapsed:?}（目录要读 800ms）"
    );

    let reply = replies.recv_timeout(WAIT).expect("慢目录最终读完");
    assert!(reply.result.is_ok());
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "桩确实慢：真正的读取发生在提交之后"
    );
    // 读取（含 is_dir）都不在调用线程上。
    let me = std::thread::current().id();
    let threads = fs.threads.lock().expect("线程记录").clone();
    assert!(!threads.is_empty() && threads.iter().all(|t| *t != me));
}

#[test]
fn a_late_result_for_the_old_directory_never_replaces_the_new_one() {
    let mut stub = StubFs::default();
    stub.delays
        .insert("A".to_owned(), Duration::from_millis(600));
    let (lister, fs) = lister(stub, 4, 16);
    let gate = SeqGate::default();
    let (tx, rx) = channel();

    let started = Instant::now();
    let send = |path: &str| {
        let tx = tx.clone();
        request_listing(&lister, &gate, dir(path), move |reply| {
            let _ = tx.send(reply);
        })
        .expect("提交");
    };
    send("A");
    // 等 A 真正开始读（它在慢盘上卡着），用户这时立刻切去 B。
    while !fs.listed().contains(&"A".to_owned()) {
        assert!(started.elapsed() < WAIT, "A 一直没开始读");
        std::thread::sleep(Duration::from_millis(5));
    }
    let before_b = Instant::now();
    send("B");
    assert!(before_b.elapsed() < UI_BUDGET, "A 还在读时切目录也不卡界面");

    // 界面一侧的接收逻辑：按到达顺序处理，只有最新一次的结果才换上列表。
    let mut shown: Option<String> = None;
    let mut arrival = Vec::new();
    let mut discarded = Vec::new();
    for _ in 0..2 {
        let reply = rx.recv_timeout(WAIT).expect("两次请求都有回报");
        arrival.push(reply.target.clone());
        if accepts(&gate, &reply) {
            shown = Some(reply.target);
        } else {
            discarded.push(reply.target);
        }
    }
    assert_eq!(arrival, ["B", "A"], "A 读得慢，比 B 晚到");
    assert_eq!(discarded, ["A"], "迟到的 A 被丢弃");
    assert_eq!(shown.as_deref(), Some("B"), "界面停在 B，没被 A 盖回去");
}

#[test]
fn queued_requests_that_were_already_replaced_are_not_read_at_all() {
    let latch = Arc::new(Latch::default());
    let mut stub = StubFs::default();
    stub.blocked.insert("A".to_owned(), latch.clone());
    // 只有一个工作线程：A 卡住时，后面的请求只能在队里排着。
    let (lister, fs) = lister(stub, 1, 16);
    let gate = SeqGate::default();

    let (first, a_replies) = ask(&lister, &gate, dir("A"));
    assert!(first.is_ok());
    // 等 A 真正开始读，再连点三次。
    let began = Instant::now();
    while !fs.listed().contains(&"A".to_owned()) {
        assert!(began.elapsed() < WAIT, "A 一直没开始读");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (_, _b) = ask(&lister, &gate, dir("B"));
    let (_, _c) = ask(&lister, &gate, dir("C"));
    let (last, d_replies) = ask(&lister, &gate, dir("D"));
    assert!(last.is_ok());
    latch.open();

    let d = d_replies.recv_timeout(WAIT).expect("最后一次请求有结果");
    assert!(accepts(&gate, &d) && d.target == "D");
    let a = a_replies.recv_timeout(WAIT).expect("A 读完后照样回报");
    assert!(!accepts(&gate, &a), "A 的结果已经作废");
    assert_eq!(
        fs.listed(),
        ["A", "D"],
        "排队时就被取代的 B、C 不该再去读磁盘"
    );
}

#[test]
fn a_full_queue_refuses_new_work_instead_of_blocking_the_ui() {
    let latch = Arc::new(Latch::default());
    let mut stub = StubFs::default();
    stub.blocked.insert("hold".to_owned(), latch.clone());
    let (lister, _fs) = lister(stub, 1, 2);
    let gate = SeqGate::default();

    let (first, _held) = ask(&lister, &gate, dir("hold"));
    assert!(first.is_ok());
    let mut refused = false;
    for index in 0..10 {
        let started = Instant::now();
        let (outcome, _rx) = ask(&lister, &gate, dir(&format!("p{index}")));
        assert!(started.elapsed() < UI_BUDGET, "提交永远不阻塞");
        if outcome.is_err() {
            assert!(outcome.err().is_some_and(|m| m.contains("排队")));
            refused = true;
            break;
        }
    }
    latch.open();
    assert!(refused, "队列是有界的：塞满后要明确拒绝");
}

#[test]
fn a_panicking_filesystem_reports_a_failure_and_the_worker_survives() {
    let mut stub = StubFs::default();
    stub.panics.insert("boom".to_owned());
    // 池里只有一个线程：panic 要是带走了它，下面第二个请求就永远没有回应。
    let (lister, _fs) = lister(stub, 1, 8);
    let gate = SeqGate::default();

    let (_, replies) = ask(&lister, &gate, dir("boom"));
    let reply = replies.recv_timeout(WAIT).expect("panic 也要回报");
    assert!(matches!(reply.result, Err(ScanError::Failed(_))));
    assert_eq!(reply.target, "boom");

    let (_, replies) = ask(&lister, &gate, dir("fine"));
    let reply = replies.recv_timeout(WAIT).expect("工作线程还活着");
    assert!(reply.result.is_ok());
}

#[test]
fn scan_tells_a_missing_directory_from_an_unreadable_one() {
    let mut stub = StubFs::default();
    stub.not_dirs.insert("file.txt".to_owned());
    stub.broken.insert("locked".to_owned());
    assert_eq!(scan(&stub, "file.txt").err(), Some(ScanError::NotDir));
    assert!(
        stub.listed().is_empty(),
        "不是目录就不再去读它（和以前先 is_dir 一致）"
    );
    assert!(matches!(
        scan(&stub, "locked"),
        Err(ScanError::Failed(message)) if message.contains("读不了")
    ));
    // “此电脑”（空路径）不是真目录，不做 is_dir 校验。
    stub.not_dirs.insert(String::new());
    assert!(scan(&stub, "").is_ok());
}

#[test]
fn the_landing_directory_of_a_new_view_is_decided_off_the_ui_thread() {
    let mut stub = StubFs::default();
    stub.not_dirs.insert("gone".to_owned());
    let (lister, fs) = lister(stub, 2, 8);
    let gate = SeqGate::default();

    let (_, replies) = ask(&lister, &gate, LocalRequest::Start(Some("gone".to_owned())));
    let reply = replies.recv_timeout(WAIT).expect("首屏");
    assert_eq!(reply.target, "DEFAULT", "带来的目录已不是目录：退回默认");
    assert!(reply.result.is_ok());

    let (_, replies) = ask(&lister, &gate, LocalRequest::Start(Some("kept".to_owned())));
    assert_eq!(replies.recv_timeout(WAIT).expect("首屏").target, "kept");

    let me = std::thread::current().id();
    let threads = fs.threads.lock().expect("线程记录").clone();
    assert!(threads.iter().all(|t| *t != me), "定落点也在后台线程");
}

#[test]
fn the_move_dialog_browses_local_folders_through_the_same_bounded_path() {
    let mut stub = StubFs::default();
    stub.delays
        .insert("slow".to_owned(), Duration::from_millis(500));
    let (lister, _fs) = lister(stub, 2, 8);
    let gate = SeqGate::default();
    let (tx, rx) = channel();

    let started = Instant::now();
    let sent = tx.clone();
    lister
        .spawn(
            &gate,
            |fs| super::move_to::local_folders(fs, "slow"),
            move |seq, outcome| {
                let _ = sent.send((seq, outcome));
            },
        )
        .expect("提交");
    assert!(
        started.elapsed() < UI_BUDGET,
        "对话框里浏览本地目录不卡界面"
    );

    let (seq, outcome) = rx.recv_timeout(WAIT).expect("读完");
    assert!(gate.is_latest(seq));
    let folders = outcome.expect("没 panic").expect("读得出来");
    assert_eq!(folders.len(), 1, "只列文件夹，不列文件");
    assert_eq!(folders[0].name, "slow-dir");
}

#[test]
fn a_real_directory_is_listed_on_a_worker_thread() {
    let _root_guard = tempfile::Builder::new()
        .prefix("zssh-c3-list-")
        .tempdir()
        .expect("建临时目录");
    let root = _root_guard.path().to_path_buf();
    std::fs::create_dir_all(root.join("sub")).expect("建临时目录");
    std::fs::write(root.join("a.txt"), b"hello").expect("写文件");

    let lister = LocalLister::real(super::local_start::StartPlan::default());
    let gate = SeqGate::default();
    let target = root.display().to_string();
    let (_, replies) = ask(&lister, &gate, dir(&target));
    let reply = replies.recv_timeout(WAIT).expect("真目录读完");
    let mut names: Vec<String> = reply
        .result
        .expect("读得出来")
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "sub"]);

    let missing = root.join("nope").display().to_string();
    let (_, replies) = ask(&lister, &gate, dir(&missing));
    assert_eq!(
        replies.recv_timeout(WAIT).expect("回报").result.err(),
        Some(ScanError::NotDir)
    );
}
