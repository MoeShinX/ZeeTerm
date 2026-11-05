//! Windows 串口：`CreateFileW` + 重叠 I/O + `SetCommState` / `SetCommTimeouts`，以及设备枚举。
//!
//! 为什么不用 `serialport` crate：它在 Windows 上用同步句柄，读和写共用一个文件对象，
//! 一个读在等数据时写会被挡住（只能靠很短的读超时轮询来缓解，空闲时每秒醒几十次）；它也不支持
//! 1.5 停止位和 Mark / Space 校验，而这两样在 `SerialOptions` 里是有的。这里用重叠 I/O：
//! 读线程挂在 `WaitForMultipleObjects`（数据事件 + 停止事件）上，空闲时不占 CPU、写不受读影响，
//! 关闭时 `CancelIoEx` 立刻唤醒。系统调用本身（打开、DCB、超时、Break）没有硬件就没法实测，
//! 但"重叠读写 + 取消"这一层对任何重叠句柄都一样，测试里用命名管道覆盖。

use std::io;

use windows::Win32::Devices::Communication::{
    COMMTIMEOUTS, ClearCommBreak, DCB, DCB_PARITY, DCB_STOP_BITS, GetCommState, PURGE_COMM_FLAGS,
    PurgeComm, SetCommBreak, SetCommState, SetCommTimeouts, SetupComm,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV, SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegQueryValueExW};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};
use windows::core::{PCWSTR, w};

use super::SerialPortEntry;
use super::params::{
    DcbSettings, OpenStage, SerialError, XOFF_CHAR, XOFF_LIMIT, XON_CHAR, XON_LIMIT,
    classify_open_error, dcb_settings,
};
use zssh_core::model::SerialOptions;

/// 写超时：硬件流控下对端一直不放行（CTS 一直低）时，写不能永远挂着。
const WRITE_TIMEOUT_MS: u32 = 5000;
/// 读的字节间隔超时：到点没有新字节就把已收到的交出去。只在第一个字节到达之后才开始计时，
/// 所以空闲时读会一直等下去，不是轮询。
const READ_INTERVAL_MS: u32 = 5;
/// 驱动的收发缓冲建议值。
const DRIVER_BUFFER: u32 = 16 * 1024;

/// 一个事件对象（重叠 I/O 完成时置位）。每个线程各用各的。
pub struct IoEvent(HANDLE);

unsafe impl Send for IoEvent {}

impl IoEvent {
    pub fn new() -> io::Result<Self> {
        // 手动复位、初始未置位。
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|error| io::Error::from_raw_os_error(win32_code(&error) as i32))?;
        Ok(Self(handle))
    }
}

impl Drop for IoEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 一个以重叠方式打开的句柄加一个"停止"事件。线程之间用 `Arc` 共享；最后一个引用释放时关闭句柄，
/// 串口也就此释放（别的程序随后能再打开它）。
pub struct CommHandle {
    handle: HANDLE,
    stop: HANDLE,
}

unsafe impl Send for CommHandle {}
unsafe impl Sync for CommHandle {}

impl Drop for CommHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.stop);
        }
    }
}

/// 读到了什么。
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Data(usize),
    /// 被 [`CommHandle::stop`] 叫停。
    Stopped,
}

/// 写的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    Stopped,
    /// 写超时（硬件流控一直不放行）。
    TimedOut,
}

fn win32_code(error: &windows::core::Error) -> u32 {
    // HRESULT_FROM_WIN32(code) = 0x8007_0000 | code
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}

fn last_error() -> u32 {
    unsafe { GetLastError().0 }
}

impl CommHandle {
    /// 包一个已经以 `FILE_FLAG_OVERLAPPED` 打开的句柄（测试里是命名管道）。句柄归本对象所有。
    pub fn from_overlapped_handle(handle: HANDLE) -> io::Result<Self> {
        let stop = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|error| io::Error::from_raw_os_error(win32_code(&error) as i32))?;
        Ok(Self { handle, stop })
    }

    /// 打开串口并设好参数。`path` 是 `\\.\COM3` 这种形式（已经整理过）。
    pub fn open_comm(
        path: &str,
        device: &str,
        options: &SerialOptions,
    ) -> Result<Self, SerialError> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // 独占打开（共享模式 0）：别的程序占着时这里失败，错误能分类成"被占用"。
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|error| {
            let code = win32_code(&error);
            classify_open_error(device, OpenStage::Create, code, &error.message())
        })?;
        if handle == INVALID_HANDLE_VALUE {
            return Err(classify_open_error(
                device,
                OpenStage::Create,
                last_error(),
                &crate::lang::serial_open_failed(),
            ));
        }
        // 从这里开始句柄归 `CommHandle`，任何一步失败都会随它关闭。
        let comm = Self::from_overlapped_handle(handle).map_err(|error| SerialError::Os {
            device: device.to_owned(),
            code: error.raw_os_error().unwrap_or(0) as u32,
            what: error.to_string(),
        })?;
        comm.configure(device, &dcb_settings(options))?;
        Ok(comm)
    }

    fn configure(&self, device: &str, settings: &DcbSettings) -> Result<(), SerialError> {
        let fail = |error: windows::core::Error| {
            classify_open_error(
                device,
                OpenStage::Configure,
                win32_code(&error),
                &error.message(),
            )
        };
        unsafe {
            let mut dcb = DCB {
                DCBlength: std::mem::size_of::<DCB>() as u32,
                ..Default::default()
            };
            GetCommState(self.handle, &mut dcb).map_err(fail)?;
            dcb.BaudRate = settings.baud_rate;
            dcb.ByteSize = settings.byte_size;
            dcb.Parity = DCB_PARITY(settings.parity);
            dcb.StopBits = DCB_STOP_BITS(settings.stop_bits);
            dcb._bitfield = settings.flags;
            dcb.XonLim = XON_LIMIT;
            dcb.XoffLim = XOFF_LIMIT;
            dcb.XonChar = XON_CHAR as _;
            dcb.XoffChar = XOFF_CHAR as _;
            SetCommState(self.handle, &dcb).map_err(fail)?;

            // 缓冲建议值：失败了不要紧，驱动用默认的。
            let _ = SetupComm(self.handle, DRIVER_BUFFER, DRIVER_BUFFER);
            let timeouts = COMMTIMEOUTS {
                ReadIntervalTimeout: READ_INTERVAL_MS,
                ReadTotalTimeoutMultiplier: 0,
                ReadTotalTimeoutConstant: 0,
                WriteTotalTimeoutMultiplier: 0,
                WriteTotalTimeoutConstant: WRITE_TIMEOUT_MS,
            };
            SetCommTimeouts(self.handle, &timeouts).map_err(fail)?;
            // 清掉上一个使用者留在驱动缓冲里的残余。
            let _ = PurgeComm(
                self.handle,
                PURGE_COMM_FLAGS(0x0004 | 0x0008 | 0x0001 | 0x0002),
            );
        }
        Ok(())
    }

    /// 叫停所有正在等待的读写（读写线程各自返回 `Stopped`）。可以重复调用。
    pub fn stop(&self) {
        unsafe {
            let _ = SetEvent(self.stop);
        }
    }

    /// 读到至少一个字节为止（或被叫停）。`event` 是调用线程自己的事件。
    pub fn read(&self, event: &IoEvent, buffer: &mut [u8]) -> io::Result<ReadOutcome> {
        unsafe {
            let mut overlapped = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let _ = ResetEvent(event.0);
            let mut transferred = 0u32;
            match ReadFile(
                self.handle,
                Some(buffer),
                Some(&mut transferred),
                Some(&mut overlapped),
            ) {
                Ok(()) => return Ok(ReadOutcome::Data(transferred as usize)),
                Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {}
                Err(error) => return Err(os_error(&error)),
            }
            match self.wait(event, &mut overlapped, &mut transferred)? {
                Waited::Done => Ok(ReadOutcome::Data(transferred as usize)),
                Waited::Stopped => Ok(ReadOutcome::Stopped),
            }
        }
    }

    /// 把 `data` 全部写出去（或被叫停 / 超时）。
    pub fn write_all(&self, event: &IoEvent, mut data: &[u8]) -> io::Result<WriteOutcome> {
        while !data.is_empty() {
            unsafe {
                let mut overlapped = OVERLAPPED {
                    hEvent: event.0,
                    ..Default::default()
                };
                let _ = ResetEvent(event.0);
                let mut transferred = 0u32;
                match WriteFile(
                    self.handle,
                    Some(data),
                    Some(&mut transferred),
                    Some(&mut overlapped),
                ) {
                    Ok(()) => {}
                    Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                        match self.wait(event, &mut overlapped, &mut transferred)? {
                            Waited::Done => {}
                            Waited::Stopped => return Ok(WriteOutcome::Stopped),
                        }
                    }
                    Err(error) => return Err(os_error(&error)),
                }
                if transferred == 0 {
                    // 驱动的写超时：一个字节都没写出去。
                    return Ok(WriteOutcome::TimedOut);
                }
                data = &data[transferred as usize..];
            }
        }
        Ok(WriteOutcome::Written)
    }

    /// 等一个挂起的重叠操作完成，或停止事件置位（那就取消它）。
    ///
    /// # Safety
    /// `overlapped` 和它对应的缓冲在整个等待期间必须有效（调用者的栈帧保证）。
    unsafe fn wait(
        &self,
        event: &IoEvent,
        overlapped: &mut OVERLAPPED,
        transferred: &mut u32,
    ) -> io::Result<Waited> {
        let handles = [event.0, self.stop];
        let signaled = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if signaled == WAIT_OBJECT_0 {
            unsafe { GetOverlappedResult(self.handle, overlapped, transferred, false) }
                .map_err(|error| os_error(&error))?;
            return Ok(Waited::Done);
        }
        // 停止事件（或等待本身出错）：取消，并且等取消真正完成——缓冲在我们的栈上，
        // 操作没结束就返回会让驱动写进已经失效的内存。
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
            let _ = GetOverlappedResult(self.handle, overlapped, transferred, true);
        }
        Ok(Waited::Stopped)
    }

    /// 发送 Break：拉低 TX 线 `hold`，然后恢复。
    pub fn send_break(&self, hold: std::time::Duration) -> io::Result<()> {
        unsafe {
            SetCommBreak(self.handle).map_err(|error| os_error(&error))?;
            std::thread::sleep(hold);
            ClearCommBreak(self.handle).map_err(|error| os_error(&error))?;
        }
        Ok(())
    }
}

enum Waited {
    Done,
    Stopped,
}

fn os_error(error: &windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(win32_code(error) as i32)
}

// ---------------------------------------------------------------- 枚举

/// 设备安装类"端口 (COM 和 LPT)"。
const GUID_DEVCLASS_PORTS: windows::core::GUID =
    windows::core::GUID::from_u128(0x4d36e978_e325_11ce_bfc1_08002be10318);
const SPDRP_FRIENDLYNAME: u32 = 0x0000000C;
const SPDRP_DEVICEDESC: u32 = 0x00000000;

/// 列出本机当前存在的 COM 口（含友好名，如 "USB-SERIAL CH340 (COM3)"）。没有串口就是空列表。
pub fn list_ports() -> Result<Vec<SerialPortEntry>, SerialError> {
    let mut entries = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_PORTS),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )
        .map_err(|error| SerialError::Os {
            device: crate::lang::serial_list_label(),
            code: win32_code(&error),
            what: error.message(),
        })?;
        let mut index = 0u32;
        loop {
            let mut info = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, index, &mut info).is_err() {
                break;
            }
            index += 1;
            let Some(port_name) = port_name(set, &info) else {
                continue;
            };
            // LPT 口等不是串口。
            if !port_name.to_ascii_uppercase().starts_with("COM") {
                continue;
            }
            let friendly = registry_string(set, &info, SPDRP_FRIENDLYNAME)
                .or_else(|| registry_string(set, &info, SPDRP_DEVICEDESC))
                .map(|name| {
                    if name.contains(&port_name) {
                        name
                    } else {
                        format!("{name} ({port_name})")
                    }
                })
                .unwrap_or_else(|| port_name.clone());
            entries.push(SerialPortEntry {
                device: port_name,
                display_name: friendly,
            });
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    Ok(entries)
}

unsafe fn registry_string(
    set: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    info: &SP_DEVINFO_DATA,
    property: u32,
) -> Option<String> {
    let mut buffer = vec![0u8; 1024];
    let mut required = 0u32;
    unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            info,
            SETUP_DI_REGISTRY_PROPERTY(property),
            None,
            Some(&mut buffer),
            Some(&mut required),
        )
    }
    .ok()?;
    let units: Vec<u16> = buffer[..(required as usize).min(buffer.len())]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    let text = String::from_utf16_lossy(&units);
    (!text.trim().is_empty()).then_some(text)
}

/// 设备的 `PortName` 注册表值（"COM3"）。
unsafe fn port_name(
    set: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    info: &SP_DEVINFO_DATA,
) -> Option<String> {
    unsafe {
        let key: HKEY =
            SetupDiOpenDevRegKey(set, info, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0).ok()?;
        let mut buffer = [0u16; 128];
        let mut size = (buffer.len() * 2) as u32;
        let result = RegQueryValueExW(
            key,
            w!("PortName"),
            None,
            None,
            Some(buffer.as_mut_ptr().cast::<u8>()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if result.is_err() {
            return None;
        }
        let length = (size as usize / 2).min(buffer.len());
        let name = String::from_utf16_lossy(&buffer[..length]);
        let name = name.trim_end_matches('\0').trim().to_owned();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(test)]
pub(crate) mod test_pipe {
    //! 命名管道对：两端都是重叠句柄，用来在没有串口硬件时测"重叠读写 + 取消 + 关闭"这一层。

    use std::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::core::PCWSTR;

    use super::CommHandle;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// `(客户端, 服务端)`：客户端扮演"串口"，服务端扮演"设备"。
    pub fn pair() -> (CommHandle, CommHandle) {
        let name = format!(
            r"\\.\pipe\zssh-protocols-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let server = CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                NAMED_PIPE_MODE(PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0),
                1,
                64 * 1024,
                64 * 1024,
                0,
                None,
            );
            assert!(
                !server.is_invalid(),
                "创建命名管道失败：{}",
                super::last_error()
            );
            let client = CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .expect("打开管道客户端");
            // 服务端不需要显式 ConnectNamedPipe：客户端已经连上，读写直接可用。
            (
                CommHandle::from_overlapped_handle(client).expect("包装客户端"),
                CommHandle::from_overlapped_handle(server).expect("包装服务端"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::serial::params::SerialError;

    #[test]
    fn a_pending_read_is_woken_by_stop_and_does_not_poll() {
        let (client, _device) = test_pipe::pair();
        let client = Arc::new(client);
        let reader = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let event = IoEvent::new().unwrap();
                let mut buffer = [0u8; 16];
                let started = Instant::now();
                let outcome = client.read(&event, &mut buffer).unwrap();
                (outcome, started.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        client.stop();
        let (outcome, waited) = reader.join().unwrap();
        assert_eq!(outcome, ReadOutcome::Stopped);
        assert!(
            waited >= Duration::from_millis(250),
            "读在没有数据时应当一直等：{waited:?}"
        );
        assert!(
            waited < Duration::from_secs(3),
            "stop 之后应当立刻醒：{waited:?}"
        );
    }

    #[test]
    fn reads_and_writes_run_concurrently_on_one_handle() {
        // 读线程挂着等数据的同时，写不能被挡住（同步句柄做不到这一点）。
        let (client, device) = test_pipe::pair();
        let client = Arc::new(client);
        let device = Arc::new(device);
        let reader = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let event = IoEvent::new().unwrap();
                let mut buffer = [0u8; 16];
                client
                    .read(&event, &mut buffer)
                    .map(|outcome| (outcome, buffer))
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        let write_event = IoEvent::new().unwrap();
        let started = Instant::now();
        assert_eq!(
            client.write_all(&write_event, b"ping").unwrap(),
            WriteOutcome::Written
        );
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "写被读挡住了"
        );
        let device_event = IoEvent::new().unwrap();
        let mut got = [0u8; 8];
        assert_eq!(
            device.read(&device_event, &mut got).unwrap(),
            ReadOutcome::Data(4)
        );
        assert_eq!(&got[..4], b"ping");
        device.write_all(&device_event, b"pong").unwrap();
        let (outcome, buffer) = reader.join().unwrap().unwrap();
        assert_eq!(outcome, ReadOutcome::Data(4));
        assert_eq!(&buffer[..4], b"pong");
    }

    #[test]
    fn a_closed_peer_is_an_error_not_a_hang() {
        let (client, device) = test_pipe::pair();
        drop(device);
        let event = IoEvent::new().unwrap();
        let mut buffer = [0u8; 8];
        assert!(client.read(&event, &mut buffer).is_err());
    }

    #[test]
    fn a_missing_com_port_is_reported_as_not_found() {
        let error = CommHandle::open_comm(r"\\.\COM9999", "COM9999", &SerialOptions::default())
            .err()
            .expect("COM9999 不该存在");
        assert_eq!(
            error,
            SerialError::NotFound {
                device: "COM9999".to_owned()
            }
        );
        assert!(error.to_string().starts_with("COM9999 不存在"));
    }

    #[test]
    fn a_device_that_is_not_a_serial_port_is_rejected_after_opening() {
        // NUL 设备能以重叠方式打开，但没有 DCB：GetCommState 失败。
        let error = CommHandle::open_comm(r"\\.\NUL", "NUL", &SerialOptions::default())
            .err()
            .expect("NUL 不是串口");
        assert!(
            matches!(
                error,
                SerialError::NotASerialPort { .. } | SerialError::Os { .. }
            ),
            "{error:?}"
        );
        eprintln!("NUL 当串口打开的错误：{error}");
    }
}
