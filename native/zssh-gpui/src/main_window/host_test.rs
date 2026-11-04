//! 主机属性对话框的「测试连接」（V2-U13）的界面侧：把表单里**当前的值**拼成一份测试计划，交给
//! `zssh_core::ssh::connection_test` 去跑，结果显示在按钮栏上面。逻辑（阶段划分、错误分类、结果文案、
//! 指纹不写 known_hosts）都在核心库里，这里只做三件事：
//!
//! 1. 拼计划（[`build_job`]，纯函数 + 读主机库，有测试）：目标那一跳用表单里的地址 / 端口 / 用户名 /
//!    密码（框里没填就用这台主机已经存的密码）/ 选的密钥；跳板机取库里那条链（只有第一跳的代理生效）；
//!    代理按表单里选的方式解析；Telnet 只测 TCP 能不能连上；串口按参数打开再关闭；
//! 2. 在 SSH 运行时上跑，可以随时取消（取消 = 终止那个任务，连接随之关掉）；关掉对话框也会终止；
//! 3. 结果落在表单状态里（成功绿色 / 失败红色），「复制详情」复制多行详情。
//!
//! 测试用**独立连接**：不进连接池（不碰 `ssh_backend` 的池）、不影响已打开的标签；日志和结果里没有密码。

use std::time::Instant;

use ::gpui::{ClipboardItem, Window};
use gpui_kit::*;
use zssh_core::model::{
    AuthMethod, HostDraft, Protocol, ProtocolSettings, ProxyMode, SerialOptions,
};
use zssh_core::ssh::connection_test::{
    self, SshTestPlan, TEST_TIMEOUT, TcpRoute, TcpTestPlan, TestOutcome,
};
use zssh_core::ssh::{AuthConfig, ChainHop, ProxyConfig};
use zssh_core::store::Database;

use super::MainWindow;
use super::host_form::{FormInput, effective_routing};
use super::protocol_form::{FieldError, FormField};
use crate::ssh_backend::{known_hosts, resolve_proxy};

/// 正在跑的那次测试：丢掉它（对话框关了 / 重新测试）就终止任务。
pub struct RunningTest {
    pub generation: u64,
    abort: tokio::task::AbortHandle,
}

impl Drop for RunningTest {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// 表单上和测试连接有关的状态。
#[derive(Default)]
pub struct TestState {
    pub running: Option<RunningTest>,
    pub result: Option<TestOutcome>,
    next_generation: u64,
}

impl TestState {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn begin(&mut self) -> u64 {
        self.next_generation += 1;
        self.result = None;
        self.next_generation
    }
}

/// 一次要跑的测试。
pub enum Job {
    Ssh(SshTestPlan),
    Telnet(TcpTestPlan),
    Serial(SerialOptions),
}

/// 拼计划要的东西（都是表单里的值 / 保存规则已经校验过的草稿）。
pub struct JobInput<'a> {
    /// 用在路线文字里（「目标 web-1」）。
    pub name: &'a str,
    /// 地址 / 端口 / 用户名 / 认证方式 / 选的密钥 / 旧算法兼容 / 代理 / 跳板机。
    pub draft: &'a HostDraft,
    pub settings: &'a ProtocolSettings,
    /// 密码框里的字（可能是空的）。
    pub typed_password: &'a str,
    /// 编辑已有主机时，它现在关联的密码身份（框里没填密码就用它存的）。
    pub stored_password_identity: Option<i64>,
}

fn proxy_config(
    db: &Database,
    mode: ProxyMode,
    proxy_id: Option<i64>,
) -> Result<Option<ProxyConfig>, FieldError> {
    let id = match mode {
        ProxyMode::None => return Ok(None),
        ProxyMode::Default => match db
            .default_proxy_id()
            .map_err(|err| FieldError::new(FormField::Proxy, err.to_string()))?
        {
            Some(id) => id,
            None => return Ok(None),
        },
        ProxyMode::Custom => proxy_id
            .ok_or_else(|| FieldError::new(FormField::Proxy, "选了指定代理，但没有选是哪个代理"))?,
    };
    let login = db
        .proxy_login(id)
        .map_err(|err| FieldError::new(FormField::Proxy, err.to_string()))?;
    Ok(Some(ProxyConfig {
        kind: login.kind,
        host: login.host,
        port: login.port,
        username: login.username,
        password: login.password,
    }))
}

/// 跳板机那条链（最外层在前，最后一个就是选中的跳板机）。每一跳用它自己那条主机记录的认证；
/// 只有第一跳的代理生效（后面各跳走在前一跳的 SSH 通道里），所以只给第一跳解析代理。
fn jump_hops(db: &Database, jump_id: i64) -> Result<Vec<ChainHop>, FieldError> {
    let jump_error = |message: String| FieldError::new(FormField::Jump, message);
    let hosts = db
        .host_chain(jump_id)
        .map_err(|err| jump_error(err.to_string()))?;
    let default_proxy = db
        .default_proxy_id()
        .map_err(|err| jump_error(err.to_string()))?;
    let known = known_hosts();
    let mut hops = Vec::with_capacity(hosts.len());
    for (index, host) in hosts.iter().enumerate() {
        let login = db
            .host_login(host.id)
            .map_err(|err| jump_error(err.to_string()))?;
        let proxy = if index == 0 {
            resolve_proxy(db, host, default_proxy).map_err(jump_error)?
        } else {
            None
        };
        hops.push(ChainHop {
            label: format!("跳板机 {}", host.name),
            host: login.host,
            port: login.port,
            username: login.username,
            proxy,
            auth: login.auth,
            known_hosts: known.clone(),
            legacy_algorithms: login.legacy_algorithms,
        });
    }
    Ok(hops)
}

/// 把表单里的值拼成一份测试计划。错误带着该显示在哪个字段旁边（和保存时的校验错误一样显示）。
pub fn build_job(db: &Database, input: &JobInput<'_>) -> Result<Job, FieldError> {
    let draft = input.draft;
    match input.settings.protocol {
        Protocol::Serial => {
            let options = input
                .settings
                .serial
                .clone()
                .ok_or_else(|| FieldError::new(FormField::Device, "请选择或填写串口设备"))?;
            Ok(Job::Serial(options))
        }
        Protocol::Telnet => {
            let route = match draft.jump_host_id {
                // 设了跳板机时目标自己的代理不再用（和真实连接一样）。
                Some(jump) => TcpRoute::Jump(jump_hops(db, jump)?),
                None => match proxy_config(db, draft.proxy_mode, draft.proxy_id)? {
                    Some(proxy) => TcpRoute::Proxy(proxy),
                    None => TcpRoute::Direct,
                },
            };
            Ok(Job::Telnet(TcpTestPlan {
                host: draft.host.clone(),
                port: draft.port,
                route,
            }))
        }
        Protocol::Ssh => {
            let auth = match draft.auth_method {
                AuthMethod::Password => {
                    if !input.typed_password.is_empty() {
                        AuthConfig::password(Some(input.typed_password.to_owned()))
                    } else if let Some(identity) = input.stored_password_identity {
                        db.auth_for_login(AuthMethod::Password, Some(identity), input.name)
                            .map_err(|err| FieldError::new(FormField::Password, err.to_string()))?
                    } else {
                        // 没有任何密码：认证时会问，测试连接不弹窗，结果里说明「需要密码」。
                        AuthConfig::password(None)
                    }
                }
                method => db
                    .auth_for_login(method, draft.identity_id, input.name)
                    .map_err(|err| FieldError::new(FormField::Identity, err.to_string()))?,
            };
            let mut hops = match draft.jump_host_id {
                Some(jump) => jump_hops(db, jump)?,
                None => Vec::new(),
            };
            let proxy = if hops.is_empty() {
                proxy_config(db, draft.proxy_mode, draft.proxy_id)?
            } else {
                None
            };
            hops.push(ChainHop {
                label: format!("目标 {}", input.name),
                host: draft.host.clone(),
                port: draft.port,
                username: draft.username.clone(),
                proxy,
                auth,
                known_hosts: known_hosts(),
                legacy_algorithms: draft.legacy_algorithms,
            });
            Ok(Job::Ssh(SshTestPlan { hops }))
        }
    }
}

/// 串口：在阻塞线程里按参数打开再关闭（驱动可能卡住，所以有时限）。
async fn run_serial(options: SerialOptions) -> TestOutcome {
    let device = options.device.clone();
    let started = Instant::now();
    let task = tokio::task::spawn_blocking(move || zssh_protocols::serial::probe_serial(&options));
    let result = tokio::time::timeout(TEST_TIMEOUT, task).await;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(Ok(Ok(()))) => connection_test::serial_success(&device, elapsed),
        Ok(Ok(Err(err))) => connection_test::serial_failure(&device, &err.to_string(), elapsed),
        Ok(Err(_)) => connection_test::serial_failure(&device, "测试任务意外结束", elapsed),
        Err(_) => connection_test::serial_failure(
            &device,
            "打开串口超过 10 秒没有返回（驱动可能卡住了）",
            elapsed,
        ),
    }
}

pub async fn run_job(job: Job) -> TestOutcome {
    match job {
        Job::Ssh(plan) => connection_test::test_ssh(plan).await,
        Job::Telnet(plan) => connection_test::test_tcp(plan).await,
        Job::Serial(options) => run_serial(options).await,
    }
}

impl MainWindow {
    /// 「测试连接」：校验表单（和保存同一套规则，错误同样落在字段旁边并滚过去），然后在后台测。
    pub(crate) fn host_test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.host_form else {
            return;
        };
        if form.test.is_running() {
            return;
        }
        let name = form.name.read(cx).value().to_string();
        let address = form.address.read(cx).value().to_string();
        let port = form.port.read(cx).value().to_string();
        let username = form.username.read(cx).value().to_string();
        let password = form.password.read(cx).value().to_string();
        let terminal_type = form.telnet.terminal_type.read(cx).value().to_string();
        let username_prompts = form.telnet.username_prompts.read(cx).value().to_string();
        let password_prompts = form.telnet.password_prompts.read(cx).value().to_string();
        let device = form.serial.device.read(cx).value().to_string();
        let baud = form.serial.baud.read(cx).value().to_string();
        let routing = effective_routing(
            form.conn_mode,
            form.proxy_mode,
            form.proxy,
            form.jump,
            form.jump_keeps_proxy,
        );
        // 测试不需要名称：留空时给个占位，免得「名称不能为空」挡住测试。
        let shown_name = if name.trim().is_empty() {
            address.trim().to_owned()
        } else {
            name.trim().to_owned()
        };
        let input = FormInput {
            name: if name.trim().is_empty() {
                "测试"
            } else {
                &name
            },
            address: &address,
            port: &port,
            username: &username,
            password: &password,
            group: form.group,
            auth: form.auth,
            identity: form.identity,
            proxy_mode: routing.proxy_mode,
            proxy: routing.proxy,
            jump: routing.jump,
            legacy: form.legacy,
            protocol: form.protocol,
            x11: form.x11,
            telnet: super::host_form::TelnetInput {
                newline: form.telnet.newline,
                local_echo: form.telnet.local_echo,
                naws: form.telnet.naws,
                terminal_type: &terminal_type,
                encoding: form.telnet.encoding,
                auto_login: form.telnet.auto_login,
                username_prompts: &username_prompts,
                password_prompts: &password_prompts,
                raw_tcp: form.telnet.raw_tcp,
            },
            serial: super::host_form::SerialInput {
                device: &device,
                baud: &baud,
                data_bits: form.serial.data_bits,
                parity: form.serial.parity,
                stop_bits: form.serial.stop_bits,
                flow_control: form.serial.flow_control,
                newline: form.serial.newline,
                local_echo: form.serial.local_echo,
                encoding: form.serial.encoding,
            },
        };
        let existing = form.existing.clone();
        // 「连接方式」选了跳板机却没选是哪台：和保存一样当场指出。
        if form.protocol != Protocol::Serial
            && form.conn_mode == super::host_form::ConnMode::Jump
            && form.jump.is_none()
        {
            self.show_form_error(FormField::Jump, "请选择跳板机", window, cx);
            return;
        }
        let planned = super::host_form::plan_save(
            &input,
            existing.as_ref(),
            self.store.identities(),
            self.store.hosts(),
            None,
        );
        let plan = match planned {
            Ok(plan) => plan,
            Err(error) => {
                self.show_form_error(error.field, &error.message, window, cx);
                return;
            }
        };
        let settings = match input.protocol_settings() {
            Ok(settings) => settings,
            Err(error) => {
                self.show_form_error(error.field, &error.message, window, cx);
                return;
            }
        };
        let stored_password_identity =
            super::host_form::password_identity(existing.as_ref(), self.store.identities())
                .map(|identity| identity.id);
        let job = {
            let db = match self.store.db() {
                Ok(db) => db,
                Err(err) => {
                    self.notice_error(&err, window, cx);
                    return;
                }
            };
            build_job(
                db,
                &JobInput {
                    name: &shown_name,
                    draft: &plan.draft,
                    settings: &settings,
                    typed_password: &password,
                    stored_password_identity,
                },
            )
        };
        let job = match job {
            Ok(job) => job,
            Err(error) => {
                self.show_form_error(error.field, &error.message, window, cx);
                return;
            }
        };
        let Ok(runtime) = crate::ssh_backend::runtime() else {
            self.notice("后台运行时没起来，不能测试连接", window, cx);
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = runtime.spawn(async move {
            let outcome = run_job(job).await;
            let _ = tx.send(outcome);
        });
        let Some(form) = &mut self.host_form else {
            handle.abort();
            return;
        };
        form.errors.clear();
        let generation = form.test.begin();
        form.test.running = Some(RunningTest {
            generation,
            abort: handle.abort_handle(),
        });
        self.await_result(rx, window, cx, move |this, outcome, _window, cx| {
            this.host_test_done(generation, outcome, cx);
        });
        cx.notify();
    }

    fn host_test_done(&mut self, generation: u64, outcome: TestOutcome, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.host_form
            && form
                .test
                .running
                .as_ref()
                .is_some_and(|running| running.generation == generation)
        {
            form.test.running = None;
            form.test.result = Some(outcome);
            cx.notify();
        }
    }

    /// 「取消」：终止正在跑的测试（连接随任务一起关掉）。
    pub(crate) fn host_test_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.host_form
            && form.test.running.take().is_some()
        {
            form.test.result = Some(connection_test::cancelled());
            cx.notify();
        }
    }

    /// 「复制详情」。
    pub(crate) fn host_test_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let detail = self
            .host_form
            .as_ref()
            .and_then(|form| form.test.result.as_ref())
            .map(|outcome| outcome.detail.clone());
        if let Some(detail) = detail {
            cx.write_to_clipboard(ClipboardItem::new_string(detail));
            self.notice("已复制测试详情", window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{Job, JobInput, build_job};
    use crate::main_window::protocol_form::FormField;
    use crate::test_ssh_server;
    use zssh_core::model::{
        AuthMethod, HostDraft, IdentityDraft, IdentityKind, Protocol, ProtocolSettings, ProxyDraft,
        ProxyKind, ProxyMode,
    };
    use zssh_core::ssh::connection_test::{
        SshTestPlan, TcpRoute, TestStage, test_ssh, test_ssh_with,
    };
    // 只有用假 Telnet 设备的用例会用到（它们只在 debug 下编译）。
    #[cfg(debug_assertions)]
    use zssh_core::ssh::connection_test::{TcpTestPlan, test_tcp, test_tcp_with};
    use zssh_core::ssh::{AuthConfig, ChainHop, KnownHosts, ProxyConfig};
    use zssh_core::store::Database;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("运行时")
    }

    fn temp_known_hosts(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("zssh-u13-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn hop(label: &str, addr: SocketAddr, password: &str, known_hosts: &PathBuf) -> ChainHop {
        ChainHop {
            label: label.to_owned(),
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth: AuthConfig::password(Some(password.to_owned())),
            known_hosts: KnownHosts::new(known_hosts),
            legacy_algorithms: false,
        }
    }

    /// 一个没人监听的本机端口（绑定后立刻放掉）。
    fn closed_port() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let addr = listener.local_addr().expect("地址");
        drop(listener);
        addr
    }

    // ---- 端到端：进程内迷你 SSH 服务器 ----

    #[test]
    fn an_ssh_test_succeeds_for_a_trusted_host_and_never_writes_known_hosts() {
        let known = temp_known_hosts("ok");
        let (stopped, outcome) = runtime().block_on(async {
            // 要连两次（先陌生、再信任），所以用能一直接受连接的那个。
            let (addr, _probe) = test_ssh_server::spawn_counting().await;
            // 没见过这台主机：测试停在握手 + 指纹，不登录（V2-C7a S1），也不保存指纹。
            let stopped = test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", addr, "pw", &known)],
            })
            .await;
            assert!(!known.exists(), "测试连接不能写 known_hosts");
            test_ssh_server::trust_host_key(addr, &known);
            let saved = std::fs::read(&known).expect("读");
            let outcome = test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", addr, "pw", &known)],
            })
            .await;
            assert_eq!(
                std::fs::read(&known).expect("读"),
                saved,
                "测试连接不能改 known_hosts"
            );
            (stopped, outcome)
        });
        assert!(
            !stopped.ok && stopped.stage.is_none(),
            "{}",
            stopped.headline
        );
        assert!(
            stopped
                .headline
                .contains("没有测试登录；请先正式连接一次并核对指纹"),
            "{}",
            stopped.headline
        );
        assert!(stopped.untrusted_fingerprint.is_some());
        assert!(stopped.detail.contains("直连") && stopped.detail.contains("没有保存"));
        // 已信任：照常登录，结果里带服务器版本。
        assert!(outcome.ok, "{}", outcome.headline);
        assert!(
            outcome.headline.contains("服务器 SSH-2.0-"),
            "{}",
            outcome.headline
        );
        assert!(outcome.untrusted_fingerprint.is_none());
    }

    #[test]
    fn a_wrong_password_is_an_authentication_failure_and_the_right_one_succeeds() {
        let known = temp_known_hosts("auth");
        let (bad, good, saved) = runtime().block_on(async {
            // 主机已信任：测试才会真的去登录。
            let addr = test_ssh_server::spawn_requiring_password("right-pw").await;
            test_ssh_server::trust_host_key(addr, &known);
            let bad = test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", addr, "wrong-pw", &known)],
            })
            .await;
            let addr = test_ssh_server::spawn_requiring_password("right-pw").await;
            test_ssh_server::trust_host_key(addr, &known);
            let saved = std::fs::read(&known).expect("读");
            let good = test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", addr, "right-pw", &known)],
            })
            .await;
            (bad, good, saved)
        });
        assert!(!bad.ok);
        assert_eq!(bad.stage, Some(TestStage::Auth));
        assert!(bad.headline.contains("认证失败"), "{}", bad.headline);
        // 结果和详情里都不能出现密码。
        for text in [&bad.headline, &bad.detail] {
            assert!(
                !text.contains("wrong-pw") && !text.contains("right-pw"),
                "{text}"
            );
        }
        assert!(good.ok, "{}", good.headline);
        assert_eq!(
            std::fs::read(&known).expect("读"),
            saved,
            "失败和成功的测试都不改 known_hosts"
        );
    }

    #[test]
    fn a_changed_host_key_fails_and_leaves_the_saved_one_alone() {
        let known = temp_known_hosts("changed");
        let outcome = runtime().block_on(async {
            let addr = test_ssh_server::spawn().await;
            // 先给这台主机存一把别的密钥：服务器的密钥是现场随机生成的，所以一定对不上。
            let other =
                russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                    .expect("密钥");
            KnownHosts::new(&known)
                .append(
                    &addr.ip().to_string(),
                    addr.port(),
                    &other.public_key().clone(),
                )
                .expect("预先保存");
            let before = std::fs::read(&known).expect("读");
            let outcome = test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", addr, "pw", &known)],
            })
            .await;
            assert_eq!(
                std::fs::read(&known).expect("读"),
                before,
                "已保存的指纹一个字节都不能动"
            );
            outcome
        });
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, Some(TestStage::HostKey));
        assert!(
            outcome.headline.contains("主机密钥与保存的不一致"),
            "{}",
            outcome.headline
        );
    }

    #[test]
    fn a_refused_connection_is_reported_as_refused_at_the_connect_stage() {
        let outcome = runtime().block_on(async {
            test_ssh(SshTestPlan {
                hops: vec![hop(
                    "目标 lab-1",
                    closed_port(),
                    "pw",
                    &temp_known_hosts("refused"),
                )],
            })
            .await
        });
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, Some(TestStage::Connect));
        assert!(
            outcome.headline.contains("连接被拒绝"),
            "{}",
            outcome.headline
        );
    }

    #[test]
    fn a_server_that_never_answers_times_out_and_names_the_stage() {
        let outcome = runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("绑定");
            let addr = listener.local_addr().expect("地址");
            // 接受连接但什么都不说：握手卡住。
            tokio::spawn(async move {
                let _held = listener.accept().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
            test_ssh_with(
                SshTestPlan {
                    hops: vec![hop("目标 lab-1", addr, "pw", &temp_known_hosts("slow"))],
                },
                Duration::from_millis(700),
            )
            .await
        });
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, Some(TestStage::Handshake));
        assert!(
            outcome.headline.contains("卡在：SSH 握手"),
            "{}",
            outcome.headline
        );
    }

    #[test]
    fn an_unreachable_proxy_is_a_proxy_failure() {
        let outcome = runtime().block_on(async {
            let mut target = hop(
                "目标 lab-1",
                "127.0.0.1:22".parse().expect("地址"),
                "pw",
                &temp_known_hosts("proxy"),
            );
            let proxy = closed_port();
            target.proxy = Some(ProxyConfig {
                kind: ProxyKind::Socks5,
                host: proxy.ip().to_string(),
                port: proxy.port(),
                username: None,
                password: Some("proxy-secret".to_owned()),
            });
            test_ssh(SshTestPlan { hops: vec![target] }).await
        });
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, Some(TestStage::Proxy));
        assert!(
            outcome.headline.starts_with("代理失败"),
            "{}",
            outcome.headline
        );
        assert!(!outcome.detail.contains("proxy-secret"));
    }

    #[test]
    fn ssh_through_a_jump_host_works_and_the_route_names_it() {
        let known = temp_known_hosts("jump");
        let (outcome, saved) = runtime().block_on(async {
            let jump = test_ssh_server::spawn_forwarding().await;
            let target = test_ssh_server::spawn().await;
            test_ssh_server::trust_host_key(jump, &known);
            test_ssh_server::trust_host_key(target, &known);
            let saved = std::fs::read(&known).expect("读");
            let outcome = test_ssh(SshTestPlan {
                hops: vec![
                    hop("跳板机 edge-1", jump, "pw", &known),
                    hop("目标 lab-1", target, "pw", &known),
                ],
            })
            .await;
            (outcome, saved)
        });
        assert!(outcome.ok, "{}", outcome.headline);
        assert!(
            outcome.detail.contains("经 跳板机 edge-1"),
            "{}",
            outcome.detail
        );
        assert_eq!(std::fs::read(&known).expect("读"), saved);
    }

    // ---- 端到端：Telnet 只测 TCP ----
    // 假 Telnet 设备（`fake_telnet`）只在 debug 构建里存在，用它的用例也只在 debug 下编译；
    // 否则 `cargo test --release` 编不过。

    #[cfg(debug_assertions)]
    fn spawn_fake_telnet() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let addr = listener.local_addr().expect("地址");
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = crate::fake_telnet::serve(stream);
            }
        });
        addr
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_telnet_device_that_speaks_first_passes_and_one_that_stays_quiet_passes_too() {
        let (talkative, quiet, refused) = runtime().block_on(async {
            let device = spawn_fake_telnet();
            let talkative = test_tcp(TcpTestPlan {
                host: device.ip().to_string(),
                port: device.port(),
                route: TcpRoute::Direct,
            })
            .await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("绑定");
            let silent = listener.local_addr().expect("地址");
            tokio::spawn(async move {
                let _held = listener.accept().await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
            let quiet = test_tcp_with(
                TcpTestPlan {
                    host: silent.ip().to_string(),
                    port: silent.port(),
                    route: TcpRoute::Direct,
                },
                Duration::from_secs(5),
                Duration::from_millis(300),
            )
            .await;
            let dead = closed_port();
            let refused = test_tcp(TcpTestPlan {
                host: dead.ip().to_string(),
                port: dead.port(),
                route: TcpRoute::Direct,
            })
            .await;
            (talkative, quiet, refused)
        });
        assert!(
            talkative.ok && talkative.headline.contains("对方已回应"),
            "{}",
            talkative.headline
        );
        assert!(
            quiet.ok && quiet.headline.contains("没有主动发数据"),
            "{}",
            quiet.headline
        );
        assert!(
            !refused.ok && refused.headline.contains("连接被拒绝"),
            "{}",
            refused.headline
        );
        assert_eq!(refused.stage, Some(TestStage::Connect));
        // Telnet 测试只连 TCP：不做自动登录。
        assert!(talkative.detail.contains("未做登录"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_telnet_test_can_go_through_an_ssh_jump_host() {
        let known = temp_known_hosts("tjump");
        let outcome = runtime().block_on(async {
            let jump = test_ssh_server::spawn_forwarding().await;
            test_ssh_server::trust_host_key(jump, &known);
            let device = spawn_fake_telnet();
            test_tcp(TcpTestPlan {
                host: device.ip().to_string(),
                port: device.port(),
                route: TcpRoute::Jump(vec![hop("跳板机 edge-1", jump, "pw", &known)]),
            })
            .await
        });
        assert!(outcome.ok, "{}", outcome.headline);
        assert!(
            outcome.detail.contains("经跳板机 跳板机 edge-1"),
            "{}",
            outcome.detail
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_serial_port_that_does_not_exist_says_so() {
        let outcome = runtime().block_on(async {
            let options = zssh_core::model::SerialOptions {
                device: "COM98".to_owned(),
                ..Default::default()
            };
            super::run_job(Job::Serial(options)).await
        });
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, Some(TestStage::Open));
        assert!(
            outcome.headline.contains("COM98") && outcome.headline.contains("不存在"),
            "{}",
            outcome.headline
        );
    }

    // ---- 拼计划：读主机库，不碰网络 ----

    fn memory_db() -> Database {
        Database::open_in_memory().expect("内存库")
    }

    fn draft(name: &str) -> HostDraft {
        HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        }
    }

    fn save_proxy(db: &Database, name: &str, port: u16) -> i64 {
        db.proxy_save(&ProxyDraft {
            id: None,
            name: name.to_owned(),
            kind: ProxyKind::Socks5,
            host: "198.51.100.5".to_owned(),
            port,
            username: Some("pu".to_owned()),
            secret: Some("proxy-pass".to_owned()),
        })
        .expect("代理")
        .id
    }

    fn input<'a>(
        draft: &'a HostDraft,
        settings: &'a ProtocolSettings,
        typed: &'a str,
        stored: Option<i64>,
    ) -> JobInput<'a> {
        JobInput {
            name: "lab-1",
            draft,
            settings,
            typed_password: typed,
            stored_password_identity: stored,
        }
    }

    fn ssh_hops(job: Job) -> Vec<ChainHop> {
        match job {
            Job::Ssh(plan) => plan.hops,
            _ => panic!("应当是 SSH 测试"),
        }
    }

    #[test]
    fn the_target_hop_uses_the_typed_password_then_the_stored_one_then_none() {
        let db = memory_db();
        let stored = db
            .identity_save(&IdentityDraft {
                id: None,
                name: "lab-1 的密码".to_owned(),
                kind: IdentityKind::Password,
                key_path: None,
                secret: Some("stored-pw".to_owned()),
                passphrase: None,
            })
            .expect("凭据")
            .id;
        let settings = ProtocolSettings::default();
        let d = draft("lab-1");
        let typed = ssh_hops(
            build_job(&db, &input(&d, &settings, "typed-pw", Some(stored))).expect("计划"),
        );
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].auth.password.as_deref(), Some("typed-pw"));
        assert_eq!(typed[0].label, "目标 lab-1");
        assert_eq!(
            (
                typed[0].host.as_str(),
                typed[0].port,
                typed[0].username.as_str()
            ),
            ("192.0.2.10", 22, "ops")
        );
        let from_vault =
            ssh_hops(build_job(&db, &input(&d, &settings, "", Some(stored))).expect("计划"));
        assert_eq!(from_vault[0].auth.password.as_deref(), Some("stored-pw"));
        let none = ssh_hops(build_job(&db, &input(&d, &settings, "", None)).expect("计划"));
        assert_eq!(
            none[0].auth.password, None,
            "没有任何密码：认证时会问，测试里说明「需要密码」"
        );
        // 日志 / 断言输出里不出现明文密码。
        assert!(!format!("{typed:?}").contains("typed-pw"));
    }

    #[test]
    fn a_key_login_needs_a_chosen_key_and_resolves_it_from_the_vault() {
        let db = memory_db();
        let mut d = draft("lab-1");
        d.auth_method = AuthMethod::Key;
        let settings = ProtocolSettings::default();
        let Err(error) = build_job(&db, &input(&d, &settings, "", None)) else {
            panic!("私钥认证没选密钥应当报错");
        };
        assert_eq!(error.field, FormField::Identity);
        assert!(error.message.contains("没选凭据"), "{}", error.message);
        let key = db
            .identity_save(&IdentityDraft {
                id: None,
                name: "部署密钥".to_owned(),
                // 只是个路径：解析认证材料不会去读文件（真正连接时才读）。
                kind: IdentityKind::KeyFile,
                key_path: Some("C:/zssh-test-keys/id_ed25519".to_owned()),
                secret: None,
                passphrase: None,
            })
            .expect("密钥")
            .id;
        d.identity_id = Some(key);
        let hops = ssh_hops(build_job(&db, &input(&d, &settings, "", None)).expect("计划"));
        assert_eq!(hops[0].auth.method, AuthMethod::Key);
    }

    #[test]
    fn the_proxy_follows_the_form_choice_default_custom_or_none() {
        let db = memory_db();
        let settings = ProtocolSettings::default();
        let proxy_id = save_proxy(&db, "办公室代理", 1080);
        let mut d = draft("lab-1");
        // 不用代理
        let hops = ssh_hops(build_job(&db, &input(&d, &settings, "x", None)).expect("计划"));
        assert!(hops[0].proxy.is_none());
        // 跟随默认代理：库里没设默认 = 不走代理；设了就走它
        d.proxy_mode = ProxyMode::Default;
        let hops = ssh_hops(build_job(&db, &input(&d, &settings, "x", None)).expect("计划"));
        assert!(hops[0].proxy.is_none());
        db.set_default_proxy_id(Some(proxy_id)).expect("设默认");
        let hops = ssh_hops(build_job(&db, &input(&d, &settings, "x", None)).expect("计划"));
        assert_eq!(hops[0].proxy.as_ref().map(|p| p.port), Some(1080));
        // 指定代理没选是哪个：错误落在代理那一行
        d.proxy_mode = ProxyMode::Custom;
        d.proxy_id = None;
        let Err(error) = build_job(&db, &input(&d, &settings, "x", None)) else {
            panic!("指定代理没选应当报错");
        };
        assert_eq!(error.field, FormField::Proxy);
        d.proxy_id = Some(proxy_id);
        let hops = ssh_hops(build_job(&db, &input(&d, &settings, "x", None)).expect("计划"));
        assert_eq!(
            hops[0].proxy.as_ref().map(|p| p.host.as_str()),
            Some("198.51.100.5")
        );
        assert!(
            !format!("{hops:?}").contains("proxy-pass"),
            "代理密码不进调试输出"
        );
    }

    #[test]
    fn a_jump_host_brings_its_own_chain_and_only_the_first_hop_keeps_a_proxy() {
        let db = memory_db();
        let settings = ProtocolSettings::default();
        let proxy_id = save_proxy(&db, "办公室代理", 1080);
        let mut outer = draft("outer");
        outer.host = "192.0.2.1".to_owned();
        outer.proxy_mode = ProxyMode::Custom;
        outer.proxy_id = Some(proxy_id);
        let outer = db.host_save(&outer).expect("外层跳板机");
        let mut inner = draft("inner");
        inner.host = "192.0.2.2".to_owned();
        inner.jump_host_id = Some(outer.id);
        let inner = db.host_save(&inner).expect("里层跳板机");
        // 目标自己的代理设置在设了跳板机时不参与（只有第一跳的代理生效）
        let mut target = draft("lab-1");
        target.jump_host_id = Some(inner.id);
        target.proxy_mode = ProxyMode::Custom;
        target.proxy_id = Some(proxy_id);
        let hops = ssh_hops(build_job(&db, &input(&target, &settings, "x", None)).expect("计划"));
        let labels: Vec<&str> = hops.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(labels, ["跳板机 outer", "跳板机 inner", "目标 lab-1"]);
        assert!(hops[0].proxy.is_some(), "第一跳（外层跳板机）自己设了代理");
        assert!(hops[1].proxy.is_none() && hops[2].proxy.is_none());
        // 跳板机自己的认证用它那条记录的（这里没存密码：认证时才问）
        assert_eq!(hops[0].auth.password, None);
    }

    #[test]
    fn telnet_and_serial_jobs_pick_the_right_route_and_ignore_ssh_only_fields() {
        let db = memory_db();
        let proxy_id = save_proxy(&db, "办公室代理", 1080);
        let telnet = ProtocolSettings::new(Protocol::Telnet);
        let mut d = draft("sw-1");
        d.port = 23;
        match build_job(&db, &input(&d, &telnet, "", None)).expect("计划") {
            Job::Telnet(plan) => {
                assert_eq!((plan.host.as_str(), plan.port), ("192.0.2.10", 23));
                assert!(matches!(plan.route, TcpRoute::Direct));
            }
            _ => panic!("应当是 Telnet 测试"),
        }
        d.proxy_mode = ProxyMode::Custom;
        d.proxy_id = Some(proxy_id);
        match build_job(&db, &input(&d, &telnet, "", None)).expect("计划") {
            Job::Telnet(plan) => assert!(matches!(plan.route, TcpRoute::Proxy(_))),
            _ => panic!("应当是 Telnet 测试"),
        }
        let jump = db.host_save(&draft("edge-1")).expect("跳板机");
        d.jump_host_id = Some(jump.id);
        match build_job(&db, &input(&d, &telnet, "", None)).expect("计划") {
            // 设了跳板机：走跳板机，目标自己的代理不再用（和真实连接一样）
            Job::Telnet(plan) => {
                assert!(matches!(plan.route, TcpRoute::Jump(ref hops) if hops.len() == 1))
            }
            _ => panic!("应当是 Telnet 测试"),
        }
        let mut serial = ProtocolSettings::new(Protocol::Serial);
        if let Some(options) = serial.serial.as_mut() {
            options.device = "COM3".to_owned();
        }
        match build_job(&db, &input(&draft("console"), &serial, "", None)).expect("计划") {
            Job::Serial(options) => assert_eq!(options.device, "COM3"),
            _ => panic!("应当是串口测试"),
        }
    }
}
