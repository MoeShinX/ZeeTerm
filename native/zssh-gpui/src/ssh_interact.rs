//! GPUI 入口的 SSH 交互：主机指纹确认用系统对话框，密码/口令暂时不支持。
//!
//! 指纹确认在 Tokio 的阻塞线程池里弹 MessageBox，不占异步 worker；
//! 自动化可用 `ZSSH_SSH_TRUST_ONCE=1` 跳过对话框（只信任本次，不写 known_hosts）；
//! 该钩子只在调试构建里生效，见 `dev_hooks`。

use std::sync::Arc;

use zssh_core::ssh::{
    BoxFuture, HostKeyDecision, HostKeyInfo, HostKeyStatus, Interactor, PromptAnswers,
    PromptRequest,
};

/// 用系统对话框确认主机指纹。
pub struct NativeInteractor;

impl Interactor for NativeInteractor {
    fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async move {
            if crate::dev_hooks::var_os("ZSSH_SSH_TRUST_ONCE").is_some() {
                return HostKeyDecision::TrustOnce;
            }
            tokio::task::spawn_blocking(move || ask_host_key(&info))
                .await
                .unwrap_or(HostKeyDecision::Reject)
        })
    }

    fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || ask_prompt(&request))
                .await
                .ok()
                .flatten()
        })
    }
}

/// 密码/口令/单轮 keyboard-interactive 的输入框；多题会依次列出。
fn ask_prompt(request: &PromptRequest) -> Option<PromptAnswers> {
    if request.prompts.is_empty() {
        return None;
    }
    let mut header = String::new();
    if !request.name.is_empty() {
        header.push_str(&request.name);
        header.push('\n');
    }
    if !request.instructions.is_empty() {
        header.push_str(&request.instructions);
        header.push('\n');
    }
    let fields: Vec<(String, bool)> = request
        .prompts
        .iter()
        .map(|item| (item.text.clone(), !item.echo))
        .collect();
    let answers = show_prompt(&header, &fields)?;
    Some(PromptAnswers {
        answers,
        remember: false,
    })
}

#[derive(Default)]
struct PromptState {
    edits: Vec<windows::Win32::Foundation::HWND>,
    answers: Option<Vec<String>>,
}

thread_local! {
    static PROMPT: std::cell::RefCell<Option<PromptState>> =
        const { std::cell::RefCell::new(None) };
}

fn show_prompt(header: &str, fields: &[(String, bool)]) -> Option<Vec<String>> {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        BS_DEFPUSHBUTTON, CreateWindowExW, DispatchMessageW, ES_AUTOHSCROLL, ES_PASSWORD,
        GetMessageW, GetSystemMetrics, MSG, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
        SetForegroundWindow, ShowWindow, TranslateMessage, WINDOW_STYLE, WNDCLASSW, WS_CAPTION,
        WS_CHILD, WS_EX_TOPMOST, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    let hinstance = unsafe { GetModuleHandleW(None) }.ok()?;
    let hinstance = windows::Win32::Foundation::HINSTANCE(hinstance.0);
    let class = w!("ZsshPromptWindow");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(prompt_proc),
        hInstance: hinstance,
        lpszClassName: class,
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };

    const ROW: i32 = 54;
    let width = 500;
    let header_h = if header.trim().is_empty() { 12 } else { 56 };
    let buttons_y = 12 + header_h + fields.len() as i32 * ROW + 8;
    let height = buttons_y + 64;
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let x = (screen_w - width) / 2;
    let y = (screen_h - height) / 2;
    let title = HSTRING::from(format!("{} 需要输入", crate::product::NAME));
    PROMPT.with(|slot| *slot.borrow_mut() = Some(PromptState::default()));
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(hinstance),
            None,
        )
    }
    .ok()?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    if !header.trim().is_empty() {
        let header_wide: Vec<u16> = header.encode_utf16().chain(Some(0)).collect();
        let _ = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                PCWSTR(header_wide.as_ptr()),
                WS_CHILD | WS_VISIBLE,
                16,
                10,
                width - 32,
                header_h - 8,
                Some(hwnd),
                None,
                Some(hinstance),
                None,
            )
        };
    }

    let mut edits = Vec::with_capacity(fields.len());
    for (index, (label, password)) in fields.iter().enumerate() {
        let top = 12 + header_h + index as i32 * ROW;
        let label_wide: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
        let _ = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                PCWSTR(label_wide.as_ptr()),
                WS_CHILD | WS_VISIBLE,
                16,
                top,
                width - 32,
                18,
                Some(hwnd),
                None,
                Some(hinstance),
                None,
            )
        };
        let edit_style = WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WINDOW_STYLE(ES_AUTOHSCROLL as u32)
            | if *password {
                WINDOW_STYLE(ES_PASSWORD as u32)
            } else {
                WINDOW_STYLE(0)
            };
        let edit = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("EDIT"),
                PCWSTR::null(),
                edit_style,
                16,
                top + 20,
                width - 32,
                26,
                Some(hwnd),
                None,
                Some(hinstance),
                None,
            )
        }
        .ok()?;
        edits.push(edit);
    }
    PROMPT.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.edits = edits.clone();
        }
    });
    if let Some(first) = edits.first() {
        unsafe {
            let _ = SetFocus(Some(*first));
        }
    }

    for (id, text, left) in [(1usize, "确定", width - 190), (2, "取消", width - 100)] {
        let text = HSTRING::from(text);
        let _ = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("BUTTON"),
                PCWSTR(text.as_ptr()),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WINDOW_STYLE((if id == 1 { BS_DEFPUSHBUTTON } else { 0 }) as u32),
                left,
                buttons_y,
                80,
                28,
                Some(hwnd),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                    id as *mut core::ffi::c_void,
                )),
                Some(hinstance),
                None,
            )
        };
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    PROMPT
        .with(|slot| slot.borrow_mut().take())
        .and_then(|s| s.answers)
}

unsafe extern "system" fn prompt_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, GetWindowTextLengthW, GetWindowTextW, PostQuitMessage,
        WM_CLOSE, WM_COMMAND, WM_DESTROY,
    };
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            if id == 1 {
                let answers = PROMPT.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    slot.as_mut().map(|state| {
                        state
                            .edits
                            .iter()
                            .map(|edit| {
                                let len = unsafe { GetWindowTextLengthW(*edit) };
                                let mut buf = vec![0u16; len.max(0) as usize + 1];
                                let copied = unsafe { GetWindowTextW(*edit, &mut buf) };
                                String::from_utf16_lossy(&buf[..copied.max(0) as usize])
                            })
                            .collect::<Vec<_>>()
                    })
                });
                PROMPT.with(|slot| {
                    if let Some(state) = slot.borrow_mut().as_mut() {
                        state.answers = answers;
                    }
                });
                let _ = unsafe { DestroyWindow(hwnd) };
            } else if id == 2 {
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            windows::Win32::Foundation::LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            windows::Win32::Foundation::LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            windows::Win32::Foundation::LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn ask_host_key(info: &HostKeyInfo) -> HostKeyDecision {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_TOPMOST, MB_YESNO, MessageBoxW,
    };
    use windows::core::HSTRING;

    let status = match info.status {
        HostKeyStatus::Unknown => "第一次连接这台主机".to_owned(),
        HostKeyStatus::OtherKeyTypesSaved => "这台主机只保存过其它类型的密钥".to_owned(),
        HostKeyStatus::Changed => format!(
            "指纹已变化！\n已保存：{}",
            info.stored_fingerprint.as_deref().unwrap_or("（未知）")
        ),
        HostKeyStatus::Revoked => "这个指纹已被标记为吊销".to_owned(),
    };
    let text = format!(
        "主机：{}:{}\n密钥类型：{}\n指纹：{}\n\n{}\n\n信任并保存到 known_hosts？",
        info.host, info.port, info.key_type, info.fingerprint, status
    );
    let title = HSTRING::from(format!("{} 主机指纹确认", crate::product::NAME));
    let body = HSTRING::from(text);
    let result = unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(body.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST,
        )
    };
    if result == IDYES {
        HostKeyDecision::TrustAndSave
    } else {
        HostKeyDecision::Reject
    }
}

/// 让入口只拿一个 `Arc<dyn Interactor>`。
pub fn interactor() -> Arc<dyn Interactor> {
    Arc::new(NativeInteractor)
}
