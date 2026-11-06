//! 串口会话：一个读线程、一个写线程，不忙等（Windows 挂在重叠 I/O 上，Linux 挂在 poll 上）。
//!
//! - 读线程：先问流控门（终端没消化完就不读，驱动缓冲满了之后由线路流控挡住设备），然后阻塞在
//!   `ReadFile`（Linux 是 `poll`）上直到有字节到达；解码成 UTF-8 送给界面；
//! - 写线程：取命令（键入、Break、关闭），转换回车 / 编码后写出；
//! - 关闭（用户关标签、后端被丢弃、设备被拔出）：置停止事件（Linux 是 eventfd），两个线程的等待立刻返回，
//!   线程退出时放掉最后一个 `Arc<CommHandle>`，句柄关闭，COM 口随即可以被重新打开。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use zssh_core::model::SerialOptions;
use zssh_core::ssh::FlowGate;
use zssh_session::{SessionCommand, SessionEvent};

use super::os::{CommHandle, IoEvent, ReadOutcome, WriteOutcome};
use super::params::{SerialError, describe_framing_in, normalize_device_for_os};
use crate::backend::{BackendCommand, BackendEnds, COMMAND_QUEUE, EventOut, StreamBackend};
use crate::input::{InputFilter, serial_enter_bytes};
use crate::text::{TextDecoder, TextEncoder};

/// Break 信号持续多久。
const BREAK_HOLD: Duration = Duration::from_millis(250);
const READ_BUFFER: usize = 4096;

/// 打开串口并开始会话。`runtime` 只用来在线程里等流控门和命令（不会在它上面跑重活）。
///
/// 打开本身是同步的（`CreateFileW` 对有毛病的驱动可能卡几秒）：界面那一层要放进阻塞线程池里调用。
/// 失败返回分类好的中文错误（"COM3 被别的程序占用"、"COM3 不存在"……）。
pub fn open_serial(
    options: &SerialOptions,
    generation: u64,
    runtime: Handle,
) -> Result<
    (
        StreamBackend,
        std::sync::mpsc::Receiver<SessionEvent>,
        Arc<FlowGate>,
    ),
    SerialError,
> {
    let (path, label) = normalize_device_for_os(&options.device)?;
    let comm = Arc::new(CommHandle::open_comm(&path, &label, options)?);
    Ok(start(comm, label, options, generation, runtime))
}

/// 在一个已经打开的 `CommHandle` 上起读写线程（测试里是命名管道）。
pub(super) fn start(
    comm: Arc<CommHandle>,
    label: String,
    options: &SerialOptions,
    generation: u64,
    runtime: Handle,
) -> (
    StreamBackend,
    std::sync::mpsc::Receiver<SessionEvent>,
    Arc<FlowGate>,
) {
    let (events, event_rx) = std::sync::mpsc::channel();
    let (backend, ends) = StreamBackend::new(events.clone(), generation, COMMAND_QUEUE);
    let gate = Arc::new(FlowGate::new());
    let out = EventOut {
        events,
        generation,
        gate: Arc::clone(&gate),
    };
    out.notice(&crate::lang::serial_opened_in(
        crate::lang::language(),
        &label,
        &describe_framing_in(crate::lang::language(), options),
    ));
    // 用户主动关闭 / 对端消失：读线程据此知道要不要报错。
    let closed = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = watch::channel(false);

    {
        let comm = Arc::clone(&comm);
        let out = out.clone();
        let closed = Arc::clone(&closed);
        let runtime = runtime.clone();
        let label = label.clone();
        let encoding = options.encoding;
        let done_tx = done_tx.clone();
        std::thread::Builder::new()
            .name("zssh-serial-read".to_owned())
            .spawn(move || {
                read_loop(&comm, &label, encoding, &out, &closed, &runtime);
                let _ = done_tx.send(true);
                comm.stop();
                out.gate.close();
            })
            .ok();
    }
    {
        let options = options.clone();
        std::thread::Builder::new()
            .name("zssh-serial-write".to_owned())
            .spawn(move || {
                write_loop(
                    &comm, &label, &options, &out, &closed, &runtime, ends, done_rx,
                );
                // 停止事件让读线程的等待返回；流控门关掉让它不会卡在等 ack。
                comm.stop();
                out.gate.close();
                drop(done_tx);
            })
            .ok();
    }
    (backend, event_rx, gate)
}

fn read_loop(
    comm: &CommHandle,
    label: &str,
    encoding: zssh_core::model::TextEncoding,
    out: &EventOut,
    closed: &AtomicBool,
    runtime: &Handle,
) {
    let Ok(event) = IoEvent::new() else {
        out.error(crate::lang::serial_event_failed(label, true));
        return;
    };
    let mut decoder = TextDecoder::new(encoding);
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        // 终端来不及渲染时先别读：门关了说明会话在收尾。
        if !runtime.block_on(out.gate.wait_ready()) {
            return;
        }
        match comm.read(&event, &mut buffer) {
            Ok(ReadOutcome::Data(0)) => {}
            Ok(ReadOutcome::Data(count)) => {
                if !out.data(decoder.decode(&buffer[..count])) {
                    return;
                }
            }
            Ok(ReadOutcome::Stopped) => return,
            Err(error) => {
                if !closed.load(Ordering::Acquire) {
                    out.error(io_failure_message(label, &error, false));
                }
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_loop(
    comm: &CommHandle,
    label: &str,
    options: &SerialOptions,
    out: &EventOut,
    closed: &AtomicBool,
    runtime: &Handle,
    ends: BackendEnds,
    mut done: watch::Receiver<bool>,
) {
    // 串口没有窗口大小这回事，最新尺寸那一路不用。
    let BackendEnds {
        mut commands,
        mut closing,
        ..
    } = ends;
    let Ok(event) = IoEvent::new() else {
        out.error(crate::lang::serial_event_failed(label, false));
        closed.store(true, Ordering::Release);
        return;
    };
    let enter = serial_enter_bytes(options.newline);
    let mut filter = InputFilter::default();
    let mut encoder = TextEncoder::new(options.encoding);
    let mut warned_timeout = false;
    loop {
        let command = runtime.block_on(async {
            tokio::select! {
                _ = closing.changed() => None,
                _ = done.changed() => None,
                command = commands.recv() => command,
            }
        });
        match command {
            Some(BackendCommand::Session(SessionCommand::Write(bytes))) => {
                let (send, echo) = filter.prepare(&bytes, enter, options.local_echo);
                let wire = encoder.encode(&send);
                match comm.write_all(&event, &wire) {
                    Ok(WriteOutcome::Written) => {
                        warned_timeout = false;
                        if !out.data(echo) {
                            break;
                        }
                    }
                    Ok(WriteOutcome::Stopped) => break,
                    Ok(WriteOutcome::TimedOut) => {
                        if !warned_timeout {
                            warned_timeout = true;
                            out.notice(&crate::lang::serial_write_timeout(label));
                        }
                    }
                    Err(error) => {
                        if !closed.load(Ordering::Acquire) {
                            out.error(io_failure_message(label, &error, true));
                        }
                        break;
                    }
                }
            }
            // 协议应答：照原样编码写出，不经输入过滤（不转换回车）、不回显。写不出去（超时 / 停止）就丢，
            // 不提示：用户输入那条路径已经会提示。
            Some(BackendCommand::Reply(bytes)) => {
                let wire = encoder.encode(&bytes);
                match comm.write_all(&event, &wire) {
                    Ok(WriteOutcome::Written | WriteOutcome::TimedOut) => {}
                    Ok(WriteOutcome::Stopped) => break,
                    Err(error) => {
                        if !closed.load(Ordering::Acquire) {
                            out.error(io_failure_message(label, &error, true));
                        }
                        break;
                    }
                }
            }
            Some(BackendCommand::Break) => {
                if let Err(error) = comm.send_break(BREAK_HOLD) {
                    out.notice(&crate::lang::serial_break_failed(label, &error.to_string()));
                }
            }
            // 串口没有窗口大小；Close 走关闭信号。
            Some(BackendCommand::Session(_)) => {}
            None => break,
        }
    }
    closed.store(true, Ordering::Release);
}

#[cfg(windows)]
fn io_failure_message(label: &str, error: &std::io::Error, writing: bool) -> String {
    use super::params::{OpenStage, classify_open_error, io_error_means_disconnected};
    let code = error.raw_os_error().unwrap_or(0) as u32;
    if io_error_means_disconnected(code) {
        return SerialError::Disconnected {
            device: label.to_owned(),
        }
        .message(crate::lang::language());
    }
    classify_open_error(
        label,
        OpenStage::Configure,
        code,
        &crate::lang::serial_io_failed(writing, &error.to_string()),
    )
    .message(crate::lang::language())
}

/// Linux：读写出错时 `errno` 是 EIO / ENXIO / ENODEV / EPIPE 基本就是设备没了（USB 串口被拔出）。
#[cfg(not(windows))]
fn io_failure_message(label: &str, error: &std::io::Error, writing: bool) -> String {
    use super::params::errno_means_disconnected;
    let errno = error.raw_os_error().unwrap_or(0);
    if errno_means_disconnected(errno) {
        return SerialError::Disconnected {
            device: label.to_owned(),
        }
        .message(crate::lang::language());
    }
    SerialError::Os {
        device: label.to_owned(),
        code: errno as u32,
        what: crate::lang::serial_io_failed(writing, &error.to_string()),
    }
    .message(crate::lang::language())
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;
    use crate::serial::os::test_pipe;
    use std::time::Instant;
    use zssh_core::model::SerialNewline;
    use zssh_session::SessionBackend;

    const WAIT: Duration = Duration::from_secs(5);

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("运行时")
    }

    struct Rig {
        backend: StreamBackend,
        events: std::sync::mpsc::Receiver<SessionEvent>,
        gate: Arc<FlowGate>,
        /// 管道的另一端，扮演"设备"。
        device: Arc<CommHandle>,
        device_event: IoEvent,
        _runtime: tokio::runtime::Runtime,
    }

    fn rig(options: &SerialOptions) -> Rig {
        let (client, device) = test_pipe::pair();
        let runtime = runtime();
        let (backend, events, gate) = start(
            Arc::new(client),
            "COM9".to_owned(),
            options,
            1,
            runtime.handle().clone(),
        );
        Rig {
            backend,
            events,
            gate,
            device: Arc::new(device),
            device_event: IoEvent::new().unwrap(),
            _runtime: runtime,
        }
    }

    impl Rig {
        fn device_write(&self, bytes: &[u8]) {
            assert_eq!(
                self.device.write_all(&self.device_event, bytes).unwrap(),
                WriteOutcome::Written
            );
        }

        fn device_read_until(&self, needle: &[u8]) -> Vec<u8> {
            let mut seen = Vec::new();
            let mut buffer = [0u8; 256];
            let deadline = Instant::now() + WAIT;
            while !seen.windows(needle.len()).any(|w| w == needle) {
                assert!(
                    Instant::now() < deadline,
                    "设备端等不到 {needle:?}，收到 {seen:?}"
                );
                match self.device.read(&self.device_event, &mut buffer).unwrap() {
                    ReadOutcome::Data(count) => seen.extend_from_slice(&buffer[..count]),
                    ReadOutcome::Stopped => panic!("被叫停"),
                }
            }
            seen
        }

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
        let mut rig = rig(&SerialOptions::default());
        rig.device_write(b"boot> ");
        rig.wait_text("boot> ");
        // 默认回车 CR，本地回显关。
        rig.backend.send(SessionCommand::Write(b"help\r".to_vec()));
        rig.device_read_until(b"help\r");
    }

    #[test]
    fn newline_mode_and_local_echo_follow_the_options() {
        let mut rig = rig(&SerialOptions {
            newline: SerialNewline::CrLf,
            local_echo: true,
            ..Default::default()
        });
        rig.backend.send(SessionCommand::Write(b"ls\r".to_vec()));
        rig.device_read_until(b"ls\r\n");
        rig.wait_text("ls\r\n");
    }

    #[test]
    fn gbk_text_is_converted_both_ways() {
        let mut rig = rig(&SerialOptions {
            encoding: zssh_core::model::TextEncoding::Gbk,
            ..Default::default()
        });
        rig.device_write(b"\xd6\xd0");
        rig.wait_text("中");
        rig.backend
            .send(SessionCommand::Write("中".as_bytes().to_vec()));
        rig.device_read_until(b"\xd6\xd0");
    }

    #[test]
    fn closing_stops_the_threads_and_releases_the_handle() {
        let mut rig = rig(&SerialOptions::default());
        rig.backend.send(SessionCommand::Close);
        // 句柄关闭 = 管道的另一端读到断开。真实串口就是"再次打开不会被占用"。
        let mut buffer = [0u8; 16];
        let deadline = Instant::now() + WAIT;
        loop {
            match rig.device.read(&rig.device_event, &mut buffer) {
                Err(error) => {
                    assert!(error.raw_os_error().is_some());
                    break;
                }
                Ok(_) => assert!(Instant::now() < deadline, "关闭后句柄一直没释放"),
            }
        }
        assert!(rig.gate.is_closed());
    }

    #[test]
    fn dropping_the_backend_also_releases_the_handle() {
        let Rig {
            backend,
            device,
            device_event,
            ..
        } = rig(&SerialOptions::default());
        drop(backend);
        let mut buffer = [0u8; 16];
        let deadline = Instant::now() + WAIT;
        while device.read(&device_event, &mut buffer).is_ok() {
            assert!(Instant::now() < deadline);
        }
    }

    #[test]
    fn the_device_vanishing_is_reported_once_as_an_error() {
        let rig = rig(&SerialOptions::default());
        // 设备端关掉 = 对端消失。
        let Rig {
            events,
            device,
            backend: _backend,
            ..
        } = rig;
        drop(device);
        let deadline = Instant::now() + WAIT;
        loop {
            match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(SessionEvent::Error { message, .. }) => {
                    assert!(message.contains("COM9"), "{message}");
                    break;
                }
                Ok(_) => {}
                Err(_) => panic!("没有收到错误事件"),
            }
        }
    }

    #[test]
    fn break_on_a_non_comm_handle_reports_a_notice_instead_of_crashing() {
        let mut rig = rig(&SerialOptions::default());
        rig.backend.send_break();
        let text = rig.wait_text("Break");
        assert!(text.contains("COM9"), "{text}");
        // 之后还能正常收发。
        rig.device_write(b"ok");
        rig.wait_text("ok");
    }

    #[test]
    fn the_reader_stops_when_the_terminal_does_not_keep_up() {
        let rig = rig(&SerialOptions::default());
        // 一个 ack 都不给，灌 3 MiB：读线程必须在高水位附近停下。
        let chunk = vec![b'z'; 32 * 1024];
        let device = Arc::clone(&rig.device);
        let event = IoEvent::new().unwrap();
        let writer = std::thread::spawn(move || {
            for _ in 0..96 {
                if device.write_all(&event, &chunk).is_err() {
                    break;
                }
            }
        });
        std::thread::sleep(Duration::from_millis(800));
        let high = zssh_core::ssh::flow::HIGH_WATERMARK + READ_BUFFER;
        assert!(rig.gate.unacked() <= high, "{}", rig.gate.unacked());
        assert!(!writer.is_finished(), "管道写满之后设备端应当被挡住");
        // 开始消化：全部数据到齐。
        let mut received = 0usize;
        let deadline = Instant::now() + Duration::from_secs(20);
        while received < 96 * 32 * 1024 {
            assert!(Instant::now() < deadline);
            if let Ok(SessionEvent::Data { bytes, .. }) =
                rig.events.recv_timeout(Duration::from_millis(200))
            {
                received += bytes.len();
                rig.gate.on_ack(bytes.len());
            }
        }
        writer.join().unwrap();
    }
}
