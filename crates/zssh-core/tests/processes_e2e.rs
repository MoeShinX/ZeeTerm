//! 进程管理的端到端（V2-U18，不需要真服务器）。
//!
//! 进程内迷你 SSH 服务器只监听本机回环、能执行 exec 请求：它按命令行**原文**分派（采集脚本 / `kill -TERM|KILL <pid>`），
//! 背后是一张可变的"进程表"，所以整条链路都是真的——真 SSH 连接、真 exec 通道、真的 `collect_processes` /
//! `terminate_process`——只有服务器上那一端是桩：它不真跑 `ps` / `kill`（本机是 Windows），而是对着这张表给出
//! procps 的输出和 dash 风格的 kill 结果（成功 / 没权限 / 没有这个进程 / 忽略 TERM）。
//!
//! 要点：每条命令都被记下来，断言"没权限时不会再试 sudo"、"PID 变了就根本不发 kill"、"非法 PID 一条命令都不发"。

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;
use zssh_core::ssh::{
    collect_processes, kill_verdict, processes_command, terminate_process, Connection, KillError,
    KillSignal, KillVerdict, ProcRisk, ProcSource,
};

use common::{TempKnownHosts, TestServer};

/// 服务器上的一个进程（桩）。
#[derive(Clone)]
struct Proc {
    pid: u32,
    ppid: u32,
    user: &'static str,
    cpu: f64,
    mem: f64,
    stat: &'static str,
    elapsed: u64,
    command: &'static str,
    /// 忽略 SIGTERM（只有 SIGKILL 才结束它）。
    ignores_term: bool,
}

struct World {
    login_user: &'static str,
    procs: Vec<Proc>,
    /// 收到的每一条 exec 命令原文。
    commands: Vec<String>,
}

impl World {
    fn new() -> Self {
        let p = |pid, ppid, user, cpu, mem, stat, elapsed, command| Proc {
            pid,
            ppid,
            user,
            cpu,
            mem,
            stat,
            elapsed,
            command,
            ignores_term: false,
        };
        let mut stubborn = p(
            900,
            1,
            "app",
            12.0,
            2.0,
            "S",
            5000,
            "/usr/bin/stubborn --ignore-term",
        );
        stubborn.ignores_term = true;
        Self {
            login_user: "app",
            procs: vec![
                p(1, 0, "root", 0.0, 0.3, "Ss", 90_000, "/sbin/init"),
                p(
                    800,
                    1,
                    "root",
                    0.0,
                    0.1,
                    "Ss",
                    90_000,
                    "sshd: /usr/sbin/sshd -D [listener]",
                ),
                p(3090, 800, "root", 0.0, 0.1, "Ss", 600, "sshd: app [priv]"),
                p(3100, 3090, "app", 0.0, 0.1, "S", 600, "sshd: app@notty"),
                p(
                    812,
                    1,
                    "app",
                    55.5,
                    12.4,
                    "Ssl",
                    93_784,
                    "/usr/bin/app-worker --queue=high",
                ),
                p(
                    1290,
                    1,
                    "www-data",
                    7.0,
                    6.1,
                    "S",
                    5000,
                    "/usr/sbin/web-server -g daemon on;",
                ),
                p(2000, 812, "app", 0.0, 0.0, "Z", 100, "[worker] <defunct>"),
                stubborn,
            ],
            commands: Vec::new(),
        }
    }

    /// 采集脚本的输出（procps 格式）：脚本自己的 shell（4242，父进程是 3100）和它的 `ps` 子进程也在表里。
    fn listing(&self) -> String {
        let mut out = format!(
            "@@ZSSH uid 1000\n@@ZSSH user {}\n@@ZSSH self 4242\n@@ZSSH memtotal 16000000\n@@ZSSH source procps\n\
             PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND\n",
            self.login_user
        );
        let line = |p: &Proc| {
            format!(
                "{} {} {} {:.1} {:.1} {} {} {} {}\n",
                p.pid,
                p.ppid,
                p.user,
                p.cpu,
                p.mem,
                (p.mem * 10_000.0) as u64,
                p.stat,
                p.elapsed,
                p.command
            )
        };
        for p in &self.procs {
            out.push_str(&line(p));
        }
        out.push_str("4242 3100 app 0.4 0.0 3000 Ss 0 sh -c exec\n");
        out.push_str("4250 4242 app 99.0 0.0 3200 R 0 ps -ww -eo pid\n");
        out
    }

    /// 应用一条 `kill`：返回 (stderr, 退出码)。
    fn kill(&mut self, pid: u32, signal: &str) -> (String, u32) {
        let Some(at) = self.procs.iter().position(|p| p.pid == pid) else {
            return ("sh: 1: kill: No such process\n".to_owned(), 1);
        };
        let owner = self.procs[at].user;
        if owner != self.login_user && self.login_user != "root" {
            return ("sh: 1: kill: Operation not permitted\n".to_owned(), 1);
        }
        let proc = &self.procs[at];
        // 僵尸已经结束了，信号对它没有效果；忽略 TERM 的进程只有 KILL 才结束。
        if proc.stat.starts_with('Z') || (signal == "TERM" && proc.ignores_term) {
            return (String::new(), 0);
        }
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
            if command == processes_command() {
                (world.listing(), String::new(), 0)
            } else if let Some(rest) = command.strip_prefix("exec sh -c 'kill -") {
                // `exec sh -c 'kill -TERM 812'`
                let rest = rest.trim_end_matches('\'');
                let mut parts = rest.split(' ');
                let (signal, pid) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                match pid.parse::<u32>() {
                    Ok(pid) => {
                        let (stderr, code) = world.kill(pid, signal);
                        (String::new(), stderr, code)
                    }
                    Err(_) => (String::new(), "sh: 1: kill: bad pid\n".to_owned(), 2),
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

async fn start() -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let addr = listener.local_addr().expect("端口");
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("主机密钥")],
        ..Default::default()
    });
    let world = Arc::new(Mutex::new(World::new()));
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
    let known_hosts = TempKnownHosts::new("processes-e2e");
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

fn commands(harness: &Harness) -> Vec<String> {
    harness.world.lock().expect("世界").commands.clone()
}

fn kill_commands(harness: &Harness) -> Vec<String> {
    commands(harness)
        .into_iter()
        .filter(|c| c.contains("kill"))
        .collect()
}

#[tokio::test]
async fn collecting_over_a_real_exec_channel_lists_processes_and_hides_the_script_itself() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    assert_eq!(report.source, ProcSource::Procps);
    assert_eq!(report.user.as_deref(), Some("app"));
    assert_eq!(report.rows.len(), 8, "脚本自己的 shell 和 ps 不在列表里");
    assert!(report.row(4242).is_none() && report.row(4250).is_none());
    let app = report.row(812).expect("812");
    assert_eq!(app.command, "/usr/bin/app-worker --queue=high");
    assert_eq!(app.cpu, Some(55.5));
    // 承载这条 SSH 连接的进程被认出来了。
    assert_eq!(report.session_chain(), vec![3100, 3090, 800]);
    assert_eq!(report.risk_of(3090), ProcRisk::SshSession);
    assert_eq!(report.risk_of(1), ProcRisk::Init);
    // 一次采集就是一条 exec，命令就是 `processes_command()`（只读脚本），没有任何写操作。
    assert_eq!(commands(&harness), vec![processes_command()]);
}

#[tokio::test]
async fn terminating_a_normal_process_sends_exactly_one_kill_and_it_is_gone_afterwards() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let target = report.row(812).expect("812").clone();

    terminate_process(&harness.connection, &target, KillSignal::Term)
        .await
        .expect("结束");
    assert_eq!(kill_commands(&harness), vec!["exec sh -c 'kill -TERM 812'"]);

    // 结束后重新采一次：进程不在了。
    let after = collect_processes(&harness.connection).await.expect("采集");
    assert_eq!(kill_verdict(&after, &target), KillVerdict::Gone);
    assert!(after.row(812).is_none());
}

#[tokio::test]
async fn a_process_that_ignores_term_stays_running_until_force_killed() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let target = report.row(900).expect("900").clone();

    terminate_process(&harness.connection, &target, KillSignal::Term)
        .await
        .expect("信号发出去了");
    let after = collect_processes(&harness.connection).await.expect("采集");
    assert_eq!(kill_verdict(&after, &target), KillVerdict::StillRunning);

    terminate_process(&harness.connection, &target, KillSignal::Kill)
        .await
        .expect("强制结束");
    assert_eq!(
        kill_commands(&harness),
        vec!["exec sh -c 'kill -TERM 900'", "exec sh -c 'kill -KILL 900'"]
    );
    let after = collect_processes(&harness.connection).await.expect("采集");
    assert_eq!(kill_verdict(&after, &target), KillVerdict::Gone);
}

#[tokio::test]
async fn killing_a_zombie_succeeds_but_the_verdict_says_zombie() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let target = report.row(2000).expect("僵尸").clone();
    terminate_process(&harness.connection, &target, KillSignal::Kill)
        .await
        .expect("kill 本身成功");
    let after = collect_processes(&harness.connection).await.expect("采集");
    assert_eq!(kill_verdict(&after, &target), KillVerdict::Zombie);
}

#[tokio::test]
async fn a_process_owned_by_someone_else_is_a_permission_error_and_sudo_is_never_tried() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let target = report.row(1290).expect("www-data 的进程").clone();
    assert_eq!(target.user, "www-data");

    let error = terminate_process(&harness.connection, &target, KillSignal::Term)
        .await
        .expect_err("没权限");
    assert_eq!(error, KillError::NoPermission);
    assert_eq!(
        error.describe(&target.user, report.user.as_deref()),
        "没有权限结束这个进程（属于用户 www-data，你现在是 app），需要 root 或 sudo。"
    );
    // 只发过这一条 kill，没有 sudo、没有重试。
    assert_eq!(
        kill_commands(&harness),
        vec!["exec sh -c 'kill -TERM 1290'"]
    );
    assert!(commands(&harness).iter().all(|c| !c.contains("sudo")));
    // 进程还在。
    let after = collect_processes(&harness.connection).await.expect("采集");
    assert_eq!(kill_verdict(&after, &target), KillVerdict::StillRunning);
}

#[tokio::test]
async fn a_pid_that_now_belongs_to_another_process_is_never_killed() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let target = report.row(812).expect("812").clone();

    // 列表过时：812 退出了，同一个 PID 被另一个进程（运行时间很短）复用。
    {
        let mut world = harness.world.lock().expect("世界");
        let reused = world.procs.iter_mut().find(|p| p.pid == 812).expect("812");
        reused.command = "/usr/bin/something-else";
        reused.elapsed = 3;
    }
    let error = terminate_process(&harness.connection, &target, KillSignal::Kill)
        .await
        .expect_err("PID 变了");
    assert_eq!(error, KillError::Changed);
    assert!(
        kill_commands(&harness).is_empty(),
        "核对没通过就不发 kill：{:?}",
        commands(&harness)
    );
}

#[tokio::test]
async fn a_process_that_already_exited_is_reported_without_sending_kill() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let target = report.row(812).expect("812").clone();
    harness
        .world
        .lock()
        .expect("世界")
        .procs
        .retain(|p| p.pid != 812);
    let error = terminate_process(&harness.connection, &target, KillSignal::Term)
        .await
        .expect_err("已经没了");
    assert_eq!(error, KillError::NoSuchProcess);
    assert!(kill_commands(&harness).is_empty());
}

#[tokio::test]
async fn an_invalid_pid_sends_nothing_at_all() {
    let harness = start().await;
    let report = collect_processes(&harness.connection).await.expect("采集");
    let mut bogus = report.row(812).expect("812").clone();
    for pid in [0, 4_194_305, u32::MAX] {
        bogus.pid = pid;
        let error = terminate_process(&harness.connection, &bogus, KillSignal::Kill)
            .await
            .expect_err("非法 PID");
        assert_eq!(error, KillError::InvalidPid, "{pid}");
    }
    // 只有开头那一次采集，之后一条命令都没有发。
    assert_eq!(commands(&harness), vec![processes_command()]);
}
