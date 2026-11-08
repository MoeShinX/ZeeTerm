//! 内部 sh 脚本经 stdin 交给 `sh -s` 的端到端（不需要真服务器）。
//!
//! 进程内迷你 SSH 服务器只监听本机回环，模拟**登录 shell 是 fish / csh 的主机**：sshd 把 exec 的命令行交给登录
//! shell 解析，而它读不懂 POSIX 写法——所以服务器对任何不是 `sh -s` 的命令一律回 `Unknown command`、退出码 127。
//! 命令恰好是 `sh -s` 时才"起一个 sh"：服务器把客户端写进 stdin 的内容收齐（等 EOF），交给**本机的真 sh** 执行
//! 并把它的输出和退出码原样返回。整条链路（真 SSH 连接、真 exec 通道、真的采集器和解析）都是真的，只有"远端
//! 机器"是本机（Windows 上没有 `/proc`，采集出来的数字是兜底值，但 `CORES:` 一定有）。本机没有 `sh` 就跳过。

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;
use zssh_core::ssh::stats::{StatsCollector, STATS_SCRIPT};
use zssh_core::ssh::{script_stdin, Connection, SCRIPT_EXEC_COMMAND};

use common::{TempKnownHosts, TestServer};

#[derive(Default)]
struct World {
    /// 收到的每一条 exec 命令原文。
    commands: Vec<String>,
    /// 每个通道上 exec 之后客户端写进 stdin 的内容（收齐后才执行）。
    stdin: HashMap<ChannelId, Vec<u8>>,
    /// 每次 `sh -s` 实际收到的脚本文本。
    scripts: Vec<String>,
}

struct MiniSsh {
    world: Arc<Mutex<World>>,
    /// 等 EOF 再执行的 `sh -s` 通道。
    waiting: HashMap<ChannelId, ()>,
}

/// 本机能不能起 `sh`（装了 Git for Windows 之类）。
fn local_sh_works() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("echo ok")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "ok")
}

/// 在本机真跑一遍 `sh -s`：stdin 喂脚本，返回 (stdout, stderr, 退出码)。
fn run_local_sh(script: &[u8]) -> (String, String, u32) {
    let mut child = Command::new("sh")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("起 sh");
    let mut stdin = child.stdin.take().expect("stdin");
    let input = script.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().expect("等 sh");
    let _ = writer.join();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(1) as u32,
    )
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
        self.world.lock().unwrap().commands.push(command.clone());
        session.channel_success(channel)?;
        if command == SCRIPT_EXEC_COMMAND {
            // 起了 sh：等客户端把脚本写完（EOF）再跑。
            self.waiting.insert(channel, ());
            return Ok(());
        }
        // fish / csh 读不懂 POSIX 命令行。
        session.extended_data(
            channel,
            1,
            b"fish: Unknown command: cores=$(nproc)\n".to_vec(),
        )?;
        session.exit_status_request(channel, 127)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.world
            .lock()
            .unwrap()
            .stdin
            .entry(channel)
            .or_default()
            .extend_from_slice(data);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.waiting.remove(&channel).is_none() {
            return Ok(());
        }
        let script = self
            .world
            .lock()
            .unwrap()
            .stdin
            .remove(&channel)
            .unwrap_or_default();
        self.world
            .lock()
            .unwrap()
            .scripts
            .push(String::from_utf8_lossy(&script).into_owned());
        let (stdout, stderr, code) = run_local_sh(&script);
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
    let world = Arc::new(Mutex::new(World::default()));
    let for_server = Arc::clone(&world);
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = MiniSsh {
            world: for_server,
            waiting: HashMap::new(),
        };
        if let Ok(running) = server::run_stream(config, socket, handler).await {
            let _ = running.await;
        }
    });
    let known_hosts = TempKnownHosts::new("script-exec-e2e");
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

#[tokio::test]
async fn a_login_shell_that_cannot_parse_posix_rejects_the_script_as_a_command_line() {
    // 对照组：旧做法（脚本整段当命令行）在这台"fish 主机"上拿不到任何输出——证明模拟有区分力。
    let harness = start().await;
    let result = harness.connection.exec(STATS_SCRIPT).await;
    let error = result.expect_err("fish 解析不了 POSIX 命令行");
    assert!(error.to_string().contains("Unknown command"), "{error}");
}

#[tokio::test]
async fn stats_collection_works_when_the_login_shell_is_not_posix() {
    if !local_sh_works() {
        println!("跳过：本机没有可用的 sh");
        return;
    }
    let harness = start().await;
    let mut collector = StatsCollector::new();
    let stats = collector
        .collect(&harness.connection)
        .await
        .expect("采集不报错")
        .expect("登录 shell 不是 POSIX 也采得到（输出可解析）");
    // 重要的是拿到了结果，而不是 parseable=false 后停止采集（数值取决于本机：有没有 /proc、nproc）。
    assert!(
        stats.cpu_cores.is_some_and(|n| n >= 1),
        "{:?}",
        stats.cpu_cores
    );

    let world = harness.world.lock().unwrap();
    // exec 命令只有 `sh -s`：没有任何要登录 shell 解析的内容。
    assert_eq!(world.commands, vec![SCRIPT_EXEC_COMMAND.to_owned()]);
    assert!(!world.commands[0].contains("$("));
    // 脚本是经 stdin 到的：整段包在函数里（先读完再执行，里面的命令读不到脚本自己的文本）。
    assert_eq!(world.scripts.len(), 1);
    let script = &world.scripts[0];
    assert!(script.starts_with("__zssh_script() {\n"), "{script}");
    assert!(
        script.ends_with("}\n__zssh_script </dev/null\n"),
        "{script}"
    );
    assert!(script.contains("CPURAW:"), "统计脚本整段都在 stdin 里");
    assert!(script.contains("CPUMODEL:"), "第一次采集带型号探测");
}

#[tokio::test]
async fn script_output_and_exit_status_come_back_through_exec_script_capture() {
    if !local_sh_works() {
        println!("跳过：本机没有可用的 sh");
        return;
    }
    let harness = start().await;
    // 含引号、`$(…)`、`!`、反斜杠、多行：这些都是登录 shell（fish / csh）会解析坏的东西，走 stdin 就原样到达。
    let script =
        "x=$(printf 'a b'); echo \"got: $x !\"\nprintf 'tab:\\t|\\n'\necho to-stderr >&2\nexit 3";
    let capture = harness
        .connection
        .exec_script_capture(script, Duration::from_secs(10), 4096)
        .await
        .expect("执行");
    assert_eq!(capture.stdout, "got: a b !\ntab:\t|\n");
    assert_eq!(capture.stderr.trim(), "to-stderr");
    assert_eq!(capture.exit_code, Some(3));
    assert!(!capture.timed_out);
    // 输出上限照旧生效（和 exec_capture 同一个 run_exec）。
    let capped = harness
        .connection
        .exec_script_capture("echo 0123456789", Duration::from_secs(10), 4)
        .await
        .expect("执行");
    assert_eq!(capped.stdout, "0123");
    assert_eq!(capped.stdout_truncated_bytes, 7);
}

#[test]
fn script_stdin_wraps_the_script_in_a_function_called_with_dev_null() {
    let bytes = script_stdin("echo hi");
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(
        text,
        "__zssh_script() {\necho hi\n}\n__zssh_script </dev/null\n"
    );
}

#[test]
fn a_command_that_reads_stdin_cannot_swallow_the_rest_of_the_script() {
    if !local_sh_works() {
        println!("跳过：本机没有可用的 sh");
        return;
    }
    // `cat` 不带文件名会读 stdin 直到 EOF。
    let script = "echo before\ncat >/dev/null\necho after";
    // 不包函数（对照组）：bash 从管道读脚本是边读边执行，cat 会把后面的 `echo after` 当输入吃掉；
    // dash（Ubuntu / Debian 的 sh）先把整段读进来，吃不到。两种都是真实服务器上会遇到的 sh，
    // 所以对照组只要求是这两种结果之一，真正要保证的是下面包了函数之后一定能跑完。
    let (raw, _, _) = run_local_sh(script.as_bytes());
    match raw.trim() {
        "before" => {}
        "before
after" => println!("本机 sh 先读完整段脚本（dash 一类），对照组吃不到后半段"),
        other => panic!("对照组输出意外：{other:?}"),
    }
    // 包了函数：整个脚本跑完。
    let (wrapped, _, code) = run_local_sh(&script_stdin(script));
    assert_eq!(wrapped.trim(), "before\nafter");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn exec_script_returns_stdout_and_turns_a_failing_exit_into_an_error() {
    if !local_sh_works() {
        println!("跳过：本机没有可用的 sh");
        return;
    }
    let harness = start().await;
    let out = harness
        .connection
        .exec_script("echo ZSSH_KEY_ADDED", Duration::from_secs(10))
        .await
        .expect("执行");
    assert_eq!(out.trim(), "ZSSH_KEY_ADDED");
    let error = harness
        .connection
        .exec_script("echo nope >&2; exit 4", Duration::from_secs(10))
        .await
        .expect_err("退出码非 0 是错误");
    assert!(error.to_string().contains("nope"), "{error}");
}
