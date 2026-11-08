//! 终端里写出来的提示行和串口错误的语言。
//!
//! 本 crate 不依赖界面层：界面在语言切换时调 [`set_language`] 告诉它，会话运行时写出的
//! 提示（带颜色的 `[提示] ……`）和错误按当时的语言生成。默认中文（单测和没设过语言的进程）。
//! 设备名、系统错误原文（系统语言的 Win32 消息）作为参数原样保留，不翻译。
//!
//! 每个文案都有 `*_in(lang, ..)` 版本，方便单测逐语言检查；不带 `_in` 的读当前语言。

use std::sync::atomic::{AtomicU8, Ordering};

/// 提示的语言：只有简体中文和英文。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    Zh,
    En,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 界面语言变了：之后新写出的提示用这个语言（已经写进终端的不会回头改）。
pub fn set_language(language: Language) {
    CURRENT.store(
        match language {
            Language::Zh => 0,
            Language::En => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn language() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::En,
        _ => Language::Zh,
    }
}

/// 一条文案的两个语言版本；`{name}` 是参数占位符（设备名、系统错误原文等原样填进去）。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Text {
    PlaintextNotice,
    RawTcpNotice,
    InputDropped,
    SerialOpened,
    WriteTimeout,
    BreakFailed,
    ReadEventFailed,
    WriteEventFailed,
    ReadFailed,
    WriteFailed,
    OpenFailed,
    ListLabel,
    FlowNone,
    FlowSoftware,
    FlowHardware,
    FramingLine,
    ErrInvalidName,
    ErrNotFound,
    ErrBusy,
    ErrDisconnected,
    ErrNotASerialPort,
    ErrUnsupportedSettings,
    ErrOs,
    ErrInvalidNameUnix,
    ErrNotFoundUnix,
    ErrBusyUnix,
    ErrPermission,
    WindowsOnly,
    RemoteWriteClosed,
    RemoteWriteFailed,
    RemoteReadFailed,
    NegotiationFlood,
}

impl Text {
    fn zh(&self) -> &'static str {
        match self {
            Self::PlaintextNotice => {
                "\x1b[33m[提示] Telnet 不加密：用户名、密码和所有内容都以明文在网络上传输，请只在可信的网络里使用。\x1b[0m\r\n"
            }
            Self::RawTcpNotice => {
                "\x1b[33m[提示] 纯 TCP 连接：不做 Telnet 协商，也不加密，内容以明文传输。\x1b[0m\r\n"
            }
            Self::InputDropped => "\r\n\x1b[31m[远端暂时不接收输入，已丢弃部分按键]\x1b[0m\r\n",
            Self::SerialOpened => "\x1b[90m[已打开 {label}：{framing}]\x1b[0m\n",
            Self::WriteTimeout => {
                "\r\n\x1b[31m[{label} 写入超时：设备一直没有放行（流控设置对吗？）]\x1b[0m\r\n"
            }
            Self::BreakFailed => "\r\n\x1b[31m[{label} 发送 Break 失败：{error}]\x1b[0m\r\n",
            Self::ReadEventFailed => "无法使用 {label}：创建读事件失败",
            Self::WriteEventFailed => "无法使用 {label}：创建写事件失败",
            Self::ReadFailed => "读取失败：{error}",
            Self::WriteFailed => "写入失败：{error}",
            Self::OpenFailed => "打开失败",
            Self::ListLabel => "串口列表",
            Self::FlowNone => "无流控",
            Self::FlowSoftware => "XON/XOFF 流控",
            Self::FlowHardware => "RTS/CTS 流控",
            Self::FramingLine => "{bits}，{flow}",
            Self::ErrInvalidName => "串口设备名「{name}」不合法：请填 COM 口的名字，例如 COM3",
            Self::ErrNotFound => {
                "{device} 不存在：设备没有插好，或者设备名写错了（可以在设备管理器的“端口”里查看当前的 COM 口）"
            }
            Self::ErrBusy => {
                "{device} 被别的程序占用（或者没有访问权限）：请先关闭正在使用它的程序，再重新连接"
            }
            Self::ErrDisconnected => "{device} 已断开：设备可能被拔出了，或者驱动出了故障",
            Self::ErrNotASerialPort => "{device} 不是串口设备，不能用串口方式打开",
            Self::ErrUnsupportedSettings => {
                "{device} 不接受这组串口参数（驱动拒绝了波特率或帧格式）：请换一个波特率 / 数据位 / 校验 / 停止位组合"
            }
            Self::ErrOs => "无法使用 {device}：{what}（系统错误码 {code}）",
            Self::ErrInvalidNameUnix => {
                "串口设备名「{name}」不合法：请填设备路径，例如 /dev/ttyUSB0"
            }
            Self::ErrNotFoundUnix => {
                "{device} 不存在：设备没有插好，或者设备名写错了（可以在终端里运行 ls /dev/tty* 查看当前的串口设备）"
            }
            Self::ErrBusyUnix => {
                "{device} 被别的程序占用：请先关闭正在使用它的程序（例如另一个串口终端，或者 ModemManager），再重新连接"
            }
            Self::ErrPermission => {
                "没有权限打开 {device}：当前用户需要加入 {group} 组。在终端里运行 sudo usermod -aG {group} $USER，然后注销并重新登录才会生效"
            }
            Self::WindowsOnly => "串口只在 Windows 和 Linux 上支持",
            Self::RemoteWriteClosed => "写入远端失败：连接已关闭",
            Self::RemoteWriteFailed => "写入远端失败：{error}",
            Self::RemoteReadFailed => "读取远端失败：{error}",
            Self::NegotiationFlood => "服务器发来的协商请求过多，应答积压超过上限，已断开连接",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::PlaintextNotice => {
                "\x1b[33m[Notice] Telnet is not encrypted: the user name, password and all content cross the network in clear text. Use it only on a trusted network.\x1b[0m\r\n"
            }
            Self::RawTcpNotice => {
                "\x1b[33m[Notice] Raw TCP connection: no Telnet negotiation and no encryption; the content is sent in clear text.\x1b[0m\r\n"
            }
            Self::InputDropped => {
                "\r\n\x1b[31m[The remote end is not accepting input right now; some keystrokes were dropped]\x1b[0m\r\n"
            }
            Self::SerialOpened => "\x1b[90m[Opened {label}: {framing}]\x1b[0m\n",
            Self::WriteTimeout => {
                "\r\n\x1b[31m[{label} write timed out: the device never let data through (is the flow control setting right?)]\x1b[0m\r\n"
            }
            Self::BreakFailed => "\r\n\x1b[31m[{label}: failed to send Break: {error}]\x1b[0m\r\n",
            Self::ReadEventFailed => "Cannot use {label}: failed to create the read event",
            Self::WriteEventFailed => "Cannot use {label}: failed to create the write event",
            Self::ReadFailed => "Read failed: {error}",
            Self::WriteFailed => "Write failed: {error}",
            Self::OpenFailed => "Open failed",
            Self::ListLabel => "Serial port list",
            Self::FlowNone => "no flow control",
            Self::FlowSoftware => "XON/XOFF flow control",
            Self::FlowHardware => "RTS/CTS flow control",
            Self::FramingLine => "{bits}, {flow}",
            Self::ErrInvalidName => {
                "Serial device name \"{name}\" is not valid: enter a COM port name such as COM3"
            }
            Self::ErrNotFound => {
                "{device} does not exist: the device is not plugged in, or the name is wrong (check the current COM ports under \"Ports\" in Device Manager)"
            }
            Self::ErrBusy => {
                "{device} is in use by another program (or access is denied): close the program that is using it, then connect again"
            }
            Self::ErrDisconnected => {
                "{device} was disconnected: the device may have been unplugged, or the driver failed"
            }
            Self::ErrNotASerialPort => {
                "{device} is not a serial device and cannot be opened as one"
            }
            Self::ErrUnsupportedSettings => {
                "{device} does not accept these serial settings (the driver rejected the baud rate or framing): try another baud rate / data bits / parity / stop bits combination"
            }
            Self::ErrOs => "Cannot use {device}: {what} (system error code {code})",
            Self::ErrInvalidNameUnix => {
                "Serial device name \"{name}\" is not valid: enter a device path such as /dev/ttyUSB0"
            }
            Self::ErrNotFoundUnix => {
                "{device} does not exist: the device is not plugged in, or the name is wrong (run ls /dev/tty* in a terminal to see the current serial devices)"
            }
            Self::ErrBusyUnix => {
                "{device} is in use by another program: close the program that is using it (for example another serial terminal, or ModemManager), then connect again"
            }
            Self::ErrPermission => {
                "No permission to open {device}: your user must be in the {group} group. Run sudo usermod -aG {group} $USER in a terminal, then log out and back in for it to take effect"
            }
            Self::WindowsOnly => "Serial ports are only supported on Windows and Linux",
            Self::RemoteWriteClosed => {
                "Failed to write to the remote end: the connection is closed"
            }
            Self::RemoteWriteFailed => "Failed to write to the remote end: {error}",
            Self::RemoteReadFailed => "Failed to read from the remote end: {error}",
            Self::NegotiationFlood => {
                "The server sent too many negotiation requests and the pending replies exceeded the limit; the connection was closed"
            }
        }
    }
}

/// 取某个语言的文案并填参数。
pub(crate) fn render(lang: Language, text: Text, params: &[(&str, &str)]) -> String {
    let mut out = match lang {
        Language::Zh => text.zh(),
        Language::En => text.en(),
    }
    .to_owned();
    for (name, value) in params {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Telnet 明文传输提示。
pub fn plaintext_notice_in(lang: Language) -> String {
    render(lang, Text::PlaintextNotice, &[])
}

/// 纯 TCP 模式的提示。
pub fn raw_tcp_notice_in(lang: Language) -> String {
    render(lang, Text::RawTcpNotice, &[])
}

/// 发送队列满了、丢了按键。
pub fn input_dropped_notice_in(lang: Language) -> String {
    render(lang, Text::InputDropped, &[])
}

pub fn input_dropped_notice() -> String {
    input_dropped_notice_in(language())
}

/// 串口打开后的第一行。
pub fn serial_opened_in(lang: Language, label: &str, framing: &str) -> String {
    render(
        lang,
        Text::SerialOpened,
        &[("label", label), ("framing", framing)],
    )
}

/// 串口写超时。
pub fn serial_write_timeout_in(lang: Language, label: &str) -> String {
    render(lang, Text::WriteTimeout, &[("label", label)])
}

pub fn serial_write_timeout(label: &str) -> String {
    serial_write_timeout_in(language(), label)
}

/// 发送 Break 失败；`error` 是系统错误原文，原样带上。
pub fn serial_break_failed_in(lang: Language, label: &str, error: &str) -> String {
    render(
        lang,
        Text::BreakFailed,
        &[("label", label), ("error", error)],
    )
}

pub fn serial_break_failed(label: &str, error: &str) -> String {
    serial_break_failed_in(language(), label, error)
}

/// 读 / 写事件创建失败（系统资源不足之类）。
pub fn serial_event_failed_in(lang: Language, label: &str, reading: bool) -> String {
    let text = if reading {
        Text::ReadEventFailed
    } else {
        Text::WriteEventFailed
    };
    render(lang, text, &[("label", label)])
}

pub fn serial_event_failed(label: &str, reading: bool) -> String {
    serial_event_failed_in(language(), label, reading)
}

/// 串口读写出错时给 `SerialError::Os` 的 `what`：`error` 是系统错误原文。
pub fn serial_io_failed_in(lang: Language, writing: bool, error: &str) -> String {
    let text = if writing {
        Text::WriteFailed
    } else {
        Text::ReadFailed
    };
    render(lang, text, &[("error", error)])
}

pub fn serial_io_failed(writing: bool, error: &str) -> String {
    serial_io_failed_in(language(), writing, error)
}

/// `CreateFile` 没给出系统错误时的兜底说法。
pub fn serial_open_failed_in(lang: Language) -> String {
    render(lang, Text::OpenFailed, &[])
}

pub fn serial_open_failed() -> String {
    serial_open_failed_in(language())
}

/// 非 Windows 平台上串口不可用时的说法。
pub fn serial_windows_only() -> String {
    render(language(), Text::WindowsOnly, &[])
}

/// Telnet 会话里写 / 读远端出错；`error` 是系统错误原文。`error` 为 `None` 表示对端已关闭连接。
pub fn remote_write_failed(error: Option<&str>) -> String {
    match error {
        Some(error) => render(language(), Text::RemoteWriteFailed, &[("error", error)]),
        None => render(language(), Text::RemoteWriteClosed, &[]),
    }
}

pub fn remote_read_failed(error: &str) -> String {
    render(language(), Text::RemoteReadFailed, &[("error", error)])
}

/// 服务器连发协商请求、应答在发送缓冲里积压超过上限：会话以这句话结束。
pub fn negotiation_flood() -> String {
    render(language(), Text::NegotiationFlood, &[])
}

/// 枚举串口列表失败时的"设备名"。
pub fn serial_list_label_in(lang: Language) -> String {
    render(lang, Text::ListLabel, &[])
}

pub fn serial_list_label() -> String {
    serial_list_label_in(language())
}

/// 串口参数的一句话：`115200 8N1，无流控`。
pub fn framing_in(lang: Language, baud_and_bits: &str, flow: FlowWord) -> String {
    let flow = match flow {
        FlowWord::None => Text::FlowNone,
        FlowWord::Software => Text::FlowSoftware,
        FlowWord::Hardware => Text::FlowHardware,
    };
    let flow = render(lang, flow, &[]);
    render(
        lang,
        Text::FramingLine,
        &[("bits", baud_and_bits), ("flow", &flow)],
    )
}

/// 流控方式（给 [`framing_in`] 取词用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowWord {
    None,
    Software,
    Hardware,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }

    /// 每条文案：英文没有汉字，参数（设备名、系统错误原文）两种语言都在。
    #[test]
    fn english_notices_have_no_chinese_and_keep_their_parameters() {
        let en = Language::En;
        let all = [
            plaintext_notice_in(en),
            raw_tcp_notice_in(en),
            input_dropped_notice_in(en),
            serial_opened_in(en, "COM3", "115200 8N1, no flow control"),
            serial_write_timeout_in(en, "COM3"),
            serial_break_failed_in(en, "COM3", "拒绝访问。"),
            serial_event_failed_in(en, "COM3", true),
            serial_event_failed_in(en, "COM3", false),
            serial_io_failed_in(en, true, "boom"),
            serial_open_failed_in(en),
            serial_list_label_in(en),
        ];
        for text in &all {
            // 系统错误原文（可能是系统语言）不算模板里的汉字。
            let text = text.replace("拒绝访问。", "");
            assert!(!has_cjk(&text), "{text:?}");
        }
        assert!(serial_opened_in(en, "COM3", "x").contains("COM3"));
        assert!(serial_break_failed_in(en, "COM3", "拒绝访问。").contains("拒绝访问。"));
        assert!(serial_break_failed_in(Language::Zh, "COM3", "拒绝访问。").contains("拒绝访问。"));
    }

    /// 中文和以前写死的文案逐字相同。
    #[test]
    fn chinese_notices_are_the_old_text() {
        let zh = Language::Zh;
        assert_eq!(
            plaintext_notice_in(zh),
            "\x1b[33m[提示] Telnet 不加密：用户名、密码和所有内容都以明文在网络上传输，请只在可信的网络里使用。\x1b[0m\r\n"
        );
        assert_eq!(
            raw_tcp_notice_in(zh),
            "\x1b[33m[提示] 纯 TCP 连接：不做 Telnet 协商，也不加密，内容以明文传输。\x1b[0m\r\n"
        );
        assert_eq!(
            input_dropped_notice_in(zh),
            "\r\n\x1b[31m[远端暂时不接收输入，已丢弃部分按键]\x1b[0m\r\n"
        );
        assert_eq!(
            serial_opened_in(zh, "COM3", "115200 8N1，无流控"),
            "\x1b[90m[已打开 COM3：115200 8N1，无流控]\x1b[0m\n"
        );
        assert_eq!(serial_io_failed_in(zh, true, "x"), "写入失败：x");
        assert_eq!(serial_io_failed_in(zh, false, "x"), "读取失败：x");
    }

    #[test]
    fn framing_follows_the_language() {
        assert_eq!(
            framing_in(Language::Zh, "9600 7E2", FlowWord::Hardware),
            "9600 7E2，RTS/CTS 流控"
        );
        assert_eq!(
            framing_in(Language::En, "9600 7E2", FlowWord::Hardware),
            "9600 7E2, RTS/CTS flow control"
        );
    }
}
