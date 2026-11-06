//! 本机串口会话后端（Windows 与 Linux）。
//!
//! - [`params`]：设备名整理、`SerialOptions` → DCB 的映射、错误分类（纯函数，可完整单测）。
//! - `os`：系统相关的部分——打开 / 配置、读写与叫停、设备枚举。Windows 是 Win32 重叠 I/O
//!   （`os.rs`），Linux 是 termios + poll（`unix.rs`），两边对外接口相同。
//! - `session`：读线程 + 写线程，接到 `SessionBackend` / `SessionEvent` / `FlowGate`，两个平台共用。
//!
//! 依赖选择见 `os.rs` / `unix.rs` 顶部：直接用系统接口，不引入 `serialport` crate。

pub mod params;

#[cfg(windows)]
mod os;
#[cfg(target_os = "linux")]
#[path = "unix.rs"]
mod os;
#[cfg(any(windows, target_os = "linux"))]
mod session;
#[cfg(test)]
#[cfg(target_os = "linux")]
mod unix_tests;

pub use params::SerialError;

/// 本机的一个串口，给界面的下拉列表用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortEntry {
    /// 设备名，存进主机设置的就是它，如 `COM3`、`/dev/ttyUSB0`。
    pub device: String,
    /// 给人看的名字，如 `USB-SERIAL CH340 (COM3)`；取不到友好名时就是设备名。
    pub display_name: String,
}

/// COM 口按编号排（COM2 在 COM10 前面）。
#[cfg(windows)]
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

/// 设备文件按「前缀字母 + 结尾数字」自然排序（ttyS2 在 ttyS10 前面）。
#[cfg(target_os = "linux")]
fn sort_ports(entries: &mut [SerialPortEntry]) {
    entries.sort_by_key(|entry| params::unix_port_sort_key(&entry.device));
}

/// 列出本机当前存在的串口。本机没有串口时返回空列表，不算错误。
#[cfg(any(windows, target_os = "linux"))]
pub fn list_ports() -> Result<Vec<SerialPortEntry>, SerialError> {
    let mut entries = os::list_ports()?;
    sort_ports(&mut entries);
    // Windows 的设备名不分大小写；Linux 的路径分。
    #[cfg(windows)]
    entries.dedup_by(|a, b| a.device.eq_ignore_ascii_case(&b.device));
    #[cfg(not(windows))]
    entries.dedup_by(|a, b| a.device == b.device);
    Ok(entries)
}

#[cfg(any(windows, target_os = "linux"))]
pub use session::open_serial;

/// 「测试连接」：按参数打开串口再立刻关闭，不起读写线程、不发送任何数据。
/// 打开可能卡在有毛病的驱动上，调用方要放进阻塞线程里。失败是分类好的中文原因
/// （被占用 / 不存在 / 已断开 / 参数不被接受 / 没有权限 ……）。
#[cfg(any(windows, target_os = "linux"))]
pub fn probe_serial(options: &zssh_core::model::SerialOptions) -> Result<(), SerialError> {
    let (path, label) = params::normalize_device_for_os(&options.device)?;
    // `CommHandle` 离开作用域就关句柄，端口马上还给别的程序。
    let _comm = os::CommHandle::open_comm(&path, &label, options)?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn probe_serial(options: &zssh_core::model::SerialOptions) -> Result<(), SerialError> {
    Err(SerialError::Os {
        device: options.device.clone(),
        code: 0,
        what: crate::lang::serial_windows_only(),
    })
}

/// 其他平台没有实现（ZeeTerm 只面向 Windows 和 Linux）；保留签名让 crate 在别处也能编译。
#[cfg(not(any(windows, target_os = "linux")))]
pub fn list_ports() -> Result<Vec<SerialPortEntry>, SerialError> {
    Ok(Vec::new())
}

#[cfg(not(any(windows, target_os = "linux")))]
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

#[cfg(all(test, windows))]
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

    /// 本机有没有串口都不算失败：能列出就列出，没有就是空列表。
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
