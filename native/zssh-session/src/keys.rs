//! 键盘输入到终端字节的映射。
//!
//! 只做与 UI 无关的编码：调用方把 GPUI 的 `Keystroke` 拆成 `KeyInput` 再进来。
//! 覆盖 shell 日常输入、Ctrl 组合、DECCKM 应用光标模式和 xterm 修改键参数。
//! Kitty keyboard 协议和 Win32 输入模式还没做。

pub struct KeyInput<'a> {
    pub key: &'a str,
    /// 可打印字符；功能键/组合键为 `None`。
    pub key_char: Option<&'a str>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// DECCKM：远端要求方向键用 `ESC O A` 而不是 `ESC [ A`。
    pub app_cursor: bool,
}

/// xterm 的修改键参数：1 + Shift(1) + Alt(2) + Ctrl(4)。
fn modifier_param(input: &KeyInput<'_>) -> u8 {
    1 + u8::from(input.shift) + u8::from(input.alt) * 2 + u8::from(input.ctrl) * 4
}

pub fn encode_key(input: KeyInput<'_>) -> Option<Vec<u8>> {
    if input.ctrl {
        if input.key.len() == 1 {
            let ch = input.key.chars().next()?;
            if ch.is_ascii_alphabetic() {
                return Some(vec![ch.to_ascii_lowercase() as u8 - b'a' + 1]);
            }
            match ch {
                ' ' => return Some(vec![0]),
                '[' => return Some(vec![0x1b]),
                ']' => return Some(vec![0x1d]),
                '\\' => return Some(vec![0x1c]),
                _ => {}
            }
        }
        if input.key == "space" {
            return Some(vec![0]);
        }
    }

    let modified = modifier_param(&input);

    // 方向键 / Home / End：DECCKM 只影响无修改键的形式。
    if let Some(suffix) = match input.key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    } {
        return Some(if modified == 1 {
            if input.app_cursor {
                format!("\x1bO{suffix}").into_bytes()
            } else {
                format!("\x1b[{suffix}").into_bytes()
            }
        } else {
            format!("\x1b[1;{modified}{suffix}").into_bytes()
        });
    }

    if let Some(number) = function_key_number(input.key) {
        return Some(if number < 5 {
            if modified == 1 {
                format!("\x1bO{}", (b'P' + number - 1) as char).into_bytes()
            } else {
                format!("\x1b[1;{modified}{}", (b'P' + number - 1) as char).into_bytes()
            }
        } else {
            let suffix = if modified == 1 {
                format!("\x1b[{number}~")
            } else {
                format!("\x1b[{number};{modified}~")
            };
            suffix.into_bytes()
        });
    }

    match input.key {
        "enter" => return Some(vec![b'\r']),
        // Wayland 的输入法单独提交一个 ASCII 字符时，GPUI 把它伪装成按键：`key` 就是字符本身
        // （真正的回车键是 "enter"）。换行符走到 `key_char` 那条路会原样发出 LF，和按 Enter 的 CR 不一致。
        "\n" | "\r" => return Some(vec![b'\r']),
        "tab" if input.shift => return Some(b"\x1b[Z".to_vec()),
        "tab" => return Some(vec![b'\t']),
        "backspace" => return Some(vec![0x7f]),
        "escape" => return Some(vec![0x1b]),
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        "insert" => return Some(b"\x1b[2~".to_vec()),
        "delete" => return Some(b"\x1b[3~".to_vec()),
        _ => {}
    }

    let text = input.key_char?;
    if text.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() + 1);
    if input.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

/// 括号粘贴的结束标记。
const PASTE_END: &str = "\x1b[201~";

/// 剪贴板文字转终端字节：换行统一成 `\r`；远端开启 bracketed paste 时包一层；
/// 内容里自带的结束标记要删掉，否则粘贴内容可以提前结束括号模式。
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    // 删一遍不够：`ESC[2` + `ESC[201~` + `01~` 删掉中间那个后会重新拼出结束标记，
    // 所以删到再也找不到为止。
    let mut sanitized = normalized;
    while sanitized.contains(PASTE_END) {
        sanitized = sanitized.replace(PASTE_END, "");
    }
    if bracketed {
        format!("\x1b[200~{sanitized}\x1b[201~").into_bytes()
    } else {
        sanitized.into_bytes()
    }
}

/// 鼠标上报动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
    WheelUp,
    WheelDown,
}

/// 鼠标按键；移动事件没有按键时用 `None`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

/// 一次鼠标上报；坐标从 0 开始，编码时换成 1 开始。
pub struct MouseInput {
    pub action: MouseAction,
    pub button: MouseButton,
    pub col: usize,
    pub row: usize,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// 远端开启 1006 时用 SGR 扩展坐标，否则退回 X10。
    pub sgr: bool,
}

/// SGR 1006 的坐标上限；再大终端也塞不进去，直接钳住。
const SGR_MAX: usize = 2048;
/// X10 坐标加上 32 后必须落在一个字节里。
const X10_MAX: usize = 223;

/// 鼠标事件转终端字节：按键码、拖动位、修饰键位按 xterm 约定拼。
pub fn encode_mouse(input: MouseInput) -> Vec<u8> {
    let mut code: u16 = match input.action {
        MouseAction::Press | MouseAction::Release | MouseAction::Motion => match input.button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::None => 3,
        },
        MouseAction::WheelUp => 64,
        MouseAction::WheelDown => 65,
    };
    if input.action == MouseAction::Motion {
        code += 32;
    }
    // X10 没有独立的松开码，统一用 3 表示松开了某个键。
    if input.action == MouseAction::Release && !input.sgr {
        code = 3;
    }
    if input.shift {
        code += 4;
    }
    if input.alt {
        code += 8;
    }
    if input.ctrl {
        code += 16;
    }

    let col = (input.col + 1).min(if input.sgr { SGR_MAX } else { X10_MAX });
    let row = (input.row + 1).min(if input.sgr { SGR_MAX } else { X10_MAX });
    if input.sgr {
        let suffix = if input.action == MouseAction::Release {
            'm'
        } else {
            'M'
        };
        format!("\x1b[<{code};{col};{row}{suffix}").into_bytes()
    } else {
        vec![
            0x1b,
            b'[',
            b'M',
            code as u8 + 32,
            col as u8 + 32,
            row as u8 + 32,
        ]
    }
}

fn function_key_number(key: &str) -> Option<u8> {
    match key {
        "f1" => Some(1),
        "f2" => Some(2),
        "f3" => Some(3),
        "f4" => Some(4),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: &str) -> KeyInput<'_> {
        KeyInput {
            key,
            key_char: None,
            ctrl: false,
            alt: false,
            shift: false,
            app_cursor: false,
        }
    }

    fn character(key: &str) -> KeyInput<'_> {
        KeyInput {
            key,
            key_char: Some(key),
            ctrl: false,
            alt: false,
            shift: false,
            app_cursor: false,
        }
    }

    #[test]
    fn printable_text_goes_through_utf8() {
        assert_eq!(encode_key(character("a")).unwrap(), b"a");
        assert_eq!(
            encode_key(character("\u{4e2d}")).unwrap(),
            "\u{4e2d}".as_bytes()
        );
    }

    #[test]
    fn ctrl_letters_become_control_bytes() {
        for (letter, expected) in [("c", 0x03), ("d", 0x04), ("z", 0x1a)] {
            let bytes = encode_key(KeyInput {
                key: letter,
                key_char: Some(letter),
                ctrl: true,
                alt: false,
                shift: false,
                app_cursor: false,
            })
            .unwrap();
            assert_eq!(bytes, vec![expected]);
        }
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(encode_key(key("enter")).unwrap(), b"\r");
        assert_eq!(encode_key(key("backspace")).unwrap(), vec![0x7f]);
        assert_eq!(encode_key(key("escape")).unwrap(), vec![0x1b]);
        assert_eq!(encode_key(key("delete")).unwrap(), b"\x1b[3~");
        assert_eq!(encode_key(key("f5")).unwrap(), b"\x1b[15~");
    }

    /// Wayland 的 `commit_string` 恰好一个字节时 GPUI 发的是 `key == key_char == 字符` 的按键：
    /// 换行必须和 Enter 一样发 CR，不能原样发 LF。
    #[test]
    fn a_disguised_newline_commit_is_enter() {
        for text in ["\n", "\r"] {
            assert_eq!(encode_key(character(text)).unwrap(), b"\r", "{text:?}");
        }
        // 真的回车键和 Ctrl+J 不受影响。
        assert_eq!(encode_key(key("enter")).unwrap(), b"\r");
        let ctrl_j = KeyInput {
            ctrl: true,
            ..character("j")
        };
        assert_eq!(encode_key(ctrl_j).unwrap(), vec![0x0a]);
    }

    #[test]
    fn dec_ckm_switches_arrow_encoding() {
        assert_eq!(encode_key(key("up")).unwrap(), b"\x1b[A");
        let app = KeyInput {
            app_cursor: true,
            ..key("up")
        };
        assert_eq!(encode_key(app).unwrap(), b"\x1bOA");
    }

    #[test]
    fn modified_arrows_use_xterm_parameters() {
        let ctrl_up = KeyInput {
            ctrl: true,
            ..key("up")
        };
        assert_eq!(encode_key(ctrl_up).unwrap(), b"\x1b[1;5A");

        let shift_left = KeyInput {
            shift: true,
            ..key("left")
        };
        assert_eq!(encode_key(shift_left).unwrap(), b"\x1b[1;2D");

        let alt_right = KeyInput {
            alt: true,
            ..key("right")
        };
        assert_eq!(encode_key(alt_right).unwrap(), b"\x1b[1;3C");
    }

    #[test]
    fn shift_tab_is_backtab() {
        let bytes = encode_key(KeyInput {
            key: "tab",
            key_char: None,
            ctrl: false,
            alt: false,
            shift: true,
            app_cursor: false,
        })
        .unwrap();
        assert_eq!(bytes, b"\x1b[Z");
    }

    #[test]
    fn paste_normalizes_newlines() {
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
        assert_eq!(encode_paste("a\r\nb\rc", false), b"a\rb\rc");
    }

    #[test]
    fn bracketed_paste_wraps_once_and_strips_the_end_marker() {
        let bytes = encode_paste("evil\x1b[201~injected", true);
        assert_eq!(bytes, b"\x1b[200~evilinjected\x1b[201~".to_vec());
    }

    #[test]
    fn nested_end_marker_cannot_be_reassembled() {
        let nested = "a\x1b[2\x1b[201~01~; rm -rf x";
        let bytes = encode_paste(nested, true);
        assert_eq!(bytes, b"\x1b[200~a; rm -rf x\x1b[201~".to_vec());
        // 多层嵌套同样删干净：结束标记只能出现在最后一个位置。
        let deep = "\x1b[\x1b[2\x1b[201~01~201~tail";
        let bytes = String::from_utf8(encode_paste(deep, true)).unwrap();
        assert_eq!(bytes.matches(PASTE_END).count(), 1);
        assert!(bytes.ends_with(PASTE_END));
        // 不带括号模式时也不留下结束标记。
        let plain = String::from_utf8(encode_paste(nested, false)).unwrap();
        assert!(!plain.contains(PASTE_END));
    }

    #[test]
    fn alt_prefixes_escape() {
        let bytes = encode_key(KeyInput {
            key: "x",
            key_char: Some("x"),
            ctrl: false,
            alt: true,
            shift: false,
            app_cursor: false,
        })
        .unwrap();
        assert_eq!(bytes, b"\x1bx");
    }

    fn mouse(action: MouseAction, button: MouseButton, sgr: bool) -> MouseInput {
        MouseInput {
            action,
            button,
            col: 4,
            row: 2,
            shift: false,
            alt: false,
            ctrl: false,
            sgr,
        }
    }

    #[test]
    fn sgr_mouse_press_and_release() {
        assert_eq!(
            encode_mouse(mouse(MouseAction::Press, MouseButton::Left, true)),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse(mouse(MouseAction::Release, MouseButton::Left, true)),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            encode_mouse(mouse(MouseAction::Press, MouseButton::Right, true)),
            b"\x1b[<2;5;3M"
        );
    }

    #[test]
    fn x10_mouse_reporting_uses_byte_offsets() {
        assert_eq!(
            encode_mouse(mouse(MouseAction::Press, MouseButton::Left, false)),
            vec![0x1b, b'[', b'M', 32, 37, 35]
        );
        // 松开不带按键信息，统一是 3 + 32。
        assert_eq!(
            encode_mouse(mouse(MouseAction::Release, MouseButton::Right, false)),
            vec![0x1b, b'[', b'M', 35, 37, 35]
        );
    }

    #[test]
    fn motion_sets_the_drag_bit() {
        assert_eq!(
            encode_mouse(mouse(MouseAction::Motion, MouseButton::Left, true)),
            b"\x1b[<32;5;3M"
        );
        assert_eq!(
            encode_mouse(mouse(MouseAction::Motion, MouseButton::None, true)),
            b"\x1b[<35;5;3M"
        );
    }

    #[test]
    fn wheel_and_modifiers() {
        assert_eq!(
            encode_mouse(mouse(MouseAction::WheelUp, MouseButton::None, true)),
            b"\x1b[<64;5;3M"
        );
        assert_eq!(
            encode_mouse(mouse(MouseAction::WheelDown, MouseButton::None, true)),
            b"\x1b[<65;5;3M"
        );
        let mut input = mouse(MouseAction::Press, MouseButton::Left, true);
        input.shift = true;
        input.alt = true;
        input.ctrl = true;
        assert_eq!(encode_mouse(input), b"\x1b[<28;5;3M");
    }

    #[test]
    fn coordinates_are_clamped_per_protocol() {
        let mut x10 = mouse(MouseAction::Press, MouseButton::Left, false);
        x10.col = 999;
        x10.row = 999;
        assert_eq!(encode_mouse(x10), vec![0x1b, b'[', b'M', 32, 255, 255]);

        let mut sgr = mouse(MouseAction::Press, MouseButton::Left, true);
        sgr.col = 9999;
        sgr.row = 9999;
        assert_eq!(encode_mouse(sgr), b"\x1b[<0;2048;2048M");
    }
}
