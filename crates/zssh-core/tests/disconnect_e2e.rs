//! "所有网络连接"右键菜单的端到端（V2-U21，不需要真服务器）。
//!
//! 进程内迷你 SSH 服务器只监听本机回环、能执行 exec 请求：它按命令行**原文**分派（连接采集脚本 / 进程采集脚本 /
//! `ss -K ...` / `kill -TERM|KILL <pid>`），背后是一张可变的"连接表"和"进程表"，所以整条链路是真的——真 SSH 连接、
//! 真 exec 通道、真的 `collect_connections` / `disconnect_connection` / `terminate_process`——只有服务器那一端是桩：
//! 它不真跑 `ss`，而是对着这张表给出 `ss` 的输出和各种失败（内核不支持、没权限、没有 ss、静默跳过）。
//!
//! 要点：每条命令都被记下来，断言"没权限时不会再试 sudo"、"自己的连接 / UDP / 监听 / 不合法的地址一条 `ss -K` 都不发"、
//! "菜单里的结束进程走的就是 U18 的 `terminate_process`"（同样的 `kill -TERM <pid>` 命令、同样的受保护判定）。

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;
use zssh_core::ssh::{
    collect_connections, collect_processes, connections_command, disconnect_check,
    disconnect_connection, kill_check, kill_verdict, processes_command, terminate_process,
    Connection, ConnectionsReport, DisconnectBlock, DisconnectError, KillBlock, KillError,
    KillSignal, KillVerdict, NetConn, Protection,
};

use common::{TempKnownHosts, TestServer};

/// `ss -K` 在这台桩服务器上的表现。
#[derive(Clone, Copy, PartialEq)]
enum SsMode {
    /// 内核支持：关掉匹配的 socket 并打印它。
    Works,
    /// 没开 CONFIG_INET_DIAG_DESTROY：报 Operation not supported。
    Unsupported,
    /// 不是 root：Operation not permitted。
    Denied,
    /// 内核不支持时 `ss -K` 真实的静默行为：退出码 0、什么都不打印。
    SilentSkip,
}

struct StubConn {
    proto: &'static str,
    state: &'static str,
    local: &'static str,
    remote: &'static str,
    /// `users:(...)` 里的 `("名字",pid=N,fd=..)`。
    users: &'static [(&'static str, u32)],
}

struct World {
    uid: u32,
    login_user: &'static str,
    /// `$SSH_CONNECTION`；`None` = 服务器没有这个变量。
    ssh_connection: Option<&'static str>,
    /// 来源：`ss` 还是 `netstat`（没有 ss 的服务器）。
    source_is_ss: bool,
    ss: SsMode,
    conns: Vec<StubConn>,
    /// 进程表：(pid, ppid, 用户, 状态, 命令)。
    procs: Vec<(u32, u32, &'static str, &'static str, &'static str)>,
    /// 收到的每一条 exec 命令原文。
    commands: Vec<String>,
}

impl World {
    fn new() -> Self {
        let conn = |proto, state, local, remote, users| StubConn {
            proto,
            state,
            local,
            remote,
            users,
        };
        Self {
            uid: 0,
            login_user: "root",
            ssh_connection: Some("198.51.100.7 51234 192.0.2.10 22"),
            source_is_ss: true,
            ss: SsMode::Works,
            conns: vec![
                conn(
                    "udp",
                    "UNCONN",
                    "127.0.0.53%lo:53",
                    "0.0.0.0:*",
                    &[("resolver", 620)],
                ),
                conn(
                    "tcp",
                    "LISTEN",
                    "0.0.0.0:80",
                    "0.0.0.0:*",
                    &[("web-server", 123), ("web-server", 124)],
                ),
                // ZeeTerm 自己的这条 SSH 连接。
                conn(
                    "tcp",
                    "ESTAB",
                    "192.0.2.10:22",
                    "198.51.100.7:51234",
                    &[("sshd", 3090), ("sshd", 3100)],
                ),
                // 同一台主机上别人的 SSH 连接。
                conn(
                    "tcp",
                    "ESTAB",
                    "192.0.2.10:22",
                    "198.51.100.8:40000",
                    &[("sshd", 1200)],
                ),
                conn(
                    "tcp",
                    "ESTAB",
                    "192.0.2.10:443",
                    "198.51.100.21:40112",
                    &[("web-server", 124)],
                ),
                conn(
                    "tcp",
                    "ESTAB",
                    "[2001:db8::10]:443",
                    "[2001:db8::77]:40000",
                    &[("web-server", 124)],
                ),
                conn(
                    "tcp",
                    "ESTAB",
                    "192.0.2.10:8081",
                    "198.51.100.30:50000",
                    &[("app-worker", 812)],
                ),
                conn(
                    "tcp",
                    "TIME-WAIT",
                    "192.0.2.10:8080",
                    "198.51.100.9:33333",
                    &[],
                ),
            ],
            procs: vec![
                (1, 0, "root", "Ss", "/sbin/init"),
                (800, 1, "root", "Ss", "sshd: /usr/sbin/sshd -D [listener]"),
                (3090, 800, "root", "Ss", "sshd: root [priv]"),
                (3100, 3090, "root", "S", "sshd: root@notty"),
                (1200, 800, "root", "Ss", "sshd: other [priv]"),
                (123, 1, "www-data", "S", "web-server: master process"),
                (124, 123, "www-data", "S", "web-server: worker process"),
                (812, 1, "root", "Ssl", "/usr/bin/app-worker --queue=high"),
            ],
            commands: Vec::new(),
        }
    }

    fn connections_output(&self) -> String {
        let mut out = format!("@@ZSSH uid {}\n", self.uid);
        if let Some(value) = self.ssh_connection {
            out.push_str(&format!("@@ZSSH ssh_connection {value}\n"));
        } else {
            out.push_str("@@ZSSH ssh_connection \n");
        }
        if self.source_is_ss {
            out.push_str("@@ZSSH source ss\n");
            for c in &self.conns {
                out.push_str(&Self::ss_line(c));
            }
        } else {
            out.push_str("@@ZSSH source ss\n@@ZSSH source netstat\n");
            out.push_str(
                "Proto Recv-Q Send-Q Local Address Foreign Address State PID/Program name\n",
            );
            for c in &self.conns {
                if let Some(line) = Self::netstat_line(c) {
                    out.push_str(&line);
                }
            }
        }
        out
    }

    fn ss_line(c: &StubConn) -> String {
        let users = if c.users.is_empty() {
            String::new()
        } else {
            let each: Vec<String> = c
                .users
                .iter()
                .map(|(name, pid)| format!("(\"{name}\",pid={pid},fd=4)"))
                .collect();
            format!(" users:({})", each.join(","))
        };
        format!(
            "{} {} 0 0 {} {}{users}\n",
            c.proto, c.state, c.local, c.remote
        )
    }

    /// netstat 的写法只给 IPv4 行（够这个测试用）。
    fn netstat_line(c: &StubConn) -> Option<String> {
        if c.local.starts_with('[') || c.local.contains('%') {
            return None;
        }
        let state = c.state.replace("ESTAB", "ESTABLISHED").replace('-', "_");
        let prog = c
            .users
            .first()
            .map_or("-".to_owned(), |(name, pid)| format!("{pid}/{name}"));
        Some(format!(
            "{} 0 0 {} {} {state} {prog}\n",
            c.proto, c.local, c.remote
        ))
    }

    fn processes_output(&self) -> String {
        let mut out = format!(
            "@@ZSSH uid {}\n@@ZSSH user {}\n@@ZSSH self 4242\n@@ZSSH memtotal 16000000\n@@ZSSH source procps\n\
             PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND\n",
            self.uid, self.login_user
        );
        for (pid, ppid, user, stat, command) in &self.procs {
            out.push_str(&format!(
                "{pid} {ppid} {user} 1.0 1.0 1000 {stat} 5000 {command}\n"
            ));
        }
        out.push_str("4242 3100 root 0.4 0.0 3000 Ss 0 sh -c exec stuff\n");
        out
    }

    /// 一条 `ss -K ...`：返回 (stdout, stderr, 退出码)。
    fn ss_kill(&mut self, args: &str) -> (String, String, u32) {
        match self.ss {
            SsMode::Unsupported => {
                return (
                    String::new(),
                    "RTNETLINK answers: Operation not supported\n".into(),
                    1,
                )
            }
            SsMode::Denied => {
                return (
                    String::new(),
                    "RTNETLINK answers: Operation not permitted\n".into(),
                    1,
                )
            }
            SsMode::SilentSkip => return (String::new(), String::new(), 0),
            SsMode::Works => {}
        }
        // `args` 形如 `state established src 192.0.2.10 sport = :443 dst 198.51.100.21 dport = :40112`
        let words: Vec<&str> = args.split_whitespace().collect();
        let value = |key: &str| {
            words
                .iter()
                .position(|w| *w == key)
                .and_then(|i| words.get(i + 1))
                .map(|w| w.trim_matches('"').to_owned())
        };
        let port = |key: &str| {
            words
                .iter()
                .position(|w| *w == key)
                .and_then(|i| words.get(i + 2))
                .map(|w| w.trim_start_matches(':').to_owned())
        };
        let (src, dst) = (
            value("src").unwrap_or_default(),
            value("dst").unwrap_or_default(),
        );
        let (sport, dport) = (
            port("sport").unwrap_or_default(),
            port("dport").unwrap_or_default(),
        );
        let local = format!("{src}:{sport}");
        let remote = format!("{dst}:{dport}");
        let mut printed =
            String::from("State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n");
        let before = self.conns.len();
        self.conns.retain(|c| {
            let hit = c.local == local && c.remote == remote;
            if hit {
                // `ss -t` 不带 Netid 列：`State Recv-Q Send-Q Local Peer Process`。
                printed.push_str(&format!(
                    "{} 0 0 {} {}
",
                    c.state, c.local, c.remote
                ));
            }
            !hit
        });
        if self.conns.len() == before {
            // 没匹配：ss 什么都不打印（只有表头也不打印）。
            return (String::new(), String::new(), 0);
        }
        (printed, String::new(), 0)
    }

    fn kill(&mut self, pid: u32) -> (String, u32) {
        let Some(at) = self.procs.iter().position(|p| p.0 == pid) else {
            return ("sh: 1: kill: No such process\n".to_owned(), 1);
        };
        self.procs.remove(at);
        (String::new(), 0)
    }
}

struct MiniSsh {
    world: Arc<Mutex<World>>,
}

impl server::Handler for MiniSsh {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
            partial_success: false,
        })
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let (stdout, stderr, code) = {
            let mut world = self.world.lock().expect("世界");
            world.commands.push(command.clone());
            if command == connections_command() {
                (world.connections_output(), String::new(), 0)
            } else if command == processes_command() {
                (world.processes_output(), String::new(), 0)
            } else if let Some(rest) = command.strip_prefix("exec sh -c 'ss -K -t ") {
                let (out, err, code) = world.ss_kill(rest.trim_end_matches('\''));
                (out, err, code)
            } else if let Some(rest) = command.strip_prefix("exec sh -c 'kill -") {
                let rest = rest.trim_end_matches('\'');
                let pid = rest.split(' ').nth(1).and_then(|p| p.parse::<u32>().ok());
                match pid {
                    Some(pid) => {
                        let (stderr, code) = world.kill(pid);
                        (String::new(), stderr, code)
                    }
                    None => (String::new(), "sh: 1: kill: bad pid\n".to_owned(), 2),
                }
            } else {
                (String::new(), "sh: 1: unexpected command\n".to_owned(), 127)
            }
        };
        session.channel_success(channel)?;
        if !stdout.is_empty() {
            session.data(channel, stdout.into_bytes())?;
        }
        if !stderr.is_empty() {
            session.extended_data(channel, 1, stderr.into_bytes())?;
        }
        session.exit_status_request(channel, code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

struct Harness {
    connection: Arc<Connection>,
    world: Arc<Mutex<World>>,
    _known_hosts: TempKnownHosts,
}

async fn start_with(tweak: impl FnOnce(&mut World)) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let addr = listener.local_addr().expect("端口");
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("主机密钥")],
        ..Default::default()
    });
    let mut initial = World::new();
    tweak(&mut initial);
    let world = Arc::new(Mutex::new(initial));
    let for_server = Arc::clone(&world);
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = MiniSsh { world: for_server };
        if let Ok(running) = server::run_stream(config, socket, handler).await {
            let _ = running.await;
        }
    });
    let known_hosts = TempKnownHosts::new("disconnect-e2e");
    let connection = TestServer::fake("127.0.0.1", addr.port(), "tester", "pw")
        .connect(&known_hosts)
        .await
        .expect("连接迷你服务器");
    Harness {
        connection: Arc::new(connection),
        world,
        _known_hosts: known_hosts,
    }
}

async fn start() -> Harness {
    start_with(|_| {}).await
}

fn commands(harness: &Harness) -> Vec<String> {
    harness.world.lock().expect("世界").commands.clone()
}

fn ss_kill_commands(harness: &Harness) -> Vec<String> {
    commands(harness)
        .into_iter()
        .filter(|c| c.contains("ss -K"))
        .collect()
}

fn kill_commands(harness: &Harness) -> Vec<String> {
    commands(harness)
        .into_iter()
        .filter(|c| c.contains("'kill -"))
        .collect()
}

async fn report(harness: &Harness) -> ConnectionsReport {
    collect_connections(&harness.connection)
        .await
        .expect("采集")
}

fn row<'a>(report: &'a ConnectionsReport, local: &str, remote: &str) -> &'a NetConn {
    report
        .rows
        .iter()
        .find(|r| r.local.display() == local && r.remote.display() == remote)
        .unwrap_or_else(|| panic!("没有 {local} {remote}"))
}

#[tokio::test]
async fn disconnecting_sends_one_exact_ss_k_command_and_the_connection_is_gone_afterwards() {
    let harness = start().await;
    let before = report(&harness).await;
    let target = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();

    disconnect_connection(&harness.connection, &target)
        .await
        .expect("断开");

    // 命令形状固定；没有 sudo；先重新采了一次连接表再发 ss -K。
    assert_eq!(
        ss_kill_commands(&harness),
        vec![
            "exec sh -c 'ss -K -t state established src 192.0.2.10 sport = :443 dst 198.51.100.21 dport = :40112'"
        ]
    );
    let all = commands(&harness);
    assert!(all.iter().all(|c| !c.contains("sudo")));
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(all[..2], [connections_command(), connections_command()]);
    assert!(all[2].contains("ss -K"));
    let after = report(&harness).await;
    assert!(
        after
            .rows
            .iter()
            .all(|r| r.local.display() != "192.0.2.10:443"),
        "已经不在了"
    );
    // 别的连接没受影响。
    assert_eq!(after.rows.len(), before.rows.len() - 1);
}

#[tokio::test]
async fn an_ipv6_connection_is_disconnected_with_the_bracketed_quoted_address() {
    let harness = start().await;
    let before = report(&harness).await;
    let target = row(&before, "[2001:db8::10]:443", "[2001:db8::77]:40000").clone();
    disconnect_connection(&harness.connection, &target)
        .await
        .expect("断开");
    assert_eq!(
        ss_kill_commands(&harness),
        vec![
            "exec sh -c 'ss -K -t state established src \"[2001:db8::10]\" sport = :443 dst \"[2001:db8::77]\" dport = :40000'"
        ]
    );
}

#[tokio::test]
async fn zeeterms_own_ssh_connection_is_locked_in_the_report_and_never_disconnected() {
    let harness = start().await;
    let before = report(&harness).await;
    let own = row(&before, "192.0.2.10:22", "198.51.100.7:51234").clone();
    assert!(before.is_own_connection(&own));
    assert_eq!(
        disconnect_check(&before, &own),
        Err(DisconnectBlock::OwnConnection)
    );
    let error = disconnect_connection(&harness.connection, &own)
        .await
        .expect_err("自己的连接");
    assert_eq!(
        error,
        DisconnectError::Blocked(DisconnectBlock::OwnConnection)
    );
    assert!(ss_kill_commands(&harness).is_empty(), "一条 ss -K 都不能发");
    // 同一台主机上别人的 SSH 连接照常可以断。
    let other = row(&before, "192.0.2.10:22", "198.51.100.8:40000").clone();
    assert!(!before.is_own_connection(&other));
    disconnect_connection(&harness.connection, &other)
        .await
        .expect("别人的 SSH 连接可以断");
    assert_eq!(ss_kill_commands(&harness).len(), 1);
}

#[tokio::test]
async fn a_connection_that_vanished_since_the_list_was_taken_sends_nothing() {
    let harness = start().await;
    let before = report(&harness).await;
    let target = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();
    harness
        .world
        .lock()
        .expect("世界")
        .conns
        .retain(|c| c.local != "192.0.2.10:443");
    let error = disconnect_connection(&harness.connection, &target)
        .await
        .expect_err("已经不在了");
    assert_eq!(error, DisconnectError::Gone);
    assert!(ss_kill_commands(&harness).is_empty());
}

#[tokio::test]
async fn listening_udp_time_wait_and_unsafe_targets_never_reach_the_server() {
    let harness = start().await;
    let before = report(&harness).await;
    let calls_before = commands(&harness).len();
    for (local, remote, block) in [
        ("0.0.0.0:80", "0.0.0.0:*", DisconnectBlock::Listening),
        ("127.0.0.53%lo:53", "0.0.0.0:*", DisconnectBlock::Udp),
        (
            "192.0.2.10:8080",
            "198.51.100.9:33333",
            DisconnectBlock::TimeWait,
        ),
    ] {
        let target = row(&before, local, remote).clone();
        assert_eq!(
            disconnect_connection(&harness.connection, &target).await,
            Err(DisconnectError::Blocked(block)),
            "{local}"
        );
    }
    // 通配 / 带 scope / 注入的地址：校验不过，连采集都不发。
    let mut wild = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();
    for bad in ["*", "fe80::1%eth0", "192.0.2.1;reboot", "$(id)", ""] {
        wild.remote.host = bad.to_owned();
        let error = disconnect_connection(&harness.connection, &wild)
            .await
            .expect_err("不合法");
        assert!(
            matches!(error, DisconnectError::InvalidAddress(_)),
            "{bad}: {error:?}"
        );
    }
    wild.remote.host = "198.51.100.21".to_owned();
    wild.remote.port = "*".to_owned();
    assert!(matches!(
        disconnect_connection(&harness.connection, &wild).await,
        Err(DisconnectError::InvalidAddress(_))
    ));
    assert_eq!(commands(&harness).len(), calls_before, "一条命令都没有发");
}

#[tokio::test]
async fn a_kernel_without_sock_destroy_is_reported_and_the_connection_stays() {
    let harness = start_with(|w| w.ss = SsMode::Unsupported).await;
    let before = report(&harness).await;
    let target = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();
    let error = disconnect_connection(&harness.connection, &target)
        .await
        .expect_err("不支持");
    assert_eq!(error, DisconnectError::NotSupported);
    assert_eq!(ss_kill_commands(&harness).len(), 1, "只试一次");
    assert_eq!(report(&harness).await.rows.len(), before.rows.len());
}

#[tokio::test]
async fn without_root_it_says_so_and_never_falls_back_to_sudo() {
    let harness = start_with(|w| {
        w.uid = 1000;
        w.login_user = "app";
        w.ss = SsMode::Denied;
    })
    .await;
    let before = report(&harness).await;
    let target = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();
    let error = disconnect_connection(&harness.connection, &target)
        .await
        .expect_err("没权限");
    assert_eq!(error, DisconnectError::NoPermission);
    assert_eq!(error.to_string(), "需要 root 才能断开连接。");
    assert_eq!(ss_kill_commands(&harness).len(), 1);
    assert!(commands(&harness).iter().all(|c| !c.contains("sudo")));
}

#[tokio::test]
async fn a_silent_skip_with_no_output_is_reported_as_not_found() {
    let harness = start_with(|w| w.ss = SsMode::SilentSkip).await;
    let before = report(&harness).await;
    let target = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();
    let error = disconnect_connection(&harness.connection, &target)
        .await
        .expect_err("没有输出");
    assert_eq!(error, DisconnectError::NoMatch);
    assert!(error
        .to_string()
        .starts_with("没有找到这条连接（可能已经关闭）"));
}

#[tokio::test]
async fn without_ss_on_the_server_nothing_can_be_disconnected() {
    let harness = start_with(|w| w.source_is_ss = false).await;
    let before = report(&harness).await;
    assert_eq!(before.source, zssh_core::ssh::ConnSource::Netstat);
    let target = row(&before, "192.0.2.10:443", "198.51.100.21:40112").clone();
    assert_eq!(
        disconnect_check(&before, &target),
        Err(DisconnectBlock::NoSs)
    );
    let error = disconnect_connection(&harness.connection, &target)
        .await
        .expect_err("没有 ss");
    assert_eq!(error, DisconnectError::NoSs);
    assert!(ss_kill_commands(&harness).is_empty());
}

#[tokio::test]
async fn ending_a_process_from_a_connection_row_uses_the_same_terminate_process_as_the_process_manager(
) {
    let harness = start().await;
    let conns = report(&harness).await;
    let procs = collect_processes(&harness.connection).await.expect("进程");
    let target_row = row(&conns, "192.0.2.10:8081", "198.51.100.30:50000");
    let pid = target_row.unique_processes()[0].pid;
    assert_eq!(pid, 812);
    // 菜单项可用 → 取进程管理那份 ProcRow → 同一个 terminate_process。
    assert_eq!(kill_check(&conns, target_row, Some(&procs), pid), Ok(()));
    let proc_row = procs.row(pid).expect("进程").clone();
    terminate_process(&harness.connection, &proc_row, KillSignal::Term)
        .await
        .expect("结束");
    assert_eq!(kill_commands(&harness), vec!["exec sh -c 'kill -TERM 812'"]);
    assert!(commands(&harness).iter().all(|c| !c.contains("sudo")));
    let after = collect_processes(&harness.connection).await.expect("进程");
    assert_eq!(kill_verdict(&after, &proc_row), KillVerdict::Gone);
}

#[tokio::test]
async fn protected_processes_are_greyed_out_by_the_same_rule_and_core_refuses_them_too() {
    let harness = start().await;
    let conns = report(&harness).await;
    let procs = collect_processes(&harness.connection).await.expect("进程");
    // 本次会话链上的 sshd（3100 / 3090）：自己的连接整行都置灰；
    // 别的用户的 sshd 会话（1200）不受保护，可以结束。
    let own = row(&conns, "192.0.2.10:22", "198.51.100.7:51234");
    assert_eq!(
        kill_check(&conns, own, Some(&procs), 3100),
        Err(KillBlock::OwnConnection)
    );
    let other = row(&conns, "192.0.2.10:22", "198.51.100.8:40000");
    assert_eq!(kill_check(&conns, other, Some(&procs), 1200), Ok(()));
    // 即使有人把受保护进程硬塞给 terminate_process，核心也拒绝（和进程管理一样）。
    for (pid, protection) in [(3100, Protection::SshSession), (1, Protection::Init)] {
        let target = procs.row(pid).expect("进程").clone();
        assert_eq!(
            terminate_process(&harness.connection, &target, KillSignal::Kill).await,
            Err(KillError::Protected(protection))
        );
    }
    assert!(kill_commands(&harness).is_empty(), "一条 kill 都没发");
    // sshd 监听者（800）在会话链上，同样受保护。
    let mut protected = other.clone();
    protected.processes = vec![zssh_core::ssh::ProcRef {
        pid: 800,
        name: "sshd".into(),
    }];
    assert_eq!(
        kill_check(&conns, &protected, Some(&procs), 800),
        Err(KillBlock::Protected(Protection::SshSession))
    );
}

#[tokio::test]
async fn a_server_without_the_ssh_connection_variable_locks_nothing() {
    let harness = start_with(|w| w.ssh_connection = None).await;
    let before = report(&harness).await;
    assert!(before.ssh_connection.is_none());
    assert!(before.rows.iter().all(|r| !before.is_own_connection(r)));
}
