//! V2-G10b 的离线流程测试：真的工具桥（TCP + 能力密钥）+ 真的 `NativeBridgeHost`，终端用登记表里的
//! 假终端，文件系统用内存实现。覆盖：文件工具四种、两种执行通道（终端 / 后台的判定）、读终端、Ctrl+C、
//! 目标「当前」与迟到回报、长期记忆、审批超时。联网的真实服务器用例在 `live_support` 里的 `live_g10b_*`。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::TcpListener;
use zssh_core::ai::bridge::{AuditEntry, AuditLog, ExecMode, ExecModeProvider};
use zssh_core::ai::files::{FsEntry, FsKind, FsStat, RemoteFs, RenameFailure};
use zssh_core::ssh::BoxFuture;
use zssh_core::store::Database;
use zssh_session::{CoreFilter, SessionDriver};

use crate::agent_host::{HostTarget, NativeBridgeHost, ToolChannel};
use crate::agent_setup::fake_tools::{BridgeClient, CallOutcome, FakeBridge};
use crate::ai_terminals::{terminals, testing};

struct RecordingAudit(Mutex<Vec<AuditEntry>>);

impl AuditLog for RecordingAudit {
    fn record(&self, entry: AuditEntry) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(entry);
    }
}

struct FixedMode(ExecMode);

impl ExecModeProvider for FixedMode {
    fn mode(&self) -> ExecMode {
        self.0
    }
}

/// 极小的内存文件系统：只实现文件工具会用到的那几个操作。
#[derive(Default)]
struct MemFs {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    dirs: Mutex<Vec<String>>,
}

impl MemFs {
    fn with_file(path: &str, content: &str) -> Arc<Self> {
        let fs = Arc::new(Self::default());
        fs.dirs.lock().unwrap_or_else(|p| p.into_inner()).extend([
            "/".to_owned(),
            "/home/u".to_owned(),
            "/srv".to_owned(),
        ]);
        fs.files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(path.to_owned(), content.as_bytes().to_vec());
        fs
    }

    fn read(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(path)
            .map(|data| String::from_utf8_lossy(data).into_owned())
    }

    fn names(&self) -> Vec<String> {
        self.files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

impl RemoteFs for MemFs {
    fn home<'a>(&'a self) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Ok("/home/u".to_owned()) })
    }

    fn stat<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<FsStat>, String>> {
        Box::pin(async move {
            if let Some(data) = self
                .files
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(path)
            {
                return Ok(Some(FsStat {
                    kind: FsKind::File,
                    size: data.len() as u64,
                    mtime: Some(1_700_000_000),
                    mode: Some(0o644),
                    uid: Some(1000),
                    gid: Some(1000),
                }));
            }
            let is_dir = self
                .dirs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|dir| dir == path);
            Ok(is_dir.then_some(FsStat {
                kind: FsKind::Dir,
                size: 0,
                mtime: Some(1_700_000_000),
                mode: Some(0o755),
                uid: Some(1000),
                gid: Some(1000),
            }))
        })
    }

    fn realpath<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { Ok(path.to_owned()) })
    }

    fn list<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<FsEntry>, String>> {
        Box::pin(async move {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            let entries = self
                .files
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter_map(|(name, data)| {
                    let rest = name.strip_prefix(&prefix)?;
                    (!rest.contains('/')).then(|| FsEntry {
                        name: rest.to_owned(),
                        kind: FsKind::File,
                        size: Some(data.len() as u64),
                        mtime: Some(1_700_000_000),
                        mode: Some(0o644),
                        link_target: None,
                    })
                })
                .collect();
            Ok(entries)
        })
    }

    fn read_range<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        len: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            let data = files.get(path).ok_or("文件不存在")?;
            let start = (offset as usize).min(data.len());
            let end = (start + len).min(data.len());
            Ok(data[start..end].to_vec())
        })
    }

    fn read_all<'a>(&'a self, path: &'a str, limit: u64) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            let data = files.get(path).ok_or("文件不存在")?;
            if data.len() as u64 > limit {
                return Err("文件太大".to_owned());
            }
            Ok(data.clone())
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a str,
        data: &'a [u8],
        _mode: Option<u32>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.files
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(path.to_owned(), data.to_vec());
            Ok(())
        })
    }

    fn rename_over<'a>(
        &'a self,
        from: &'a str,
        to: &'a str,
    ) -> BoxFuture<'a, Result<(), RenameFailure>> {
        Box::pin(async move {
            let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            let data = files
                .remove(from)
                .ok_or_else(|| RenameFailure::untouched("源文件不存在"))?;
            files.insert(to.to_owned(), data);
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.files
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(path);
            Ok(())
        })
    }

    fn create_exclusive<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<(), zssh_core::ai::files::CreateExclusiveError>> {
        Box::pin(async move {
            let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            if files.contains_key(path) {
                return Err(zssh_core::ai::files::CreateExclusiveError::AlreadyExists);
            }
            files.insert(path.to_owned(), Vec::new());
            Ok(())
        })
    }

    fn set_owner<'a>(
        &'a self,
        _path: &'a str,
        _uid: Option<u32>,
        _gid: Option<u32>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// 一个对话：真的桥 + 真的宿主 + 客户端。
struct Rig {
    tools: ToolChannel,
    bridge: FakeBridge,
    client: BridgeClient,
    host: Arc<NativeBridgeHost>,
    dir: PathBuf,
    /// 放在最后：字段按声明顺序析构，宿主等先放掉，目录最后删。
    _guard: tempfile::TempDir,
}

/// 随机唯一的临时目录，由 `Rig` 带着，对话结束时整棵删掉。
fn scratch_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("zssh-g10b-{name}-"))
        .tempdir()
        .expect("建临时目录")
}

async fn rig_waiting(name: &str, mode: ExecMode, approval_wait: Duration) -> Rig {
    let guard = scratch_dir(name);
    let dir = guard.path().to_path_buf();
    let mut host_handle = None;
    let tools = ToolChannel::new_with_host(
        |reports| {
            let host = Arc::new(NativeBridgeHost::with(reports, dir.clone()));
            host_handle = Some(Arc::clone(&host));
            host
        },
        Arc::new(RecordingAudit(Mutex::new(Vec::new()))),
        Arc::new(FixedMode(mode)),
        approval_wait,
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("绑定");
    let port = listener.local_addr().expect("端口").port();
    tokio::spawn(Arc::clone(tools.bridge()).serve(listener));
    let bridge = FakeBridge {
        port,
        token: tools.bridge().connection_token().to_owned(),
        capability: tools.scope().capability().to_owned(),
    };
    let client = BridgeClient::connect(&bridge).await.expect("连上桥");
    Rig {
        tools,
        bridge,
        client,
        host: host_handle.expect("宿主"),
        dir,
        _guard: guard,
    }
}

async fn rig(name: &str, mode: ExecMode) -> Rig {
    rig_waiting(name, mode, Duration::from_secs(30)).await
}

impl Rig {
    fn target(&self, host_id: i64, current: bool) {
        let mut targets = vec![HostTarget {
            id: host_id,
            name: format!("web-{host_id}"),
            username: "root".to_owned(),
            current,
            pinned: !current,
            refusal: None,
            terminal: None,
        }];
        for existing in self.tools.scope().targets() {
            if let Some(id) = zssh_core::ai::protocol::parse_host_target_id(&existing.id)
                && id != host_id
            {
                targets.push(HostTarget {
                    id,
                    name: existing.host_label.clone(),
                    username: existing.username.clone(),
                    current: existing.current,
                    pinned: existing.pinned,
                    refusal: None,
                    terminal: None,
                });
            }
        }
        self.tools.sync_hosts(&targets);
    }

    async fn call(&mut self, method: &str, params: Value) -> CallOutcome {
        self.client.call(method, params).await.expect("工具桥通着")
    }

    /// 再开一条到同一个桥的连接（把调用放到后台任务里等的时候用）。
    async fn extra_client(&self) -> BridgeClient {
        BridgeClient::connect(&self.bridge).await.expect("连上桥")
    }

    fn scope_id(&self) -> String {
        self.tools.scope().scope_id().to_owned()
    }
}

async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等不到：{what}");
}

// ---------------------------------------------------------------------------
// 终端里执行 / 读终端
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commands_run_in_the_focus_terminal_and_read_terminal_sees_the_output() {
    let mut rig = rig("term", ExecMode::Auto).await;
    let term = testing::spawn(Some(9101), "hang");
    rig.target(9101, true);

    let outcome = rig
        .call("run_command", json!({"command": "echo hello-terminal"}))
        .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(
        outcome.text.contains("location: terminal"),
        "{}",
        outcome.text
    );
    assert!(outcome.text.contains("exit_code: 0"), "{}", outcome.text);
    assert!(outcome.text.contains("hello-terminal"), "{}", outcome.text);
    assert_eq!(term.log(), vec!["write:echo hello-terminal".to_owned()]);

    // 执行位置回报：落在这个终端上，卡片据此显示「查看终端」。
    let report = rig.tools.next_report().expect("要有执行位置回报");
    assert_eq!(report.tool, "run_command");
    assert_eq!(report.target_id, "host-9101");
    assert_eq!(report.session_id.as_deref(), Some(term.entry.id.as_str()));

    // read_terminal 读的是同一个终端的缓冲，且是原始输出。
    let read = rig.call("read_terminal", json!({"max_lines": 50})).await;
    assert!(read.ok, "{}", read.text);
    assert!(read.text.contains("hello-terminal"), "{}", read.text);
    assert!(
        terminals().runs().running(&rig.scope_id()).is_none(),
        "命令结束后不再登记「在终端里跑」"
    );
}

#[tokio::test]
async fn a_host_with_two_terminals_uses_the_one_that_got_focus() {
    let mut rig = rig("focus", ExecMode::Auto).await;
    let first = testing::spawn(Some(9102), "hang");
    let second = testing::spawn(Some(9102), "hang");
    rig.target(9102, true);

    terminals().mark_focused(&second.entry.id);
    let outcome = rig
        .call("run_command", json!({"command": "echo second"}))
        .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(second.log(), vec!["write:echo second".to_owned()]);
    assert!(first.log().is_empty(), "没聚焦的终端不能被写");

    terminals().mark_focused(&first.entry.id);
    let outcome = rig
        .call("run_command", json!({"command": "echo first"}))
        .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(first.log(), vec!["write:echo first".to_owned()]);

    let report = rig.tools.next_report().expect("回报");
    assert_eq!(report.session_id.as_deref(), Some(second.entry.id.as_str()));
}

#[tokio::test]
async fn placement_falls_back_to_the_background_with_a_reason() {
    let rig = rig("placement", ExecMode::Auto).await;
    use zssh_core::ai::bridge::{BridgeHost, ExecPlacement};

    // 没有任何打开的终端：后台通道。
    match rig.host.exec_placement("host-9103").await {
        ExecPlacement::Background { reason } => {
            assert!(reason.contains("没有打开的终端"), "{reason}")
        }
        other => panic!("没有终端该走后台：{other:?}"),
    }

    // 终端忙（输入到一半）：后台通道，原因写明。
    let term = testing::spawn(Some(9103), "hang");
    term.print("\r\nsome output still running");
    match rig.host.exec_placement("host-9103").await {
        ExecPlacement::Background { reason } => assert!(reason.contains("忙"), "{reason}"),
        other => panic!("终端忙该走后台：{other:?}"),
    }

    // 全屏程序里：后台通道。
    term.print("\x1b[?1049h");
    match rig.host.exec_placement("host-9103").await {
        ExecPlacement::Background { reason } => assert!(reason.contains("全屏"), "{reason}"),
        other => panic!("全屏程序里该走后台：{other:?}"),
    }
    term.print("\x1b[?1049lbye\r\nuser@host:~$ ");

    // 回到提示符：终端里执行，并占住终端，释放后才能再占。
    assert_eq!(
        rig.host.exec_placement("host-9103").await,
        ExecPlacement::Terminal
    );
    match rig.host.exec_placement("host-9103").await {
        ExecPlacement::Background { reason } => {
            assert!(reason.contains("已有一条 AI 命令"), "{reason}")
        }
        other => panic!("同一个终端不能叠两条 AI 命令：{other:?}"),
    }
    rig.host.release_terminal("host-9103").await;
    assert_eq!(
        rig.host.exec_placement("host-9103").await,
        ExecPlacement::Terminal
    );
    rig.host.release_terminal("host-9103").await;
}

#[tokio::test]
async fn read_terminal_without_a_terminal_says_so() {
    let mut rig = rig("noterm", ExecMode::Auto).await;
    rig.target(9104, true);
    let outcome = rig.call("read_terminal", json!({})).await;
    assert!(!outcome.ok);
    assert!(outcome.text.contains("没有打开的终端"), "{}", outcome.text);
}

#[tokio::test]
async fn ctrl_c_goes_only_to_the_terminal_where_this_conversation_runs_a_command() {
    let rig = rig("ctrlc", ExecMode::Auto).await;
    let term = testing::spawn(Some(9105), "hang");
    rig.target(9105, true);
    let scope = rig.scope_id();
    let bridge = Arc::clone(rig.tools.bridge());

    // 没有 AI 命令在跑：停止按钮不能乱发 Ctrl+C（用户可能正在终端里跑自己的程序）。
    bridge.interrupt_terminal(&scope).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(term.interrupts.load(Ordering::SeqCst), 0);

    // 一条一直不结束的命令，放在后台任务里等。
    let mut client = rig.extra_client().await;
    let running = tokio::spawn(async move {
        client
            .call("run_command", json!({"command": "hang", "timeout_sec": 30}))
            .await
    });
    eventually("命令登记为「在终端里跑」", || {
        terminals().runs().running(&scope).is_some()
    })
    .await;
    assert_eq!(
        terminals().runs().running(&scope).map(|run| run.session_id),
        Some(term.entry.id.clone())
    );

    // 停止：先作废桥里的请求，再向这个终端发 Ctrl+C。
    bridge.interrupt_terminal(&scope).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("Ctrl+C 之后命令要很快结束")
        .expect("任务不 panic")
        .expect("桥通着");
    assert_eq!(
        term.interrupts.load(Ordering::SeqCst),
        1,
        "只发了一次 Ctrl+C"
    );
    assert!(outcome.text.contains("exit_code: 130"), "{}", outcome.text);
    assert!(
        terminals().runs().running(&scope).is_none(),
        "命令结束，记录清掉；之后的停止不会再打断这个终端"
    );
    bridge.interrupt_terminal(&scope).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(term.interrupts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_command_that_times_out_keeps_the_terminal_state_until_it_really_ends() {
    let mut rig = rig("timeout", ExecMode::Auto).await;
    let term = testing::spawn(Some(9106), "hang");
    rig.target(9106, true);
    let scope = rig.scope_id();

    let outcome = rig
        .call("run_command", json!({"command": "hang", "timeout_sec": 1}))
        .await;
    assert!(outcome.ok, "超时不是错误：{}", outcome.text);
    assert!(
        outcome.text.contains("exit_code: unknown"),
        "还在跑，没有退出码：{}",
        outcome.text
    );
    assert!(
        term.log().iter().any(|line| line.starts_with("timed-out:")),
        "要通知会话任务：显示过滤器退出完整过滤 {:?}",
        term.log()
    );
    // 命令还在终端里跑：记录留着，停止按钮对它有效。
    assert!(terminals().runs().running(&scope).is_some());

    // 用户（或停止按钮）按了 Ctrl+C，命令的结束标记迟到地出现：观察任务把记录清掉。
    rig.tools.bridge().interrupt_terminal(&scope).await;
    eventually("迟到的结束标记清掉记录", || {
        terminals().runs().running(&scope).is_none()
    })
    .await;
    assert_eq!(term.interrupts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stopping_while_waiting_for_approval_never_touches_the_terminal() {
    let rig = rig("stopwait", ExecMode::Manual).await;
    let term = testing::spawn(Some(9107), "hang");
    rig.target(9107, true);
    let mut client = rig.extra_client().await;
    let running = tokio::spawn(async move {
        client
            .call("run_command", json!({"command": "echo never"}))
            .await
    });
    let mut prompt = None;
    eventually("确认请求到了窗口", || {
        prompt = prompt.take().or_else(|| rig.tools.next_prompt());
        prompt.is_some()
    })
    .await;
    rig.tools.stop();
    let outcome = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("停止后请求马上结束")
        .expect("任务不 panic")
        .expect("桥通着");
    assert!(!outcome.ok, "{}", outcome.text);
    assert!(
        term.log().is_empty(),
        "终端里什么都不能出现：{:?}",
        term.log()
    );
}

// ---------------------------------------------------------------------------
// 审批超时
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approval_that_runs_out_of_time_is_a_denial() {
    let mut rig = rig_waiting("expire", ExecMode::Manual, Duration::from_millis(250)).await;
    let term = testing::spawn(Some(9108), "hang");
    rig.target(9108, true);
    let outcome = rig
        .call("run_command", json!({"command": "echo too-late"}))
        .await;
    assert!(!outcome.ok, "{}", outcome.text);
    assert!(
        outcome.text.contains("超时"),
        "要说明是审批超时：{}",
        outcome.text
    );
    assert!(term.log().is_empty(), "按拒绝处理：命令一次都没执行");
    // 卡片侧：队列里那条提示已经不是活的，窗口据此把卡片标成失败（见 `expire_dead_approvals`）。
    let prompt = rig.tools.next_prompt();
    assert!(
        prompt.is_none(),
        "超时的确认不能再交给界面当作可点的卡片：{prompt:?}"
    );
}

// ---------------------------------------------------------------------------
// 文件工具
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_four_file_tools_work_through_the_bridge() {
    let mut rig = rig("files", ExecMode::Auto).await;
    let fs = MemFs::with_file("/srv/app.conf", "port=80\nmode=dev\n");
    rig.host.set_test_fs(fs.clone());
    rig.target(9109, true);

    let list = rig.call("list_dir", json!({"path": "/srv"})).await;
    assert!(list.ok, "{}", list.text);
    assert!(list.text.contains("app.conf"), "{}", list.text);

    let stat = rig.call("stat", json!({"path": "/srv/app.conf"})).await;
    assert!(
        stat.ok && stat.text.contains("exists: true"),
        "{}",
        stat.text
    );
    let missing = rig.call("stat", json!({"path": "/srv/none"})).await;
    assert!(
        missing.ok && missing.text.contains("exists: false"),
        "{}",
        missing.text
    );

    let read = rig
        .call("read_file", json!({"path": "/srv/app.conf"}))
        .await;
    assert!(read.ok && read.text.contains("mode=dev"), "{}", read.text);

    // 覆盖已有文件：先留备份。
    let write = rig
        .call(
            "write_file",
            json!({"path": "/srv/app.conf", "content": "port=8080\n"}),
        )
        .await;
    assert!(write.ok, "{}", write.text);
    assert!(
        write
            .text
            .contains("saved on the server at /srv/app.conf.zssh-bak-"),
        "要告诉用户原文件备份到了哪儿：{}",
        write.text
    );
    assert_eq!(fs.read("/srv/app.conf").as_deref(), Some("port=8080\n"));
    assert!(
        fs.names().iter().any(|name| name.contains(".zssh-bak-")),
        "服务器上要真的有备份文件：{:?}",
        fs.names()
    );

    // 新建文件：没有备份。
    let created = rig
        .call(
            "write_file",
            json!({"path": "/srv/new.txt", "content": "hi", "create_only": true}),
        )
        .await;
    assert!(
        created.ok && created.text.contains("backup: none"),
        "{}",
        created.text
    );

    // 每次文件工具调用都回报了执行位置，没有终端也不例外。
    let mut tools = Vec::new();
    while let Some(report) = rig.tools.next_report() {
        assert_eq!(report.target_id, "host-9109");
        assert_eq!(report.session_id, None, "没有终端：不绑会话");
        tools.push(report.tool);
    }
    assert_eq!(
        tools,
        vec![
            "list_dir",
            "stat",
            "stat",
            "read_file",
            "write_file",
            "write_file"
        ]
    );
}

#[tokio::test]
async fn a_write_approval_shows_the_diff_and_a_denial_leaves_the_file_alone() {
    let rig = rig("diff", ExecMode::Manual).await;
    let fs = MemFs::with_file("/srv/app.conf", "a=1\nb=2\n");
    rig.host.set_test_fs(fs.clone());
    rig.target(9110, true);
    let mut client = rig.extra_client().await;
    let running = tokio::spawn(async move {
        client
            .call(
                "write_file",
                json!({"path": "/srv/app.conf", "content": "a=1\nb=3\n"}),
            )
            .await
    });
    let mut prompt = None;
    eventually("write_file 的确认请求", || {
        prompt = prompt.take().or_else(|| rig.tools.next_prompt());
        prompt.is_some()
    })
    .await;
    let prompt = prompt.expect("确认请求");
    let file = prompt
        .request
        .file
        .as_ref()
        .expect("write_file 的确认带文件细节");
    assert_eq!(file.path, "/srv/app.conf");
    assert!(!file.is_new, "覆盖已有文件");
    assert!(
        file.diff.removed >= 1 && file.diff.added >= 1,
        "有差异：{:?}",
        file.diff
    );
    assert_eq!(prompt.request.target_id, "host-9110");

    assert!(rig.tools.resolve_approval(prompt.id, false));
    let outcome = running.await.expect("任务不 panic").expect("桥通着");
    assert!(!outcome.ok, "{}", outcome.text);
    assert_eq!(
        fs.read("/srv/app.conf").as_deref(),
        Some("a=1\nb=2\n"),
        "拒绝后文件没动"
    );
    assert_eq!(
        fs.names().len(),
        1,
        "也没有留下备份或临时文件：{:?}",
        fs.names()
    );
}

// ---------------------------------------------------------------------------
// 目标：「当前」、迟到的回报
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_call_without_target_id_lands_on_the_current_host() {
    let mut rig = rig("current", ExecMode::Auto).await;
    let web = testing::spawn(Some(9111), "hang");
    let db = testing::spawn(Some(9112), "hang");
    rig.target(9112, false);
    rig.target(9111, true);

    let outcome = rig
        .call("run_command", json!({"command": "echo who"}))
        .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(
        web.log(),
        vec!["write:echo who".to_owned()],
        "没写 target_id：落在「当前」那台"
    );
    assert!(db.log().is_empty());

    // 点名另一台主机照常。
    let outcome = rig
        .call(
            "run_command",
            json!({"command": "echo named", "target_id": "host-9112"}),
        )
        .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(db.log(), vec!["write:echo named".to_owned()]);

    // 目标栏里没有的主机：拒绝。
    let outcome = rig
        .call(
            "run_command",
            json!({"command": "echo no", "target_id": "host-9999"}),
        )
        .await;
    assert!(!outcome.ok);
}

#[tokio::test]
async fn late_reports_never_reach_a_conversation_that_replaced_the_old_one() {
    use zssh_core::ai::bridge::{BridgeHost, ExecTargetReport};
    let rig = rig("late", ExecMode::Auto).await;
    let old_scope = rig.scope_id();
    let report = ExecTargetReport {
        scope: old_scope.clone(),
        exec_id: "late-1".to_owned(),
        tool: "run_command".to_owned(),
        key: "uptime".to_owned(),
        target_id: "host-1".to_owned(),
        session_id: None,
        host_label: "web-1".to_owned(),
    };
    // 对话还在：回报送得到。
    rig.host.report_target(report.clone());
    assert_eq!(
        rig.tools.next_report().map(|r| r.exec_id),
        Some("late-1".to_owned())
    );

    // 对话结束（新对话 / 关闭）：它的出口摘掉，迟到的回报直接丢掉。
    rig.tools.close();
    rig.host.report_target(report);
    assert!(
        rig.tools.next_report().is_none(),
        "结束了的对话不再收到回报"
    );
}

// ---------------------------------------------------------------------------
// 长期记忆
// ---------------------------------------------------------------------------

fn sample_host(name: &str, address: &str) -> zssh_core::model::HostDraft {
    zssh_core::model::HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: name.to_owned(),
        host: address.to_owned(),
        port: 22,
        username: "root".to_owned(),
        auth_method: zssh_core::model::AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: zssh_core::model::ProxyMode::None,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

#[tokio::test]
async fn remember_host_saves_cleaned_notes_and_list_targets_brings_them_back() {
    let mut rig = rig("memory", ExecMode::Manual).await;
    let db = Database::open(&rig.dir.join("zssh.db")).expect("建库");
    let host = db
        .host_save(&sample_host("web-1", "192.0.2.5"))
        .expect("存主机");
    let target = format!("host-{}", host.id);
    rig.target(host.id, true);
    let before = crate::host_activity::memory_version();

    let saved = rig
        .call(
            "remember_host",
            json!({"target_id": target, "notes": "服务：nginx\ndb password: hunter2"}),
        )
        .await;
    assert!(saved.ok, "{}", saved.text);

    let stored = db
        .host_memory_get(host.id)
        .expect("读记忆")
        .expect("有记忆")
        .content;
    assert!(stored.contains("服务：nginx"), "{stored}");
    assert!(
        !stored.contains("hunter2") && stored.contains("[已隐藏]"),
        "密码在写进库之前就被 core 藏掉了：{stored}"
    );
    let (_, changed) = crate::host_activity::memory_changes_since(before);
    assert!(changed.contains(&host.id), "主机详情里的记忆框据此刷新");

    // 下次对话：list_targets 把笔记带给模型（手动模式）。
    let listed = rig.call("list_targets", json!({})).await;
    assert!(listed.ok, "{}", listed.text);
    assert!(listed.text.contains("nginx"), "{}", listed.text);

    // 清空：空串。
    let cleared = rig
        .call("remember_host", json!({"target_id": target, "notes": ""}))
        .await;
    assert!(cleared.ok, "{}", cleared.text);
    assert_eq!(db.host_memory_get(host.id).expect("读记忆"), None);

    // 不在主机库里的目标不能存记忆。
    rig.tools
        .scope()
        .add_target("sess-9", "quick", "root", "connected", false, true);
    let refused = rig
        .call(
            "remember_host",
            json!({"target_id": "sess-9", "notes": "x"}),
        )
        .await;
    assert!(!refused.ok, "{}", refused.text);
}

#[tokio::test]
async fn full_access_mode_does_not_hand_notes_to_the_model() {
    let mut rig = rig("memory-auto", ExecMode::Auto).await;
    let db = Database::open(&rig.dir.join("zssh.db")).expect("建库");
    let host = db
        .host_save(&sample_host("web-2", "192.0.2.6"))
        .expect("存主机");
    db.host_memory_set(host.id, "埋了一条指令").expect("写记忆");
    rig.target(host.id, true);
    let listed = rig.call("list_targets", json!({})).await;
    assert!(listed.ok);
    assert!(!listed.text.contains("埋了一条指令"), "{}", listed.text);
}

// ---------------------------------------------------------------------------
// 联网：测试服务器上真实执行（需要 `ZSSH_LIVE_FULL=1` 和测试服务器环境变量，缺了就跳过）
// ---------------------------------------------------------------------------

/// 联网用例里这个终端对应的「主机库主机」编号（库里并没有这台主机，工具桥只认编号）。
const LIVE_HOST: i64 = 77_001;

/// 让界面这一侧的泵跑一会儿：消费事件、给流控 ack、把输出送进终端模型。
async fn settle(driver: &mut SessionDriver<CoreFilter>, millis: u64) {
    let steps = (millis / 25).max(1);
    for _ in 0..steps {
        driver.poll();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn screen_of(driver: &SessionDriver<CoreFilter>) -> String {
    driver
        .pump()
        .snapshot()
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn live_g10b_terminal_commands_ctrl_c_and_file_tools() {
    let Some(live) = crate::live_support::LiveShell::connect("g10b") else {
        return;
    };
    let runtime = crate::ssh_backend::runtime().expect("运行时");
    runtime.block_on(live_scenario(&live));
}

async fn live_scenario(live: &crate::live_support::LiveShell) {
    use zssh_session::{SessionBackend, SessionCommand, SessionPump};
    use zssh_terminal::Size;

    // ---- 开一个真实终端（和主窗口窗格同一条路：open_async → 登记 → 泵）----
    let size = Size {
        rows: 30,
        cols: 140,
    };
    let (mut backend, events, gate) = live
        .factory
        .open_async(size, 1)
        .await
        .expect("在测试服务器上开终端");
    let terminal_id = backend.terminal_id().expect("终端登记了");
    terminals().describe(&terminal_id, Some(LIVE_HOST));
    let mut driver = SessionDriver::new(
        SessionPump::new(size, 2000, gate, CoreFilter::new(), 1),
        events,
    );
    let entry = terminals().get(&terminal_id).expect("登记表里有它");
    // 提示符改成 `$ `、清屏（别的 shell 配置不在这里管）；等 shell 空闲在提示符上。
    backend.send(SessionCommand::Write(b" PS1='$ '; clear\r".to_vec()));
    for _ in 0..200 {
        settle(&mut driver, 50).await;
        if entry.state.is_idle() {
            break;
        }
    }
    assert!(
        entry.state.is_idle(),
        "shell 没停在提示符上：{}",
        screen_of(&driver)
    );

    let mut rig = rig("live", ExecMode::Auto).await;
    rig.target(LIVE_HOST, true);
    let scope = rig.scope_id();
    let dir = live.dir.clone();

    // ---- 1. 在终端里执行 + 读终端 ----
    let ran = rig
        .call("run_command", json!({"command": "echo live-g10b-$((6*7))"}))
        .await;
    assert!(ran.ok, "{}", ran.text);
    assert!(ran.text.contains("location: terminal"), "{}", ran.text);
    assert!(
        ran.text.contains("live-g10b-42") && ran.text.contains("exit_code: 0"),
        "{}",
        ran.text
    );
    let report = rig.tools.next_report().expect("执行位置回报");
    assert_eq!(report.session_id.as_deref(), Some(terminal_id.as_str()));
    let read = rig.call("read_terminal", json!({"max_lines": 40})).await;
    assert!(
        read.ok && read.text.contains("live-g10b-42"),
        "{}",
        read.text
    );

    // 用户在终端里看到的：命令原文（合成回显）和输出，没有包装脚本和标记。
    settle(&mut driver, 400).await;
    let screen = screen_of(&driver);
    assert!(screen.contains("live-g10b-42"), "屏幕上要有输出：{screen}");
    assert!(
        screen.contains("echo live-g10b-$((6*7))"),
        "屏幕上要有命令原文：{screen}"
    );
    assert!(
        !screen.contains("__ZSSH_") && !screen.contains("trap ':' INT"),
        "{screen}"
    );

    // ---- 2. 后台通道：命令里有 Tab 不走终端 ----
    let background = rig
        .call("run_command", json!({"command": "echo\tbg-g10b"}))
        .await;
    assert!(background.ok, "{}", background.text);
    assert!(
        background.text.contains("location: background"),
        "{}",
        background.text
    );
    assert!(
        background.text.contains("ran in the background because"),
        "{}",
        background.text
    );

    // ---- 3. 文件工具（只在专用目录里）----
    let made = rig
        .call(
            "run_command",
            json!({"command": format!("mkdir -p '{dir}'")}),
        )
        .await;
    assert!(
        made.ok && made.text.contains("exit_code: 0"),
        "{}",
        made.text
    );
    let file = format!("{dir}/a.txt");
    let created = rig
        .call(
            "write_file",
            json!({"path": file, "content": "line1\nline2\n", "create_only": true}),
        )
        .await;
    assert!(
        created.ok && created.text.contains("backup: none"),
        "{}",
        created.text
    );
    let read = rig.call("read_file", json!({"path": file})).await;
    assert!(read.ok && read.text.contains("line2"), "{}", read.text);
    let stat = rig.call("stat", json!({"path": file})).await;
    assert!(
        stat.ok && stat.text.contains("exists: true") && stat.text.contains("size: 12"),
        "{}",
        stat.text
    );
    let overwritten = rig
        .call("write_file", json!({"path": file, "content": "changed\n"}))
        .await;
    assert!(overwritten.ok, "{}", overwritten.text);
    assert!(
        overwritten
            .text
            .contains(&format!("saved on the server at {file}.zssh-bak-")),
        "覆盖前要在服务器上留备份：{}",
        overwritten.text
    );
    let listed = rig.call("list_dir", json!({"path": dir})).await;
    assert!(
        listed.ok && listed.text.contains("a.txt") && listed.text.contains(".zssh-bak-"),
        "{}",
        listed.text
    );
    let cat = rig
        .call("run_command", json!({"command": format!("cat '{file}'")}))
        .await;
    assert!(cat.ok && cat.text.contains("changed"), "{}", cat.text);

    // ---- 4. Ctrl+C：打断终端里的 sleep 100，终端之后还能用 ----
    let mut sleeper = rig.extra_client().await;
    let sleeping = tokio::spawn(async move {
        sleeper
            .call(
                "run_command",
                json!({"command": "sleep 100", "timeout_sec": 90}),
            )
            .await
    });
    for _ in 0..100 {
        settle(&mut driver, 50).await;
        if terminals().runs().running(&scope).is_some() {
            break;
        }
    }
    assert!(
        terminals().runs().running(&scope).is_some(),
        "sleep 100 没有登记成在终端里跑"
    );
    settle(&mut driver, 1200).await;
    let started = std::time::Instant::now();
    rig.tools.bridge().interrupt_terminal(&scope).await;
    let interrupted = tokio::time::timeout(Duration::from_secs(20), sleeping)
        .await
        .expect("Ctrl+C 之后 sleep 100 要很快结束")
        .expect("任务不 panic")
        .expect("桥通着");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "没有等 100 秒，是被打断的"
    );
    assert!(
        interrupted.text.contains("exit_code: 130"),
        "{}",
        interrupted.text
    );
    assert!(terminals().runs().running(&scope).is_none(), "记录要清掉");
    let after = rig
        .call("run_command", json!({"command": "echo after-ctrl-c"}))
        .await;
    assert!(
        after.ok
            && after.text.contains("after-ctrl-c")
            && after.text.contains("location: terminal"),
        "打断之后终端还能用：{}",
        after.text
    );

    // ---- 清理：删专用目录（`LiveShell` 的 Drop 也会再删一次，panic 时同样）----
    let cleaned = rig
        .call(
            "run_command",
            json!({"command": format!("rm -rf '{dir}' && echo cleaned")}),
        )
        .await;
    assert!(
        cleaned.ok && cleaned.text.contains("cleaned"),
        "{}",
        cleaned.text
    );
    // 服务器上不能留下东西：本次用例的目录（和同前缀的残留）一个都没有。
    let left = live
        .connection
        .exec_capture(
            "ls -d /tmp/zssh-live-g10b-* 2>/dev/null | wc -l",
            Duration::from_secs(15),
            4096,
        )
        .await
        .expect("核对残留");
    println!("LEFT={}", left.stdout.trim());
    assert_eq!(left.stdout.trim(), "0", "服务器上有残留");
    backend.send(SessionCommand::Close);
}
