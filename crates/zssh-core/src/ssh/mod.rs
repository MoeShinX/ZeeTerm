//! SSH 连接与 shell 通道。

pub mod auth;
pub mod connection;
pub mod connection_test;
pub mod connections;
pub mod cwd;
pub mod dialer;
pub mod disconnect;
#[cfg(test)]
mod fake_server;
pub mod flow;
pub mod forward;
mod hostkey_order;
mod id_tap;
pub mod interact;
pub mod keygen;
pub mod known_hosts;
pub mod latency;
pub mod os_probe;
pub mod pool;
pub mod processes;
pub mod session_log;
pub mod shell;
pub mod stats;
pub mod stream;

pub use auth::{AuthConfig, KeySource, RememberedSecret};
pub use connection::{
    keepalive_interval, script_stdin, set_keepalive_interval, ChainHop, ConnectParams, Connection,
    ExecCapture, ServerKey, EXEC_MAX_STREAM_BYTES, EXEC_OUTPUT_LIMIT, SCRIPT_EXEC_COMMAND,
};
pub use connections::{
    collect_connections, connections_command, matches_query, parse_connections,
    parse_ssh_connection, same_endpoint, view_indices, ConnFilter, ConnSource, ConnState,
    ConnectionsError, ConnectionsReport, Endpoint, NetConn, NetProto, ProcRef, ProcessHint,
    SortKey, SortSpec, SshConnection, CONNECTIONS_MAX_ROWS,
};
pub use cwd::{parse_probe_output, pick_shell, probe_command, probe_shell_cwds, ShellCwd};
pub use dialer::{connect_direct, BoxedStream, ByteStream, ProxyConfig, HOP_TIMEOUT};
pub use disconnect::{
    classify_disconnect, disconnect_check, disconnect_command, disconnect_connection, kill_check,
    kill_output_mentions, menu_processes, DisconnectBlock, DisconnectError, KillBlock,
    DISCONNECT_TIMEOUT, MENU_MAX_PROCESSES,
};
pub use flow::FlowGate;
pub use interact::{
    BoxFuture, HopProgress, HostKeyDecision, HostKeyInfo, HostKeyStatus, Interactor,
    NonInteractive, PromptAnswers, PromptItem, PromptKind, PromptRequest, SavedHostKey,
};
pub use known_hosts::KnownHosts;
pub use pool::{ConnectionGuard, ConnectionPool, PoolConfig, Pooled};
pub use processes::{
    classify_kill_failure, collect_processes, elapsed_label, kill_command, kill_verdict,
    parse_processes, processes_command, same_process, terminate_process, validate_pid, KillError,
    KillSignal, KillVerdict, ProcMode, ProcRisk, ProcRow, ProcSort, ProcSortKey, ProcSource,
    ProcessReport, ProcessesError, Protection, CRITICAL_SERVICES, PROCESSES_MAX_ROWS,
};
pub use shell::{
    SessionBuffer, ShellEvent, ShellEventStream, ShellSession, ShellWriter, DEFAULT_BUFFER_LIMIT,
};
pub use stats::{
    CpuCounters, CpuProcessInfo, NetCounters, NetInterfaceSpeed, PreviousSample, ProcessInfo,
    ServerStats, StatsCollector, StatsSample, COLLECT_TIMEOUT,
};
