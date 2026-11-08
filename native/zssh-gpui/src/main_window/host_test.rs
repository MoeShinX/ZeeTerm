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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ::gpui::{ClipboardItem, Window};
use gpui_kit::*;
use zssh_core::model::{
    AuthMethod, HostDraft, Protocol, ProtocolSettings, ProxyMode, SerialOptions,
};
use zssh_core::ssh::connection_test::{
    self, CheckState, IdentityState, SshTestPlan, TEST_TIMEOUT, TcpRoute, TcpTestPlan, TestOutcome,
    TestResultKind,
};
use zssh_core::ssh::{AuthConfig, ChainHop, ProxyConfig};
use zssh_core::store::Database;

use crate::i18n::t;

use super::MainWindow;
use super::host_form::{FormInput, effective_routing};
use super::protocol_form::{FieldError, FormField};
use crate::ssh_backend::{chain_hops, known_hosts, resolve_proxy_choice};

/// 影响测试结论的表单参数（路线、认证、目标）的快照，**不含任何密码原文或可恢复的哈希**：
/// 密码只用 `password_version`（框里的内容每变一次就加一）。名称、分组、标签这类纯展示的修改不在里面，
/// 改它们不必重测，也不会让旧结果失效。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamsSignature {
    pub protocol: Protocol,
    pub address: String,
    pub port: String,
    pub username: String,
    pub auth: AuthMethod,
    pub identity: Option<i64>,
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
    pub legacy: bool,
    pub password_version: u64,
    /// 串口：设备名和波特率（别的串口参数不影响「能不能打开」，但改了就应当重测，所以都列出）。
    pub serial: String,
}

/// 下一个表单实例编号。每次打开表单领一个新的，关闭再打开也不会重复。
static NEXT_FORM_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// 一次测试的身份：哪个表单实例、第几次测试、当时的参数修订。结果回来时三者都对得上才算数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestTicket {
    pub form_instance: u64,
    pub attempt: u64,
    pub params_revision: u64,
}

/// 正在跑的那次测试：丢掉它（对话框关了 / 重新测试 / 参数改了）就终止任务。
pub struct RunningTest {
    pub ticket: TestTicket,
    abort: tokio::task::AbortHandle,
}

impl Drop for RunningTest {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// 表单上和测试连接有关的状态。
///
/// 三重身份：`form_instance`（每次打开表单分配一个，整个表单生命周期不变）、`attempt`（每次测试加一）、
/// `params_revision`（连接参数每变一次加一）。参数改动、关闭 / 重开表单（实例变了）、取消、重新测试
/// 都让旧任务终止、旧结果失效；迟到的结果对不上身份就丢掉，不会回填到别的表单或改过的参数上。
pub struct TestState {
    pub running: Option<RunningTest>,
    result: Option<TestOutcome>,
    form_instance: u64,
    attempt: u64,
    params_revision: u64,
    signature: Option<ParamsSignature>,
}

impl Default for TestState {
    fn default() -> Self {
        Self::new()
    }
}

impl TestState {
    pub fn new() -> Self {
        Self {
            running: None,
            result: None,
            form_instance: NEXT_FORM_INSTANCE.fetch_add(1, Ordering::Relaxed),
            attempt: 0,
            params_revision: 0,
            signature: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// 表单当前的参数是 `signature`：和上次看到的不一样就是参数变了——终止进行中的测试、丢掉旧结果。
    /// 返回参数有没有变。
    pub fn params_changed(&mut self, signature: &ParamsSignature) -> bool {
        if self.signature.as_ref() == Some(signature) {
            return false;
        }
        let first = self.signature.is_none();
        self.signature = Some(signature.clone());
        if first && self.running.is_none() && self.result.is_none() {
            // 第一次登记当前参数：没有什么要作废的，修订号也不用动。
            return false;
        }
        self.params_revision += 1;
        self.running = None;
        self.result = None;
        true
    }

    /// 开始新的一次测试：登记当前参数、作废旧任务和旧结果、换一个新的 attempt。
    pub fn begin(
        &mut self,
        signature: &ParamsSignature,
        abort: tokio::task::AbortHandle,
    ) -> TestTicket {
        self.params_changed(signature);
        self.attempt += 1;
        self.result = None;
        let ticket = TestTicket {
            form_instance: self.form_instance,
            attempt: self.attempt,
            params_revision: self.params_revision,
        };
        self.running = Some(RunningTest { ticket, abort });
        ticket
    }

    /// 结果回来了。身份对不上（表单换了、又测了一次、参数改了、已取消）就丢掉，返回 `false`。
    pub fn finish(
        &mut self,
        ticket: TestTicket,
        current: &ParamsSignature,
        outcome: TestOutcome,
    ) -> bool {
        // 先看参数有没有变：变了会把进行中的任务作废，下面的核对自然通不过。
        self.params_changed(current);
        let live = self.running.as_ref().is_some_and(|running| {
            running.ticket == ticket
                && ticket.form_instance == self.form_instance
                && ticket.params_revision == self.params_revision
        });
        if !live {
            return false;
        }
        self.running = None;
        self.result = Some(outcome);
        true
    }

    /// 取消：终止任务，结果是「已取消」。没有在跑就什么都不做。
    pub fn cancel(&mut self) -> bool {
        if self.running.take().is_none() {
            return false;
        }
        self.result = Some(connection_test::cancelled());
        true
    }

    /// 丢掉当前显示的结果（进行中的测试不动）。
    pub fn clear_result(&mut self) {
        self.result = None;
    }

    /// 要显示的结果：参数改过之后的旧结果不显示（即使还没来得及作废）。
    pub fn shown_result(&self, current: &ParamsSignature) -> Option<&TestOutcome> {
        if self.signature.as_ref() == Some(current) {
            self.result.as_ref()
        } else {
            None
        }
    }

    #[cfg(debug_assertions)]
    pub fn set_result(&mut self, outcome: TestOutcome, current: &ParamsSignature) {
        self.params_changed(current);
        self.result = Some(outcome);
    }

    #[cfg(test)]
    pub fn form_instance(&self) -> u64 {
        self.form_instance
    }
}

/// 结果的显示等级：颜色和图标只由它决定（见 [`present`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// 检查范围内全部完成。
    Success,
    /// 部分完成 / 已取消：中性，不是失败也不是成功。
    Neutral,
    /// 失败。
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultIcon {
    Success,
    Info,
    Error,
    Cancelled,
}

/// 结果条里的检查项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckItem {
    Connectivity,
    Identity,
    Login,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckLine {
    pub item: CheckItem,
    pub state: CheckState,
    /// 身份项的具体情况（别的项是 `None`）。
    pub identity: Option<IdentityState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presentation {
    pub tone: Tone,
    pub icon: ResultIcon,
    pub lines: Vec<CheckLine>,
}

/// **唯一**把测试结果映射成显示等级的地方：Complete 绿色对勾；Partial 中性提示图标，加上通过项目和
/// 中性的待办标签（不画失败叉，也不把待验证的登录画成绿色成功）；Failed 红色错误图标；Cancelled 中性取消图标。
pub fn present(outcome: &TestOutcome) -> Presentation {
    let (tone, icon) = match outcome.kind {
        TestResultKind::Complete => (Tone::Success, ResultIcon::Success),
        TestResultKind::Partial => (Tone::Neutral, ResultIcon::Info),
        TestResultKind::Failed => (Tone::Danger, ResultIcon::Error),
        TestResultKind::Cancelled => (Tone::Neutral, ResultIcon::Cancelled),
    };
    let mut lines = Vec::new();
    if outcome.kind != TestResultKind::Cancelled {
        if outcome.connectivity != CheckState::NotApplicable {
            lines.push(CheckLine {
                item: CheckItem::Connectivity,
                state: outcome.connectivity,
                identity: None,
            });
        }
        if let Some(identity) = outcome.identity {
            let state = match identity {
                IdentityState::Matched => CheckState::Passed,
                IdentityState::Unconfirmed | IdentityState::OtherKeyTypes => CheckState::Pending,
                IdentityState::Changed | IdentityState::Revoked | IdentityState::Unreadable => {
                    CheckState::Failed
                }
            };
            lines.push(CheckLine {
                item: CheckItem::Identity,
                state,
                identity: Some(identity),
            });
        }
        if outcome.authentication != CheckState::NotApplicable {
            lines.push(CheckLine {
                item: CheckItem::Login,
                state: outcome.authentication,
                identity: None,
            });
        }
    }
    Presentation { tone, icon, lines }
}

/// 检查项一行的文字，例如「登录：待首次连接验证」。
pub fn check_line_text(line: &CheckLine) -> String {
    let item = match line.item {
        CheckItem::Connectivity => t(HostTestMsg::ItemConnectivity),
        CheckItem::Identity => t(HostTestMsg::ItemIdentity),
        CheckItem::Login => t(HostTestMsg::ItemLogin),
    };
    let state = match (line.item, line.identity, line.state) {
        (CheckItem::Identity, Some(IdentityState::Matched), _) => t(HostTestMsg::IdentityMatched),
        (CheckItem::Identity, Some(IdentityState::Unconfirmed), _) => {
            t(HostTestMsg::IdentityUnconfirmed)
        }
        (CheckItem::Identity, Some(IdentityState::Changed), _) => t(HostTestMsg::IdentityChanged),
        (CheckItem::Identity, Some(IdentityState::Revoked), _) => t(HostTestMsg::IdentityRevoked),
        (CheckItem::Identity, Some(IdentityState::Unreadable), _) => {
            t(HostTestMsg::IdentityUnreadable)
        }
        (CheckItem::Identity, Some(IdentityState::OtherKeyTypes), _) => {
            t(HostTestMsg::IdentityOtherKeyTypes)
        }
        (_, _, CheckState::Passed) => t(HostTestMsg::StatePassed),
        (_, _, CheckState::Pending) => t(HostTestMsg::StatePending),
        (_, _, CheckState::Failed) => t(HostTestMsg::StateFailed),
        (_, _, CheckState::NotApplicable) => "",
    };
    format!("{item}{}{state}", t(HostTestMsg::Colon))
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

/// 表单里选的代理方式 → 代理配置。解析规则和正式连接是同一份（`ssh_backend::resolve_proxy_choice`），
/// 只是错误换成落在「代理」那一行的字段错误。
fn proxy_config(
    db: &Database,
    mode: ProxyMode,
    proxy_id: Option<i64>,
    name: &str,
) -> Result<Option<ProxyConfig>, FieldError> {
    let proxy_error = |message: String| FieldError::new(FormField::Proxy, message);
    if mode == ProxyMode::Custom && proxy_id.is_none() {
        return Err(proxy_error(t(HostTestMsg::ProxyNotChosen).to_owned()));
    }
    let default_proxy = db
        .default_proxy_id()
        .map_err(|err| proxy_error(err.to_string()))?;
    resolve_proxy_choice(db, mode, proxy_id, default_proxy, name).map_err(proxy_error)
}

/// 跳板机那条链（最外层在前，最后一个就是选中的跳板机）。和正式连接用同一个 `ssh_backend::chain_hops`：
/// 每一跳用它自己那条主机记录的认证、代理、known_hosts 和「旧算法兼容」；只有第一跳的代理生效
/// （后面各跳走在前一跳的 SSH 通道里）。这里只决定每一跳在界面上叫什么。
fn jump_hops(db: &Database, jump_id: i64) -> Result<Vec<ChainHop>, FieldError> {
    chain_hops(db, jump_id, |host, _index, _total, _proxy| {
        format!("{}{}", t(HostTestMsg::JumpLabelPrefix), host.name)
    })
    .map_err(|message| FieldError::new(FormField::Jump, message))
}

/// 把表单里的值拼成一份测试计划。错误带着该显示在哪个字段旁边（和保存时的校验错误一样显示）。
pub fn build_job(db: &Database, input: &JobInput<'_>) -> Result<Job, FieldError> {
    let draft = input.draft;
    match input.settings.protocol {
        Protocol::Serial => {
            let options = input.settings.serial.clone().ok_or_else(|| {
                FieldError::new(FormField::Device, t(HostTestMsg::ChooseSerialDevice))
            })?;
            Ok(Job::Serial(options))
        }
        Protocol::Telnet => {
            let route = match draft.jump_host_id {
                // 设了跳板机时目标自己的代理不再用（和真实连接一样）。
                Some(jump) => TcpRoute::Jump(jump_hops(db, jump)?),
                None => match proxy_config(db, draft.proxy_mode, draft.proxy_id, input.name)? {
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
                proxy_config(db, draft.proxy_mode, draft.proxy_id, input.name)?
            } else {
                None
            };
            hops.push(ChainHop {
                label: format!("{}{}", t(HostTestMsg::TargetLabelPrefix), input.name),
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
        Ok(Ok(Err(err))) => connection_test::serial_failure(
            &device,
            &err.message(zssh_protocols::language()),
            elapsed,
        ),
        Ok(Err(_)) => {
            connection_test::serial_failure(&device, t(HostTestMsg::TaskEndedUnexpectedly), elapsed)
        }
        Err(_) => {
            connection_test::serial_failure(&device, t(HostTestMsg::SerialOpenTimeout), elapsed)
        }
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
                t(HostTestMsg::DefaultTestName)
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
            self.show_form_error(FormField::Jump, t(HostTestMsg::ChooseJumpHost), window, cx);
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
            self.notice(t(HostTestMsg::RuntimeUnavailable), window, cx);
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
        let signature = form.test_signature(cx);
        let ticket = form.test.begin(&signature, handle.abort_handle());
        self.await_result(rx, window, cx, move |this, outcome, _window, cx| {
            this.host_test_done(ticket, outcome, cx);
        });
        cx.notify();
    }

    /// 测试结果回来了：只有表单实例、第几次测试、参数修订都对得上才落到表单上；
    /// 表单已经关了 / 重开过、又测了一次、参数改过、已取消，一律丢掉。
    fn host_test_done(&mut self, ticket: TestTicket, outcome: TestOutcome, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.host_form {
            let signature = form.test_signature(cx);
            if form.test.finish(ticket, &signature, outcome) {
                cx.notify();
            }
        }
    }

    /// 表单里和连接有关的参数可能变了：对一下，变了就终止进行中的测试、作废旧结果。
    pub(crate) fn host_test_sync(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.host_form {
            let signature = form.test_signature(cx);
            if form.test.params_changed(&signature) {
                cx.notify();
            }
        }
    }

    /// 「取消」：终止正在跑的测试（连接随任务一起关掉）。
    pub(crate) fn host_test_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.host_form
            && form.test.cancel()
        {
            cx.notify();
        }
    }

    /// 「复制详情」。
    pub(crate) fn host_test_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let detail = self
            .host_form
            .as_ref()
            .and_then(|form| form.test.shown_result(&form.test_signature(cx)))
            .map(|outcome| outcome.detail.clone());
        if let Some(detail) = detail {
            cx.write_to_clipboard(ClipboardItem::new_string(detail));
            self.notice(t(HostTestMsg::CopiedDetail), window, cx);
        }
    }
}

/// 「测试连接」对话框里的提示文字。
#[derive(Clone, Copy, Debug)]
enum HostTestMsg {
    ProxyNotChosen,
    ChooseSerialDevice,
    ChooseJumpHost,
    JumpLabelPrefix,
    TargetLabelPrefix,
    TaskEndedUnexpectedly,
    SerialOpenTimeout,
    RuntimeUnavailable,
    CopiedDetail,
    DefaultTestName,
    ItemConnectivity,
    ItemIdentity,
    ItemLogin,
    StatePassed,
    StatePending,
    StateFailed,
    IdentityMatched,
    IdentityUnconfirmed,
    IdentityChanged,
    IdentityRevoked,
    IdentityUnreadable,
    IdentityOtherKeyTypes,
    Colon,
}

impl crate::i18n::Msg for HostTestMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ProxyNotChosen => "选了指定代理，但没有选是哪个代理",
            Self::ChooseSerialDevice => "请选择或填写串口设备",
            Self::ChooseJumpHost => "请选择跳板机",
            Self::JumpLabelPrefix => "跳板机 ",
            Self::TargetLabelPrefix => "目标 ",
            Self::TaskEndedUnexpectedly => "测试任务意外结束",
            Self::SerialOpenTimeout => "打开串口超过 10 秒没有返回（驱动可能卡住了）",
            Self::RuntimeUnavailable => "后台运行时没起来，不能测试连接",
            Self::CopiedDetail => "已复制测试详情",
            Self::DefaultTestName => "测试",
            Self::ItemConnectivity => "连通性",
            Self::ItemIdentity => "服务器身份",
            Self::ItemLogin => "登录",
            Self::StatePassed => "通过",
            Self::StatePending => "待验证",
            Self::StateFailed => "失败",
            Self::IdentityMatched => "与已保存的记录一致",
            Self::IdentityUnconfirmed => "尚未确认，首次连接时核对",
            Self::IdentityChanged => "与已保存的记录不一致",
            Self::IdentityRevoked => "已被吊销",
            Self::IdentityUnreadable => "无法读取信任记录",
            Self::IdentityOtherKeyTypes => "密钥类型与已保存的不同，待核对",
            Self::Colon => "：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ProxyNotChosen => "A specific proxy was selected, but none was chosen",
            Self::ChooseSerialDevice => "Select or enter a serial device",
            Self::ChooseJumpHost => "Select a jump host",
            Self::JumpLabelPrefix => "Jump host ",
            Self::TargetLabelPrefix => "Target ",
            Self::TaskEndedUnexpectedly => "The test task ended unexpectedly",
            Self::SerialOpenTimeout => {
                "Opening the serial port did not return within 10 seconds (the driver may be stuck)"
            }
            Self::RuntimeUnavailable => {
                "The background runtime is not running, so the connection cannot be tested"
            }
            Self::CopiedDetail => "Test details copied",
            Self::DefaultTestName => "test",
            Self::ItemConnectivity => "Connectivity",
            Self::ItemIdentity => "Server identity",
            Self::ItemLogin => "Login",
            Self::StatePassed => "passed",
            Self::StatePending => "to be verified",
            Self::StateFailed => "failed",
            Self::IdentityMatched => "matches the saved record",
            Self::IdentityUnconfirmed => "not confirmed yet, checked on the first connection",
            Self::IdentityChanged => "differs from the saved record",
            Self::IdentityRevoked => "revoked",
            Self::IdentityUnreadable => "trust records could not be read",
            Self::IdentityOtherKeyTypes => "key type differs from the saved ones, to be checked",
            Self::Colon => ": ",
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
        CheckState, IdentityState, SshTestPlan, TcpRoute, TestResultKind, TestStage, test_ssh,
        test_ssh_with,
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

    /// 随机唯一的临时目录里的 known_hosts 路径（文件还不存在）；`TempDir` 要拿到用例结束。
    fn temp_known_hosts(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("zssh-u13-{tag}-"))
            .tempdir()
            .expect("临时目录");
        let path = dir.path().join("known_hosts");
        (dir, path)
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
        let (_known_dir, known) = temp_known_hosts("ok");
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
        assert_eq!(stopped.kind, TestResultKind::Partial, "{}", stopped.reason);
        assert!(stopped.failing_stage.is_none(), "部分完成不是失败");
        assert_eq!(
            stopped.reason,
            "网络与 SSH 响应检测通过，登录待首次连接验证"
        );
        assert_eq!(stopped.identity, Some(IdentityState::Unconfirmed));
        assert_eq!(stopped.authentication, CheckState::Pending);
        assert!(stopped.detail.contains("直连") && stopped.detail.contains("没有保存"));
        // 已信任：照常登录，结果里带服务器版本。
        assert_eq!(outcome.kind, TestResultKind::Complete, "{}", outcome.reason);
        assert_eq!(outcome.identity, Some(IdentityState::Matched));
        assert!(
            outcome.reason.contains("服务器 SSH-2.0-"),
            "{}",
            outcome.reason
        );
    }

    #[test]
    fn a_wrong_password_is_an_authentication_failure_and_the_right_one_succeeds() {
        let (_known_dir, known) = temp_known_hosts("auth");
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
        assert_eq!(bad.kind, TestResultKind::Failed);
        assert_eq!(bad.failing_stage, Some(TestStage::Auth));
        assert!(bad.reason.contains("认证失败"), "{}", bad.reason);
        // 结果和详情里都不能出现密码。
        for text in [bad.reason.to_string(), bad.detail.clone()] {
            assert!(
                !text.contains("wrong-pw") && !text.contains("right-pw"),
                "{text}"
            );
        }
        assert_eq!(good.kind, TestResultKind::Complete, "{}", good.reason);
        assert_eq!(
            std::fs::read(&known).expect("读"),
            saved,
            "失败和成功的测试都不改 known_hosts"
        );
    }

    #[test]
    fn a_changed_host_key_fails_and_leaves_the_saved_one_alone() {
        let (_known_dir, known) = temp_known_hosts("changed");
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
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::HostKey));
        assert!(
            outcome.reason.contains("主机密钥与保存的不一致"),
            "{}",
            outcome.reason
        );
    }

    #[test]
    fn a_refused_connection_is_reported_as_refused_at_the_connect_stage() {
        let (_known_dir, known) = temp_known_hosts("refused");
        let outcome = runtime().block_on(async {
            test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", closed_port(), "pw", &known)],
            })
            .await
        });
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::Connect));
        assert!(outcome.reason.contains("连接被拒绝"), "{}", outcome.reason);
    }

    #[test]
    fn a_server_that_never_answers_times_out_and_names_the_stage() {
        let (_known_dir, known) = temp_known_hosts("slow");
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
                    hops: vec![hop("目标 lab-1", addr, "pw", &known)],
                },
                Duration::from_millis(700),
            )
            .await
        });
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::Handshake));
        assert!(
            outcome.reason.contains("卡在：SSH 握手"),
            "{}",
            outcome.reason
        );
    }

    #[test]
    fn an_unreachable_proxy_is_a_proxy_failure() {
        let (_known_dir, known) = temp_known_hosts("proxy");
        let outcome = runtime().block_on(async {
            let mut target = hop(
                "目标 lab-1",
                "127.0.0.1:22".parse().expect("地址"),
                "pw",
                &known,
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
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::Proxy));
        assert!(outcome.reason.starts_with("代理失败"), "{}", outcome.reason);
        assert!(!outcome.detail.contains("proxy-secret"));
    }

    #[test]
    fn ssh_through_a_jump_host_works_and_the_route_names_it() {
        let (_known_dir, known) = temp_known_hosts("jump");
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
        assert_eq!(outcome.kind, TestResultKind::Complete, "{}", outcome.reason);
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
            talkative.kind == TestResultKind::Complete && talkative.reason.contains("对方已回应"),
            "{}",
            talkative.reason
        );
        assert!(
            quiet.kind == TestResultKind::Complete && quiet.reason.contains("没有主动发数据"),
            "{}",
            quiet.reason
        );
        assert!(
            refused.kind == TestResultKind::Failed && refused.reason.contains("连接被拒绝"),
            "{}",
            refused.reason
        );
        assert_eq!(refused.failing_stage, Some(TestStage::Connect));
        // Telnet 测试只连 TCP：不做自动登录。
        assert!(talkative.detail.contains("未做登录"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_telnet_test_can_go_through_an_ssh_jump_host() {
        let (_known_dir, known) = temp_known_hosts("tjump");
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
        assert_eq!(outcome.kind, TestResultKind::Complete, "{}", outcome.reason);
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
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::Open));
        assert!(
            outcome.reason.contains("COM98") && outcome.reason.contains("不存在"),
            "{}",
            outcome.reason
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

    /// 同一份配置：表单里（还没保存）拼出来的测试路线，和保存之后正式连接用的路线，
    /// 逐跳的地址 / 端口 / 用户 / 代理 / 旧算法兼容 / 认证方式完全一致——测试没有另造一份路线规则。
    #[test]
    fn the_test_route_equals_the_route_of_a_normal_connection_to_the_same_host() {
        let db = memory_db();
        let settings = ProtocolSettings::default();
        let proxy_id = save_proxy(&db, "办公室代理", 1080);
        let mut outer = draft("outer");
        outer.host = "192.0.2.1".to_owned();
        outer.proxy_mode = ProxyMode::Custom;
        outer.proxy_id = Some(proxy_id);
        outer.legacy_algorithms = true;
        let outer = db.host_save(&outer).expect("外层跳板机");
        let mut target = draft("lab-1");
        target.host = "192.0.2.9".to_owned();
        target.port = 2222;
        target.jump_host_id = Some(outer.id);
        target.legacy_algorithms = true;
        let saved = db.host_save(&target).expect("目标");
        let formal = crate::ssh_backend::chain_hops(&db, saved.id, |_, _, _, _| String::new())
            .expect("正式连接的路线");
        let tested = ssh_hops(build_job(&db, &input(&target, &settings, "x", None)).expect("计划"));
        assert_eq!(formal.len(), tested.len());
        for (formal, tested) in formal.iter().zip(&tested) {
            assert_eq!(
                (
                    &formal.host,
                    formal.port,
                    &formal.username,
                    formal.legacy_algorithms,
                    formal.auth.method,
                    formal.proxy.as_ref().map(|p| (&p.host, p.port)),
                ),
                (
                    &tested.host,
                    tested.port,
                    &tested.username,
                    tested.legacy_algorithms,
                    tested.auth.method,
                    tested.proxy.as_ref().map(|p| (&p.host, p.port)),
                )
            );
        }
    }

    // ---- 结果的显示等级（唯一的映射函数）----

    use super::{CheckItem, ResultIcon, Tone, present};
    use zssh_core::ssh::connection_test::{TestOutcome, cancelled, serial_failure, serial_success};

    fn outcome(
        kind: TestResultKind,
        connectivity: CheckState,
        identity: Option<IdentityState>,
        authentication: CheckState,
    ) -> TestOutcome {
        TestOutcome {
            kind,
            connectivity,
            identity,
            authentication,
            failing_stage: None,
            checked_hop: None,
            untested_hops: Vec::new(),
            reason: zssh_core::error::Reason::text("x"),
            detail: String::new(),
            elapsed_ms: 1,
        }
    }

    #[test]
    fn each_result_kind_has_its_own_tone_and_icon() {
        let complete = present(&outcome(
            TestResultKind::Complete,
            CheckState::Passed,
            Some(IdentityState::Matched),
            CheckState::Passed,
        ));
        assert_eq!(
            (complete.tone, complete.icon),
            (Tone::Success, ResultIcon::Success)
        );
        assert!(
            complete
                .lines
                .iter()
                .all(|line| line.state == CheckState::Passed)
        );
        assert_eq!(complete.lines.len(), 3);

        let partial = present(&outcome(
            TestResultKind::Partial,
            CheckState::Passed,
            Some(IdentityState::Unconfirmed),
            CheckState::Pending,
        ));
        // 部分完成：中性提示图标，不是失败叉，也不是绿色成功。
        assert_eq!(
            (partial.tone, partial.icon),
            (Tone::Neutral, ResultIcon::Info)
        );
        let login = partial
            .lines
            .iter()
            .find(|line| line.item == CheckItem::Login)
            .expect("有登录一项");
        assert_eq!(login.state, CheckState::Pending, "待验证的登录不画成通过");
        let connectivity = partial
            .lines
            .iter()
            .find(|line| line.item == CheckItem::Connectivity)
            .expect("有连通性一项");
        assert_eq!(connectivity.state, CheckState::Passed);
        assert!(
            partial
                .lines
                .iter()
                .all(|line| line.state != CheckState::Failed),
            "部分完成里不能有失败项"
        );

        let failed = present(&outcome(
            TestResultKind::Failed,
            CheckState::Passed,
            Some(IdentityState::Changed),
            CheckState::NotApplicable,
        ));
        assert_eq!(
            (failed.tone, failed.icon),
            (Tone::Danger, ResultIcon::Error)
        );
        assert!(
            failed
                .lines
                .iter()
                .any(|line| line.state == CheckState::Failed),
            "指纹变化画成失败项"
        );
        assert!(
            failed
                .lines
                .iter()
                .all(|line| line.item != CheckItem::Login),
            "认证之前就停了：没有登录这一项"
        );

        let cancelled = present(&cancelled());
        assert_eq!(
            (cancelled.tone, cancelled.icon),
            (Tone::Neutral, ResultIcon::Cancelled)
        );
        assert!(cancelled.lines.is_empty(), "取消什么都没证明");
    }

    #[test]
    fn every_identity_state_is_drawn_consistently() {
        use IdentityState::*;
        let state_of = |identity| {
            present(&outcome(
                TestResultKind::Partial,
                CheckState::Passed,
                Some(identity),
                CheckState::Pending,
            ))
            .lines
            .into_iter()
            .find(|line| line.item == CheckItem::Identity)
            .map(|line| line.state)
        };
        assert_eq!(state_of(Matched), Some(CheckState::Passed));
        assert_eq!(state_of(Unconfirmed), Some(CheckState::Pending));
        assert_eq!(state_of(OtherKeyTypes), Some(CheckState::Pending));
        assert_eq!(state_of(Changed), Some(CheckState::Failed));
        assert_eq!(state_of(Revoked), Some(CheckState::Failed));
        assert_eq!(state_of(Unreadable), Some(CheckState::Failed));
        // 每种身份状态都有自己的文字（中文、英文都不是空的，彼此不同）。
        let texts: Vec<String> = [
            Matched,
            Unconfirmed,
            OtherKeyTypes,
            Changed,
            Revoked,
            Unreadable,
        ]
        .into_iter()
        .map(|identity| {
            let shown = present(&outcome(
                TestResultKind::Failed,
                CheckState::Passed,
                Some(identity),
                CheckState::NotApplicable,
            ));
            let line = shown
                .lines
                .iter()
                .find(|line| line.item == CheckItem::Identity)
                .expect("身份一项");
            super::check_line_text(line)
        })
        .collect();
        for (i, a) in texts.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &texts[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn serial_and_telnet_results_have_no_identity_or_login_line() {
        let ok = present(&serial_success("COM3", 5));
        assert_eq!(ok.tone, Tone::Success);
        assert_eq!(ok.lines.len(), 1);
        assert_eq!(ok.lines[0].item, CheckItem::Connectivity);
        let bad = present(&serial_failure("COM3", "不存在", 5));
        assert_eq!(bad.tone, Tone::Danger);
        assert_eq!(bad.lines[0].state, CheckState::Failed);
    }

    // ---- 表单实例 / 第几次测试 / 参数修订：迟到的结果不回填 ----

    use super::{ParamsSignature, TestState};

    fn signature(version: u64) -> ParamsSignature {
        ParamsSignature {
            protocol: Protocol::Ssh,
            address: "192.0.2.10".to_owned(),
            port: "22".to_owned(),
            username: "ops".to_owned(),
            auth: AuthMethod::Password,
            identity: None,
            proxy_mode: ProxyMode::None,
            proxy: None,
            jump: None,
            legacy: false,
            password_version: version,
            serial: String::new(),
        }
    }

    fn done() -> TestOutcome {
        outcome(
            TestResultKind::Complete,
            CheckState::Passed,
            Some(IdentityState::Matched),
            CheckState::Passed,
        )
    }

    fn idle_task(rt: &tokio::runtime::Runtime) -> tokio::task::AbortHandle {
        rt.spawn(std::future::pending::<()>()).abort_handle()
    }

    #[test]
    fn a_result_for_the_current_test_is_accepted_once() {
        let rt = runtime();
        let mut state = TestState::new();
        let sig = signature(0);
        let ticket = state.begin(&sig, idle_task(&rt));
        assert!(state.is_running());
        assert!(state.finish(ticket, &sig, done()));
        assert!(!state.is_running());
        assert!(state.shown_result(&sig).is_some());
        // 同一张票据再回一次：已经用过了。
        assert!(!state.finish(ticket, &sig, cancelled()));
        assert_eq!(
            state.shown_result(&sig).map(|o| o.kind),
            Some(TestResultKind::Complete)
        );
    }

    #[test]
    fn a_late_result_never_fills_a_reopened_form() {
        let rt = runtime();
        // 关闭再打开：新的表单实例，新的状态。
        let mut old_form = TestState::new();
        let sig = signature(0);
        let stale = old_form.begin(&sig, idle_task(&rt));
        drop(old_form);
        let mut new_form = TestState::new();
        assert_ne!(stale.form_instance, new_form.form_instance());
        // 新表单还没测过：迟到的结果落不下来。
        assert!(!new_form.finish(stale, &sig, done()));
        assert!(new_form.shown_result(&sig).is_none());
        // 新表单自己测着的时候，旧结果也不能顶替它。
        let current = new_form.begin(&sig, idle_task(&rt));
        assert!(!new_form.finish(stale, &sig, done()));
        assert!(new_form.is_running(), "还在等自己的结果");
        assert!(new_form.finish(current, &sig, done()));
    }

    #[test]
    fn changing_a_connection_parameter_stops_the_test_and_drops_its_late_result() {
        let rt = runtime();
        let mut state = TestState::new();
        let before = signature(0);
        let ticket = state.begin(&before, idle_task(&rt));
        // 改了地址。
        let mut after = before.clone();
        after.address = "192.0.2.11".to_owned();
        assert!(state.params_changed(&after), "参数变了");
        assert!(!state.is_running(), "进行中的测试被终止");
        // 迟到的结果：对不上了。
        assert!(!state.finish(ticket, &after, done()));
        assert!(state.shown_result(&after).is_none());
        // 重新测试得到的新票据带着新的参数修订。
        let next = state.begin(&after, idle_task(&rt));
        assert_ne!(next.params_revision, ticket.params_revision);
        assert_ne!(next.attempt, ticket.attempt);
        assert!(state.finish(next, &after, done()));
    }

    #[test]
    fn a_parameter_change_the_state_has_not_seen_yet_still_rejects_the_result() {
        let rt = runtime();
        let mut state = TestState::new();
        let before = signature(0);
        let ticket = state.begin(&before, idle_task(&rt));
        // 没有人通知过 state，但结果回来时表单里的参数已经变了（密码改过一次）。
        let now = signature(1);
        assert!(!state.finish(ticket, &now, done()));
        assert!(state.shown_result(&now).is_none());
    }

    #[test]
    fn an_old_result_is_hidden_as_soon_as_the_form_differs() {
        let rt = runtime();
        let mut state = TestState::new();
        let before = signature(0);
        let ticket = state.begin(&before, idle_task(&rt));
        assert!(state.finish(ticket, &before, done()));
        assert!(state.shown_result(&before).is_some());
        // 参数变了但还没通知到 state：不显示旧结果。
        assert!(state.shown_result(&signature(1)).is_none());
        // 通知之后旧结果被清掉，改回原样也不会复活。
        assert!(state.params_changed(&signature(1)));
        assert!(state.shown_result(&before).is_none());
        state.params_changed(&before);
        assert!(state.shown_result(&before).is_none());
    }

    #[test]
    fn display_only_changes_are_not_part_of_the_signature() {
        // 签名里没有名称、分组、标签：改它们不会让结果失效（也就不必重拨）。
        let a = signature(0);
        let rendered = format!("{a:?}");
        for shown_only in [" name:", " group:", " tags:"] {
            assert!(!rendered.contains(shown_only), "{rendered}");
        }
        let rt = runtime();
        let mut state = TestState::new();
        let ticket = state.begin(&a, idle_task(&rt));
        assert!(state.finish(ticket, &a, done()));
        assert!(!state.params_changed(&a.clone()), "同样的参数不算变");
        assert!(state.shown_result(&a).is_some());
    }

    #[test]
    fn the_password_is_tracked_by_a_version_never_by_its_text() {
        let rendered = format!("{:?}", signature(7));
        assert!(rendered.contains("password_version: 7"), "{rendered}");
        assert!(!rendered.contains("password:"), "{rendered}");
        let rt = runtime();
        let mut state = TestState::new();
        let ticket = state.begin(&signature(0), idle_task(&rt));
        // 密码框内容变了一次：版本号变了，结果作废。
        assert!(!state.finish(ticket, &signature(1), done()));
    }

    #[test]
    fn a_second_test_supersedes_the_first_and_cancel_drops_the_late_result() {
        let rt = runtime();
        let mut state = TestState::new();
        let sig = signature(0);
        let first = state.begin(&sig, idle_task(&rt));
        let second = state.begin(&sig, idle_task(&rt));
        assert_ne!(first.attempt, second.attempt);
        assert!(
            !state.finish(first, &sig, done()),
            "第一次的结果不能顶替第二次"
        );
        assert!(state.cancel());
        assert_eq!(
            state.shown_result(&sig).map(|o| o.kind),
            Some(TestResultKind::Cancelled)
        );
        assert!(
            !state.finish(second, &sig, done()),
            "取消之后迟到的结果丢掉"
        );
        assert_eq!(
            state.shown_result(&sig).map(|o| o.kind),
            Some(TestResultKind::Cancelled)
        );
        assert!(!state.cancel(), "没有在跑就没什么可取消的");
    }
}
