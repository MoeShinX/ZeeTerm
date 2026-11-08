//! Linux 串口的测试。没有真串口，就用伪终端（pty）当假串口：从端交给 `CommHandle::open_comm`
//! 当「串口」，主端留在测试手里扮演「设备」。
//!
//! 伪终端和真串口的差别：内核把 pty 从端的数据位 / 校验固定成 8N，调制解调器控制线（DTR / RTS / Break）
//! 也不支持，所以数据位和校验靠 `termios_bits` 的纯计算测试覆盖，波特率 / 停止位 / 流控 / 原始模式
//! 才能靠 `tcgetattr` 读回来对。

use std::ffi::CStr;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zssh_core::model::{
    SerialDataBits, SerialFlowControl, SerialNewline, SerialOptions, SerialParity, SerialStopBits,
};
use zssh_core::ssh::FlowGate;
use zssh_session::{SessionBackend, SessionCommand, SessionEvent};

use super::os::{CommHandle, IoEvent, ReadOutcome, WriteOutcome};
use super::os::{FRAMING_MASK, scan_ports, standard_speed, termios_bits};
use super::params::SerialError;
use super::session::start;
use super::{SerialPortEntry, sort_ports};
use crate::backend::StreamBackend;

const WAIT: Duration = Duration::from_secs(5);

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---------------------------------------------------------------- 伪终端

struct Pty {
    /// 「设备」那一端。
    master: CommHandle,
    /// 「串口」那一端的设备路径（`/dev/pts/N`）。
    slave_path: String,
}

fn open_pty() -> Pty {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt 失败：{}", errno());
        let master = OwnedFd::from_raw_fd(master);
        let raw = std::os::fd::AsRawFd::as_raw_fd(&master);
        assert_eq!(libc::grantpt(raw), 0);
        assert_eq!(libc::unlockpt(raw), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(raw, name.as_mut_ptr(), name.len()), 0);
        let slave_path = CStr::from_ptr(name.as_ptr()).to_str().unwrap().to_owned();
        Pty {
            master: CommHandle::from_fd(master).unwrap(),
            slave_path,
        }
    }
}

impl Pty {
    /// 按 `options` 把从端当串口打开。
    fn open(&self, options: &SerialOptions) -> Result<CommHandle, SerialError> {
        CommHandle::open_comm(&self.slave_path, &self.slave_path, options)
    }
}

/// 设备端读，直到收到 `needle`（读不到就等，最多 `WAIT`）。
fn device_read_until(master: &CommHandle, needle: &[u8]) -> Vec<u8> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + WAIT;
    while !seen.windows(needle.len()).any(|window| window == needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "设备端等不到 {needle:?}，收到 {:?}",
            String::from_utf8_lossy(&seen)
        );
        let mut pfd = libc::pollfd {
            fd: master.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(200) as i32) };
        if ready <= 0 {
            continue;
        }
        let mut buffer = [0u8; 4096];
        let count =
            unsafe { libc::read(master.raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        assert!(
            count > 0 || errno() == libc::EAGAIN,
            "设备端读失败：{}",
            errno()
        );
        if count > 0 {
            seen.extend_from_slice(&buffer[..count as usize]);
        }
    }
    seen
}

fn device_write(master: &CommHandle, bytes: &[u8]) {
    assert_eq!(
        master.write_all(&IoEvent, bytes).unwrap(),
        WriteOutcome::Written
    );
}

/// 从端被关闭之后，主端读得到 EIO / 挂断。
fn wait_for_slave_closed(master: &CommHandle) {
    let deadline = Instant::now() + WAIT;
    loop {
        let mut pfd = libc::pollfd {
            fd: master.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready > 0 && pfd.revents & libc::POLLHUP != 0 {
            return;
        }
        let mut buffer = [0u8; 64];
        let count =
            unsafe { libc::read(master.raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 && errno() == libc::EIO {
            return;
        }
        assert!(Instant::now() < deadline, "关闭后串口一直没释放");
    }
}

fn opts() -> SerialOptions {
    SerialOptions::default()
}

// ---------------------------------------------------------------- 纯计算

#[test]
fn every_parity_stop_bit_and_data_bit_value_maps_to_its_termios_bits() {
    let bits = |options: SerialOptions| termios_bits(&options).unwrap();
    let base = bits(opts());
    assert_eq!(base.cflag, libc::CS8);
    assert_eq!(base.iflag, 0);

    for (parity, expected) in [
        (SerialParity::None, 0),
        (SerialParity::Even, libc::PARENB),
        (SerialParity::Odd, libc::PARENB | libc::PARODD),
        (SerialParity::Space, libc::PARENB | libc::CMSPAR),
        (
            SerialParity::Mark,
            libc::PARENB | libc::PARODD | libc::CMSPAR,
        ),
    ] {
        let got = bits(SerialOptions { parity, ..opts() });
        assert_eq!(got.cflag, libc::CS8 | expected, "{parity:?}");
    }
    for (data_bits, expected) in [
        (SerialDataBits::Five, libc::CS5),
        (SerialDataBits::Six, libc::CS6),
        (SerialDataBits::Seven, libc::CS7),
        (SerialDataBits::Eight, libc::CS8),
    ] {
        let got = bits(SerialOptions {
            data_bits,
            ..opts()
        });
        assert_eq!(got.cflag, expected, "{data_bits:?}");
        assert_eq!(got.cflag & !libc::CSIZE, 0);
    }
    // 2 个停止位只配 6~8 位数据位，1.5 个只配 5 位。
    assert_eq!(
        bits(SerialOptions {
            stop_bits: SerialStopBits::Two,
            ..opts()
        })
        .cflag,
        libc::CS8 | libc::CSTOPB
    );
    assert_eq!(
        bits(SerialOptions {
            data_bits: SerialDataBits::Five,
            stop_bits: SerialStopBits::OnePointFive,
            ..opts()
        })
        .cflag,
        libc::CS5 | libc::CSTOPB
    );
    for (data_bits, stop_bits) in [
        (SerialDataBits::Five, SerialStopBits::Two),
        (SerialDataBits::Six, SerialStopBits::OnePointFive),
        (SerialDataBits::Seven, SerialStopBits::OnePointFive),
        (SerialDataBits::Eight, SerialStopBits::OnePointFive),
    ] {
        assert_eq!(
            termios_bits(&SerialOptions {
                data_bits,
                stop_bits,
                ..opts()
            }),
            None,
            "{data_bits:?}/{stop_bits:?}"
        );
    }
}

#[test]
fn flow_control_maps_to_crtscts_or_xon_xoff_and_every_bit_is_inside_the_mask() {
    let hardware = termios_bits(&SerialOptions {
        flow_control: SerialFlowControl::Hardware,
        ..opts()
    })
    .unwrap();
    assert_eq!(hardware.cflag, libc::CS8 | libc::CRTSCTS);
    assert_eq!(hardware.iflag, 0);
    let software = termios_bits(&SerialOptions {
        flow_control: SerialFlowControl::Software,
        ..opts()
    })
    .unwrap();
    assert_eq!(software.cflag, libc::CS8);
    assert_eq!(software.iflag, libc::IXON | libc::IXOFF);
    // 写之前清掉、读回来核对用的掩码必须盖住所有会被设置的 cflag 位。
    for parity in [
        SerialParity::None,
        SerialParity::Odd,
        SerialParity::Even,
        SerialParity::Mark,
        SerialParity::Space,
    ] {
        let got = termios_bits(&SerialOptions {
            parity,
            flow_control: SerialFlowControl::Hardware,
            stop_bits: SerialStopBits::Two,
            ..opts()
        })
        .unwrap();
        assert_eq!(got.cflag & !FRAMING_MASK, 0);
    }
}

#[test]
fn standard_baud_rates_use_the_b_constants_and_the_rest_go_through_termios2() {
    assert_eq!(standard_speed(9600), Some(libc::B9600));
    assert_eq!(standard_speed(115_200), Some(libc::B115200));
    assert_eq!(standard_speed(921_600), Some(libc::B921600));
    assert_eq!(standard_speed(4_000_000), Some(libc::B4000000));
    assert_eq!(standard_speed(50), Some(libc::B50));
    for custom in [0, 1, 250_000, 123_456, 4_000_001, 5_000_000] {
        assert_eq!(standard_speed(custom), None, "{custom}");
    }
}

// ---------------------------------------------------------------- 打开与参数（伪终端）

#[test]
fn opening_applies_raw_mode_and_the_line_settings() {
    let pty = open_pty();
    let comm = pty
        .open(&SerialOptions {
            baud_rate: 9600,
            stop_bits: SerialStopBits::Two,
            flow_control: SerialFlowControl::Hardware,
            ..opts()
        })
        .unwrap();
    let term = comm.termios().unwrap();
    // 原始模式：没有行规程、回显、信号字符和输出转换。
    assert_eq!(
        term.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN),
        0
    );
    assert_eq!(term.c_oflag & libc::OPOST, 0);
    assert_eq!(
        term.c_iflag & (libc::ICRNL | libc::INLCR | libc::IGNCR | libc::ISTRIP | libc::IXON),
        0
    );
    assert_eq!(term.c_cflag & libc::CLOCAL, libc::CLOCAL);
    assert_eq!(term.c_cflag & libc::CREAD, libc::CREAD);
    assert_eq!(term.c_cflag & libc::CSTOPB, libc::CSTOPB);
    assert_eq!(term.c_cflag & libc::CRTSCTS, libc::CRTSCTS);
    assert_eq!(unsafe { libc::cfgetospeed(&term) }, libc::B9600);
    assert_eq!(unsafe { libc::cfgetispeed(&term) }, libc::B9600);
    assert_eq!((term.c_cc[libc::VMIN], term.c_cc[libc::VTIME]), (1, 0));
}

#[test]
fn software_flow_control_sets_xon_xoff_and_the_control_characters() {
    let pty = open_pty();
    let comm = pty
        .open(&SerialOptions {
            baud_rate: 115_200,
            flow_control: SerialFlowControl::Software,
            ..opts()
        })
        .unwrap();
    let term = comm.termios().unwrap();
    assert_eq!(
        term.c_iflag & (libc::IXON | libc::IXOFF),
        libc::IXON | libc::IXOFF
    );
    assert_eq!(term.c_cflag & libc::CRTSCTS, 0);
    assert_eq!(
        (term.c_cc[libc::VSTART], term.c_cc[libc::VSTOP]),
        (0x11, 0x13)
    );
    assert_eq!(unsafe { libc::cfgetospeed(&term) }, libc::B115200);
}

#[test]
fn a_non_standard_baud_rate_is_set_through_termios2() {
    let pty = open_pty();
    let comm = pty
        .open(&SerialOptions {
            baud_rate: 250_000,
            ..opts()
        })
        .unwrap();
    let term = comm.termios2().unwrap();
    assert_eq!(term.c_cflag & libc::CBAUD, libc::BOTHER);
    assert_eq!((term.c_ispeed, term.c_ospeed), (250_000, 250_000));
    // 换回标准值之后 BOTHER 要消失。
    drop(comm);
    let comm = pty
        .open(&SerialOptions {
            baud_rate: 57_600,
            ..opts()
        })
        .unwrap();
    assert_eq!(comm.termios2().unwrap().c_cflag & libc::CBAUD, libc::B57600);
}

#[test]
fn opening_takes_the_port_exclusively() {
    let pty = open_pty();
    let comm = pty.open(&opts()).unwrap();
    let mut exclusive: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(comm.raw_fd(), libc::TIOCGEXCL, &mut exclusive) };
    assert_eq!(rc, 0, "TIOCGEXCL：{}", errno());
    assert_eq!(exclusive, 1, "应当已独占");
    // 非 root 的第二次打开会得到 EBUSY；root 可以无视独占，所以 root 下让一个降权的子进程去打开。
    if unsafe { libc::geteuid() } != 0 {
        assert!(matches!(pty.open(&opts()), Err(SerialError::Busy { .. })));
    } else if let Some(errno) = open_as_nobody(&pty.slave_path, true) {
        assert_eq!(errno, libc::EBUSY, "第二次打开应当得到 EBUSY");
    }
}

#[test]
fn impossible_framing_is_refused_before_touching_the_port() {
    let pty = open_pty();
    let result = pty.open(&SerialOptions {
        data_bits: SerialDataBits::Five,
        stop_bits: SerialStopBits::Two,
        ..opts()
    });
    assert!(
        matches!(result, Err(SerialError::UnsupportedSettings { .. })),
        "{:?}",
        result.err()
    );
}

#[test]
fn devices_that_are_not_serial_ports_are_classified() {
    let open = |path: &str| CommHandle::open_comm(path, path, &opts()).err();
    // 是字符设备但不是 tty。
    assert!(matches!(
        open("/dev/null"),
        Some(SerialError::NotASerialPort { .. })
    ));
    // 不是字符设备（目录、普通文件）：打开之前就拒绝，不去碰它。
    assert!(matches!(
        open("/dev"),
        Some(SerialError::NotASerialPort { .. })
    ));
    assert!(matches!(
        open("/etc/hostname"),
        Some(SerialError::NotASerialPort { .. }) | Some(SerialError::NotFound { .. })
    ));
    assert!(matches!(
        open("/dev/zeeterm-no-such-device"),
        Some(SerialError::NotFound { .. })
    ));
    assert!(matches!(
        open("/dev/zeeterm-no-such-dir/ttyUSB0"),
        Some(SerialError::NotFound { .. })
    ));
}

/// 以 root 运行时，在子进程里降成 nobody 去打开 `path`，返回得到的 errno（打开成功是 0）。
/// `open_up`：先把设备文件放开给所有人（伪终端从端是测试自己的，可以随便改）。
/// 不是 root 时返回 `None`。子进程里只调异步信号安全的系统调用。
fn open_as_nobody(path: &str, open_up: bool) -> Option<i32> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("不是 root：无法降权模拟，跳过");
        return None;
    }
    let path = std::ffi::CString::new(path).unwrap();
    if open_up {
        assert_eq!(unsafe { libc::chmod(path.as_ptr(), 0o666) }, 0);
    }
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            let code = if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(65534) != 0
                || libc::setuid(65534) != 0
            {
                200
            } else {
                let fd = libc::open(
                    path.as_ptr(),
                    libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
                );
                if fd >= 0 { 0 } else { errno() }
            };
            libc::_exit(code);
        }
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status));
    let code = libc::WEXITSTATUS(status);
    assert_ne!(code, 200, "子进程降权失败");
    Some(code)
}

/// 没有权限的普通用户打开伪终端从端（属主 root，模式 0620）得到 EACCES，错误分类给出带组名的权限提示。
#[test]
fn an_unprivileged_user_gets_a_permission_hint() {
    let pty = open_pty();
    let Some(errno) = open_as_nobody(&pty.slave_path, false) else {
        return;
    };
    assert_eq!(errno, libc::EACCES, "子进程应当得到 EACCES");
    let error = super::params::classify_errno(
        &pty.slave_path,
        super::params::OpenStage::Create,
        errno,
        "x",
        "dialout",
    );
    assert!(error.to_string().contains("sudo usermod -aG dialout $USER"));
}

// ---------------------------------------------------------------- 读写与叫停

#[test]
fn bytes_travel_both_ways() {
    let pty = open_pty();
    let comm = pty.open(&opts()).unwrap();
    device_write(&pty.master, b"hello\r\n\x03");
    let mut buffer = [0u8; 64];
    let mut got = Vec::new();
    while got.len() < 8 {
        match comm.read(&IoEvent, &mut buffer).unwrap() {
            ReadOutcome::Data(count) => got.extend_from_slice(&buffer[..count]),
            ReadOutcome::Stopped => panic!("不该被叫停"),
        }
    }
    // 原始模式：回车换行、Ctrl+C 都原样到达，没有被行规程吃掉或转换。
    assert_eq!(got, b"hello\r\n\x03");
    assert_eq!(
        comm.write_all(&IoEvent, b"AT\r\n\n\x7f").unwrap(),
        WriteOutcome::Written
    );
    // 输出端不做 \n → \r\n 转换。
    assert_eq!(
        device_read_until(&pty.master, b"AT\r\n\n\x7f"),
        b"AT\r\n\n\x7f"
    );
}

#[test]
fn stop_wakes_a_blocked_reader_at_once() {
    let pty = open_pty();
    let comm = Arc::new(pty.open(&opts()).unwrap());
    let reader = {
        let comm = Arc::clone(&comm);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 16];
            let started = Instant::now();
            let outcome = comm.read(&IoEvent, &mut buffer).unwrap();
            (outcome, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(150));
    let stopped_at = Instant::now();
    comm.stop();
    let (outcome, _) = reader.join().unwrap();
    assert_eq!(outcome, ReadOutcome::Stopped);
    assert!(
        stopped_at.elapsed() < Duration::from_millis(500),
        "叫停之后应当立刻返回，实际 {:?}",
        stopped_at.elapsed()
    );
    // 叫停之后再读也立刻返回，不会阻塞。
    let mut buffer = [0u8; 16];
    assert_eq!(
        comm.read(&IoEvent, &mut buffer).unwrap(),
        ReadOutcome::Stopped
    );
}

#[test]
fn stop_wakes_a_writer_stuck_on_a_full_buffer() {
    let pty = open_pty();
    let comm = Arc::new(pty.open(&opts()).unwrap());
    // 主端不读：灌满内核缓冲之后写就会挂在 poll 上。
    let writer = {
        let comm = Arc::clone(&comm);
        std::thread::spawn(move || {
            let chunk = vec![b'x'; 64 * 1024];
            loop {
                match comm.write_all(&IoEvent, &chunk).unwrap() {
                    WriteOutcome::Written => {}
                    other => return other,
                }
            }
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    assert!(!writer.is_finished(), "缓冲满了之后写应当被挡住");
    let stopped_at = Instant::now();
    comm.stop();
    assert_eq!(writer.join().unwrap(), WriteOutcome::Stopped);
    assert!(stopped_at.elapsed() < Duration::from_millis(500));
}

#[test]
fn a_write_that_never_gets_through_times_out() {
    let pty = open_pty();
    let comm = pty.open(&opts()).unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    let started = Instant::now();
    loop {
        match comm
            .write_all_within(&IoEvent, &chunk, Duration::from_millis(300))
            .unwrap()
        {
            WriteOutcome::Written => assert!(started.elapsed() < WAIT, "一直没有写满"),
            WriteOutcome::TimedOut => break,
            WriteOutcome::Stopped => panic!("不该被叫停"),
        }
    }
}

#[test]
fn the_peer_disappearing_is_an_error_not_a_busy_loop() {
    let pty = open_pty();
    let comm = Arc::new(pty.open(&opts()).unwrap());
    let reader = {
        let comm = Arc::clone(&comm);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 16];
            comm.read(&IoEvent, &mut buffer)
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    let Pty {
        master,
        slave_path: _,
    } = pty;
    drop(master);
    let deadline = Instant::now() + WAIT;
    while !reader.is_finished() {
        assert!(Instant::now() < deadline, "对端关闭后读一直没有返回");
        std::thread::sleep(Duration::from_millis(10));
    }
    let error = reader.join().unwrap().unwrap_err();
    assert!(
        super::params::errno_means_disconnected(error.raw_os_error().unwrap_or(0)),
        "{error:?}"
    );
    // 之后再读 / 再写也是错误，而不是挂住或返回 0 字节。
    let mut buffer = [0u8; 16];
    assert!(comm.read(&IoEvent, &mut buffer).is_err());
    assert!(comm.write_all(&IoEvent, b"x").is_err());
}

#[test]
fn break_on_a_pty_does_not_crash() {
    // 伪终端不支持 Break：要么成功要么得到一个明确的错误，不能 panic、不能卡住。
    let pty = open_pty();
    let comm = pty.open(&opts()).unwrap();
    let result = comm.send_break(Duration::from_millis(20));
    eprintln!("伪终端上的 Break：{result:?}");
}

// ---------------------------------------------------------------- 枚举

/// 一个随机唯一的临时目录，drop 时删掉。
struct Tree(tempfile::TempDir);

impl Tree {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("zssh-serial-tree-")
                .tempdir()
                .unwrap(),
        )
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

/// 造一个 `/sys/class/tty/<name>`：`device` 是个（悬空也无所谓的）符号链接，`ty` 是 `type` 文件的内容。
fn sys_tty(sys: &Path, name: &str, with_device: bool, ty: Option<&str>) {
    let dir = sys.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    if with_device {
        std::os::unix::fs::symlink("../../../some-device", dir.join("device")).unwrap();
    }
    if let Some(ty) = ty {
        std::fs::write(dir.join("type"), format!("{ty}\n")).unwrap();
    }
}

fn dev_node(dev: &Path, name: &str) {
    std::fs::write(dev.join(name), b"").unwrap();
}

#[test]
fn scanning_a_constructed_sysfs_tree_lists_only_real_ports() {
    let tree = Tree::new();
    let sys = tree.path().join("sys/class/tty");
    let dev = tree.path().join("dev");
    std::fs::create_dir_all(dev.join("serial/by-id")).unwrap();

    // 虚拟控制台、console、ptmx：没有 device 链接。
    for name in [
        "tty",
        "tty0",
        "tty1",
        "tty63",
        "console",
        "ptmx",
        "ttyprintk",
    ] {
        sys_tty(&sys, name, false, None);
        dev_node(&dev, name);
    }
    // 内核给每个可能的 UART 都建了 ttyS*；type 为 0 的没有硬件。
    sys_tty(&sys, "ttyS0", true, Some("0"));
    dev_node(&dev, "ttyS0");
    sys_tty(&sys, "ttyS1", true, Some("4"));
    dev_node(&dev, "ttyS1");
    sys_tty(&sys, "ttyS10", true, Some("4"));
    dev_node(&dev, "ttyS10");
    sys_tty(&sys, "ttyS2", true, Some("4"));
    dev_node(&dev, "ttyS2");
    // USB 串口 / CDC / 板载 UART。
    sys_tty(&sys, "ttyUSB0", true, None);
    dev_node(&dev, "ttyUSB0");
    sys_tty(&sys, "ttyACM0", true, None);
    dev_node(&dev, "ttyACM0");
    sys_tty(&sys, "ttyAMA0", true, None);
    dev_node(&dev, "ttyAMA0");
    // 有 sysfs 但 /dev 里还没有设备文件：不列。
    sys_tty(&sys, "ttyUSB9", true, None);
    // 稳定名字：给 ttyUSB0 一个型号。
    std::os::unix::fs::symlink(
        "../../ttyUSB0",
        dev.join("serial/by-id/usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0"),
    )
    .unwrap();

    let mut ports = scan_ports(&sys, &dev);
    sort_ports(&mut ports);
    let devices: Vec<String> = ports
        .iter()
        .map(|port| {
            Path::new(&port.device)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        devices,
        [
            "ttyACM0",
            "ttyAMA0",
            "ttyS1",
            "ttyS2",
            "ttyS10",
            "usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0"
        ],
        "{ports:?}"
    );
    // 有稳定名字的存 by-id 路径（插拔顺序变了也指向同一只适配器），显示里带上当前的 tty 名。
    let usb = ports
        .iter()
        .find(|port| port.device.ends_with("-port0"))
        .unwrap();
    assert_eq!(
        usb.device,
        dev.join("serial/by-id/usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0")
            .to_string_lossy()
    );
    assert_eq!(
        usb.display_name,
        format!(
            "FTDI FT232R USB UART A50285BI ({})",
            dev.join("ttyUSB0").to_string_lossy()
        )
    );
    // 没有 by-id 别名的，友好名就是设备路径。
    let acm = ports
        .iter()
        .find(|port| port.device.ends_with("/ttyACM0"))
        .unwrap();
    assert_eq!(acm.display_name, acm.device);
    assert_eq!(acm.device, dev.join("ttyACM0").to_string_lossy());
}

#[test]
fn a_ttys_without_a_type_file_is_still_listed() {
    // 老内核 / 特殊驱动可能没有 type 文件：有 device 链接就信它。
    let tree = Tree::new();
    let sys = tree.path().join("sys");
    let dev = tree.path().join("dev");
    std::fs::create_dir_all(&dev).unwrap();
    sys_tty(&sys, "ttyS0", true, None);
    dev_node(&dev, "ttyS0");
    assert_eq!(scan_ports(&sys, &dev).len(), 1);
}

#[test]
fn a_missing_sysfs_means_no_ports_not_an_error() {
    let tree = Tree::new();
    assert!(scan_ports(&tree.path().join("nope"), &tree.path().join("dev")).is_empty());
}

#[test]
fn several_by_id_aliases_for_one_port_pick_a_stable_one() {
    let tree = Tree::new();
    let sys = tree.path().join("sys");
    let dev = tree.path().join("dev");
    std::fs::create_dir_all(dev.join("serial/by-id")).unwrap();
    sys_tty(&sys, "ttyUSB0", true, None);
    dev_node(&dev, "ttyUSB0");
    for id in [
        "usb-Zeta_Adapter-if00-port0",
        "usb-Alpha_Adapter-if00-port0",
    ] {
        std::os::unix::fs::symlink("../../ttyUSB0", dev.join("serial/by-id").join(id)).unwrap();
    }
    let ports = scan_ports(&sys, &dev);
    assert_eq!(ports.len(), 1);
    assert!(
        ports[0].display_name.starts_with("Alpha Adapter ("),
        "{ports:?}"
    );
}

#[test]
fn ports_sort_by_number_not_by_text() {
    let mut ports: Vec<SerialPortEntry> =
        ["/dev/ttyS10", "/dev/ttyS2", "/dev/ttyS1", "/dev/ttyS11"]
            .into_iter()
            .map(|name| SerialPortEntry {
                device: name.to_owned(),
                display_name: name.to_owned(),
            })
            .collect();
    sort_ports(&mut ports);
    let order: Vec<_> = ports.iter().map(|port| port.device.as_str()).collect();
    assert_eq!(
        order,
        ["/dev/ttyS1", "/dev/ttyS2", "/dev/ttyS10", "/dev/ttyS11"]
    );
}

/// 这台机器上有没有串口都不算失败：能列出就列出，没有就是空列表。
#[test]
fn listing_ports_works_with_or_without_hardware() {
    let ports = super::list_ports().expect("枚举不该失败");
    eprintln!("本机串口数：{}", ports.len());
    for port in &ports {
        assert!(port.device.starts_with("/dev/"), "{port:?}");
        assert!(port.display_name.contains("/dev/tty"), "{port:?}");
    }
}

// ---------------------------------------------------------------- 会话（读写线程）

struct Rig {
    backend: StreamBackend,
    events: std::sync::mpsc::Receiver<SessionEvent>,
    gate: Arc<FlowGate>,
    /// 伪终端主端，扮演「设备」。
    device: Arc<CommHandle>,
    label: String,
    _runtime: tokio::runtime::Runtime,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("运行时")
}

fn rig(options: &SerialOptions) -> Rig {
    let pty = open_pty();
    let comm = pty.open(options).unwrap();
    let runtime = runtime();
    let label = pty.slave_path.clone();
    let (backend, events, gate) = start(
        Arc::new(comm),
        label.clone(),
        options,
        1,
        runtime.handle().clone(),
    );
    Rig {
        backend,
        events,
        gate,
        device: Arc::new(pty.master),
        label,
        _runtime: runtime,
    }
}

impl Rig {
    fn wait_text(&self, needle: &str) -> String {
        let mut text = String::new();
        let deadline = Instant::now() + WAIT;
        while !text.contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(SessionEvent::Data { bytes, .. }) => {
                    self.gate.on_ack(bytes.len());
                    text.push_str(&String::from_utf8_lossy(&bytes));
                }
                Ok(SessionEvent::Notice { text: notice, .. }) => text.push_str(&notice),
                Ok(other) => panic!("意外事件 {other:?}，已收到 {text:?}"),
                Err(_) => panic!("等不到 {needle:?}，已收到 {text:?}"),
            }
        }
        text
    }
}

#[test]
fn data_from_the_device_reaches_the_terminal_and_typing_reaches_the_device() {
    let mut rig = rig(&opts());
    device_write(&rig.device, b"boot> ");
    rig.wait_text("boot> ");
    // 默认回车 CR，本地回显关。
    rig.backend.send(SessionCommand::Write(b"help\r".to_vec()));
    device_read_until(&rig.device, b"help\r");
}

#[test]
fn newline_mode_and_local_echo_follow_the_options() {
    let mut rig = rig(&SerialOptions {
        newline: SerialNewline::CrLf,
        local_echo: true,
        ..opts()
    });
    rig.backend.send(SessionCommand::Write(b"ls\r".to_vec()));
    device_read_until(&rig.device, b"ls\r\n");
    rig.wait_text("ls\r\n");
}

#[test]
fn gbk_text_is_converted_both_ways() {
    let mut rig = rig(&SerialOptions {
        encoding: zssh_core::model::TextEncoding::Gbk,
        ..opts()
    });
    device_write(&rig.device, b"\xd6\xd0");
    rig.wait_text("中");
    rig.backend
        .send(SessionCommand::Write("中".as_bytes().to_vec()));
    device_read_until(&rig.device, b"\xd6\xd0");
}

#[test]
fn closing_stops_the_threads_and_releases_the_port() {
    let mut rig = rig(&opts());
    rig.backend.send(SessionCommand::Close);
    // 串口 fd 关闭 = 伪终端主端读到挂断。真实串口就是「再次打开不会被占用」。
    wait_for_slave_closed(&rig.device);
    assert!(rig.gate.is_closed());
}

#[test]
fn dropping_the_backend_also_releases_the_port() {
    let Rig {
        backend, device, ..
    } = rig(&opts());
    drop(backend);
    wait_for_slave_closed(&device);
}

#[test]
fn the_device_vanishing_is_reported_once_as_a_disconnect() {
    let Rig {
        events,
        device,
        backend: _backend,
        label,
        ..
    } = rig(&opts());
    // 设备端关掉 = 拔掉 USB 串口：读写线程读到 EIO。
    drop(device);
    let deadline = Instant::now() + WAIT;
    let mut errors = 0;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(SessionEvent::Error { message, .. }) => {
                assert!(
                    message.contains(&label) && message.contains("已断开"),
                    "{message}"
                );
                errors += 1;
            }
            Ok(_) => {}
            Err(_) => break,
        }
        if errors > 0 {
            // 报出之后再等一会儿，确认不会重复刷错误（空转的迹象）。
            std::thread::sleep(Duration::from_millis(300));
            while let Ok(event) = events.try_recv() {
                assert!(
                    !matches!(event, SessionEvent::Error { .. }),
                    "断开只该报一次：{event:?}"
                );
            }
            break;
        }
    }
    assert_eq!(errors, 1, "没有收到断开错误");
}

#[test]
fn break_does_not_disturb_the_session() {
    let mut rig = rig(&opts());
    rig.backend.send_break();
    // 伪终端对 Break 的处理由内核决定（接受或拒绝都行）；不管哪种，之后都还能正常收发。
    device_write(&rig.device, b"ok");
    let text = rig.wait_text("ok");
    eprintln!("Break 之后收到：{text:?}（{}）", rig.label);
}

#[test]
fn the_reader_stops_when_the_terminal_does_not_keep_up() {
    let rig = rig(&opts());
    // 一个 ack 都不给，灌 3 MiB：读线程必须在高水位附近停下。
    let chunk = vec![b'z'; 32 * 1024];
    let device = Arc::clone(&rig.device);
    let writer = std::thread::spawn(move || {
        for _ in 0..96 {
            if device.write_all(&IoEvent, &chunk).is_err() {
                break;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(800));
    let high = zssh_core::ssh::flow::HIGH_WATERMARK + 4096;
    assert!(rig.gate.unacked() <= high, "{}", rig.gate.unacked());
    assert!(!writer.is_finished(), "缓冲写满之后设备端应当被挡住");
    // 开始消化：全部数据到齐。
    let mut received = 0usize;
    let deadline = Instant::now() + Duration::from_secs(20);
    while received < 96 * 32 * 1024 {
        assert!(Instant::now() < deadline, "只收到 {received}");
        if let Ok(SessionEvent::Data { bytes, .. }) =
            rig.events.recv_timeout(Duration::from_millis(200))
        {
            received += bytes.len();
            rig.gate.on_ack(bytes.len());
        }
    }
    writer.join().unwrap();
}

// ---------------------------------------------------------------- 手工验收：socat 造的一对虚拟串口

/// 需要 `socat`（没装就跳过并打印原因），默认 `#[ignore]`：用 `socat pty,raw,echo=0 pty,raw,echo=0` 造一对
/// 互联的虚拟串口，在其中一个上跑完整的会话，从另一端收发。默认忽略，手工步骤见报告。
#[test]
#[ignore = "需要 socat：cargo test -p zssh-protocols socat -- --ignored"]
fn a_socat_virtual_serial_pair_carries_a_real_session() {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut socat = match std::process::Command::new("socat")
        .args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("没有 socat（{error}）：跳过");
            return;
        }
    };
    // socat 把两个 pty 的路径打印在 stderr：`N PTY is /dev/pts/N`。
    let stderr = socat.stderr.take().unwrap();
    let mut paths = Vec::new();
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if let Some((_, path)) = line.split_once("PTY is ") {
            paths.push(path.trim().to_owned());
        }
        if paths.len() == 2 {
            break;
        }
    }
    assert_eq!(paths.len(), 2, "socat 没有报出两个 pty");

    let options = SerialOptions {
        device: paths[0].clone(),
        baud_rate: 115_200,
        ..opts()
    };
    let runtime = runtime();
    let (mut backend, events, gate) =
        super::open_serial(&options, 1, runtime.handle().clone()).expect("打开 socat 端口");
    // 另一端当「设备」：普通的文件读写就行（socat 的 pty 默认是原始模式）。
    let mut peer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths[1])
        .expect("打开另一端");
    peer.write_all(b"hello from the device\r\n").unwrap();
    let mut seen = String::new();
    let deadline = Instant::now() + WAIT;
    while !seen.contains("hello from the device") {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(SessionEvent::Data { bytes, .. }) => {
                gate.on_ack(bytes.len());
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(_) => {}
            Err(_) => panic!("会话没有收到设备发来的数据：{seen:?}"),
        }
    }
    backend.send(SessionCommand::Write(b"AT\r".to_vec()));
    let mut got = vec![0u8; 3];
    peer.read_exact(&mut got).unwrap();
    assert_eq!(got, b"AT\r");
    backend.send(SessionCommand::Close);
    let _ = socat.kill();
    let _ = socat.wait();
}
