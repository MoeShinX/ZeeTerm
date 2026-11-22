//! 宿主能力的四个 trait：[`ExecHost`]（执行 / 读终端 / 终端占位）、[`FsHost`]（文件）、
//! [`MemoryHost`]（主机记忆）、[`ReportHost`]（目标与执行位置上报）。
//!
//! 为什么拆开：宿主契约原来只有 [`BridgeHost`] 一个 trait，方法一多，哪些默认实现是
//! "宿主没有这个能力"就看不清了（读记忆默认 `Ok(None)`、写记忆默认报错、终端执行默认
//! 降级到后台）。按能力拆开以后，每个能力只声明它必须回答的问题，只依赖一种能力的代码
//! 可以把参数收窄成那个 trait，宿主也不必在一个 14 个方法的 impl 里混着写四种能力。
//!
//! 为什么 [`BridgeHost`] 仍然是宿主要实现的名字：native 侧是在
//! `impl BridgeHost for NativeBridgeHost { ... }` 里一次写完所有方法的。Rust 不允许
//! "`BridgeHost` 是四个能力的组合（supertrait）+ blanket impl" 和这种直接实现共存
//! （组合要求每个宿主拆成四个 impl，而直接 impl 会撞上 blanket impl 的 coherence 检查），
//! 所以这里保留 `BridgeHost` 这个组合名，用 blanket impl 让每个宿主自动具备四个能力；
//! 新代码可以按能力实现，再用 [`CapabilityHost`] 拼回 `BridgeHost` 交给 [`Bridge`]。
//!
//! 调用点的注意点：同一个模块里同时用到 [`BridgeHost`] 和某个能力 trait、接收者又是
//! **具体类型**时，同名方法有两个候选，要写全限定（`ExecHost::exec(&host, ...)`）；
//! `dyn BridgeHost` 和泛型参数（`impl ExecHost + ?Sized`）都不受影响。
//!
//! [`Bridge`]: super::Bridge

use std::sync::Arc;
use std::time::Duration;

use crate::ai::files::RemoteFs;
use crate::ssh::{BoxFuture, ExecCapture};

use super::{
    BridgeHost, ExecGate, ExecPlacement, ExecTargetReport, TerminalExecCapture, TerminalExecRequest,
};

/// 执行能力：在目标上跑命令、读终端，以及把命令放进用户终端里跑。
///
/// 终端就地执行的四个方法都是必须实现的：宿主必须明确表态"我接管用户终端"（
/// `exec_placement` 返回 `Terminal`，`run_in_terminal` 负责跑，`release_terminal` /
/// `interrupt_terminal` 负责收尾），而不是靠默认实现悄悄降级。
pub trait ExecHost: Send + Sync {
    /// 一个逻辑目标这次该用哪个会话执行。
    ///
    /// 默认实现信任界面的绑定：目标本身不是 `host-<id>` 时把它当会话 ID（测试替身用）。
    fn execution_session<'a>(
        &'a self,
        target_id: &'a str,
        bound: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        let session = bound.map(str::to_owned).or_else(|| {
            (!target_id.starts_with(crate::ai::protocol::HOST_TARGET_PREFIX))
                .then(|| target_id.to_owned())
        });
        Box::pin(async move { session })
    }

    fn exec<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>>;

    /// 同 [`ExecHost::exec`]，但受 `gate` 约束（R-9 F11、F12）。
    ///
    /// 默认实现只在开始前检查一次，之后等命令自己结束（测试替身用）；真正接 SSH 的宿主
    /// 应该覆盖它，让停止按钮能关掉执行通道。
    fn exec_cancellable<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
        gate: &'a ExecGate,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        Box::pin(async move {
            gate.check()?;
            self.exec(session_id, command, wait, max_stream_bytes).await
        })
    }

    fn read_terminal<'a>(
        &'a self,
        session_id: &'a str,
        max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>>;

    /// 这个目标现在能不能在用户的终端里执行；返回 `Terminal` 表示宿主**已经抢占**了终端，
    /// 由 [`ExecHost::release_terminal`] 还回去。
    fn exec_placement<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ExecPlacement>;

    /// 在用户的终端里执行。只在 [`ExecHost::exec_placement`] 返回 `Terminal` 时被调用。
    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>>;

    /// 抢占之后请求又失效了（停止 / 目标移出）：把终端占位还回去。
    fn release_terminal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, ()>;

    /// 让 AI 停止这个对话正在终端里跑的命令（发 Ctrl+C）。只有登记表里确实有一条在跑
    /// 的命令才会发。
    fn interrupt_terminal<'a>(&'a self, scope_id: &'a str) -> BoxFuture<'a, ()>;
}

/// 文件能力：目标服务器上的文件系统（一条 SFTP 通道）和覆盖写时保留几份备份。
pub trait FsHost: Send + Sync {
    fn remote_fs<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>>;

    /// `write_file` 每个文件保留几份备份（设置项 `ai_write_backup_keep`）。
    fn write_backup_keep(&self) -> usize {
        crate::store::settings::AI_WRITE_BACKUP_KEEP_DEFAULT
    }
}

/// 主机长期记忆（[`crate::ai::host_memory`]）：读、存这个目标对应主机的笔记。
pub trait MemoryHost: Send + Sync {
    fn read_memory<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;

    /// 存笔记（已经清理过；空串 = 清空）。
    fn write_memory<'a>(
        &'a self,
        target_id: &'a str,
        notes: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// 上报能力：告诉界面这次工具调用落在哪，以及哪些目标不能执行。
pub trait ReportHost: Send + Sync {
    /// 这个目标不能当 AI 的执行目标时返回原因（Telnet / 串口主机没有 SSH 的 exec 通道）。
    /// 默认都能：快速连接这类不在主机库里的目标一定是 SSH。
    fn target_refusal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, Option<String>> {
        let _ = target_id;
        Box::pin(async { None })
    }

    /// 执行层解析出目标之后、真正开始之前回报给界面（AI-SCOPE-2 第 2 节）。
    fn report_target(&self, report: ExecTargetReport);
}

/// 每个 [`BridgeHost`] 都自动具备四种能力（宿主仍然只实现 `BridgeHost` 一个 trait）。
///
/// 这几个 impl 是"视图"，不改变调用路径：方法原样转给 [`BridgeHost`]，行为和直接调用
/// 完全一致（审计过的既有实现都还走同一条路）。
impl<T: BridgeHost + ?Sized> ExecHost for T {
    fn execution_session<'a>(
        &'a self,
        target_id: &'a str,
        bound: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        BridgeHost::execution_session(self, target_id, bound)
    }

    fn exec<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        BridgeHost::exec(self, session_id, command, wait, max_stream_bytes)
    }

    fn exec_cancellable<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
        gate: &'a ExecGate,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        BridgeHost::exec_cancellable(self, session_id, command, wait, max_stream_bytes, gate)
    }

    fn read_terminal<'a>(
        &'a self,
        session_id: &'a str,
        max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        BridgeHost::read_terminal(self, session_id, max_lines)
    }

    fn exec_placement<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
        BridgeHost::exec_placement(self, session_id)
    }

    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
        BridgeHost::run_in_terminal(self, request)
    }

    fn release_terminal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, ()> {
        BridgeHost::release_terminal(self, target_id)
    }

    fn interrupt_terminal<'a>(&'a self, scope_id: &'a str) -> BoxFuture<'a, ()> {
        BridgeHost::interrupt_terminal(self, scope_id)
    }
}

impl<T: BridgeHost + ?Sized> FsHost for T {
    fn remote_fs<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
        BridgeHost::remote_fs(self, target_id)
    }

    fn write_backup_keep(&self) -> usize {
        BridgeHost::write_backup_keep(self)
    }
}

impl<T: BridgeHost + ?Sized> MemoryHost for T {
    fn read_memory<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        BridgeHost::read_memory(self, target_id)
    }

    fn write_memory<'a>(
        &'a self,
        target_id: &'a str,
        notes: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        BridgeHost::write_memory(self, target_id, notes)
    }
}

impl<T: BridgeHost + ?Sized> ReportHost for T {
    fn target_refusal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, Option<String>> {
        BridgeHost::target_refusal(self, target_id)
    }

    fn report_target(&self, report: ExecTargetReport) {
        BridgeHost::report_target(self, report)
    }
}

/// 把"按能力实现"的宿主拼回 [`BridgeHost`]，给不想在一个 impl 里写四种能力的实现方
/// （尤其是测试替身）用。
///
/// native 侧的宿主直接实现 `BridgeHost`，不需要经过这里。为什么要有这个包装：Rust 的
/// coherence 规则不允许 [`BridgeHost`] 既被 blanket impl 覆盖、又被具体类型直接实现，
/// 所以"四个能力 → 完整宿主"这一步只能由一个明确的包装类型来做。
pub struct CapabilityHost<T>(T);

impl<T> CapabilityHost<T> {
    pub fn new(host: T) -> CapabilityHost<T> {
        CapabilityHost(host)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> BridgeHost for CapabilityHost<T>
where
    T: ExecHost + FsHost + MemoryHost + ReportHost + Send + Sync,
{
    fn execution_session<'a>(
        &'a self,
        target_id: &'a str,
        bound: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        self.0.execution_session(target_id, bound)
    }

    fn target_refusal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, Option<String>> {
        self.0.target_refusal(target_id)
    }

    fn report_target(&self, report: ExecTargetReport) {
        self.0.report_target(report);
    }

    fn exec<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        self.0.exec(session_id, command, wait, max_stream_bytes)
    }

    fn exec_cancellable<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
        gate: &'a ExecGate,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        self.0
            .exec_cancellable(session_id, command, wait, max_stream_bytes, gate)
    }

    fn read_terminal<'a>(
        &'a self,
        session_id: &'a str,
        max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        self.0.read_terminal(session_id, max_lines)
    }

    fn exec_placement<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
        self.0.exec_placement(session_id)
    }

    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
        self.0.run_in_terminal(request)
    }

    fn release_terminal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, ()> {
        self.0.release_terminal(target_id)
    }

    fn interrupt_terminal<'a>(&'a self, scope_id: &'a str) -> BoxFuture<'a, ()> {
        self.0.interrupt_terminal(scope_id)
    }

    fn remote_fs<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
        self.0.remote_fs(target_id)
    }

    fn write_backup_keep(&self) -> usize {
        self.0.write_backup_keep()
    }

    fn read_memory<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        self.0.read_memory(target_id)
    }

    fn write_memory<'a>(
        &'a self,
        target_id: &'a str,
        notes: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.0.write_memory(target_id, notes)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// 最小宿主：只实现 `BridgeHost` 的两个必填方法，其余走默认实现。
    struct MinimalHost {
        commands: Mutex<Vec<String>>,
    }

    impl BridgeHost for MinimalHost {
        fn exec<'a>(
            &'a self,
            _session_id: &'a str,
            command: &'a str,
            _wait: Duration,
            _max_stream_bytes: usize,
        ) -> BoxFuture<'a, Result<ExecCapture, String>> {
            self.commands.lock().unwrap().push(command.to_owned());
            Box::pin(async {
                Ok(ExecCapture {
                    stdout: "ok\n".into(),
                    stderr: String::new(),
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    duration_ms: 1,
                })
            })
        }

        fn read_terminal<'a>(
            &'a self,
            _session_id: &'a str,
            _max_lines: usize,
        ) -> BoxFuture<'a, Result<Vec<String>, String>> {
            Box::pin(async { Ok(vec!["$ echo ok".into()]) })
        }
    }

    /// 只依赖一种能力的函数：任何宿主都能直接传进来（blanket impl）。
    fn backup_keep_of(host: &(impl FsHost + ?Sized)) -> usize {
        host.write_backup_keep()
    }

    #[tokio::test]
    async fn every_bridge_host_satisfies_each_capability() {
        let host = MinimalHost {
            commands: Mutex::new(Vec::new()),
        };

        let exec: &dyn ExecHost = &host;
        let capture = exec
            .exec("s1", "uptime", Duration::from_secs(1), 1024)
            .await
            .unwrap();
        assert_eq!(capture.stdout, "ok\n");
        assert_eq!(host.commands.lock().unwrap().as_slice(), ["uptime"]);
        assert_eq!(
            exec.read_terminal("s1", 10).await.unwrap(),
            vec!["$ echo ok".to_owned()]
        );
        // 终端放置的默认实现：不接管用户终端。
        assert!(matches!(
            exec.exec_placement("s1").await,
            ExecPlacement::Background { .. }
        ));

        let fs: &dyn FsHost = &host;
        assert_eq!(
            fs.write_backup_keep(),
            crate::store::settings::AI_WRITE_BACKUP_KEEP_DEFAULT
        );
        assert_eq!(backup_keep_of(&host), fs.write_backup_keep());

        let memory: &dyn MemoryHost = &host;
        assert!(memory.read_memory("host-1").await.unwrap().is_none());

        let report: &dyn ReportHost = &host;
        assert!(report.target_refusal("host-1").await.is_none());
    }

    /// 能力优先的宿主（四个 trait 分开实现）经 [`CapabilityHost`] 拼回 `BridgeHost`。
    struct SplitHost {
        exec: MinimalHost,
    }

    impl ExecHost for SplitHost {
        fn exec<'a>(
            &'a self,
            session_id: &'a str,
            command: &'a str,
            wait: Duration,
            max_stream_bytes: usize,
        ) -> BoxFuture<'a, Result<ExecCapture, String>> {
            // 具体类型上两个 trait 的方法同名，写全限定才是唯一候选（`dyn` / 泛型不受影响）。
            BridgeHost::exec(&self.exec, session_id, command, wait, max_stream_bytes)
        }

        fn read_terminal<'a>(
            &'a self,
            session_id: &'a str,
            max_lines: usize,
        ) -> BoxFuture<'a, Result<Vec<String>, String>> {
            BridgeHost::read_terminal(&self.exec, session_id, max_lines)
        }

        fn exec_placement<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
            Box::pin(async {
                ExecPlacement::Background {
                    reason: "测试宿主只在后台执行".to_owned(),
                }
            })
        }

        fn run_in_terminal<'a>(
            &'a self,
            _request: TerminalExecRequest,
        ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
            Box::pin(async { Err("测试宿主不在终端里执行".to_owned()) })
        }

        fn release_terminal<'a>(&'a self, _target_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn interrupt_terminal<'a>(&'a self, _scope_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    impl FsHost for SplitHost {
        fn remote_fs<'a>(
            &'a self,
            _target_id: &'a str,
        ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
            Box::pin(async { Err("测试宿主没有文件系统".to_owned()) })
        }
    }

    impl MemoryHost for SplitHost {
        fn read_memory<'a>(
            &'a self,
            _target_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, String>> {
            Box::pin(async { Ok(Some("note".to_owned())) })
        }

        fn write_memory<'a>(
            &'a self,
            _target_id: &'a str,
            _notes: &'a str,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl ReportHost for SplitHost {
        fn report_target(&self, _report: ExecTargetReport) {}
    }

    #[tokio::test]
    async fn capability_host_joins_the_four_capabilities() {
        let host = CapabilityHost::new(SplitHost {
            exec: MinimalHost {
                commands: Mutex::new(Vec::new()),
            },
        });
        let capture = BridgeHost::exec(&host, "s1", "ls", Duration::from_secs(1), 1024)
            .await
            .unwrap();
        assert_eq!(capture.exit_code, Some(0));
        assert!(matches!(
            BridgeHost::exec_placement(&host, "s1").await,
            ExecPlacement::Background { .. }
        ));
        assert_eq!(
            BridgeHost::read_memory(&host, "host-1")
                .await
                .unwrap()
                .as_deref(),
            Some("note")
        );
        assert_eq!(BridgeHost::write_backup_keep(&host), backup_keep_of(&host));
    }
}
