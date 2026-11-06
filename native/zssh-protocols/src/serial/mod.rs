//! 本机串口会话后端（Windows）。
//!
//! - [`params`]：设备名整理、`SerialOptions` → DCB 的映射、错误分类（纯函数，可完整单测）。
//! - `os`：Win32 重叠 I/O、打开 / 配置、设备枚举。
//! - `session`：读线程 + 写线程，接到 `SessionBackend` / `SessionEvent` / `FlowGate`。
//!
//! 依赖选择见 `os.rs` 顶部：直接用 Win32，不引入 `serialport` crate。

pub mod params;

#[cfg(windows)]
mod os;
#[cfg(windows)]
mod session;

pub use params::SerialError;

/// 本机的一个串口，给界面的下拉列表用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortEntry {
    /// 设备名，存进主机设置的就是它，如 `COM3`。
    pub device: String,
    /// 给人看的名字，如 `USB-SERIAL CH340 (COM3)`；取不到友好名时就是设备名。
    pub display_name: String,
}

/// COM 口按编号排（COM2 在 COM10 前面）。
#[cfg(any(windows, test))]
fn sort_ports(entries: &mut [SerialPortEntry]) {
    fn number(device: &str) -> (u64, String) {
        let digits: String = device
            .chars()
            .skip(3)
            .take_while(char::is_ascii_digit)
            .collect();
        (digits.parse().unwrap_or(u64::MAX), device.to_owned())
    }
    entries.sort_by_key(|entry| number(&entry.device));
}

/// 列出本机当前存在的串口。本机没有串口时返回空列表，不算错误。
#[cfg(windows)]
pub fn list_ports() -> Result<Vec<SerialPortEntry>, SerialError> {
    let mut entries = os::list_ports()?;
    sort_ports(&mut entries);
    entries.dedup_by(|a, b| a.device.eq_ignore_ascii_case(&b.device));
    Ok(entries)
}

#[cfg(windows)]
pub use session::open_serial;

/// 「测试连接」：按参数打开串口再立刻关闭，不起读写线程、不发送任何数据。
/// 打开可能卡在有毛病的驱动上，调用方要放进阻塞线程里。失败是分类好的中文原因
/// （被占用 / 不存在 / 已断开 / 参数不被接受 ……）。
#[cfg(windows)]
pub fn probe_serial(options: &zssh_core::model::SerialOptions) -> Result<(), SerialError> {
    let (path, label) = params::normalize_device(&options.device)?;
    // `CommHandle` 离开作用域就关句柄，端口马上还给别的程序。
    let _comm = os::CommHandle::open_comm(&path, &label, options)?;
    Ok(())
}

#[cfg(not(windows))]
pub fn probe_serial(options: &zssh_core::model::SerialOptions) -> Result<(), SerialError> {
    Err(SerialError::Os {
        device: options.device.clone(),
        code: 0,
        what: crate::lang::serial_windows_only(),
    })
}

/// macOS / Linux 首版不做串口：报错而不是返回空列表，界面据此提示“不支持”，
/// 不会把“平台不支持”显示成“这台机器没有串口”。
#[cfg(not(windows))]
pub fn list_ports() -> Result<Vec<SerialPortEntry>, SerialError> {
    Err(SerialError::Os {
        device: String::new(),
        code: 0,
        what: crate::lang::serial_windows_only(),
    })
}

#[cfg(not(windows))]
pub fn open_serial(
    options: &zssh_core::model::SerialOptions,
    _generation: u64,
    _runtime: tokio::runtime::Handle,
) -> Result<
    (
        crate::StreamBackend,
        std::sync::mpsc::Receiver<zssh_session::SessionEvent>,
        std::sync::Arc<zssh_core::ssh::FlowGate>,
    ),
    SerialError,
> {
    Err(SerialError::Os {
        device: options.device.clone(),
        code: 0,
        what: crate::lang::serial_windows_only(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_sort_by_number_not_by_text() {
        let mut ports: Vec<SerialPortEntry> = ["COM10", "COM2", "COM1", "COM11"]
            .into_iter()
            .map(|name| SerialPortEntry {
                device: name.to_owned(),
                display_name: name.to_owned(),
            })
            .collect();
        sort_ports(&mut ports);
        let order: Vec<_> = ports.iter().map(|port| port.device.as_str()).collect();
        assert_eq!(order, ["COM1", "COM2", "COM10", "COM11"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_reports_an_error() {
        assert!(list_ports().is_err());
    }

    /// 本机有没有串口都不算失败：能列出就列出，没有就是空列表。
    #[cfg(windows)]
    #[test]
    fn listing_ports_works_with_or_without_hardware() {
        let ports = list_ports().expect("枚举不该失败");
        eprintln!("本机串口数：{}", ports.len());
        for port in &ports {
            assert!(
                port.device.to_ascii_uppercase().starts_with("COM"),
                "{port:?}"
            );
            assert!(port.display_name.contains(&port.device), "{port:?}");
        }
    }
}
