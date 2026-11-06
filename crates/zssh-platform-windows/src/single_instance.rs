//! 单实例运行：Windows 用命名互斥体 + 主窗口属性；Unix 用运行目录里的 `flock` 锁文件，
//! 第二个实例通过同目录的 Unix 套接字叫第一个实例把窗口提到前台。
//!
//! 第一个实例拿到锁，后启动的实例拿不到就退出。Windows 还会把已有窗口还原置前。

#[cfg(windows)]
use windows::core::{BOOL, PCWSTR};
#[cfg(windows)]
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM,
};
#[cfg(windows)]
use windows::Win32::System::Threading::CreateMutexW;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetPropW, SetForegroundWindow, SetPropW, ShowWindow, SW_RESTORE,
};

#[cfg(windows)]
pub struct InstanceGuard(HANDLE);

// 互斥体句柄本身可以跨线程传递。
#[cfg(windows)]
unsafe impl Send for InstanceGuard {}
#[cfg(windows)]
unsafe impl Sync for InstanceGuard {}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 拿到命名互斥体；已有实例返回 `None`。创建失败时不阻止启动。
#[cfg(windows)]
pub fn acquire(name: &str) -> Option<InstanceGuard> {
    let name = wide(name);
    unsafe {
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    None
                } else {
                    Some(InstanceGuard(handle))
                }
            }
            Err(_) => Some(InstanceGuard(HANDLE::default())),
        }
    }
}

#[cfg(windows)]
struct FocusArgs {
    prop: *const u16,
    found: bool,
}

#[cfg(windows)]
unsafe extern "system" fn focus_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let args = unsafe { &mut *(lparam.0 as *mut FocusArgs) };
    let prop = unsafe { GetPropW(hwnd, PCWSTR(args.prop)) };
    if !prop.is_invalid() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        args.found = true;
        BOOL(0)
    } else {
        BOOL(1)
    }
}

/// 找到挂了指定属性的窗口并置前；返回是否找到。
#[cfg(windows)]
pub fn focus_existing(prop: &str) -> bool {
    let name = wide(prop);
    let mut args = FocusArgs {
        prop: name.as_ptr(),
        found: false,
    };
    unsafe {
        let _ = EnumWindows(Some(focus_proc), LPARAM(&mut args as *mut _ as isize));
    }
    args.found
}

/// 给窗口挂属性，供后启动的实例查找。
#[cfg(windows)]
pub fn set_window_prop(hwnd: isize, prop: &str) -> bool {
    let prop = wide(prop);
    unsafe { SetPropW(HWND(hwnd as _), PCWSTR(prop.as_ptr()), Some(HANDLE(1 as _))).is_ok() }
}

// ---------------------------------------------------------------------------
// Unix：运行目录里的 flock 锁文件 + Unix 套接字
// ---------------------------------------------------------------------------
//
// 为什么用 flock 而不是"把 PID 写进锁文件再看 /proc"：创建和写 PID 之间有竞态（另一个实例读到空文件，
// 会删锁抢占）；进程崩溃后 PID 被别的进程复用，还会误以为"已经在运行"。flock 由内核持有，
// 进程怎么死的都会释放，没有"残留的锁"这回事；锁文件本身留在那里无害，所以退出时也**不删**它
// （删了反而有竞态：别的进程刚打开旧文件、我们删掉、第三个进程新建同名文件又拿到锁，就成了两个实例）。
//
// 激活：第二个实例连第一个实例在同目录里监听的套接字，发一行 `activate`，由第一个实例自己把窗口提前。
// 套接字文件权限 0600、放在 0700 的运行目录里，并且监听方再用 `SO_PEERCRED` 核对对端 uid，
// 只认同一用户。

#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

/// 激活消息。整行比对，别的内容一律忽略。
#[cfg(unix)]
const ACTIVATE_MESSAGE: &[u8] = b"activate\n";

#[cfg(unix)]
pub struct InstanceGuard {
    /// 持有 flock 的文件；拿不到运行目录 / 打不开文件时为 `None`（此时不阻止启动，也没法被叫醒）。
    lock_file: Option<std::fs::File>,
    socket_path: Option<PathBuf>,
    /// 监听线程的停止标记；Drop 时据此清理套接字文件。
    listening: Option<Arc<AtomicBool>>,
}

#[cfg(unix)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let (Some(stop), Some(path)) = (self.listening.take(), self.socket_path.as_ref()) {
            stop.store(true, Ordering::SeqCst);
            // 唤醒阻塞在 accept 的监听线程，让它看到停止标记后退出；然后删套接字文件。
            // 此时 flock 还没放（字段在 drop 之后才释放），后来的实例不会和我们抢这个文件名。
            let _ = UnixStream::connect(path);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
impl InstanceGuard {
    /// 开始接收"激活"消息；每收到一条就在**监听线程**里调一次 `on_activate`
    /// （界面程序要自己把它转回界面线程再动窗口）。
    ///
    /// 没有运行目录、或套接字建不起来时返回错误：单实例本身不受影响，只是第二个实例叫不醒这个窗口。
    pub fn listen_for_activation(
        &mut self,
        on_activate: impl Fn() + Send + 'static,
    ) -> std::io::Result<()> {
        let path = self.socket_path.clone().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "没有可用的运行目录")
        })?;
        if self.lock_file.is_none() || self.listening.is_some() {
            return Err(std::io::Error::other("没有持有实例锁，或已在监听"));
        }
        // 持有 flock 的只有我们：目录里残留的套接字文件一定是上次异常退出留下的，可以直接删。
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let listener = UnixListener::bind(&path)?;
        // 建好立刻收紧权限；目录本身是 0700，这个窗口期里别的用户也进不来。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("zeeterm-instance-activate".to_owned())
            .spawn(move || serve(listener, &thread_stop, on_activate))?;
        self.listening = Some(stop);
        Ok(())
    }
}

#[cfg(unix)]
fn serve(listener: UnixListener, stop: &AtomicBool, on_activate: impl Fn()) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut stream) = stream else { continue };
        if !peer_is_same_user(&stream) {
            tracing::warn!("拒绝了来自其他用户的激活请求");
            continue;
        }
        // 读超时：连上来却不说话的对端不能把监听线程卡住。
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let mut buf = [0u8; 32];
        let mut len = 0;
        while len < buf.len() {
            match stream.read(&mut buf[len..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    len += n;
                    if buf[..len].ends_with(b"\n") {
                        break;
                    }
                }
            }
        }
        if &buf[..len] == ACTIVATE_MESSAGE {
            on_activate();
        }
    }
}

/// 对端进程的有效 uid 是否和自己一样。取不到凭据一律当作不是。
#[cfg(unix)]
fn peer_is_same_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd as _;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: fd 属于存活的 `stream`；`cred` 和 `len` 是本地变量，大小与 SO_PEERCRED 约定一致。
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    // SAFETY: geteuid 没有前置条件。
    status == 0 && cred.uid == unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 放锁文件和套接字的目录：`$XDG_RUNTIME_DIR`；没有就退回 `~/.cache/zeeterm`（0700）。
/// 不用全局共享的 `/tmp`：别的用户能预先占住同名文件。
#[cfg(unix)]
fn runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if dir.is_absolute() && dir.is_dir() {
            return Some(dir);
        }
    }
    let dir = std::env::home_dir()?.join(".cache").join("zeeterm");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    Some(dir)
}

/// 拿到实例锁；已有实例返回 `None`。运行目录或锁文件不可用时不阻止启动。
#[cfg(unix)]
pub fn acquire(name: &str) -> Option<InstanceGuard> {
    match runtime_dir() {
        Some(dir) => acquire_in(&dir, name),
        None => {
            tracing::warn!("没有可用的运行目录，单实例检查不生效");
            Some(unlocked_guard())
        }
    }
}

#[cfg(unix)]
fn unlocked_guard() -> InstanceGuard {
    InstanceGuard {
        lock_file: None,
        socket_path: None,
        listening: None,
    }
}

#[cfg(unix)]
fn acquire_in(dir: &Path, name: &str) -> Option<InstanceGuard> {
    let safe = safe_name(name);
    let lock_path = dir.join(format!("{safe}.lock"));
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(%err, "打不开实例锁文件，单实例检查不生效");
            return Some(unlocked_guard());
        }
    };
    match file.try_lock() {
        Ok(()) => Some(InstanceGuard {
            lock_file: Some(file),
            socket_path: Some(dir.join(format!("{safe}.sock"))),
            listening: None,
        }),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(err)) => {
            tracing::warn!(%err, "实例锁不可用，单实例检查不生效");
            Some(unlocked_guard())
        }
    }
}

/// 叫醒已在运行的实例：连它的套接字发一条激活消息。返回消息是否已送出。
///
/// 第一个实例拿到锁到绑好套接字之间有一小段空隙，所以连不上时短暂重试。
#[cfg(unix)]
pub fn activate_existing(name: &str) -> bool {
    runtime_dir().is_some_and(|dir| activate_in(&dir, name))
}

#[cfg(unix)]
fn activate_in(dir: &Path, name: &str) -> bool {
    let path = dir.join(format!("{}.sock", safe_name(name)));
    for _ in 0..20 {
        if let Ok(mut stream) = UnixStream::connect(&path) {
            return stream.write_all(ACTIVATE_MESSAGE).is_ok();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_existing_instance() {
        let name = format!("zssh-single-instance-test-{}", std::process::id());
        let first = acquire(&name).expect("第一次应拿到互斥体");
        assert!(acquire(&name).is_none(), "第二次应报告已有实例");
        drop(first);
        assert!(acquire(&name).is_some(), "释放后应能重新拿到");
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::io::BufRead as _;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    const HELPER_DIR_VAR: &str = "ZSSH_TEST_INSTANCE_DIR";

    #[test]
    fn two_acquires_exclude_each_other_and_release_on_drop() {
        let dir = tempfile::tempdir().expect("临时目录");
        let first = acquire_in(dir.path(), "inst").expect("第一次");
        assert!(acquire_in(dir.path(), "inst").is_none(), "第二次必须被挡住");
        assert!(
            acquire_in(dir.path(), "other").is_some(),
            "不同名字互不影响"
        );
        drop(first);
        assert!(acquire_in(dir.path(), "inst").is_some(), "释放后能再拿到");
    }

    #[test]
    fn a_leftover_lock_file_does_not_block_startup() {
        let dir = tempfile::tempdir().expect("临时目录");
        // 旧实现留下的"PID 锁文件"，里面写着一个肯定还活着的 PID（自己）：不能再被误认为已有实例。
        std::fs::write(dir.path().join("inst.lock"), std::process::id().to_string())
            .expect("写残留文件");
        std::fs::write(dir.path().join("inst.sock"), b"").expect("写残留套接字文件");
        let mut guard = acquire_in(dir.path(), "inst").expect("残留文件不该挡住启动");
        guard
            .listen_for_activation(|| {})
            .expect("残留的套接字文件应被清掉重建");
    }

    #[test]
    fn lock_files_are_private_and_never_deleted_on_exit() {
        let dir = tempfile::tempdir().expect("临时目录");
        let guard = acquire_in(dir.path(), "inst").expect("拿锁");
        let lock = dir.path().join("inst.lock");
        let mode = std::fs::metadata(&lock)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(guard);
        assert!(lock.exists(), "锁文件不能在退出时删除（会有竞态）");
    }

    #[test]
    fn the_activation_message_reaches_the_first_instance() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut guard = acquire_in(dir.path(), "inst").expect("拿锁");
        let (tx, rx) = mpsc::channel();
        guard
            .listen_for_activation(move || {
                let _ = tx.send(());
            })
            .expect("开始监听");
        let sock = dir.path().join("inst.sock");
        let mode = std::fs::metadata(&sock)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "套接字必须是 0600");

        assert!(activate_in(dir.path(), "inst"), "消息应送出");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("第一个实例应收到激活");

        // 乱发的内容不触发激活。
        let mut stream = UnixStream::connect(&sock).expect("连接");
        stream.write_all(b"hello\n").expect("写");
        drop(stream);
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());

        drop(guard);
        assert!(!sock.exists(), "退出时应清掉套接字文件");
    }

    #[test]
    fn our_own_connections_pass_the_peer_uid_check() {
        let dir = tempfile::tempdir().expect("临时目录");
        let listener = UnixListener::bind(dir.path().join("s.sock")).expect("绑定");
        let client = UnixStream::connect(dir.path().join("s.sock")).expect("连接");
        let (server, _) = listener.accept().expect("接受");
        assert!(peer_is_same_user(&server));
        assert!(peer_is_same_user(&client));
    }

    /// 子进程入口：拿住锁，打印 `locked`，然后一直等到被杀（模拟崩溃）。平时 `#[ignore]` 不跑，
    /// 由 `a_lock_held_by_another_process_is_released_when_it_dies` 带环境变量拉起。
    #[test]
    #[ignore = "helper for a_lock_held_by_another_process_is_released_when_it_dies"]
    fn helper_hold_lock_until_killed() {
        let Some(dir) = std::env::var_os(HELPER_DIR_VAR) else {
            return;
        };
        let _guard = acquire_in(Path::new(&dir), "inst").expect("子进程应拿到锁");
        println!("locked");
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn a_lock_held_by_another_process_is_released_when_it_dies() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut child = Command::new(std::env::current_exe().expect("当前可执行文件"))
            .args([
                "--ignored",
                "--exact",
                "single_instance::unix_tests::helper_hold_lock_until_killed",
                "--nocapture",
            ])
            .env(HELPER_DIR_VAR, dir.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("拉起子进程");
        let stdout = child.stdout.take().expect("子进程输出");
        let mut lines = std::io::BufReader::new(stdout).lines();
        let mut locked = false;
        while let Some(Ok(line)) = lines.next() {
            if line.trim() == "locked" {
                locked = true;
                break;
            }
        }
        assert!(locked, "子进程应报告已拿到锁");

        assert!(
            acquire_in(dir.path(), "inst").is_none(),
            "另一个进程持有时必须被挡住"
        );
        // 用 SIGKILL 杀掉：进程没有机会做任何清理，和崩溃一样。
        child.kill().expect("杀子进程");
        child.wait().expect("等子进程");
        assert!(
            acquire_in(dir.path(), "inst").is_some(),
            "进程崩溃后锁由内核释放，不该再有'已在运行'"
        );
    }
}
