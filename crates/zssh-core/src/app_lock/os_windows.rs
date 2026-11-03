//! 忘记锁屏密码时“证明你是这台电脑的 Windows 用户”的两条真实路径（V2-U26）。
//!
//! 只在界面的后台线程里调：两条路都会弹系统窗口并一直等到用户操作完。自动化和测试不会走到这里，
//! 它们用 `OsVerifier` 的桩（见 `app_lock::tests`）。
//!
//! - Windows Hello / PIN：`Windows.Security.Credentials.UI.UserConsentVerifier`，有窗口句柄时走
//!   `IUserConsentVerifierInterop` 让系统对话框挂在 ZeeTerm 窗口上；
//! - 退一步：`CredUIPromptForWindowsCredentialsW` 收当前用户的 Windows 密码，`LogonUserW` 核对，
//!   再比较登录出来的令牌和本进程令牌是不是同一个用户（凭据框里可以“换用户”，不能让别人的账户通过）。
//!
//! 密码缓冲区用完立刻清零；这里不写日志。

use windows::core::{factory, HSTRING, PCWSTR, PWSTR};
use windows::Security::Credentials::UI::{
    UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::Credentials::{
    CredUIPromptForWindowsCredentialsW, CredUnPackAuthenticationBufferW,
    CREDUIWIN_ENUMERATE_CURRENT_USER, CREDUI_INFOW, CRED_PACK_FLAGS,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, LogonUserW, TokenUser, LOGON32_LOGON_INTERACTIVE,
    LOGON32_PROVIDER_DEFAULT, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::WinRT::{
    IUserConsentVerifierInterop, RoInitialize, RO_INIT_MULTITHREADED,
};
use windows_future::IAsyncOperation;

use super::{OsCheck, OsVerifier};

/// ERROR_CANCELLED：用户关掉了凭据框。
const ERROR_CANCELLED: u32 = 1223;

/// 真实的系统验证。`window` 是 ZeeTerm 窗口的 HWND（没有就让系统自己挑位置）。
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsVerifier {
    pub window: Option<isize>,
}

impl WindowsVerifier {
    pub fn new(window: Option<isize>) -> Self {
        Self { window }
    }
}

impl OsVerifier for WindowsVerifier {
    fn windows_hello(&self, message: &str) -> OsCheck {
        hello(self.window, message)
    }

    fn windows_credentials(&self, message: &str) -> OsCheck {
        credentials(self.window, message)
    }
}

fn hello(window: Option<isize>, message: &str) -> OsCheck {
    // WinRT 要求线程先初始化；已经初始化过（或套间模式不同）会返回错误，不影响继续。
    // SAFETY: 无前置条件，进程内重复调用是允许的。
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let availability = match UserConsentVerifier::CheckAvailabilityAsync().and_then(|op| op.join())
    {
        Ok(value) => value,
        Err(_) => return OsCheck::Unavailable,
    };
    if availability != UserConsentVerifierAvailability::Available {
        return OsCheck::Unavailable;
    }
    let text = HSTRING::from(message);
    let operation: windows::core::Result<IAsyncOperation<UserConsentVerificationResult>> =
        match window {
            Some(hwnd) => {
                factory::<UserConsentVerifier, IUserConsentVerifierInterop>().and_then(|interop| {
                    // SAFETY: hwnd 来自存活的 ZeeTerm 窗口；返回的异步操作由 windows crate 管理生命周期。
                    unsafe {
                        interop.RequestVerificationForWindowAsync(HWND(hwnd as *mut _), &text)
                    }
                })
            }
            None => UserConsentVerifier::RequestVerificationAsync(&text),
        };
    match operation.and_then(|op| op.join()) {
        Ok(UserConsentVerificationResult::Verified) => OsCheck::Verified,
        Ok(UserConsentVerificationResult::Canceled) => OsCheck::Cancelled,
        Ok(
            UserConsentVerificationResult::DeviceNotPresent
            | UserConsentVerificationResult::NotConfiguredForUser
            | UserConsentVerificationResult::DisabledByPolicy,
        ) => OsCheck::Unavailable,
        Ok(_) => OsCheck::Failed,
        Err(_) => OsCheck::Unavailable,
    }
}

/// 以 NUL 结尾的 UTF-16。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn zero(buffer: &mut [u16]) {
    for unit in buffer.iter_mut() {
        // volatile 写：别让编译器把“用完清零”优化掉。
        // SAFETY: 指针来自可变切片里的有效元素。
        unsafe { std::ptr::write_volatile(unit, 0) };
    }
}

fn credentials(window: Option<isize>, message: &str) -> OsCheck {
    let message_w = wide(message);
    let caption_w = wide("ZeeTerm");
    let info = CREDUI_INFOW {
        cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
        hwndParent: window.map_or(HWND::default(), |hwnd| HWND(hwnd as *mut _)),
        pszMessageText: PCWSTR(message_w.as_ptr()),
        pszCaptionText: PCWSTR(caption_w.as_ptr()),
        hbmBanner: Default::default(),
    };
    let mut package = 0u32;
    let mut out_buffer: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut out_size = 0u32;
    // SAFETY: 所有指针都指向本函数里存活的局部变量；成功时 out_buffer 由系统分配，下面负责释放。
    let status = unsafe {
        CredUIPromptForWindowsCredentialsW(
            Some(&info),
            0,
            &mut package,
            None,
            0,
            &mut out_buffer,
            &mut out_size,
            None,
            CREDUIWIN_ENUMERATE_CURRENT_USER,
        )
    };
    if status == ERROR_CANCELLED {
        return OsCheck::Cancelled;
    }
    if status != 0 || out_buffer.is_null() {
        return OsCheck::Unavailable;
    }

    let result = unpack_and_logon(out_buffer, out_size);

    // 系统给的缓冲区里有明文密码：先清零再还给系统。
    // SAFETY: out_buffer/out_size 正是系统返回的那一块。
    unsafe {
        std::ptr::write_bytes(out_buffer as *mut u8, 0, out_size as usize);
        CoTaskMemFree(Some(out_buffer as *const _));
    }
    result
}

fn unpack_and_logon(buffer: *mut core::ffi::c_void, size: u32) -> OsCheck {
    let mut user = vec![0u16; 513];
    let mut domain = vec![0u16; 257];
    let mut password = vec![0u16; 257];
    let (mut user_len, mut domain_len, mut password_len) = (
        user.len() as u32,
        domain.len() as u32,
        password.len() as u32,
    );
    // SAFETY: 三个缓冲区和长度配套；buffer/size 是上一步系统返回的有效数据。
    let unpacked = unsafe {
        CredUnPackAuthenticationBufferW(
            CRED_PACK_FLAGS(0),
            buffer,
            size,
            Some(PWSTR(user.as_mut_ptr())),
            &mut user_len,
            Some(PWSTR(domain.as_mut_ptr())),
            Some(&mut domain_len),
            Some(PWSTR(password.as_mut_ptr())),
            &mut password_len,
        )
    };
    let outcome = if unpacked.is_err() {
        OsCheck::Unavailable
    } else {
        logon_as_current_user(&user, &domain, &password)
    };
    zero(&mut user);
    zero(&mut domain);
    zero(&mut password);
    outcome
}

fn logon_as_current_user(user: &[u16], domain: &[u16], password: &[u16]) -> OsCheck {
    let mut token = HANDLE::default();
    // SAFETY: 三个参数都是以 NUL 结尾的缓冲区（CredUnPack 保证）；token 是有效的输出位置。
    let logged_on = unsafe {
        LogonUserW(
            PCWSTR(user.as_ptr()),
            PCWSTR(domain.as_ptr()),
            PCWSTR(password.as_ptr()),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if logged_on.is_err() {
        return OsCheck::Failed;
    }
    let same_user = same_user_as_process(token);
    // SAFETY: token 是 LogonUserW 刚给的有效句柄，只关一次。
    unsafe {
        let _ = CloseHandle(token);
    }
    if same_user {
        OsCheck::Verified
    } else {
        // 凭据对，但不是当前这个 Windows 用户：不算通过。
        OsCheck::Failed
    }
}

/// 令牌的用户 SID 是不是本进程的用户。
fn same_user_as_process(token: HANDLE) -> bool {
    let mut process_token = HANDLE::default();
    // SAFETY: GetCurrentProcess 返回伪句柄，process_token 是有效的输出位置。
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut process_token) };
    if opened.is_err() {
        return false;
    }
    let ours = token_user(process_token);
    let theirs = token_user(token);
    // SAFETY: process_token 是上面刚打开的有效句柄。
    unsafe {
        let _ = CloseHandle(process_token);
    }
    match (ours, theirs) {
        (Some(a), Some(b)) => {
            // SAFETY: 两块缓冲区都是 GetTokenInformation 填好的 TOKEN_USER，里面的 SID 指针指向缓冲区内部。
            unsafe {
                let sid_a = (*(a.as_ptr() as *const TOKEN_USER)).User.Sid;
                let sid_b = (*(b.as_ptr() as *const TOKEN_USER)).User.Sid;
                EqualSid(sid_a, sid_b).is_ok()
            }
        }
        _ => false,
    }
}

/// 取令牌的 TOKEN_USER。缓冲区用 u64 保证对齐。
fn token_user(token: HANDLE) -> Option<Vec<u64>> {
    let mut needed = 0u32;
    // 第一次只问需要多大（预期失败并填 needed）。
    // SAFETY: token 有效；传空缓冲区和长度 0 是文档规定的“问大小”用法。
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    if needed == 0 {
        return None;
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: buffer 至少 needed 字节，且按 8 字节对齐。
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    Some(buffer)
}
