//! 调试驱动里 V2-N4（主机协议的界面）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里
//! 只剩两个空函数。动作都走和界面控件同一批方法，`assert_*` 不对就让脚本失败。
//!
//! - `host_protocol:<ssh|telnet|serial>`：主机表单里点协议分段按钮；
//! - `host_advanced:<on|off>`、`host_x11:<on|off>`；
//! - `host_input:<字段>:<文字>`：往表单的文字框里填（`name` `address` `port` `username` `password`
//!   `terminal_type` `username_prompts` `password_prompts` `device` `baud`）；
//! - `host_telnet:<项>:<值>`（`newline` `echo` `naws` `encoding` `auto_login` `raw_tcp`）、
//!   `host_serial:<项>:<值>`（`data_bits` `parity` `stop_bits` `flow` `newline` `echo` `encoding`）；
//! - `host_ports:refresh`、`host_ports:pick:<设备名>`；
//! - `assert_host_form:<项>:<值>`：`protocol` `port` `username` `advanced` `device` `error.<字段>`（值是
//!   错误里应有的文字，`none` = 没有错误）；
//! - `assert_n4:<项>:<值>`：`status_host`（`none` / `some`，状态栏订阅的主机）`active_caps`
//!   （活动标签的 `ssh`/`serial`/`other`）；
//! - `assert_stored:<@主机>:<项>:<值>`：从库里**重新读**主机检查（`protocol` `detail` `x11` `device`
//!   `baud` `username` `port` `host` `telnet.newline` `telnet.auto_login` ...）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::model::{
        LocalEcho, Protocol, SerialDataBits, SerialFlowControl, SerialNewline, SerialParity,
        SerialStopBits, TelnetNewline, TextEncoding,
    };

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::protocol_form::{self, FormField};

    const KNOWN: &[&str] = &[
        "host_protocol",
        "host_advanced",
        "host_x11",
        "host_input",
        "host_telnet",
        "host_serial",
        "host_ports",
        "assert_host_form",
        "assert_stored",
        "assert_n4",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(run_inner(this, name, a, b, window, cx))
    }

    fn on_off(value: &str) -> Result<bool, String> {
        match value {
            "on" | "true" => Ok(true),
            "off" | "false" => Ok(false),
            _ => Err("值要是 on / off".to_owned()),
        }
    }

    fn check(ok: bool, what: &str) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else {
            Err(format!("断言没有成立：{what}"))
        }
    }

    fn protocol_of(value: &str) -> Result<Protocol, String> {
        Protocol::parse(value).map_err(|err| err.to_string())
    }

    fn encoding_of(value: &str) -> Result<TextEncoding, String> {
        Ok(match value {
            "utf8" => TextEncoding::Utf8,
            "gbk" => TextEncoding::Gbk,
            "big5" => TextEncoding::Big5,
            "shift_jis" => TextEncoding::ShiftJis,
            "euc_kr" => TextEncoding::EucKr,
            "latin1" => TextEncoding::Latin1,
            _ => return Err("编码写 utf8 / gbk / big5 / shift_jis / euc_kr / latin1".to_owned()),
        })
    }

    fn field_of(name: &str) -> Result<FormField, String> {
        Ok(match name {
            "name" => FormField::Name,
            "address" => FormField::Address,
            "port" => FormField::Port,
            "username" => FormField::Username,
            "password" => FormField::Password,
            "identity" => FormField::Identity,
            "proxy" => FormField::Proxy,
            "jump" => FormField::Jump,
            "x11" => FormField::X11,
            "terminal_type" => FormField::TerminalType,
            "auto_login" => FormField::AutoLogin,
            "device" => FormField::Device,
            "baud" => FormField::Baud,
            "framing" => FormField::Framing,
            "general" => FormField::General,
            _ => return Err("不认识的错误位置".to_owned()),
        })
    }

    fn host_ref(this: &MainWindow, raw: &str) -> Result<i64, String> {
        let name = raw.strip_prefix('@').ok_or("主机要写成 @名字")?;
        this.store
            .hosts()
            .iter()
            .find(|h| h.name == name)
            .map(|h| h.id)
            .ok_or_else(|| "没有这个名字的主机".to_owned())
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        a: &str,
        b: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "host_protocol" => this.host_set_protocol(protocol_of(a)?, window, cx),
            "host_advanced" => {
                let open = on_off(a)?;
                let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
                form.advanced_open = open;
                cx.notify();
            }
            "host_x11" => {
                let on = on_off(a)?;
                let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
                form.x11 = on;
                cx.notify();
            }
            "host_input" => {
                let form = this.host_form.as_ref().ok_or("主机表单没开着")?;
                let state = match a {
                    "name" => form.name.clone(),
                    "address" => form.address.clone(),
                    "port" => form.port.clone(),
                    "username" => form.username.clone(),
                    "password" => form.password.clone(),
                    "terminal_type" => form.telnet.terminal_type.clone(),
                    "username_prompts" => form.telnet.username_prompts.clone(),
                    "password_prompts" => form.telnet.password_prompts.clone(),
                    "device" => form.serial.device.clone(),
                    "baud" => form.serial.baud.clone(),
                    _ => return Err("不认识的文字框".to_owned()),
                };
                let value = b.to_owned();
                state.update(cx, |input, cx| input.set_value(value, window, cx));
            }
            "host_telnet" => {
                let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
                match a {
                    "newline" => {
                        form.telnet.newline = match b {
                            "cr" => TelnetNewline::Cr,
                            "crlf" => TelnetNewline::CrLf,
                            "lf" => TelnetNewline::Lf,
                            _ => return Err("回车模式写 cr / crlf / lf".to_owned()),
                        }
                    }
                    "echo" => {
                        form.telnet.local_echo = match b {
                            "auto" => LocalEcho::Auto,
                            "on" => LocalEcho::On,
                            "off" => LocalEcho::Off,
                            _ => return Err("回显写 auto / on / off".to_owned()),
                        }
                    }
                    "naws" => form.telnet.naws = on_off(b)?,
                    "encoding" => form.telnet.encoding = encoding_of(b)?,
                    "auto_login" => form.telnet.auto_login = on_off(b)?,
                    "raw_tcp" => form.telnet.raw_tcp = on_off(b)?,
                    _ => return Err("不认识的 Telnet 选项".to_owned()),
                }
                cx.notify();
            }
            "host_serial" => {
                let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
                match a {
                    "data_bits" => {
                        form.serial.data_bits = match b {
                            "5" => SerialDataBits::Five,
                            "6" => SerialDataBits::Six,
                            "7" => SerialDataBits::Seven,
                            "8" => SerialDataBits::Eight,
                            _ => return Err("数据位写 5-8".to_owned()),
                        }
                    }
                    "parity" => {
                        form.serial.parity = match b {
                            "none" => SerialParity::None,
                            "odd" => SerialParity::Odd,
                            "even" => SerialParity::Even,
                            "mark" => SerialParity::Mark,
                            "space" => SerialParity::Space,
                            _ => return Err("校验位写 none / odd / even / mark / space".to_owned()),
                        }
                    }
                    "stop_bits" => {
                        form.serial.stop_bits = match b {
                            "1" => SerialStopBits::One,
                            "1.5" => SerialStopBits::OnePointFive,
                            "2" => SerialStopBits::Two,
                            _ => return Err("停止位写 1 / 1.5 / 2".to_owned()),
                        }
                    }
                    "flow" => {
                        form.serial.flow_control = match b {
                            "none" => SerialFlowControl::None,
                            "software" => SerialFlowControl::Software,
                            "hardware" => SerialFlowControl::Hardware,
                            _ => return Err("流控写 none / software / hardware".to_owned()),
                        }
                    }
                    "newline" => {
                        form.serial.newline = match b {
                            "cr" => SerialNewline::Cr,
                            "lf" => SerialNewline::Lf,
                            "crlf" => SerialNewline::CrLf,
                            _ => return Err("换行写 cr / lf / crlf".to_owned()),
                        }
                    }
                    "echo" => form.serial.local_echo = on_off(b)?,
                    "encoding" => form.serial.encoding = encoding_of(b)?,
                    _ => return Err("不认识的串口选项".to_owned()),
                }
                cx.notify();
            }
            "host_ports" => match a {
                "refresh" => this.host_refresh_ports(cx),
                "pick" => this.host_pick_port(b, window, cx),
                _ => return Err("host_ports 只能是 refresh / pick:<设备>".to_owned()),
            },
            "assert_n4" => {
                let got = match a {
                    "status_host" => (if this.status_host.is_some() {
                        "some"
                    } else {
                        "none"
                    })
                    .to_owned(),
                    "active_caps" => {
                        let caps = this.active_caps(cx);
                        if caps.ssh {
                            "ssh"
                        } else if caps.send_break {
                            "serial"
                        } else {
                            "other"
                        }
                        .to_owned()
                    }
                    _ => return Err("不认识的项".to_owned()),
                };
                return check(got == b, &format!("{a}（现在是 {got}）"));
            }
            "assert_host_form" => {
                let form = this.host_form.as_ref().ok_or("主机表单没开着")?;
                if let Some(field) = a.strip_prefix("error.") {
                    let field = field_of(field)?;
                    let got = form.error_of(field);
                    return check(
                        match (b, got) {
                            ("none", None) => true,
                            ("none", Some(_)) => false,
                            (_, Some(text)) => text.contains(b),
                            (_, None) => false,
                        },
                        "表单上这个位置的错误",
                    );
                }
                let value = match a {
                    "protocol" => form.protocol.as_str().to_owned(),
                    "port" => form.port.read(cx).value().to_string(),
                    "username" => form.username.read(cx).value().to_string(),
                    "advanced" => form.advanced_open.to_string(),
                    "device" => form.serial.device.read(cx).value().to_string(),
                    "x11" => form.x11.to_string(),
                    "ports" => form.serial.ports.len().to_string(),
                    _ => return Err("不认识的表单项".to_owned()),
                };
                return check(value == b, &format!("表单 {a}"));
            }
            "assert_stored" => {
                let id = host_ref(this, a)?;
                let (what, want) = b.split_once(':').ok_or("写成 assert_stored:@主机:项:值")?;
                let db = this.store.db()?;
                let host = db.host_get(id).map_err(|err| err.to_string())?;
                let got = match what {
                    "protocol" => host.protocol.protocol.as_str().to_owned(),
                    "detail" => protocol_form::host_detail(&host),
                    "x11" => host.protocol.x11_forwarding.to_string(),
                    "device" => host
                        .protocol
                        .serial
                        .as_ref()
                        .map(|o| o.device.clone())
                        .unwrap_or_default(),
                    "baud" => host
                        .protocol
                        .serial
                        .as_ref()
                        .map(|o| o.baud_rate.to_string())
                        .unwrap_or_default(),
                    "username" => host.username.clone(),
                    "port" => host.port.to_string(),
                    "host" => host.host.clone(),
                    "telnet.newline" => host
                        .protocol
                        .telnet
                        .as_ref()
                        .map(|o| format!("{:?}", o.newline).to_lowercase())
                        .unwrap_or_default(),
                    "telnet.auto_login" => host
                        .protocol
                        .telnet
                        .as_ref()
                        .map(|o| o.auto_login.enabled.to_string())
                        .unwrap_or_default(),
                    "telnet.terminal_type" => host
                        .protocol
                        .telnet
                        .as_ref()
                        .map(|o| o.terminal_type.clone())
                        .unwrap_or_default(),
                    "telnet.raw_tcp" => host
                        .protocol
                        .telnet
                        .as_ref()
                        .map(|o| o.raw_tcp.to_string())
                        .unwrap_or_default(),
                    "serial.parity" => host
                        .protocol
                        .serial
                        .as_ref()
                        .map(|o| format!("{:?}", o.parity).to_lowercase())
                        .unwrap_or_default(),
                    "serial.flow" => host
                        .protocol
                        .serial
                        .as_ref()
                        .map(|o| format!("{:?}", o.flow_control).to_lowercase())
                        .unwrap_or_default(),
                    "identity" => host.identity_id.is_some().to_string(),
                    "jump" => host.jump_host_id.is_some().to_string(),
                    _ => return Err("不认识的库字段".to_owned()),
                };
                return check(got == want, &format!("库里 {what}"));
            }
            _ => return Err("还没有实现".to_owned()),
        }
        Ok(Outcome::Done)
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
