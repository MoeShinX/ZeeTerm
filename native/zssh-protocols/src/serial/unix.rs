//! Linux 串口：`open(O_NONBLOCK)` + `termios` + `poll`，以及基于 sysfs 的设备枚举。
//!
//! 为什么不用 `serialport` crate：它在 Linux 上默认依赖 libudev（打包要多带一个系统库，
//! 容器 / 精简系统里没有就编不过），关掉默认特性之后枚举又要自己写；而这里要的东西很少——
//! 打开、`termios`、`poll`、`TIOCEXCL`——直接用已经在依赖树里的 `libc` 就够了，
//! 还能做到「关闭时立刻唤醒、空闲时不占 CPU」（`poll` 同时等串口和一个 eventfd）。
//!
//! 对外接口和 `os.rs`（Windows）一致，`session.rs` 两边共用：
//! 读线程挂在 `poll(串口, 停止)` 上，写线程在内核缓冲写满时才 `poll(可写, 停止)`；
//! 关闭时往 eventfd 里写一个数，两边的等待立刻返回。
//!
//! 拔出 USB 串口时内核会对这个 tty 做 hangup：之后 `read` 返回 0、`write` 返回 `EIO`、`poll`
//! 永远立即就绪。所以读到 0 字节一律当成「设备已断开」报出去，而不是再读一次（那样会空转）。

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use zssh_core::model::{
    SerialDataBits, SerialFlowControl, SerialOptions, SerialParity, SerialStopBits,
};

use super::SerialPortEntry;
use super::params::{
    DEFAULT_SERIAL_GROUP, ERRNO_EBUSY, ERRNO_EIO, OpenStage, SerialError, XOFF_CHAR, XON_CHAR,
    classify_errno, friendly_name_from_by_id,
};

/// 写超时：硬件流控下对端一直不放行（CTS 一直低）时，写不能永远挂着。和 Windows 一致。
const WRITE_TIMEOUT: Duration = Duration::from_millis(5000);

/// Windows 版需要每个线程一个事件对象；这里 `poll` 不需要，留着同名类型让 `session.rs` 两边共用。
pub struct IoEvent;

impl IoEvent {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
}

/// 一个已打开的串口加一个「停止」eventfd。线程之间用 `Arc` 共享；最后一个引用释放时关闭 fd，
/// 串口也就此释放（独占标志随之消失，别的程序随后能再打开它）。
pub struct CommHandle {
    fd: OwnedFd,
    stop: OwnedFd,
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

enum Waited {
    /// 串口那一路有事件（`revents` 原样带回，可能含 HUP / ERR）。
    Ready(i16),
    Stopped,
    TimedOut,
}

fn last_errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn new_stop_event() -> io::Result<OwnedFd> {
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

// ---------------------------------------------------------------- termios 映射（纯计算）

/// 帧格式 / 流控落到 termios 上的位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TermiosBits {
    /// `c_cflag` 里属于帧格式 / 流控的那些位（数据位、停止位、校验、RTS/CTS）。
    pub cflag: libc::tcflag_t,
    /// `c_iflag` 里的软件流控位。
    pub iflag: libc::tcflag_t,
}

/// `c_cflag` 里由选项决定的全部位：写之前先清掉，写之后读回来核对。
pub(super) const FRAMING_MASK: libc::tcflag_t =
    libc::CSIZE | libc::CSTOPB | libc::PARENB | libc::PARODD | libc::CMSPAR | libc::CRTSCTS;
const SOFT_FLOW_MASK: libc::tcflag_t = libc::IXON | libc::IXOFF | libc::IXANY;

/// 选项 → termios 位。
///
/// - 停止位：`CSTOPB` 在 5 位数据位下是 1.5 个、其余是 2 个，所以 5 位配 2 个、非 5 位配 1.5 个
///   是没法表达的组合（核心库的校验同样拒绝它们），这里返回 `None`；
/// - Mark / Space 校验用 `CMSPAR`（固定为 1 / 0 的校验位）；
/// - 硬件流控 `CRTSCTS`，软件流控 `IXON | IXOFF`（收发两个方向都开，和 Windows 版一样）。
///   输入端不开 `INPCK`：有校验错误的字节照样交给终端，和 Windows 的行为一致。
pub(super) fn termios_bits(options: &SerialOptions) -> Option<TermiosBits> {
    let mut cflag = match options.data_bits {
        SerialDataBits::Five => libc::CS5,
        SerialDataBits::Six => libc::CS6,
        SerialDataBits::Seven => libc::CS7,
        SerialDataBits::Eight => libc::CS8,
    };
    match (options.data_bits, options.stop_bits) {
        (_, SerialStopBits::One) => {}
        (SerialDataBits::Five, SerialStopBits::OnePointFive) => cflag |= libc::CSTOPB,
        (SerialDataBits::Five, SerialStopBits::Two) => return None,
        (_, SerialStopBits::OnePointFive) => return None,
        (_, SerialStopBits::Two) => cflag |= libc::CSTOPB,
    }
    cflag |= match options.parity {
        SerialParity::None => 0,
        SerialParity::Even => libc::PARENB,
        SerialParity::Odd => libc::PARENB | libc::PARODD,
        SerialParity::Space => libc::PARENB | libc::CMSPAR,
        SerialParity::Mark => libc::PARENB | libc::PARODD | libc::CMSPAR,
    };
    let mut iflag = 0;
    match options.flow_control {
        SerialFlowControl::None => {}
        SerialFlowControl::Software => iflag |= libc::IXON | libc::IXOFF,
        SerialFlowControl::Hardware => cflag |= libc::CRTSCTS,
    }
    Some(TermiosBits { cflag, iflag })
}

/// 标准波特率 → `Bxxxx` 常量；不在表里的返回 `None`（走 `termios2` 的任意波特率）。
pub(super) fn standard_speed(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        50 => libc::B50,
        75 => libc::B75,
        110 => libc::B110,
        134 => libc::B134,
        150 => libc::B150,
        200 => libc::B200,
        300 => libc::B300,
        600 => libc::B600,
        1200 => libc::B1200,
        1800 => libc::B1800,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19_200 => libc::B19200,
        38_400 => libc::B38400,
        57_600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        460_800 => libc::B460800,
        500_000 => libc::B500000,
        576_000 => libc::B576000,
        921_600 => libc::B921600,
        1_000_000 => libc::B1000000,
        1_152_000 => libc::B1152000,
        1_500_000 => libc::B1500000,
        2_000_000 => libc::B2000000,
        2_500_000 => libc::B2500000,
        3_000_000 => libc::B3000000,
        3_500_000 => libc::B3500000,
        4_000_000 => libc::B4000000,
        _ => return None,
    })
}

/// 设备文件所属的组名（权限提示里用）。取不到、或者属于 root 组（对用户没有指导意义）时返回 `None`。
fn device_group_name(path: &CStr) -> Option<String> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(path.as_ptr(), &mut stat) } != 0 || stat.st_gid == 0 {
        return None;
    }
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 2048];
    let mut found: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrgid_r(
            stat.st_gid,
            &mut group,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() || group.gr_name.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(group.gr_name) }.to_str().ok()?;
    (!name.is_empty()).then(|| name.to_owned())
}

impl CommHandle {
    /// 包一个已经打开的 fd（测试里是伪终端的主端）。设成非阻塞，归本对象所有。
    #[cfg(test)]
    pub(super) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(fd.as_raw_fd())?;
        Ok(Self {
            fd,
            stop: new_stop_event()?,
        })
    }

    /// 打开串口并设好参数。`path` 是整理过的设备路径，`device` 是给用户看的名字（两者在 Linux 上相同）。
    pub fn open_comm(
        path: &str,
        device: &str,
        options: &SerialOptions,
    ) -> Result<Self, SerialError> {
        let os_error = |error: io::Error| SerialError::Os {
            device: device.to_owned(),
            code: error.raw_os_error().unwrap_or(0) as u32,
            what: error.to_string(),
        };
        let c_path = CString::new(path).map_err(|_| SerialError::InvalidName(device.to_owned()))?;
        // `O_NOCTTY`：别让串口变成本进程的控制终端；`O_NONBLOCK`：打开不等载波，读写靠 poll。
        let raw = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            let error = io::Error::last_os_error();
            let errno = error.raw_os_error().unwrap_or(0);
            let group =
                if errno == super::params::ERRNO_EACCES || errno == super::params::ERRNO_EPERM {
                    device_group_name(&c_path)
                } else {
                    None
                };
            return Err(classify_errno(
                device,
                OpenStage::Create,
                errno,
                &error.to_string(),
                group.as_deref().unwrap_or(DEFAULT_SERIAL_GROUP),
            ));
        }
        // 从这里开始 fd 归 `CommHandle`，任何一步失败都会随它关闭。
        let comm = Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            stop: new_stop_event().map_err(os_error)?,
        };
        comm.configure(path, device, options)?;
        Ok(comm)
    }

    fn configure(
        &self,
        path: &str,
        device: &str,
        options: &SerialOptions,
    ) -> Result<(), SerialError> {
        let fail = |errno: i32| {
            classify_errno(
                device,
                OpenStage::Configure,
                errno,
                &io::Error::from_raw_os_error(errno).to_string(),
                DEFAULT_SERIAL_GROUP,
            )
        };
        let fd = self.fd.as_raw_fd();

        // 必须是字符设备、而且是 tty：否则不是串口（普通文件、块设备、管道……），
        // 在做任何改动之前就拒绝。
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(fail(last_errno()));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFCHR || unsafe { libc::isatty(fd) } != 1 {
            return Err(SerialError::NotASerialPort {
                device: device.to_owned(),
            });
        }

        // 独占：之后别的进程再打开会得到 EBUSY（除非它是 root）。失败（比如驱动不支持）不致命，
        // 但 EBUSY 说明别人已经独占了它。
        if unsafe { libc::ioctl(fd, libc::TIOCEXCL) } != 0 && last_errno() == ERRNO_EBUSY {
            return Err(SerialError::Busy {
                device: device.to_owned(),
            });
        }

        let Some(bits) = termios_bits(options) else {
            return Err(SerialError::UnsupportedSettings {
                device: device.to_owned(),
            });
        };
        let speed = standard_speed(options.baud_rate);

        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return Err(fail(last_errno()));
        }
        // 原始模式：没有回显、没有行规程、没有输入输出转换、没有信号字符。
        unsafe { libc::cfmakeraw(&mut term) };
        term.c_cflag &= !FRAMING_MASK;
        // CLOCAL：不理会 DCD 载波线（Windows 版同样不看它），否则载波一掉内核就会挂断这个 tty。
        term.c_cflag |= bits.cflag | libc::CLOCAL | libc::CREAD;
        term.c_iflag &= !SOFT_FLOW_MASK;
        term.c_iflag |= bits.iflag;
        term.c_cc[libc::VMIN] = 1;
        term.c_cc[libc::VTIME] = 0;
        term.c_cc[libc::VSTART] = XON_CHAR;
        term.c_cc[libc::VSTOP] = XOFF_CHAR;
        // 任意波特率先放一个标准值占位，下面再用 termios2 改。
        if unsafe { libc::cfsetspeed(&mut term, speed.unwrap_or(libc::B38400)) } != 0 {
            return Err(fail(last_errno()));
        }
        // tcsetattr 只要有「任何一项」改成功就返回 0，所以事后要读回来核对。
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
            return Err(fail(last_errno()));
        }
        if speed.is_none() {
            set_custom_baud(fd, options.baud_rate)
                .map_err(|error| fail(error.raw_os_error().unwrap_or(0)))?;
        }

        let mut applied: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut applied) } != 0 {
            return Err(fail(last_errno()));
        }
        // 伪终端（socat 之类的虚拟串口）的从端内核固定成 8 位无校验，数据位和校验没法核对。
        let mask = if path.starts_with("/dev/pts/") {
            libc::CSTOPB | libc::CRTSCTS
        } else {
            FRAMING_MASK
        };
        if applied.c_cflag & mask != bits.cflag & mask
            || applied.c_iflag & SOFT_FLOW_MASK != bits.iflag
        {
            return Err(SerialError::UnsupportedSettings {
                device: device.to_owned(),
            });
        }

        // DTR 一律拉高（大多数 USB 转串口和单片机要 DTR 才开始说话）；RTS 在硬件流控下归驱动管，
        // 其余情况拉高。不支持调制解调器控制线的设备（伪终端、部分 CDC）会失败，不要紧。
        let mut lines = libc::TIOCM_DTR;
        if options.flow_control != SerialFlowControl::Hardware {
            lines |= libc::TIOCM_RTS;
        }
        let _ = unsafe { libc::ioctl(fd, libc::TIOCMBIS, &lines) };
        // 清掉上一个使用者留在驱动缓冲里的残余。
        let _ = unsafe { libc::tcflush(fd, libc::TCIOFLUSH) };
        Ok(())
    }

    /// 叫停所有正在等待的读写（读写线程各自返回 `Stopped`）。可以重复调用。
    pub fn stop(&self) {
        let one: u64 = 1;
        let _ = unsafe {
            libc::write(
                self.stop.as_raw_fd(),
                (&raw const one).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// 等串口出现 `events`，或者被叫停，或者超时（`None` = 一直等）。
    fn wait(&self, events: i16, timeout: Option<Duration>) -> io::Result<Waited> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.fd.as_raw_fd(),
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let millis = match deadline {
                None => -1,
                Some(deadline) => deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128) as i32,
            };
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, millis) };
            if ready < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
            if fds[1].revents != 0 {
                return Ok(Waited::Stopped);
            }
            if ready == 0 {
                return Ok(Waited::TimedOut);
            }
            return Ok(Waited::Ready(fds[0].revents));
        }
    }

    /// 读到至少一个字节为止（或被叫停）。`_event` 只为和 Windows 版同一个签名。
    pub fn read(&self, _event: &IoEvent, buffer: &mut [u8]) -> io::Result<ReadOutcome> {
        loop {
            let revents = match self.wait(libc::POLLIN, None)? {
                Waited::Stopped => return Ok(ReadOutcome::Stopped),
                Waited::TimedOut => continue,
                Waited::Ready(revents) => revents,
            };
            let count = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            if count > 0 {
                return Ok(ReadOutcome::Data(count as usize));
            }
            if count == 0 {
                // 非阻塞 tty 没数据时是 EAGAIN，不会是 0；0 就是被挂断（USB 串口拔出）。
                return Err(io::Error::from_raw_os_error(ERRNO_EIO));
            }
            match last_errno() {
                libc::EINTR => {}
                libc::EAGAIN => {
                    // poll 说有事件但读不到数据，又带着 HUP / ERR：设备没了，别空转。
                    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                        return Err(io::Error::from_raw_os_error(ERRNO_EIO));
                    }
                }
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }

    /// 把 `data` 全部写出去（或被叫停 / 超时）。
    pub fn write_all(&self, event: &IoEvent, data: &[u8]) -> io::Result<WriteOutcome> {
        self.write_all_within(event, data, WRITE_TIMEOUT)
    }

    /// [`Self::write_all`]，写超时可调（测试里缩短）。
    pub(super) fn write_all_within(
        &self,
        _event: &IoEvent,
        mut data: &[u8],
        timeout: Duration,
    ) -> io::Result<WriteOutcome> {
        while !data.is_empty() {
            let count = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    data.as_ptr().cast::<libc::c_void>(),
                    data.len(),
                )
            };
            if count > 0 {
                data = &data[count as usize..];
                continue;
            }
            if count == 0 {
                return Err(io::Error::from_raw_os_error(ERRNO_EIO));
            }
            match last_errno() {
                libc::EINTR => {}
                // 内核发送缓冲满了（流控挡着、或者波特率慢）：等它可写；一直不放行就按超时算。
                libc::EAGAIN => match self.wait(libc::POLLOUT, Some(timeout))? {
                    Waited::Stopped => return Ok(WriteOutcome::Stopped),
                    Waited::TimedOut => return Ok(WriteOutcome::TimedOut),
                    Waited::Ready(_) => {}
                },
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(WriteOutcome::Written)
    }

    /// 发送 Break：拉低 TX 线 `hold`，然后恢复。
    pub fn send_break(&self, hold: Duration) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        if unsafe { libc::ioctl(fd, libc::TIOCSBRK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        std::thread::sleep(hold);
        if unsafe { libc::ioctl(fd, libc::TIOCCBRK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 读回当前的 termios（测试核对用）。
    #[cfg(test)]
    pub(super) fn termios(&self) -> io::Result<libc::termios> {
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(self.fd.as_raw_fd(), &mut term) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(term)
    }

    /// 读回当前的 termios2（带真实波特率，测试核对用）。
    #[cfg(test)]
    pub(super) fn termios2(&self) -> io::Result<libc::termios2> {
        let mut term: libc::termios2 = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TCGETS2, &mut term) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(term)
    }

    #[cfg(test)]
    pub(super) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 非标准波特率：`termios2` 的 `BOTHER`，由驱动按最接近的分频实现（内核不支持的值会得到 `EINVAL`）。
fn set_custom_baud(fd: RawFd, baud: u32) -> io::Result<()> {
    let mut term: libc::termios2 = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TCGETS2, &mut term) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // 输入波特率位（CIBAUD）清零 = 输入速率跟随输出速率。
    term.c_cflag &= !(libc::CBAUD | libc::CIBAUD);
    term.c_cflag |= libc::BOTHER;
    term.c_ispeed = baud;
    term.c_ospeed = baud;
    if unsafe { libc::ioctl(fd, libc::TCSETS2, &term) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------- 枚举

/// 列出本机当前存在的串口（读 `/sys/class/tty`）。没有串口就是空列表；读不了 sysfs（容器里常见）也当成空。
pub fn list_ports() -> Result<Vec<SerialPortEntry>, SerialError> {
    Ok(scan_ports(Path::new("/sys/class/tty"), Path::new("/dev")))
}

/// 在 `sys_tty`（`/sys/class/tty`）和 `dev`（`/dev`）两个根目录下找串口；根目录做成参数，测试里用构造出来的目录树。
///
/// 真实串口的判据是 sysfs 里有 `device` 链接（虚拟控制台 `tty0..tty63`、`console`、`ptmx` 都没有）；
/// 老式 `ttyS*` 例外地要再看一眼：内核给每个可能的 UART 都建了 `ttyS0..ttyS31` 并带 `device` 链接，
/// 但只有 `type` 不为 0 的才是真的有硬件。设备文件 `/dev/<名字>` 不存在的（udev 还没建好）也不列。
pub(super) fn scan_ports(sys_tty: &Path, dev: &Path) -> Vec<SerialPortEntry> {
    let Ok(read_dir) = std::fs::read_dir(sys_tty) else {
        return Vec::new();
    };
    let by_id = by_id_names(dev);
    let mut entries = Vec::new();
    for item in read_dir.flatten() {
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_real_port(sys_tty, dev, &name) {
            continue;
        }
        let device = dev.join(&name).to_string_lossy().into_owned();
        let display_name = match by_id.iter().find(|(tty, _)| *tty == name) {
            Some((_, id)) => format!("{} ({device})", friendly_name_from_by_id(id)),
            None => device.clone(),
        };
        entries.push(SerialPortEntry {
            device,
            display_name,
        });
    }
    entries
}

fn is_real_port(sys_tty: &Path, dev: &Path, name: &str) -> bool {
    let Some(rest) = name.strip_prefix("tty") else {
        return false;
    };
    // `tty`、`tty0`…`tty63` 是虚拟控制台。
    if rest.is_empty() || rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let class_dir = sys_tty.join(name);
    if std::fs::symlink_metadata(class_dir.join("device")).is_err() {
        return false;
    }
    let legacy_uart = name
        .strip_prefix("ttyS")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit()));
    if legacy_uart
        && std::fs::read_to_string(class_dir.join("type"))
            .is_ok_and(|text| text.trim().parse::<u32>() == Ok(0))
    {
        return false;
    }
    std::fs::symlink_metadata(dev.join(name)).is_ok()
}

/// `/dev/serial/by-id/` 里 `(tty 名字, by-id 名字)` 的对应，按 by-id 名字排序（同一个 tty 有几个别名时取第一个）。
fn by_id_names(dev: &Path) -> Vec<(String, String)> {
    let Ok(read_dir) = std::fs::read_dir(dev.join("serial/by-id")) else {
        return Vec::new();
    };
    let mut pairs: Vec<(String, String)> = read_dir
        .flatten()
        .filter_map(|item| {
            let id = item.file_name().to_str()?.to_owned();
            let target = std::fs::read_link(item.path()).ok()?;
            let tty = target.file_name()?.to_str()?.to_owned();
            Some((tty, id))
        })
        .collect();
    pairs.sort_by(|a, b| a.1.cmp(&b.1));
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|(tty, _)| seen.insert(tty.clone()));
    pairs
}
