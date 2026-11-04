//! 主机「属性」对话框背后的表单状态和动作（0.2.14 `HostDetailsPanel.tsx`，加上 V2-N4 的多协议；
//! V2-U7 起不再是右侧面板，界面搬到了居中对话框，见 `host_dialog`）。
//!
//! 表单状态放在 [`HostForm`] 里，换一台主机就整个换掉（同 0.2.14 的 `key` 重挂载）。切换协议只是换
//! 显示哪些字段，已填的值不清，保存时才按协议忽略不适用的（见 `host_form::plan_save`）。保存逻辑在
//! `host_form`、字段显隐和端口默认值规则在 `protocol_form`、类别和错误落点在 `host_props`（都是纯函数，
//! 有测试），这里只管状态和调用。

use std::collections::HashMap;

use ::gpui::{App, Entity};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::*;
use zssh_core::model::{
    AuthMethod, Host, IdentityKind, LocalEcho, Protocol, ProxyMode, SerialDataBits,
    SerialFlowControl, SerialNewline, SerialParity, SerialStopBits, TelnetNewline, TextEncoding,
};

use super::MainWindow;
use super::host_dialog::HostPropsDialog;
use super::host_form::{
    ConnMode, FormInput, SerialInput, TelnetInput, can_reveal, effective_routing,
    initial_conn_mode, initial_proxy_mode, jump_keeps_proxy, mismatch_note, password_identity,
    plan_save,
};
use super::host_props::{self, LayoutState, Row};
use super::host_test::TestState;
use super::modal::Modal;
use super::protocol_form::{self, FormField, PortChoice};

/// AI 记忆小节的状态。
pub struct MemoryForm {
    pub input: Entity<TextareaState>,
    /// 库里现在的正文（判断改过没有）。
    pub saved: String,
    pub updated_at: Option<i64>,
    pub error: Option<String>,
}

/// Telnet 高级设置的表单状态。
pub struct TelnetForm {
    pub newline: TelnetNewline,
    pub local_echo: LocalEcho,
    pub naws: bool,
    pub encoding: TextEncoding,
    pub auto_login: bool,
    pub raw_tcp: bool,
    pub terminal_type: Entity<InputState>,
    pub username_prompts: Entity<InputState>,
    pub password_prompts: Entity<InputState>,
}

/// 串口设置的表单状态。
pub struct SerialForm {
    pub device: Entity<InputState>,
    pub baud: Entity<InputState>,
    pub data_bits: SerialDataBits,
    pub parity: SerialParity,
    pub stop_bits: SerialStopBits,
    pub flow_control: SerialFlowControl,
    pub newline: SerialNewline,
    pub local_echo: bool,
    pub encoding: TextEncoding,
    /// 本机现在的串口（打开表单、点刷新时读一次）。
    pub ports: Vec<PortChoice>,
    /// 选了「手动输入…」：设备名用文字框填。
    pub manual: bool,
}

pub struct HostForm {
    pub host_id: Option<i64>,
    pub existing: Option<Host>,
    pub name: Entity<InputState>,
    pub address: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub password: Entity<InputState>,
    pub group: Option<i64>,
    pub auth: AuthMethod,
    /// 私钥 / ssh-agent 认证时选的凭据；密码认证不用它（密码归主机，见 `host_form`）。
    pub identity: Option<i64>,
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
    /// 「连接方式」选中的段（直接连接 / SSH 跳板 / 代理访问）；怎么映射到上面三个字段见 `host_form::ConnMode`。
    pub conn_mode: ConnMode,
    /// 旧数据里跳板机和代理都设了：「SSH 跳板」段下面也显示代理设置，保存时带着。
    pub jump_keeps_proxy: bool,
    pub legacy: bool,
    pub protocol: Protocol,
    pub x11: bool,
    pub telnet: TelnetForm,
    pub serial: SerialForm,
    /// 保存校验的错误，落在对应字段旁边。
    pub errors: HashMap<FormField, String>,
    /// 端口 / 用户名框里上一次**自动**填的值：框里还是它就说明用户没改过，换协议时跟着默认值走。
    pub port_auto: String,
    pub user_auto: String,
    /// 点小眼睛从保险库取出来的密码：（凭据 id，明文）。保存时和它一样就不算改密码。
    pub revealed: Option<(i64, String)>,
    pub password_shown: bool,
    pub mismatch_note: Option<String>,
    pub fingerprint: Option<String>,
    pub memory: Option<MemoryForm>,
    /// 「高级设置」「AI 记忆」折叠区展开了没有（默认都折叠）。
    pub advanced_open: bool,
    pub memory_open: bool,
    /// 表单滚动区的滚动句柄：校验出错时滚到出错的那一行。
    pub scroll: ::gpui::ScrollHandle,
    /// 「测试连接」的进行中 / 结果。
    pub test: TestState,
}

impl HostForm {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<MainWindow>,
        host: Option<&Host>,
        group: Option<i64>,
        memory: Option<(String, Option<i64>)>,
    ) -> Self {
        let text = |value: String, cx: &mut Context<MainWindow>, window: &mut Window| {
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let settings = host.map(|h| h.protocol.clone()).unwrap_or_default();
        let protocol = settings.protocol;
        let name = text(host.map(|h| h.name.clone()).unwrap_or_default(), cx, window);
        let address = text(host.map(|h| h.host.clone()).unwrap_or_default(), cx, window);
        // 串口主机库里的端口是占位的 0：框里放 SSH 默认值（它被藏着，也切不到别的协议）。
        let port_text = match host {
            Some(h) if h.port != 0 => h.port.to_string(),
            _ => "22".to_owned(),
        };
        let port = text(port_text, cx, window);
        // 新建时默认 root（和端口默认 22 一样，可以直接改）；编辑已有主机显示它保存的用户名。
        let username = text(
            host.map_or_else(|| "root".to_owned(), |h| h.username.clone()),
            cx,
            window,
        );
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let memory = memory.map(|(content, updated_at)| MemoryForm {
            input: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("还没有记忆。和 AI 聊这台主机时，它会把有用的事实记在这里。")
                    .auto_grow(4, 10)
                    .default_value(content.clone())
            }),
            saved: content,
            updated_at,
            error: None,
        });
        let telnet_options = settings.telnet.clone().unwrap_or_default();
        let serial_options = settings.serial.clone().unwrap_or_default();
        let telnet = TelnetForm {
            newline: telnet_options.newline,
            local_echo: telnet_options.local_echo,
            naws: telnet_options.naws,
            encoding: telnet_options.encoding,
            auto_login: telnet_options.auto_login.enabled,
            raw_tcp: telnet_options.raw_tcp,
            terminal_type: text(telnet_options.terminal_type.clone(), cx, window),
            username_prompts: text(
                protocol_form::format_patterns(&telnet_options.auto_login.username_patterns),
                cx,
                window,
            ),
            password_prompts: text(
                protocol_form::format_patterns(&telnet_options.auto_login.password_patterns),
                cx,
                window,
            ),
        };
        let device = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例如 COM3")
                .default_value(serial_options.device.clone())
        });
        let serial = SerialForm {
            device,
            baud: text(serial_options.baud_rate.to_string(), cx, window),
            data_bits: serial_options.data_bits,
            parity: serial_options.parity,
            stop_bits: serial_options.stop_bits,
            flow_control: serial_options.flow_control,
            newline: serial_options.newline,
            local_echo: serial_options.local_echo,
            encoding: serial_options.encoding,
            ports: if protocol == Protocol::Serial {
                protocol_form::list_ports()
            } else {
                Vec::new()
            },
            manual: false,
        };
        Self {
            host_id: host.map(|h| h.id),
            existing: host.cloned(),
            name,
            address,
            port,
            username,
            password,
            group: host.map_or(group, |h| h.group_id),
            auth: host.map_or(AuthMethod::Password, |h| h.auth_method),
            // 只有私钥 / agent 主机的关联才进「选凭据」下拉；密码主机的密码身份由保存规则管。
            identity: host
                .filter(|h| h.auth_method != AuthMethod::Password)
                .and_then(|h| h.identity_id),
            // 新建主机默认跟随全局默认代理（用户 11-03 定）；编辑已有主机显示它保存的值。
            proxy_mode: initial_proxy_mode(host),
            proxy: host.and_then(|h| h.proxy_id),
            jump: host.and_then(|h| h.jump_host_id),
            conn_mode: initial_conn_mode(host),
            jump_keeps_proxy: jump_keeps_proxy(host),
            legacy: host.is_some_and(|h| h.legacy_algorithms),
            protocol,
            x11: settings.x11_forwarding,
            telnet,
            serial,
            errors: HashMap::new(),
            port_auto: protocol_form::default_port_text(protocol)
                .unwrap_or_else(|| "22".to_owned()),
            user_auto: protocol_form::default_username(protocol).to_owned(),
            revealed: None,
            password_shown: false,
            mismatch_note: None,
            fingerprint: None,
            memory,
            advanced_open: false,
            memory_open: false,
            scroll: ::gpui::ScrollHandle::new(),
            test: TestState::default(),
        }
    }

    /// 真正生效的认证方式：SSH 按下拉，Telnet 恒为密码。
    pub fn effective_auth(&self) -> AuthMethod {
        if self.protocol == Protocol::Ssh {
            self.auth
        } else {
            AuthMethod::Password
        }
    }

    /// 这个表单现在有没有挂着对应字段的错误。
    pub fn error_of(&self, field: FormField) -> Option<&str> {
        self.errors.get(&field).map(String::as_str)
    }

    /// 有没有 AI 记忆这一类（只有已保存的、AI 能操作的 SSH 主机才有）。
    pub fn has_memory(&self) -> bool {
        self.memory.is_some() && protocol_form::visible(self.protocol).memory
    }

    /// 决定表单有哪些行的状态。
    pub fn layout_state(&self) -> LayoutState {
        LayoutState {
            protocol: self.protocol,
            has_memory: self.has_memory(),
            advanced_open: self.advanced_open,
            memory_open: self.memory_open,
            conn_mode: self.conn_mode,
            auto_login: self.telnet.auto_login,
        }
    }

    /// 表单里所有输入的当前值拼成一个字符串：和刚打开时的快照不一样就说明有没保存的修改。
    /// 只在内存里比较，不进日志（里面有密码框的内容）。
    pub fn snapshot(&self, cx: &App) -> String {
        let text = |state: &Entity<InputState>| state.read(cx).value().to_string();
        let telnet = &self.telnet;
        let serial = &self.serial;
        // 元组的 Debug 最多 12 项，所以分几组拼。
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            (
                text(&self.name),
                text(&self.address),
                text(&self.port),
                text(&self.username),
                text(&self.password),
                self.group,
                self.auth,
                self.identity,
            ),
            (
                self.proxy_mode,
                self.proxy,
                self.jump,
                self.legacy,
                self.protocol,
                self.x11,
                self.conn_mode,
            ),
            (
                telnet.newline,
                telnet.local_echo,
                telnet.naws,
                telnet.encoding,
                telnet.auto_login,
                telnet.raw_tcp,
                text(&telnet.terminal_type),
                text(&telnet.username_prompts),
                text(&telnet.password_prompts),
            ),
            (
                text(&serial.device),
                text(&serial.baud),
                serial.data_bits,
                serial.parity,
                serial.stop_bits,
                serial.flow_control,
                serial.newline,
                serial.local_echo,
                serial.encoding,
            ),
        )
    }

    /// AI 记忆框里有没有改了没存的内容（它不在 [`snapshot`] 里：AI 自己改了记忆时框会刷新，不算用户的修改；
    /// 点「确定」会把它一起存）。
    pub fn memory_dirty(&self, cx: &App) -> bool {
        self.host_id.is_some()
            && self.has_memory()
            && self
                .memory
                .as_ref()
                .is_some_and(|memory| memory.input.read(cx).value() != memory.saved)
    }
}

impl MainWindow {
    // ---- 打开 / 关闭 ----

    /// 打开主机「属性」对话框（所有编辑 / 新建主机的入口都走这里）；`host_id` 为空是新建（`group` 是默认分组）。
    /// 对话框盖在当前页面上：不切页面、不开侧面板，关掉以后还在原来的页面。已经开着同一台就只是回到它。
    pub(crate) fn open_host_props(
        &mut self,
        host_id: Option<i64>,
        group: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = host_id.is_some()
            && self
                .host_form
                .as_ref()
                .is_some_and(|form| form.host_id == host_id)
            && matches!(self.modal, Some(Modal::HostProps(_)));
        if same {
            cx.notify();
            return;
        }
        let host = host_id.and_then(|id| self.store.host(id).cloned());
        if host_id.is_some() && host.is_none() {
            return;
        }
        let memory = host
            .as_ref()
            .filter(|h| h.protocol.supports_ai_exec())
            .and_then(|h| self.read_memory(h.id));
        let mut form = HostForm::new(window, cx, host.as_ref(), group, memory);
        if let Some(host) = &host
            && host.protocol.protocol == Protocol::Ssh
        {
            form.fingerprint =
                crate::ssh_backend::known_hosts().saved_fingerprint(&host.host, host.port);
        }
        let initial = form.snapshot(cx);
        let focus = form.name.read(cx).focus_handle(cx);
        self.host_form = Some(form);
        self.modal = Some(Modal::HostProps(HostPropsDialog::new(initial)));
        window.focus(&focus, cx);
        cx.notify();
    }
    fn read_memory(&self, host_id: i64) -> Option<(String, Option<i64>)> {
        let db = self.store.db().ok()?;
        match db.host_memory_get(host_id) {
            Ok(Some(memory)) => Some((memory.content, Some(memory.updated_at))),
            Ok(None) => Some((String::new(), None)),
            Err(err) => {
                tracing::warn!(%err, host_id, "读取主机记忆失败");
                Some((String::new(), None))
            }
        }
    }

    // ---- 表单上的动作 ----

    pub(super) fn with_form(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut HostForm)) {
        if let Some(form) = &mut self.host_form {
            f(form);
        }
        cx.notify();
    }

    /// 直接关掉属性对话框并丢掉表单状态（保存成功、主机被删之后用；取消走 close_modal 的未保存检查）。
    pub(super) fn close_host_props_now(&mut self) {
        if matches!(self.modal, Some(Modal::HostProps(_))) {
            self.modal = None;
        }
        self.host_form = None;
    }

    /// 切换协议：只换显示哪些字段；端口和用户名框里如果还是上一次自动填的值就跟着新协议的默认值走，
    /// 用户改过的原样保留；不适用的字段已填的值不清（保存时才忽略）。
    pub(crate) fn host_set_protocol(
        &mut self,
        protocol: Protocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        if form.protocol == protocol {
            return;
        }
        let editing = form.existing.as_ref().map(|host| host.protocol.protocol);
        if !protocol_form::can_switch_to(editing, protocol) {
            self.notice(
                "串口主机和网络主机不能互相转换，要换类型请新建一台主机",
                window,
                cx,
            );
            return;
        }
        form.protocol = protocol;
        form.errors.clear();
        if let Some(default) = protocol_form::default_port_text(protocol) {
            let current = form.port.read(cx).value().to_string();
            if let Some(next) = protocol_form::follow_default(&current, &form.port_auto, &default) {
                form.port
                    .update(cx, |input, cx| input.set_value(next, window, cx));
            }
            form.port_auto = default;
        }
        if protocol != Protocol::Serial {
            let default = protocol_form::default_username(protocol);
            let current = form.username.read(cx).value().to_string();
            if let Some(next) = protocol_form::follow_default(&current, &form.user_auto, default) {
                form.username
                    .update(cx, |input, cx| input.set_value(next, window, cx));
            }
            form.user_auto = default.to_owned();
        }
        if protocol == Protocol::Serial {
            form.serial.ports = protocol_form::list_ports();
        }
        cx.notify();
    }

    /// 串口设备下拉旁的刷新按钮：重新列一遍本机串口。
    pub(crate) fn host_refresh_ports(&mut self, cx: &mut Context<Self>) {
        self.with_form(cx, |form| {
            form.serial.ports = protocol_form::list_ports();
        });
    }

    /// 选了列表里的一个串口：把设备名写进框里。
    pub(crate) fn host_pick_port(
        &mut self,
        device: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        form.serial.manual = false;
        form.errors.remove(&FormField::Device);
        let input = form.serial.device.clone();
        let device = device.to_owned();
        input.update(cx, |input, cx| input.set_value(device, window, cx));
        cx.notify();
    }

    /// 认证方式或凭据变了：取回来的明文作废，密码框回到隐藏，不配套的凭据取消选择并说明原因。
    fn after_auth_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        let identity = form.identity.and_then(|id| {
            self.store
                .identities()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        // 密码认证不选凭据（选过的密钥留着，切回私钥时还在）；私钥 / agent 之间切换时类型对不上就取消选择。
        if form.auth != AuthMethod::Password
            && let Some(item) = &identity
            && let Some(note) = mismatch_note(item, form.auth)
        {
            form.mismatch_note = Some(note);
            form.identity = None;
        }
        // 取回来的明文只在密码认证下有意义。
        let still_valid = form.revealed.is_some() && form.effective_auth() == AuthMethod::Password;
        if !still_valid && let Some((_, value)) = form.revealed.take() {
            form.password_shown = false;
            let password = form.password.clone();
            let typed = password.read(cx).value().to_string();
            password.update(cx, |input, cx| {
                input.set_masked(true, window, cx);
                if fetched_value_untouched(&value, &typed) {
                    input.set_value("", window, cx);
                }
            });
        }
        cx.notify();
    }

    pub(crate) fn host_set_auth(
        &mut self,
        auth: AuthMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_form(cx, |form| form.auth = auth);
        self.after_auth_changed(window, cx);
    }

    pub(crate) fn host_set_identity(
        &mut self,
        identity: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_form(cx, |form| {
            form.identity = identity;
            form.mismatch_note = None;
        });
        self.after_auth_changed(window, cx);
    }

    /// 密码框右边的小眼睛：框里还空着、选了存过密码的凭据时，先从保险库取出来。
    pub(crate) fn host_toggle_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        let password = form.password.clone();
        if form.password_shown {
            form.password_shown = false;
            password.update(cx, |input, cx| input.set_masked(true, window, cx));
            cx.notify();
            return;
        }
        let empty = password.read(cx).value().is_empty();
        // 小眼睛取的是这台主机自己的密码身份（表单里没有选它的地方）。
        let identity = password_identity(form.existing.as_ref(), self.store.identities()).cloned();
        if empty
            && can_reveal(identity.as_ref(), form.effective_auth())
            && let Some(identity) = identity
        {
            // 要从保险库取明文了：启用了锁屏的话先验证锁屏密码（V2-U35）。只显示用户自己刚敲的字不用问。
            if !self.reveal_allowed(super::reveal_ui::RevealAction::HostDialogEye, window, cx) {
                return;
            }
            match self.store.db().and_then(|db| {
                db.identity_saved_password(identity.id)
                    .map_err(|err| err.to_string())
            }) {
                Ok(value) => {
                    let text = value.as_str().to_owned();
                    password.update(cx, |input, cx| input.set_value(text.clone(), window, cx));
                    if let Some(form) = &mut self.host_form {
                        form.revealed = Some((identity.id, text));
                    }
                }
                Err(err) => {
                    self.notice_error(&err, window, cx);
                    return;
                }
            }
        }
        if let Some(form) = &mut self.host_form {
            form.password_shown = true;
        }
        password.update(cx, |input, cx| input.set_masked(false, window, cx));
        cx.notify();
    }

    /// 锁屏时把主机属性对话框里还显示着的密码重新遮住（V2-C9）。只切换显示方式：用户自己在框里敲的 /
    /// 改过的字原样保留；框里是点小眼睛从保险库取出来、没动过的明文，就把它从框里和内存里丢掉
    /// （框回到"空 = 沿用已保存的密码"，和取出来之前一样，下次再看要重新验证）。
    pub(super) fn remask_host_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        // 每个锁屏帧都会进来：没有要遮的就别碰输入框。
        if !form.password_shown && form.revealed.is_none() {
            return;
        }
        let revealed = form.revealed.take();
        form.password_shown = false;
        let password = form.password.clone();
        let typed = password.read(cx).value().to_string();
        let drop_fetched =
            revealed.is_some_and(|(_, value)| fetched_value_untouched(&value, &typed));
        password.update(cx, |input, cx| {
            input.set_masked(true, window, cx);
            if drop_fetched {
                input.set_value("", window, cx);
            }
        });
        cx.notify();
    }

    pub(super) fn host_forget_fingerprint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.host_form.as_ref().and_then(|f| f.existing.clone()) else {
            return;
        };
        match crate::ssh_backend::known_hosts().forget(&host.host, host.port) {
            Ok(removed) => {
                self.with_form(cx, |form| form.fingerprint = None);
                let message = if removed > 0 {
                    format!("已清除 {removed} 条指纹记录，下次连接会重新询问")
                } else {
                    "本来就没有保存过这台主机的指纹".to_owned()
                };
                self.notice(&message, window, cx);
            }
            Err(err) => self.notice_error(&err.to_string(), window, cx),
        }
    }

    // ---- 保存 / 删除 ----

    /// 保存 / 测试校验没过：错误挂在对应字段下面，自动滚到出错的那一行并把键盘焦点交给它；
    /// 出错的字段在折叠的「高级设置」里就先展开它；找不到对应字段的显示在对话框底部。
    pub(super) fn show_form_error(
        &mut self,
        field: FormField,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = message
            .strip_prefix("输入不合法：")
            .unwrap_or(message)
            .to_owned();
        let row = self
            .host_form
            .as_ref()
            .and_then(|form| host_props::row_of_field(field, form.protocol));
        if let Some(form) = &mut self.host_form {
            form.errors.clear();
            form.errors.insert(field, message);
            // 结果栏里上一次的测试结果和新的错误无关，先清掉免得并排显示。
            form.test.result = None;
            if row.is_some_and(host_props::is_advanced) {
                form.advanced_open = true;
            }
        }
        if let Some(row) = row {
            self.host_scroll_to(row, window, cx);
        }
        cx.notify();
    }

    /// 滚到这一行并把焦点交给它里面的第一个输入框（没有输入框的行不动焦点，免得停在已不可见的框上）。
    pub(crate) fn host_scroll_to(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.host_form else {
            return;
        };
        let state = form.layout_state();
        if let Some(index) = host_props::index_of(state, row) {
            form.scroll.scroll_to_top_of_item(index);
        }
        let focus = match row {
            Row::Name => Some(form.name.read(cx).focus_handle(cx)),
            Row::Endpoint if form.protocol == Protocol::Serial => {
                Some(form.serial.device.read(cx).focus_handle(cx))
            }
            Row::Endpoint => Some(form.address.read(cx).focus_handle(cx)),
            Row::Credentials => Some(form.username.read(cx).focus_handle(cx)),
            Row::Advanced(host_props::AdvRow::TelnetTerm) => {
                Some(form.telnet.terminal_type.read(cx).focus_handle(cx))
            }
            Row::MemoryBody => form
                .memory
                .as_ref()
                .map(|memory| memory.input.read(cx).focus_handle(cx)),
            _ => None,
        };
        match focus {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&self.focus, cx),
        }
        cx.notify();
    }

    /// 键盘焦点现在在「AI 记忆」那个多行框里吗（是的话 Enter 是换行，Ctrl+Enter 才是确定）。
    pub(super) fn host_memory_focused(&self, window: &Window, cx: &App) -> bool {
        matches!(self.modal, Some(Modal::HostProps(_)))
            && self
                .host_form
                .as_ref()
                .and_then(|form| form.memory.as_ref())
                .is_some_and(|memory| memory.input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// 「高级设置」标题：展开 / 折叠。
    pub(crate) fn host_toggle_advanced(&mut self, cx: &mut Context<Self>) {
        self.with_form(cx, |form| form.advanced_open = !form.advanced_open);
    }

    /// 「AI 记忆」标题：展开 / 折叠。
    pub(crate) fn host_toggle_memory(&mut self, cx: &mut Context<Self>) {
        self.with_form(cx, |form| form.memory_open = !form.memory_open);
    }

    /// 「连接方式」分段：换一段。选「代理访问」时原来是「不用代理」就落成「跟随默认代理」。
    pub(crate) fn host_set_conn_mode(&mut self, mode: ConnMode, cx: &mut Context<Self>) {
        self.with_form(cx, |form| {
            form.conn_mode = mode;
            if mode == ConnMode::Proxy && form.proxy_mode == ProxyMode::None {
                form.proxy_mode = ProxyMode::Default;
            }
            form.errors.remove(&FormField::Proxy);
            form.errors.remove(&FormField::Jump);
        });
    }

    /// 「确定」：保存并关掉对话框。
    pub(crate) fn host_form_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.host_form_commit(false, window, cx);
    }

    /// 「确定」（`connect` 为假）/「连接」（为真，保存成功之后立刻用这台主机连接）：
    /// 校验不过就停在对话框里，不保存也不连接。
    pub(crate) fn host_form_commit(
        &mut self,
        connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // AI 记忆框里改了没存：先存（它有自己的存储和校验，出错就停在「AI 记忆」类别里）。
        if self
            .host_form
            .as_ref()
            .is_some_and(|form| form.memory_dirty(cx))
        {
            self.host_memory_save(window, cx, false);
            let failed = self
                .host_form
                .as_ref()
                .and_then(|form| form.memory.as_ref())
                .is_some_and(|memory| memory.error.is_some());
            if failed {
                self.with_form(cx, |form| form.memory_open = true);
                self.host_scroll_to(Row::MemoryBody, window, cx);
                return;
            }
        }
        self.host_form_save_now(connect, window, cx);
    }

    fn host_form_save_now(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.host_form else {
            return;
        };
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
        // 「连接方式」三段 → 要保存的代理 / 跳板机三个字段（规则见 `host_form::effective_routing`）。
        let routing = effective_routing(
            form.conn_mode,
            form.proxy_mode,
            form.proxy,
            form.jump,
            form.jump_keeps_proxy,
        );
        if form.protocol != Protocol::Serial
            && form.conn_mode == ConnMode::Jump
            && routing.jump.is_none()
        {
            self.show_form_error(FormField::Jump, "请选择跳板机", window, cx);
            return;
        }
        let input = FormInput {
            name: &name,
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
            telnet: TelnetInput {
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
            serial: SerialInput {
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
        // 已有主机：读库里现在的样子，表单里没有的字段（标签、排序）带原值（F3 X1）。
        let existing = match form.host_id {
            Some(id) => match self.store.host_fresh(id) {
                Ok(host) => Some(host),
                Err(err) => {
                    self.notice_error(&format!("读取主机失败，没有保存：{err}"), window, cx);
                    return;
                }
            },
            None => None,
        };
        let revealed = form
            .revealed
            .as_ref()
            .map(|(id, value)| (*id, value.as_str()));
        let plan = match plan_save(
            &input,
            existing.as_ref(),
            self.store.identities(),
            self.store.hosts(),
            revealed,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.show_form_error(error.field, &error.message, window, cx);
                return;
            }
        };
        // 先动这台主机的密码身份，再存主机，最后释放不再用的独占密码（见 `host_form::apply_save`）。
        match self.store.save_host_plan(plan) {
            Ok(saved) => {
                self.close_host_props_now();
                self.focus_content(window, cx);
                self.tree.selected_host = Some(saved.id);
                self.notice_success(&format!("已保存主机「{}」", saved.name), window, cx);
                cx.notify();
                // 「连接」：保存成功之后立刻连（走和双击一样的入口，X11 提示、密码询问都照常）。
                if connect {
                    self.open_host(saved.id, window, cx);
                }
            }
            Err(err) => {
                let field = protocol_form::locate_error(&err);
                if field == FormField::General {
                    self.notice_error(&err, window, cx);
                }
                self.show_form_error(field, &err, window, cx);
            }
        }
    }

    pub(crate) fn do_delete_host(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .store
            .host(id)
            .map(|h| h.name.clone())
            .unwrap_or_default();
        match self.store.delete_host_with_passwords(id) {
            Ok(deleted) => {
                // 它的转发规则跟着删了：正开着的转发停掉。
                self.reconcile_forwards();
                if self
                    .host_form
                    .as_ref()
                    .is_some_and(|f| f.host_id == Some(id))
                {
                    self.close_host_props_now();
                }
                if self.tree.selected_host == Some(id) {
                    self.tree.selected_host = None;
                }
                if self.info_host == Some(id) {
                    self.info_host = None;
                }
                let mut text = format!("已删除「{name}」");
                if !deleted.passwords.is_empty() {
                    let names: Vec<String> = deleted
                        .passwords
                        .iter()
                        .map(|name| format!("「{name}」"))
                        .collect();
                    text.push_str(&format!("，并删除了它的密码{}", names.join("、")));
                }
                self.notice_success(&text, window, cx);
                if let Some(problem) = deleted.problems.first() {
                    self.notice_error(problem, window, cx);
                }
            }
            Err(err) => self.notice_error(&format!("删除失败：{err}"), window, cx),
        }
        cx.notify();
    }

    // ---- AI 记忆 ----

    pub(super) fn host_memory_save(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        clear: bool,
    ) {
        let Some(form) = &self.host_form else {
            return;
        };
        let (Some(host_id), Some(memory)) = (form.host_id, &form.memory) else {
            return;
        };
        let text = if clear {
            String::new()
        } else {
            memory.input.read(cx).value().to_string()
        };
        let input = memory.input.clone();
        let action = match crate::hosts_demo::memory_action(&text) {
            Ok(action) => action,
            Err(err) => {
                self.with_form(cx, |form| {
                    if let Some(memory) = &mut form.memory {
                        memory.error = Some(err);
                    }
                });
                return;
            }
        };
        let content = match &action {
            crate::hosts_demo::MemoryAction::Save(content) => content.clone(),
            crate::hosts_demo::MemoryAction::Clear => String::new(),
        };
        let written = self.store.db().and_then(|db| {
            db.host_memory_set(host_id, &content)
                .map_err(|e| e.to_string())
        });
        if let Err(err) = written {
            self.with_form(cx, |form| {
                if let Some(memory) = &mut form.memory {
                    memory.error = Some(err);
                }
            });
            return;
        }
        let memory_now = self.read_memory(host_id);
        // 文本框换成清理后的正文：藏掉的密码不会再留在界面上，也避免下次照原样写回去。
        input.update(cx, |state, cx| state.set_value(content.clone(), window, cx));
        self.with_form(cx, |form| {
            if let Some(memory) = &mut form.memory {
                memory.saved = content;
                memory.updated_at = memory_now.and_then(|(_, at)| at);
                memory.error = None;
            }
        });
    }

    /// AI 在对话里改了这些主机的记忆（0.2.14 的 `host-memory-changed`）：主机属性里开着的记忆框跟着刷新。
    /// 用户正在改的内容（框里和已保存的不一样）不覆盖，只提示「AI 刚更新了记忆」。
    pub(super) fn refresh_memory_from_ai(
        &mut self,
        changed: &[i64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.host_form else {
            return;
        };
        let (Some(host_id), Some(memory)) = (form.host_id, &form.memory) else {
            return;
        };
        if !changed.contains(&host_id) {
            return;
        }
        let dirty = memory.input.read(cx).value() != memory.saved;
        if dirty {
            self.with_form(cx, |form| {
                if let Some(memory) = &mut form.memory {
                    memory.error = Some(
                        "AI 刚刚更新了这台主机的记忆；你正在编辑的内容没有被覆盖，保存会替换 AI 写的。"
                            .to_owned(),
                    );
                }
            });
            return;
        }
        let input = memory.input.clone();
        let now = self.read_memory(host_id);
        let content = now
            .as_ref()
            .map(|(content, _)| content.clone())
            .unwrap_or_default();
        input.update(cx, |state, cx| state.set_value(content.clone(), window, cx));
        self.with_form(cx, |form| {
            if let Some(memory) = &mut form.memory {
                memory.saved = content;
                memory.updated_at = now.and_then(|(_, at)| at);
                memory.error = None;
            }
        });
    }

    pub(crate) fn host_set_baud(&mut self, rate: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        form.errors.remove(&FormField::Baud);
        let input = form.serial.baud.clone();
        input.update(cx, |input, cx| {
            input.set_value(rate.to_string(), window, cx)
        });
        cx.notify();
    }

    /// 对话框里「新建代理…」保存成功：主机表单自动选中它（不用再去下拉框里找），没保存的编辑原样留着。
    pub(crate) fn host_form_adopt_proxy(&mut self, proxy_id: i64) {
        if let Some(form) = &mut self.host_form {
            form.proxy_mode = ProxyMode::Custom;
            form.proxy = Some(proxy_id);
        }
    }
    /// 主机属性的某个字段在被调试脚本 / 测试读：名称输入框当前的文字。
    #[allow(dead_code)]
    pub(crate) fn host_form_name(&self, cx: &App) -> Option<String> {
        self.host_form
            .as_ref()
            .map(|form| form.name.read(cx).value().to_string())
    }

    pub(crate) fn host_kind_is_key(kind: IdentityKind) -> bool {
        matches!(kind, IdentityKind::KeyFile | IdentityKind::KeyInline)
    }
}

/// 框里的字还是不是点小眼睛取出来的那份、没被改过。是的话锁屏遮住时可以整个丢掉；
/// 不是的话那是用户自己敲的 / 改过的，不能丢。
fn fetched_value_untouched(fetched: &str, typed: &str) -> bool {
    fetched == typed
}

#[cfg(test)]
mod remask_tests {
    use super::fetched_value_untouched;

    #[test]
    fn only_an_unedited_fetched_password_may_be_dropped() {
        assert!(fetched_value_untouched("saved-pw", "saved-pw"));
        // 取出来之后用户改过一个字：这是未保存的输入，锁屏只切换显示方式，不能丢
        assert!(!fetched_value_untouched("saved-pw", "saved-pw2"));
        assert!(!fetched_value_untouched("saved-pw", ""));
        assert!(!fetched_value_untouched("saved-pw", "typed-by-user"));
    }
}
