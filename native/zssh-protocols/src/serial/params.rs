//! 串口参数的纯函数部分：设备名整理、`SerialOptions` → Win32 `DCB` 字段的映射、错误分类。
//!
//! 这些不碰系统调用，本机没有串口硬件也能完整测试。

use std::fmt;

use crate::lang::{FlowWord, Language};

use zssh_core::model::{
    SerialDataBits, SerialFlowControl, SerialOptions, SerialParity, SerialStopBits,
};

// Win32 的取值（winbase.h / DCB）。
pub const NOPARITY: u8 = 0;
pub const ODDPARITY: u8 = 1;
pub const EVENPARITY: u8 = 2;
pub const MARKPARITY: u8 = 3;
pub const SPACEPARITY: u8 = 4;
pub const ONESTOPBIT: u8 = 0;
pub const ONE5STOPBITS: u8 = 1;
pub const TWOSTOPBITS: u8 = 2;

// DCB 位域（`fBinary` 在最低位，按声明顺序往上排）。
pub const F_BINARY: u32 = 1 << 0;
pub const F_PARITY: u32 = 1 << 1;
pub const F_OUTX_CTS_FLOW: u32 = 1 << 2;
pub const F_DTR_CONTROL_SHIFT: u32 = 4;
pub const F_TX_CONTINUE_ON_XOFF: u32 = 1 << 7;
pub const F_OUT_X: u32 = 1 << 8;
pub const F_IN_X: u32 = 1 << 9;
pub const F_RTS_CONTROL_SHIFT: u32 = 12;
const DTR_CONTROL_ENABLE: u32 = 1;
const RTS_CONTROL_ENABLE: u32 = 1;
const RTS_CONTROL_HANDSHAKE: u32 = 2;

pub const XON_CHAR: u8 = 0x11;
pub const XOFF_CHAR: u8 = 0x13;
/// 软件流控的高低水位（字节）：接收缓冲还剩这么多空间时发 XOFF / 降到这么多时发 XON。
pub const XON_LIMIT: u16 = 2048;
pub const XOFF_LIMIT: u16 = 512;

/// 写进 `DCB` 的各个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcbSettings {
    pub baud_rate: u32,
    pub byte_size: u8,
    pub parity: u8,
    pub stop_bits: u8,
    /// 位域整体（`fBinary`…`fAbortOnError`）。
    pub flags: u32,
}

impl DcbSettings {
    pub fn dtr_control(&self) -> u32 {
        (self.flags >> F_DTR_CONTROL_SHIFT) & 0b11
    }

    pub fn rts_control(&self) -> u32 {
        (self.flags >> F_RTS_CONTROL_SHIFT) & 0b11
    }
}

/// 把选项映射成 DCB 字段。
///
/// - 一律 `fBinary`；`fAbortOnError` 恒为 0（否则一次线路错误之后所有读写都失败，直到手动清错）；
/// - DTR 一律拉高（大多数 USB 转串口和单片机要 DTR 才开始说话）；
/// - 硬件流控：CTS 控制发送、RTS 由驱动按接收缓冲水位控制（HANDSHAKE）；其余情况 RTS 拉高；
/// - 软件流控：XON/XOFF 收发两个方向都开。
pub fn dcb_settings(options: &SerialOptions) -> DcbSettings {
    let byte_size = match options.data_bits {
        SerialDataBits::Five => 5,
        SerialDataBits::Six => 6,
        SerialDataBits::Seven => 7,
        SerialDataBits::Eight => 8,
    };
    let parity = match options.parity {
        SerialParity::None => NOPARITY,
        SerialParity::Odd => ODDPARITY,
        SerialParity::Even => EVENPARITY,
        SerialParity::Mark => MARKPARITY,
        SerialParity::Space => SPACEPARITY,
    };
    let stop_bits = match options.stop_bits {
        SerialStopBits::One => ONESTOPBIT,
        SerialStopBits::OnePointFive => ONE5STOPBITS,
        SerialStopBits::Two => TWOSTOPBITS,
    };
    let mut flags = F_BINARY | (DTR_CONTROL_ENABLE << F_DTR_CONTROL_SHIFT);
    if options.parity != SerialParity::None {
        flags |= F_PARITY;
    }
    match options.flow_control {
        SerialFlowControl::None => {
            flags |= RTS_CONTROL_ENABLE << F_RTS_CONTROL_SHIFT;
        }
        SerialFlowControl::Software => {
            flags |= RTS_CONTROL_ENABLE << F_RTS_CONTROL_SHIFT;
            flags |= F_OUT_X | F_IN_X | F_TX_CONTINUE_ON_XOFF;
        }
        SerialFlowControl::Hardware => {
            flags |= F_OUTX_CTS_FLOW;
            flags |= RTS_CONTROL_HANDSHAKE << F_RTS_CONTROL_SHIFT;
        }
    }
    DcbSettings {
        baud_rate: options.baud_rate,
        byte_size,
        parity,
        stop_bits,
        flags,
    }
}

/// 设备名整理成 `CreateFileW` 要的路径（`\\.\COM3`），同时返回给用户看的短名（`COM3`）。
///
/// 只接受 `COM<数字>`，或者 `\\.\` 开头加字母数字下划线连字符的名字（虚拟串口驱动常用，
/// 例如 `\\.\CNCA0`）。其他一律拒绝：这里不能被用来打开任意设备或文件。
pub fn normalize_device(raw: &str) -> Result<(String, String), SerialError> {
    let name = raw.trim();
    let short = name.strip_prefix(r"\\.\").unwrap_or(name);
    let valid = !short.is_empty()
        && short.len() <= 64
        && short
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        return Err(SerialError::InvalidName(raw.trim().to_owned()));
    }
    // 带 `\\.\` 前缀时不能指向磁盘、卷、管道之类的设备（虚拟串口驱动的名字都不是这些）。
    let lower = short.to_ascii_lowercase();
    const FORBIDDEN: &[&str] = &[
        "physicaldrive",
        "harddisk",
        "volume",
        "scsi",
        "tape",
        "cdrom",
        "pipe",
        "mailslot",
        "nul",
        "con",
        "globalroot",
    ];
    if FORBIDDEN.iter().any(|prefix| lower.starts_with(prefix)) {
        return Err(SerialError::InvalidName(raw.trim().to_owned()));
    }
    // 不带 `\\.\` 前缀的只认 COM 口。
    let is_com = short.len() > 3
        && short[..3].eq_ignore_ascii_case("COM")
        && short[3..].bytes().all(|byte| byte.is_ascii_digit());
    if short == name && !is_com {
        return Err(SerialError::InvalidName(raw.trim().to_owned()));
    }
    let short = if is_com {
        format!("COM{}", &short[3..])
    } else {
        short.to_owned()
    };
    Ok((format!(r"\\.\{short}"), short))
}

/// 串口的错误。`Display` 是中文（日志和测试用）；界面上的文案走 [`SerialError::message`]，按语言生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerialError {
    InvalidName(String),
    NotFound {
        device: String,
    },
    Busy {
        device: String,
    },
    Disconnected {
        device: String,
    },
    NotASerialPort {
        device: String,
    },
    /// 驱动拒绝了波特率或帧格式。
    UnsupportedSettings {
        device: String,
    },
    Os {
        device: String,
        code: u32,
        what: String,
    },
}

impl SerialError {
    /// 当前语言的整句。设备名、系统错误原文（`Os` 的 `what`）原样保留。
    pub fn message(&self, lang: Language) -> String {
        use crate::lang::{Text, render};
        match self {
            Self::InvalidName(name) => render(lang, Text::ErrInvalidName, &[("name", name)]),
            Self::NotFound { device } => render(lang, Text::ErrNotFound, &[("device", device)]),
            Self::Busy { device } => render(lang, Text::ErrBusy, &[("device", device)]),
            Self::Disconnected { device } => {
                render(lang, Text::ErrDisconnected, &[("device", device)])
            }
            Self::NotASerialPort { device } => {
                render(lang, Text::ErrNotASerialPort, &[("device", device)])
            }
            Self::UnsupportedSettings { device } => {
                render(lang, Text::ErrUnsupportedSettings, &[("device", device)])
            }
            Self::Os { device, code, what } => render(
                lang,
                Text::ErrOs,
                &[
                    ("device", device),
                    ("what", what),
                    ("code", &code.to_string()),
                ],
            ),
        }
    }
}

impl fmt::Display for SerialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message(Language::Zh))
    }
}

impl std::error::Error for SerialError {}

// 用到的 Win32 错误码。
pub const ERROR_INVALID_FUNCTION: u32 = 1;
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_NOT_READY: u32 = 21;
pub const ERROR_BAD_COMMAND: u32 = 22;
pub const ERROR_GEN_FAILURE: u32 = 31;
pub const ERROR_SHARING_VIOLATION: u32 = 32;
pub const ERROR_NOT_SUPPORTED: u32 = 50;
pub const ERROR_DEV_NOT_EXIST: u32 = 55;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_OPERATION_ABORTED: u32 = 995;
pub const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// 打开串口的哪一步失败了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenStage {
    /// `CreateFileW`
    Create,
    /// `GetCommState` / `SetCommState` / `SetCommTimeouts`
    Configure,
}

pub fn classify_open_error(device: &str, stage: OpenStage, code: u32, what: &str) -> SerialError {
    let device = device.to_owned();
    match (stage, code) {
        (OpenStage::Create, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
            SerialError::NotFound { device }
        }
        (OpenStage::Create, ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION) => {
            SerialError::Busy { device }
        }
        (_, ERROR_GEN_FAILURE | ERROR_DEVICE_NOT_CONNECTED | ERROR_DEV_NOT_EXIST) => {
            SerialError::Disconnected { device }
        }
        (OpenStage::Configure, ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) => {
            SerialError::NotASerialPort { device }
        }
        (OpenStage::Configure, ERROR_INVALID_PARAMETER) => {
            SerialError::UnsupportedSettings { device }
        }
        _ => SerialError::Os {
            device,
            code,
            what: what.to_owned(),
        },
    }
}

/// 读写过程中出错：这些码基本都意味着设备没了（USB 转串口被拔出最常见）。
pub fn io_error_means_disconnected(code: u32) -> bool {
    matches!(
        code,
        ERROR_ACCESS_DENIED
            | ERROR_NOT_READY
            | ERROR_BAD_COMMAND
            | ERROR_GEN_FAILURE
            | ERROR_DEV_NOT_EXIST
            | ERROR_DEVICE_NOT_CONNECTED
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SerialOptions {
        SerialOptions::default()
    }

    #[test]
    fn defaults_map_to_8n1_no_flow_control_with_dtr_and_rts_up() {
        let dcb = dcb_settings(&options());
        assert_eq!(dcb.baud_rate, 115_200);
        assert_eq!(
            (dcb.byte_size, dcb.parity, dcb.stop_bits),
            (8, NOPARITY, ONESTOPBIT)
        );
        assert_eq!(dcb.flags & F_BINARY, F_BINARY);
        assert_eq!(dcb.flags & F_PARITY, 0);
        assert_eq!(dcb.flags & (F_OUTX_CTS_FLOW | F_OUT_X | F_IN_X), 0);
        assert_eq!(dcb.dtr_control(), 1);
        assert_eq!(dcb.rts_control(), 1);
        // fAbortOnError（第 14 位）不能开。
        assert_eq!(dcb.flags & (1 << 14), 0);
    }

    #[test]
    fn every_parity_and_stop_bit_value_has_its_win32_code() {
        for (parity, code) in [
            (SerialParity::None, NOPARITY),
            (SerialParity::Odd, ODDPARITY),
            (SerialParity::Even, EVENPARITY),
            (SerialParity::Mark, MARKPARITY),
            (SerialParity::Space, SPACEPARITY),
        ] {
            let dcb = dcb_settings(&SerialOptions {
                parity,
                ..options()
            });
            assert_eq!(dcb.parity, code);
            assert_eq!(dcb.flags & F_PARITY != 0, parity != SerialParity::None);
        }
        for (stop_bits, code) in [
            (SerialStopBits::One, ONESTOPBIT),
            (SerialStopBits::OnePointFive, ONE5STOPBITS),
            (SerialStopBits::Two, TWOSTOPBITS),
        ] {
            assert_eq!(
                dcb_settings(&SerialOptions {
                    stop_bits,
                    ..options()
                })
                .stop_bits,
                code
            );
        }
        for (data_bits, size) in [
            (SerialDataBits::Five, 5),
            (SerialDataBits::Six, 6),
            (SerialDataBits::Seven, 7),
            (SerialDataBits::Eight, 8),
        ] {
            assert_eq!(
                dcb_settings(&SerialOptions {
                    data_bits,
                    ..options()
                })
                .byte_size,
                size
            );
        }
    }

    #[test]
    fn custom_baud_rates_are_passed_through_unchanged() {
        for baud_rate in [50, 9600, 250_000, 921_600, 3_000_000, 4_000_000] {
            assert_eq!(
                dcb_settings(&SerialOptions {
                    baud_rate,
                    ..options()
                })
                .baud_rate,
                baud_rate
            );
        }
    }

    #[test]
    fn hardware_flow_control_uses_cts_and_rts_handshake() {
        let dcb = dcb_settings(&SerialOptions {
            flow_control: SerialFlowControl::Hardware,
            ..options()
        });
        assert_eq!(dcb.flags & F_OUTX_CTS_FLOW, F_OUTX_CTS_FLOW);
        assert_eq!(dcb.rts_control(), 2);
        assert_eq!(dcb.flags & (F_OUT_X | F_IN_X), 0);
    }

    #[test]
    fn software_flow_control_enables_xon_xoff_both_ways() {
        let dcb = dcb_settings(&SerialOptions {
            flow_control: SerialFlowControl::Software,
            ..options()
        });
        assert_eq!(dcb.flags & (F_OUT_X | F_IN_X), F_OUT_X | F_IN_X);
        assert_eq!(dcb.flags & F_OUTX_CTS_FLOW, 0);
        assert_eq!(dcb.rts_control(), 1);
    }

    #[test]
    fn device_names_are_normalized_and_arbitrary_paths_are_refused() {
        assert_eq!(
            normalize_device("COM3").unwrap(),
            (r"\\.\COM3".to_owned(), "COM3".to_owned())
        );
        assert_eq!(normalize_device(" com12 ").unwrap().1, "COM12");
        assert_eq!(normalize_device(r"\\.\COM12").unwrap().0, r"\\.\COM12");
        assert_eq!(normalize_device(r"\\.\CNCA0").unwrap().1, "CNCA0");
        for bad in [
            "",
            "COM",
            "C:\\x",
            r"\\.\C:",
            r"\\.\PhysicalDrive0\x",
            "NUL",
            r"\\.\PhysicalDrive0",
            r"\\.\NUL",
            r"\\.\pipe\x",
            "../COM3",
            r"\\server\share",
            "COM3 ",
            "COM3;",
        ] {
            let result = normalize_device(bad);
            // 带尾空格的会被 trim；那种是合法的。
            if bad.trim() == "COM3" {
                assert!(result.is_ok());
            } else {
                assert!(result.is_err(), "{bad:?} 应当被拒绝：{result:?}");
            }
        }
    }

    #[test]
    fn open_errors_are_classified_into_clear_messages() {
        let cases = [
            (OpenStage::Create, ERROR_FILE_NOT_FOUND, "COM3 不存在"),
            (OpenStage::Create, ERROR_PATH_NOT_FOUND, "COM3 不存在"),
            (
                OpenStage::Create,
                ERROR_ACCESS_DENIED,
                "COM3 被别的程序占用",
            ),
            (
                OpenStage::Create,
                ERROR_SHARING_VIOLATION,
                "COM3 被别的程序占用",
            ),
            (OpenStage::Create, ERROR_GEN_FAILURE, "COM3 已断开"),
            (OpenStage::Create, ERROR_DEVICE_NOT_CONNECTED, "COM3 已断开"),
            (
                OpenStage::Configure,
                ERROR_INVALID_FUNCTION,
                "COM3 不是串口设备",
            ),
            (
                OpenStage::Configure,
                ERROR_INVALID_PARAMETER,
                "COM3 不接受这组串口参数",
            ),
        ];
        for (stage, code, expected) in cases {
            let message = classify_open_error("COM3", stage, code, "x").to_string();
            assert!(message.starts_with(expected), "{stage:?}/{code}: {message}");
        }
        // 配置阶段的"拒绝访问"不是占用（占用只会在打开时出现）。
        assert!(matches!(
            classify_open_error("COM3", OpenStage::Configure, ERROR_ACCESS_DENIED, "x"),
            SerialError::Os { .. }
        ));
        let other = classify_open_error("COM3", OpenStage::Create, 1234, "怪事").to_string();
        assert!(other.contains("1234") && other.contains("怪事"), "{other}");
    }

    #[test]
    fn io_errors_that_mean_the_device_is_gone() {
        for code in [
            ERROR_ACCESS_DENIED,
            ERROR_GEN_FAILURE,
            ERROR_DEVICE_NOT_CONNECTED,
            ERROR_BAD_COMMAND,
        ] {
            assert!(io_error_means_disconnected(code), "{code}");
        }
        assert!(!io_error_means_disconnected(ERROR_OPERATION_ABORTED));
        assert!(!io_error_means_disconnected(ERROR_INVALID_PARAMETER));
    }
}

/// 打开成功后在终端里提示的一句：`115200 8N1，无流控`（中文，日志和测试用）。
pub fn describe_framing(options: &SerialOptions) -> String {
    describe_framing_in(Language::Zh, options)
}

/// 同 [`describe_framing`]，指定语言：`115200 8N1, no flow control`。
pub fn describe_framing_in(lang: Language, options: &SerialOptions) -> String {
    let bits = match options.data_bits {
        SerialDataBits::Five => '5',
        SerialDataBits::Six => '6',
        SerialDataBits::Seven => '7',
        SerialDataBits::Eight => '8',
    };
    let parity = match options.parity {
        SerialParity::None => 'N',
        SerialParity::Odd => 'O',
        SerialParity::Even => 'E',
        SerialParity::Mark => 'M',
        SerialParity::Space => 'S',
    };
    let stop = match options.stop_bits {
        SerialStopBits::One => "1",
        SerialStopBits::OnePointFive => "1.5",
        SerialStopBits::Two => "2",
    };
    let flow = match options.flow_control {
        SerialFlowControl::None => FlowWord::None,
        SerialFlowControl::Software => FlowWord::Software,
        SerialFlowControl::Hardware => FlowWord::Hardware,
    };
    crate::lang::framing_in(
        lang,
        &format!("{} {bits}{parity}{stop}", options.baud_rate),
        flow,
    )
}

#[cfg(test)]
mod framing_tests {
    use super::*;

    #[test]
    fn framing_is_described_in_the_usual_notation() {
        assert_eq!(
            describe_framing(&SerialOptions::default()),
            "115200 8N1，无流控"
        );
        let options = SerialOptions {
            baud_rate: 9600,
            data_bits: SerialDataBits::Seven,
            parity: SerialParity::Even,
            stop_bits: SerialStopBits::Two,
            flow_control: SerialFlowControl::Hardware,
            ..Default::default()
        };
        assert_eq!(describe_framing(&options), "9600 7E2，RTS/CTS 流控");
    }
}

#[cfg(test)]
mod localized_tests {
    use super::*;

    fn has_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }

    /// 中文和 `Display` 逐字相同；英文整句没有汉字，设备名和系统错误原文原样在。
    #[test]
    fn serial_errors_read_in_both_languages() {
        let errors = [
            SerialError::InvalidName("X1".into()),
            SerialError::NotFound {
                device: "COM3".into(),
            },
            SerialError::Busy {
                device: "COM3".into(),
            },
            SerialError::Disconnected {
                device: "COM3".into(),
            },
            SerialError::NotASerialPort {
                device: "COM3".into(),
            },
            SerialError::UnsupportedSettings {
                device: "COM3".into(),
            },
            SerialError::Os {
                device: "COM3".into(),
                code: 1234,
                what: "boom".into(),
            },
        ];
        for err in errors {
            assert_eq!(err.message(Language::Zh), err.to_string());
            let en = err.message(Language::En);
            assert!(!has_cjk(&en), "{err:?} → {en}");
            if !matches!(err, SerialError::InvalidName(_)) {
                assert!(en.contains("COM3"), "{en}");
            }
        }
        assert!(
            SerialError::Os {
                device: "COM3".into(),
                code: 5,
                what: "拒绝访问。".into()
            }
            .message(Language::En)
            .contains("拒绝访问。"),
            "系统错误原文原样保留"
        );
    }

    #[test]
    fn the_framing_line_follows_the_language() {
        let options = SerialOptions::default();
        assert_eq!(
            describe_framing(&options),
            describe_framing_in(Language::Zh, &options)
        );
        assert!(!has_cjk(&describe_framing_in(Language::En, &options)));
    }
}
