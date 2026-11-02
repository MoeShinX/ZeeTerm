//! 右侧的主机详情 / 新建面板（0.2.14 `HostDetailsPanel.tsx`，加上 V2-N4 的多协议）：
//! 名称、协议（SSH / Telnet / 串口分段按钮）、各协议自己的字段、分组、可折叠的「高级设置」
//! 和 AI 记忆。
//!
//! 表单状态放在 [`HostForm`] 里，换一台主机就整个换掉（同 0.2.14 的 `key` 重挂载）；开着凭据 /
//! 代理面板时它只是被藏在下面，里面没保存的编辑还在。切换协议只是换显示哪些字段，已填的值不清，
//! 保存时才按协议忽略不适用的（见 `host_form::plan_save`）。保存逻辑在 `host_form`、字段显隐和
//! 端口默认值规则在 `protocol_form`（都是纯函数，有测试），这里只管界面和调用。

use std::collections::HashMap;

use ::gpui::{App, Entity, SharedString};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::host_memory::MAX_MEMORY_CHARS;
use zssh_core::model::{
    AuthMethod, Host, IdentityKind, LocalEcho, Protocol, ProxyMode, SerialDataBits,
    SerialFlowControl, SerialNewline, SerialParity, SerialStopBits, TelnetNewline, TextEncoding,
};

use super::MainWindow;
use super::host_form::{
    FormInput, IdentityAction, SerialInput, TelnetInput, auth_label, can_reveal,
    identity_fits_auth, mismatch_note, password_hint, plan_save,
};
use super::page_logic::{flatten_groups, ungrouped_option_label};
use super::protocol_form::{
    self, COMMON_BAUD_RATES, DATA_BITS, ENCODINGS, FLOW_CONTROLS, FormField, LOCAL_ECHOES,
    PARITIES, PROTOCOLS, PortChoice, SERIAL_NEWLINES, STOP_BITS, TELNET_NEWLINES,
};
use super::side_panel::{PROXIES, PanelContent};
use super::widgets::{
    ButtonLook, Choice, act, collapse_header, error_note, field, field_with_error, note, pick,
    section, segmented, select, switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

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
    pub identity: Option<i64>,
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
    pub remember: bool,
    pub legacy: bool,
    pub protocol: Protocol,
    pub x11: bool,
    pub telnet: TelnetForm,
    pub serial: SerialForm,
    /// 「高级设置」折叠区是否展开。
    pub advanced_open: bool,
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
            identity: host.and_then(|h| h.identity_id),
            // 新建主机默认不使用代理（不跟随全局默认代理）；编辑已有主机显示它保存的值。
            proxy_mode: host.map_or(ProxyMode::None, |h| h.proxy_mode),
            proxy: host.and_then(|h| h.proxy_id),
            jump: host.and_then(|h| h.jump_host_id),
            remember: true,
            legacy: host.is_some_and(|h| h.legacy_algorithms),
            protocol,
            x11: settings.x11_forwarding,
            telnet,
            serial,
            advanced_open: false,
            errors: HashMap::new(),
            port_auto: protocol_form::default_port_text(protocol)
                .unwrap_or_else(|| "22".to_owned()),
            user_auto: protocol_form::default_username(protocol).to_owned(),
            revealed: None,
            password_shown: false,
            mismatch_note: None,
            fingerprint: None,
            memory,
        }
    }

    /// 这个表单现在有没有挂着对应字段的错误。
    pub fn error_of(&self, field: FormField) -> Option<&str> {
        self.errors.get(&field).map(String::as_str)
    }
}

fn proxy_mode_label(mode: ProxyMode) -> &'static str {
    match mode {
        ProxyMode::Default => "跟随默认代理",
        ProxyMode::None => "不用代理",
        ProxyMode::Custom => "指定代理",
    }
}

/// 高级设置里「标题 + 说明 + 开关」一行的生成函数（id、标题、说明、当前值、改表单的函数）。
type ToggleRow<'a> = dyn Fn(
        &'static str,
        &'static str,
        Option<&'static str>,
        bool,
        fn(&mut HostForm, bool),
    ) -> ::gpui::AnyElement
    + 'a;

/// 一个"枚举值下拉"：按钮上写当前值，菜单里列全部，选中后交给 `set` 改表单。
fn enum_select<T: Copy + PartialEq + 'static>(
    id: &'static str,
    entity: &Entity<MainWindow>,
    current: T,
    all: &'static [T],
    label: fn(T) -> &'static str,
    set: fn(&mut HostForm, T),
) -> impl IntoElement {
    let choices: Vec<Choice> = all
        .iter()
        .map(|value| Choice::new(label(*value), *value == current))
        .collect();
    select(
        id,
        label(current),
        choices,
        pick(entity, move |this, index, _window, cx| {
            if let Some(value) = all.get(index).copied() {
                this.with_form(cx, |form| set(form, value));
            }
        }),
    )
}

impl MainWindow {
    // ---- 打开 / 关闭 ----

    /// 打开主机详情；`host_id` 为空是新建（`group` 是默认分组）。已经开着同一台就只是显示它。
    pub(crate) fn open_host_details(
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
                .is_some_and(|form| form.host_id == host_id);
        self.proxy_ui.from_host_form = false;
        if !same {
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
            let focus = form.name.read(cx).focus_handle(cx);
            self.host_form = Some(form);
            window.focus(&focus, cx);
        }
        self.panels.open_host_details();
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

    fn with_form(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut HostForm)) {
        if let Some(form) = &mut self.host_form {
            f(form);
        }
        cx.notify();
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
        // Telnet 只能选密码凭据：之前选的私钥 / agent 凭据取消选择并说明。
        if protocol == Protocol::Telnet {
            form.mismatch_note = None;
            let wrong = form.identity.and_then(|id| {
                self.store
                    .identities()
                    .iter()
                    .find(|item| item.id == id && item.kind != IdentityKind::Password)
                    .map(|item| item.name.clone())
            });
            if let Some(name) = wrong {
                form.identity = None;
                form.mismatch_note = Some(format!(
                    "凭据「{name}」不是密码凭据，Telnet 主机只能选密码凭据，已取消选择"
                ));
            }
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
        if let Some(item) = &identity
            && let Some(note) = mismatch_note(item, form.auth)
        {
            form.mismatch_note = Some(note);
            form.identity = None;
        }
        let still_valid = matches!(
            (&form.revealed, form.identity),
            (Some((id, _)), Some(current)) if *id == current
        ) && form.auth == AuthMethod::Password;
        if !still_valid && let Some((_, value)) = form.revealed.take() {
            form.password_shown = false;
            let password = form.password.clone();
            let typed = password.read(cx).value().to_string();
            password.update(cx, |input, cx| {
                input.set_masked(true, window, cx);
                if typed == value {
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
        let identity = form.identity.and_then(|id| {
            self.store
                .identities()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        if empty
            && can_reveal(identity.as_ref(), form.auth)
            && let Some(identity) = identity
        {
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

    fn host_forget_fingerprint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// 保存校验没过：错误挂在对应字段旁边（在折叠的「高级设置」里的字段，顺手展开）；
    /// 找不到对应字段的显示在表单最下面。
    fn show_form_error(&mut self, field: FormField, message: &str, cx: &mut Context<Self>) {
        let message = message
            .strip_prefix("输入不合法：")
            .unwrap_or(message)
            .to_owned();
        if let Some(form) = &mut self.host_form {
            form.errors.clear();
            if matches!(
                field,
                FormField::X11 | FormField::TerminalType | FormField::AutoLogin
            ) {
                form.advanced_open = true;
            }
            form.errors.insert(field, message);
        }
        cx.notify();
    }

    pub(crate) fn host_form_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let input = FormInput {
            name: &name,
            address: &address,
            port: &port,
            username: &username,
            password: &password,
            group: form.group,
            auth: form.auth,
            identity: form.identity,
            proxy_mode: form.proxy_mode,
            proxy: form.proxy,
            jump: form.jump,
            remember: form.remember,
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
        let plan = match plan_save(&input, existing.as_ref(), self.store.identities(), revealed) {
            Ok(plan) => plan,
            Err(error) => {
                self.show_form_error(error.field, &error.message, cx);
                return;
            }
        };
        let mut draft = plan.draft;
        // 先动凭据库（填了新密码时），再存主机。
        let identity_result: Result<(), String> = match plan.identity {
            IdentityAction::None => Ok(()),
            IdentityAction::Overwrite { id, draft: lite } => self
                .store
                .db()
                .and_then(|db| {
                    db.identity_save(&lite.into_draft(Some(id)))
                        .map_err(|e| e.to_string())
                })
                .map(|_| ()),
            IdentityAction::Create { draft: lite } => self
                .store
                .db()
                .and_then(|db| {
                    db.identity_save(&lite.into_draft(None))
                        .map_err(|e| e.to_string())
                })
                .map(|created| draft.identity_id = Some(created.id)),
        };
        if let Err(err) = identity_result {
            self.notice_error(&err, window, cx);
            return;
        }
        match self.store.save_host(&draft) {
            Ok(saved) => {
                self.panels.close_host_details();
                self.host_form = None;
                self.focus_content(window, cx);
                self.tree.selected_host = Some(saved.id);
                self.notice_success(&format!("已保存主机「{}」", saved.name), window, cx);
                cx.notify();
            }
            Err(err) => {
                let field = protocol_form::locate_error(&err);
                if field == FormField::General {
                    self.notice_error(&err, window, cx);
                }
                self.show_form_error(field, &err, cx);
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
                    self.panels.close_host_details();
                    self.host_form = None;
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

    /// AI 在对话里改了这些主机的记忆（0.2.14 的 `host-memory-changed`）：主机详情里开着的记忆框跟着刷新。
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

    // ---- 画 ----

    pub(super) fn host_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        let Some(form) = &self.host_form else {
            return PanelContent {
                title: "主机详情".into(),
                subtitle: None,
                back: None,
                actions: Vec::new(),
                body: div().into_any_element(),
                footer: None,
            };
        };
        let existing = form.existing.as_ref();
        let title: SharedString = if existing.is_some() {
            "主机详情"
        } else {
            "新建主机"
        }
        .into();
        let subtitle: SharedString = match existing {
            Some(host) => protocol_form::host_detail(host).into(),
            None => "填好之后保存到主机库".into(),
        };
        let vis = protocol_form::visible(form.protocol);

        let body = v_flex()
            .w_full()
            .gap_4()
            .child(self.form_basic(form, &entity, tokens))
            .when(vis.serial, |d| {
                d.child(self.form_serial(form, &entity, tokens, cx))
            })
            .when(vis.address, |d| {
                d.child(self.form_connection(form, &entity, tokens, cx))
            })
            .when(vis.identity, |d| {
                d.child(self.form_auth(form, &entity, tokens))
            })
            .when(vis.proxy, |d| {
                d.child(self.form_network(form, &entity, tokens))
            })
            .child(self.form_group(form, &entity, tokens))
            .child(self.form_advanced(form, &entity, tokens, cx))
            .children(
                vis.memory
                    .then(|| self.form_memory(form, &entity, tokens, cx))
                    .flatten(),
            )
            .children(
                form.error_of(FormField::General)
                    .map(|text| error_note(text.to_owned(), tokens.danger)),
            )
            .into_any_element();

        let host_id = form.host_id;
        let footer = h_flex()
            .gap_2()
            .items_center()
            .when_some(host_id, |d, id| {
                d.child(div().mr_auto().child(text_button(
                    "host-delete",
                    "删除",
                    ButtonLook::Ghost,
                    act(&entity, move |this, _window, cx| {
                        this.ask_delete_host(id, cx)
                    }),
                )))
            })
            .when(host_id.is_none(), |d| d.child(div().mr_auto()))
            .child(text_button(
                "host-cancel",
                "取消",
                ButtonLook::Outline,
                act(&entity, |this, window, cx| {
                    this.cancel_host_form(window, cx)
                }),
            ))
            .child(text_button(
                "host-save",
                "保存",
                ButtonLook::Primary,
                act(&entity, |this, window, cx| this.host_form_save(window, cx)),
            ))
            .into_any_element();

        PanelContent {
            title,
            subtitle: Some(subtitle),
            back: None,
            actions: Vec::new(),
            body,
            footer: Some(footer),
        }
    }

    /// 名称 + 协议分段按钮。
    fn form_basic(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> ::gpui::AnyElement {
        let editing = form.existing.as_ref().map(|host| host.protocol.protocol);
        let options: Vec<(String, bool, Option<&'static str>)> = PROTOCOLS
            .iter()
            .map(|protocol| {
                (
                    protocol.label().to_owned(),
                    *protocol == form.protocol,
                    (!protocol_form::can_switch_to(editing, *protocol))
                        .then_some("串口主机和网络主机不能互相转换，要换类型请新建一台主机"),
                )
            })
            .collect();
        section(
            "基本",
            vec![
                field_with_error(
                    "名称",
                    None,
                    Input::new(&form.name),
                    form.error_of(FormField::Name),
                    tokens,
                )
                .into_any_element(),
                field(
                    "协议",
                    None,
                    segmented(
                        "host-protocol",
                        options,
                        pick(entity, move |this, index, window, cx| {
                            if let Some(protocol) = PROTOCOLS.get(index).copied() {
                                this.host_set_protocol(protocol, window, cx);
                            }
                        }),
                    ),
                    tokens,
                )
                .into_any_element(),
            ],
            tokens,
        )
        .into_any_element()
    }

    /// 分组下拉。
    fn form_group(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> ::gpui::AnyElement {
        let flat = self.store.flat();
        let groups = self.store.groups();
        let ordered = flatten_groups(groups);
        let mut group_choices = vec![Choice::new(
            ungrouped_option_label(flat),
            form.group.is_none(),
        )];
        let mut group_targets: Vec<Option<i64>> = vec![None];
        for (group, depth) in &ordered {
            group_choices
                .push(Choice::new(group.name.clone(), form.group == Some(group.id)).depth(*depth));
            group_targets.push(Some(group.id));
        }
        let group_label = form
            .group
            .and_then(|id| groups.iter().find(|g| g.id == id))
            .map_or_else(
                || ungrouped_option_label(flat).to_owned(),
                |g| g.name.clone(),
            );
        section(
            "分组",
            vec![
                select(
                    "host-group",
                    group_label,
                    group_choices,
                    pick(entity, move |this, index, _window, cx| {
                        if let Some(target) = group_targets.get(index).copied() {
                            this.with_form(cx, |form| form.group = target);
                        }
                    }),
                )
                .into_any_element(),
            ],
            tokens,
        )
        .into_any_element()
    }

    /// SSH / Telnet 的连接：地址、端口、用户名、密码。
    fn form_connection(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> ::gpui::AnyElement {
        let existing = form.existing.as_ref();
        let identity = form
            .identity
            .and_then(|id| self.store.identities().iter().find(|item| item.id == id));
        let auth = if form.protocol == Protocol::Ssh {
            form.auth
        } else {
            AuthMethod::Password
        };
        let revealable = can_reveal(identity, auth);
        let has_text = !form.password.read(cx).value().is_empty();
        let eye = (revealable || has_text).then(|| {
            Button::new("host-password-eye")
                .ghost()
                .compact()
                .icon(if form.password_shown {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .tooltip(if form.password_shown {
                    "隐藏密码"
                } else {
                    "显示密码"
                })
                .on_click(act(entity, |this, window, cx| {
                    this.host_toggle_password(window, cx)
                }))
        });
        let hint = password_hint(
            existing.is_some(),
            identity.is_some_and(|item| item.has_secret),
        );
        let telnet = form.protocol == Protocol::Telnet;
        section(
            "连接",
            vec![
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(field_with_error(
                        "地址",
                        None,
                        Input::new(&form.address),
                        form.error_of(FormField::Address),
                        tokens,
                    )))
                    .child(
                        div()
                            .w(::gpui::px(88.0))
                            .flex_none()
                            .child(field_with_error(
                                "端口",
                                None,
                                Input::new(&form.port),
                                form.error_of(FormField::Port),
                                tokens,
                            )),
                    )
                    .into_any_element(),
                field_with_error(
                    "用户名",
                    telnet.then(|| "可以不填".to_owned()),
                    Input::new(&form.username),
                    form.error_of(FormField::Username),
                    tokens,
                )
                .into_any_element(),
                field_with_error(
                    "密码",
                    Some(if telnet {
                        format!("{hint}；开了自动登录才会用到")
                    } else {
                        hint.to_owned()
                    }),
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(Input::new(&form.password)))
                        .children(eye),
                    form.error_of(FormField::Password),
                    tokens,
                )
                .into_any_element(),
                switch_row(
                    "host-remember",
                    "没有选凭据时，把密码存成一条新凭据",
                    None,
                    form.remember,
                    {
                        let entity = entity.clone();
                        move |_window, cx| {
                            entity.update(cx, |this, cx| {
                                this.with_form(cx, |form| form.remember = !form.remember);
                            });
                        }
                    },
                    tokens,
                )
                .into_any_element(),
            ],
            tokens,
        )
        .into_any_element()
    }

    /// 认证：SSH 选认证方式 + 凭据；Telnet 只有密码凭据（可选）。
    fn form_auth(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> ::gpui::AnyElement {
        let vis = protocol_form::visible(form.protocol);
        let effective = if vis.auth_method {
            form.auth
        } else {
            AuthMethod::Password
        };
        let identity = form
            .identity
            .and_then(|id| self.store.identities().iter().find(|item| item.id == id));
        let auth_methods = [AuthMethod::Password, AuthMethod::Key, AuthMethod::Agent];
        let auth_choices: Vec<Choice> = auth_methods
            .iter()
            .map(|m| Choice::new(auth_label(*m), *m == form.auth))
            .collect();
        let fitting: Vec<&zssh_core::model::Identity> = self
            .store
            .identities()
            .iter()
            .filter(|item| identity_fits_auth(item.kind, effective))
            .collect();
        let describe = |item: &zssh_core::model::Identity| {
            format!(
                "{}{}",
                item.name,
                if item.has_secret {
                    ""
                } else {
                    "（没有密码）"
                }
            )
        };
        let mut identity_choices = vec![Choice::new("不用", form.identity.is_none())];
        let mut identity_targets: Vec<Option<i64>> = vec![None];
        for item in &fitting {
            identity_choices.push(Choice::new(describe(item), form.identity == Some(item.id)));
            identity_targets.push(Some(item.id));
        }
        let identity_label = identity.map_or_else(|| "不用".to_owned(), describe);
        let mut rows: Vec<::gpui::AnyElement> = Vec::new();
        if vis.auth_method {
            rows.push(
                field(
                    "认证方式",
                    None,
                    select(
                        "host-auth",
                        auth_label(form.auth),
                        auth_choices,
                        pick(entity, move |this, index, window, cx| {
                            if let Some(method) = auth_methods.get(index).copied() {
                                this.host_set_auth(method, window, cx);
                            }
                        }),
                    ),
                    tokens,
                )
                .into_any_element(),
            );
        }
        rows.push(
            v_flex()
                .gap_1p5()
                .child(field_with_error(
                    "保存的凭据",
                    (form.protocol == Protocol::Telnet).then(|| "可选，只能选密码凭据".to_owned()),
                    select(
                        "host-identity",
                        identity_label,
                        identity_choices,
                        pick(entity, move |this, index, window, cx| {
                            if let Some(target) = identity_targets.get(index).copied() {
                                this.host_set_identity(target, window, cx);
                            }
                        }),
                    ),
                    form.error_of(FormField::Identity),
                    tokens,
                ))
                .children(
                    form.mismatch_note
                        .clone()
                        .map(|text| error_note(text, tokens.danger)),
                )
                .into_any_element(),
        );
        section("认证", rows, tokens).into_any_element()
    }

    /// 网络：代理和跳板机（跳板机只能选 SSH 主机）。
    fn form_network(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> ::gpui::AnyElement {
        let proxies = self.store.proxies();
        let modes = [ProxyMode::Default, ProxyMode::None, ProxyMode::Custom];
        let mode_choices: Vec<Choice> = modes
            .iter()
            .map(|m| Choice::new(proxy_mode_label(*m), *m == form.proxy_mode))
            .collect();
        let mut proxy_choices = vec![Choice::new("（请选择）", form.proxy.is_none())];
        let mut proxy_targets: Vec<Option<i64>> = vec![None];
        for item in proxies {
            proxy_choices.push(Choice::new(
                format!(
                    "{}（{}）",
                    item.name,
                    crate::proxies_demo::kind_label(item.kind)
                ),
                form.proxy == Some(item.id),
            ));
            proxy_targets.push(Some(item.id));
        }
        let proxy_label = form
            .proxy
            .and_then(|id| proxies.iter().find(|p| p.id == id))
            .map_or_else(
                || "（请选择）".to_owned(),
                |p| format!("{}（{}）", p.name, crate::proxies_demo::kind_label(p.kind)),
            );
        // 跳板机只能是 SSH 主机（Telnet / 串口主机不能当跳板）。
        let others: Vec<&Host> = self
            .store
            .hosts()
            .iter()
            .filter(|item| Some(item.id) != form.host_id && item.protocol.protocol == Protocol::Ssh)
            .collect();
        let mut jump_choices = vec![Choice::new("不使用", form.jump.is_none())];
        let mut jump_targets: Vec<Option<i64>> = vec![None];
        for item in &others {
            jump_choices.push(Choice::new(item.name.clone(), form.jump == Some(item.id)));
            jump_targets.push(Some(item.id));
        }
        let jump_label = form
            .jump
            .and_then(|id| others.iter().find(|h| h.id == id))
            .map_or_else(|| "不使用".to_owned(), |h| h.name.clone());
        let mut network = vec![
            field_with_error(
                "代理",
                None,
                select(
                    "host-proxy-mode",
                    proxy_mode_label(form.proxy_mode),
                    mode_choices,
                    pick(entity, move |this, index, _window, cx| {
                        if let Some(mode) = modes.get(index).copied() {
                            this.with_form(cx, |form| form.proxy_mode = mode);
                        }
                    }),
                ),
                form.error_of(FormField::Proxy),
                tokens,
            )
            .into_any_element(),
        ];
        if form.proxy_mode == ProxyMode::Custom {
            network.push(
                v_flex()
                    .gap_1p5()
                    .child(field(
                        "用哪个代理",
                        None,
                        select(
                            "host-proxy",
                            proxy_label,
                            proxy_choices,
                            pick(entity, move |this, index, _window, cx| {
                                if let Some(target) = proxy_targets.get(index).copied() {
                                    this.with_form(cx, |form| form.proxy = target);
                                }
                            }),
                        ),
                        tokens,
                    ))
                    .when(proxies.is_empty(), |d| {
                        d.child(note(
                            "还没有代理，点下面的「新建 / 管理代理」添加一个。",
                            tokens,
                        ))
                    })
                    .child(
                        Button::new("host-manage-proxies")
                            .outline()
                            .compact()
                            .icon(IconName::Network)
                            .label("新建 / 管理代理")
                            .on_click(act(entity, |this, window, cx| {
                                this.manage_proxies_from_host_form(window, cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        network.push(
            field_with_error(
                "跳板机",
                Some("只能选 SSH 主机".to_owned()),
                select(
                    "host-jump",
                    jump_label,
                    jump_choices,
                    pick(entity, move |this, index, _window, cx| {
                        if let Some(target) = jump_targets.get(index).copied() {
                            this.with_form(cx, |form| form.jump = target);
                        }
                    }),
                ),
                form.error_of(FormField::Jump),
                tokens,
            )
            .into_any_element(),
        );
        section("网络", network, tokens).into_any_element()
    }

    /// 串口：设备（本机列表 + 刷新 / 手填）、波特率、数据位 / 校验位 / 停止位。
    fn form_serial(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> ::gpui::AnyElement {
        let serial = &form.serial;
        let device_text = serial.device.read(cx).value().to_string();
        let listed = serial
            .ports
            .iter()
            .any(|port| port.device.eq_ignore_ascii_case(device_text.trim()));
        // 没有串口、选了「手动输入」、或框里的设备不在列表里（拔掉了 / 以前手填的）：显示文字框。
        let manual =
            serial.manual || serial.ports.is_empty() || (!device_text.trim().is_empty() && !listed);
        let ports = serial.ports.clone();
        let mut choices: Vec<Choice> = ports
            .iter()
            .map(|port| {
                Choice::new(
                    port.label.clone(),
                    port.device.eq_ignore_ascii_case(device_text.trim()),
                )
            })
            .collect();
        choices.push(Choice::new("手动输入…", manual && !ports.is_empty()));
        let picker: ::gpui::AnyElement = if ports.is_empty() {
            Button::new("serial-device-none")
                .outline()
                .label("未检测到串口")
                .w_full()
                .disabled(true)
                .into_any_element()
        } else {
            let devices: Vec<String> = ports.iter().map(|port| port.device.clone()).collect();
            select(
                "serial-device",
                protocol_form::device_button_label(&device_text, &ports),
                choices,
                pick(entity, move |this, index, window, cx| {
                    match devices.get(index) {
                        Some(device) => this.host_pick_port(device, window, cx),
                        None => this.with_form(cx, |form| form.serial.manual = true),
                    }
                }),
            )
            .into_any_element()
        };
        let device_control = v_flex()
            .w_full()
            .gap_1p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(picker))
                    .child(
                        Button::new("serial-refresh")
                            .outline()
                            .compact()
                            .icon(IconName::RefreshCw)
                            .tooltip("刷新串口列表")
                            .on_click(act(entity, |this, _window, cx| this.host_refresh_ports(cx))),
                    ),
            )
            .when(manual, |d| d.child(Input::new(&serial.device)))
            .when(ports.is_empty(), |d| {
                d.child(note(
                    "没有检测到串口，可以直接填设备名（例如 COM3）；插上设备后点右边的刷新。",
                    tokens,
                ))
            });
        let baud_choices: Vec<Choice> = COMMON_BAUD_RATES
            .iter()
            .map(|rate| Choice::new(rate.to_string(), false))
            .collect();
        let baud_control = h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .child(div().flex_1().min_w_0().child(Input::new(&serial.baud)))
            .child(div().w(::gpui::px(104.0)).flex_none().child(select(
                "serial-baud-common",
                "常用值",
                baud_choices,
                pick(entity, move |this, index, window, cx| {
                    if let Some(rate) = COMMON_BAUD_RATES.get(index).copied() {
                        this.host_set_baud(rate, window, cx);
                    }
                }),
            )));
        let framing = h_flex()
            .w_full()
            .gap_2()
            .items_start()
            .child(div().flex_1().min_w_0().child(field(
                "数据位",
                None,
                enum_select(
                    "serial-data-bits",
                    entity,
                    serial.data_bits,
                    &DATA_BITS,
                    protocol_form::data_bits_label,
                    |form, value| form.serial.data_bits = value,
                ),
                tokens,
            )))
            .child(div().flex_1().min_w_0().child(field(
                "校验位",
                None,
                enum_select(
                    "serial-parity",
                    entity,
                    serial.parity,
                    &PARITIES,
                    protocol_form::parity_label,
                    |form, value| form.serial.parity = value,
                ),
                tokens,
            )))
            .child(div().flex_1().min_w_0().child(field(
                "停止位",
                None,
                enum_select(
                    "serial-stop-bits",
                    entity,
                    serial.stop_bits,
                    &STOP_BITS,
                    protocol_form::stop_bits_label,
                    |form, value| form.serial.stop_bits = value,
                ),
                tokens,
            )));
        section(
            "串口",
            vec![
                field_with_error(
                    "设备",
                    None,
                    device_control,
                    form.error_of(FormField::Device),
                    tokens,
                )
                .into_any_element(),
                field_with_error(
                    "波特率",
                    Some("可以直接填别的数值".to_owned()),
                    baud_control,
                    form.error_of(FormField::Baud),
                    tokens,
                )
                .into_any_element(),
                v_flex()
                    .gap_1p5()
                    .child(framing)
                    .children(
                        form.error_of(FormField::Framing)
                            .map(|text| error_note(text.to_owned(), tokens.danger)),
                    )
                    .into_any_element(),
            ],
            tokens,
        )
        .into_any_element()
    }

    /// 可折叠的「高级设置」：SSH 是 X11 / 旧算法 / 主机指纹，Telnet 是回车 / 回显 / NAWS / 终端类型 /
    /// 编码 / 自动登录 / 纯 TCP，串口是流控 / 换行 / 本地回显 / 编码。
    fn form_advanced(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> ::gpui::AnyElement {
        let toggle = {
            let entity = entity.clone();
            collapse_header(
                "host-advanced-toggle",
                "高级设置",
                form.advanced_open,
                move |_window, cx| {
                    entity.update(cx, |this, cx| {
                        this.with_form(cx, |form| form.advanced_open = !form.advanced_open);
                    });
                },
                tokens,
            )
        };
        if !form.advanced_open {
            return v_flex().w_full().child(toggle).into_any_element();
        }
        let toggle_row = |id: &'static str,
                          title: &'static str,
                          hint: Option<&'static str>,
                          checked: bool,
                          set: fn(&mut HostForm, bool)| {
            let entity = entity.clone();
            switch_row(
                id,
                title,
                hint.map(str::to_owned),
                checked,
                move |_window, cx| {
                    entity.update(cx, |this, cx| {
                        this.with_form(cx, |form| set(form, !checked));
                    });
                },
                tokens,
            )
            .into_any_element()
        };
        let mut rows: Vec<::gpui::AnyElement> = Vec::new();
        match form.protocol {
            Protocol::Ssh => {
                rows.push(toggle_row(
                    "host-x11",
                    "X11 转发",
                    Some("把远程图形程序显示在本机的 X 服务器上（如 VcXsrv）；本机没有时连接会提示安装。"),
                    form.x11,
                    |form, value| form.x11 = value,
                ));
                if let Some(text) = form.error_of(FormField::X11) {
                    rows.push(error_note(text.to_owned(), tokens.danger).into_any_element());
                }
                rows.push(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(tokens.border)
                        .child(toggle_row(
                            "host-legacy",
                            "旧算法兼容",
                            None,
                            form.legacy,
                            |form, value| form.legacy = value,
                        ))
                        .child(error_note(
                            "仅用于老服务器：允许 SHA1 签名的 ssh-rsa、老的密钥交换和 CBC 加密。安全性较低，只在这台机器连不上、并且确认对方是老系统时才打开。",
                            tokens.danger,
                        ))
                        .into_any_element(),
                );
                rows.push(self.fingerprint_box(form, entity, tokens));
            }
            Protocol::Telnet => {
                rows.extend(self.telnet_advanced(form, entity, tokens, &toggle_row))
            }
            Protocol::Serial => {
                let serial = &form.serial;
                rows.push(
                    field(
                        "流控",
                        None,
                        enum_select(
                            "serial-flow",
                            entity,
                            serial.flow_control,
                            &FLOW_CONTROLS,
                            protocol_form::flow_control_label,
                            |form, value| form.serial.flow_control = value,
                        ),
                        tokens,
                    )
                    .into_any_element(),
                );
                rows.push(
                    field(
                        "换行发送模式",
                        Some("按回车键时发送什么".to_owned()),
                        enum_select(
                            "serial-newline",
                            entity,
                            serial.newline,
                            &SERIAL_NEWLINES,
                            protocol_form::serial_newline_label,
                            |form, value| form.serial.newline = value,
                        ),
                        tokens,
                    )
                    .into_any_element(),
                );
                rows.push(toggle_row(
                    "serial-echo",
                    "本地回显",
                    Some("设备自己不回显键入的字符时打开"),
                    serial.local_echo,
                    |form, value| form.serial.local_echo = value,
                ));
                rows.push(
                    field(
                        "编码",
                        None,
                        enum_select(
                            "serial-encoding",
                            entity,
                            serial.encoding,
                            &ENCODINGS,
                            protocol_form::encoding_label,
                            |form, value| form.serial.encoding = value,
                        ),
                        tokens,
                    )
                    .into_any_element(),
                );
            }
        }
        let _ = cx;
        v_flex()
            .w_full()
            .gap_2()
            .child(toggle)
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .children(rows),
            )
            .into_any_element()
    }

    fn fingerprint_box(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> ::gpui::AnyElement {
        v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .child(note("已保存的主机指纹", tokens))
            .child(match form.existing.as_ref() {
                Some(_) => v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_family("Consolas")
                            .text_color(if form.fingerprint.is_some() {
                                tokens.text
                            } else {
                                tokens.text_muted
                            })
                            .child(SharedString::from(
                                form.fingerprint
                                    .clone()
                                    .unwrap_or_else(|| "（没有保存，下次连接会问）".to_owned()),
                            )),
                    )
                    .child(
                        Button::new("host-forget-fingerprint")
                            .outline()
                            .compact()
                            .label("清除已保存的指纹")
                            .on_click(act(entity, |this, window, cx| {
                                this.host_forget_fingerprint(window, cx);
                            })),
                    )
                    .child(note(
                        "服务器换过密钥（比如重装系统）时用；如果是被人冒充，先查清楚原因再清。",
                        tokens,
                    ))
                    .into_any_element(),
                None => note("保存之后才能看到。", tokens).into_any_element(),
            })
            .into_any_element()
    }

    /// Telnet 高级设置里的各行。
    fn telnet_advanced(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        toggle_row: &ToggleRow,
    ) -> Vec<::gpui::AnyElement> {
        let telnet = &form.telnet;
        let mut rows: Vec<::gpui::AnyElement> = vec![
            field(
                "回车模式",
                Some("按回车键时发送什么".to_owned()),
                enum_select(
                    "telnet-newline",
                    entity,
                    telnet.newline,
                    &TELNET_NEWLINES,
                    protocol_form::telnet_newline_label,
                    |form, value| form.telnet.newline = value,
                ),
                tokens,
            )
            .into_any_element(),
            field(
                "本地回显",
                None,
                enum_select(
                    "telnet-echo",
                    entity,
                    telnet.local_echo,
                    &LOCAL_ECHOES,
                    protocol_form::local_echo_label,
                    |form, value| form.telnet.local_echo = value,
                ),
                tokens,
            )
            .into_any_element(),
            toggle_row(
                "telnet-naws",
                "NAWS（把窗口大小告诉服务器）",
                None,
                telnet.naws,
                |form, value| form.telnet.naws = value,
            ),
            field_with_error(
                "终端类型",
                Some("回应服务器的 TTYPE 协商".to_owned()),
                Input::new(&telnet.terminal_type),
                form.error_of(FormField::TerminalType),
                tokens,
            )
            .into_any_element(),
            field(
                "编码",
                None,
                enum_select(
                    "telnet-encoding",
                    entity,
                    telnet.encoding,
                    &ENCODINGS,
                    protocol_form::encoding_label,
                    |form, value| form.telnet.encoding = value,
                ),
                tokens,
            )
            .into_any_element(),
            toggle_row(
                "telnet-auto-login",
                "自动登录",
                Some(
                    "看到用户名 / 密码提示就自动发送；用户名取上面的「用户名」，密码取选中的凭据或上面输入的密码。",
                ),
                telnet.auto_login,
                |form, value| form.telnet.auto_login = value,
            ),
        ];
        if telnet.auto_login {
            rows.push(
                v_flex()
                    .gap_2()
                    .child(field(
                        "用户名提示",
                        Some("逗号分隔，不区分大小写".to_owned()),
                        Input::new(&telnet.username_prompts),
                        tokens,
                    ))
                    .child(field(
                        "密码提示",
                        Some("逗号分隔，不区分大小写".to_owned()),
                        Input::new(&telnet.password_prompts),
                        tokens,
                    ))
                    .into_any_element(),
            );
        }
        if let Some(text) = form.error_of(FormField::AutoLogin) {
            rows.push(error_note(text.to_owned(), tokens.danger).into_any_element());
        }
        rows.push(toggle_row(
            "telnet-raw-tcp",
            "纯 TCP 模式",
            Some("不做任何 Telnet 协商，连的是裸 TCP 服务（设备调试口、netcat）时用"),
            telnet.raw_tcp,
            |form, value| form.telnet.raw_tcp = value,
        ));
        rows
    }

    /// AI 记忆小节（只有 SSH 主机有）。
    fn form_memory(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> Option<::gpui::AnyElement> {
        let memory = form.memory.as_ref()?;
        let current = memory.input.read(cx).value().to_string();
        let dirty = current != memory.saved;
        let length = current.chars().count();
        let stamp = memory
            .updated_at
            .map(|at| crate::hosts_demo::memory_summary(Some(at), crate::hosts_demo::now_unix()))
            .unwrap_or_default();
        Some(
            section(
                "AI 记忆",
                vec![v_flex()
                    .gap_2()
                    .child(note(
                        "AI 排查这台主机时记下的笔记（服务、配置位置、处理过的问题、待办），下次对话会带上。「全部放行」模式下不带。只存在这台电脑上。",
                        tokens,
                    ))
                    .child(Textarea::new(&memory.input))
                    .children(memory.error.clone().map(|e| error_note(e, tokens.danger)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .typo(Role::Caption, tokens)
                                    .child(SharedString::from(format!(
                                        "{stamp}{}",
                                        if length > 0 {
                                            format!(" · {length}/{MAX_MEMORY_CHARS}")
                                        } else {
                                            String::new()
                                        }
                                    ))),
                            )
                            .when(!memory.saved.is_empty(), |d| {
                                d.child(text_button(
                                    "host-memory-clear",
                                    "清空",
                                    ButtonLook::Ghost,
                                    act(entity, |this, window, cx| this.host_memory_save(window, cx, true)),
                                ))
                            })
                            .child(
                                text_button(
                                    "host-memory-save",
                                    "保存记忆",
                                    ButtonLook::Outline,
                                    act(entity, |this, window, cx| this.host_memory_save(window, cx, false)),
                                )
                                .disabled(!dirty),
                            ),
                    )
                    .into_any_element()],
                tokens,
            )
            .into_any_element(),
        )
    }

    /// 「取消」：丢掉编辑，收起面板。
    pub(crate) fn cancel_host_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.close_host_details();
        self.host_form = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 主机详情里「新建 / 管理代理」：盖上代理面板，返回键回到这里。
    fn manage_proxies_from_host_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let none_yet = self.store.proxies().is_empty();
        self.proxy_ui.from_host_form = true;
        self.panels.open_side(PROXIES);
        self.proxy_start_creating_if(none_yet, window, cx);
        cx.notify();
    }

    /// 代理面板的返回键 / ✕（从主机详情进来的）：回到主机详情，没保存的编辑还在。
    pub(crate) fn back_to_host_details(&mut self, cx: &mut Context<Self>) {
        self.proxy_ui.from_host_form = false;
        self.panels.close_side();
        cx.notify();
    }

    /// 代理面板里新建了代理：主机详情自动选中它（不用再去下拉框里找）。
    pub(crate) fn host_form_adopt_proxy(&mut self, proxy_id: i64) {
        if let Some(form) = &mut self.host_form {
            form.proxy_mode = ProxyMode::Custom;
            form.proxy = Some(proxy_id);
        }
    }

    /// 主机详情的某个字段在被调试脚本 / 测试读：名称输入框当前的文字。
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
