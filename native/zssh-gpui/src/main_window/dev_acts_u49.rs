//! 调试驱动里 V2-U49（终端左侧行号 / 时间戳栏）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里
//! 只剩两个空函数。
//!
//! - `u49_feed:<文字>`：往活动窗格写一段内容（离线演示标签是回环：写进去的就是终端的输出）；文字里 `\r` `\n`
//!   `\e` `\t` 会换成对应的控制字符（`\e[?1049h` 进备用屏幕）、`\s` 是空格（行尾的空格会被脚本 trim 掉），不能含 `;`
//!   （那是步骤分隔符）。
//! - 开关本身用 `ui_set:show_line_numbers:on|off` / `ui_set:show_timestamps:on|off`（`dev_acts_u2`）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;

    const KNOWN: &[&str] = &["u49_feed"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        _window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let (name, rest) = spec.split_once(':').unwrap_or((spec, ""));
        if !is_known(name) {
            return None;
        }
        // 文字里可以有冒号（`ls -la` 的时间 `12:30`），所以不按冒号拆参数，整段都是文字。
        let bytes = unescape(rest);
        this.workspace.update(cx, |ws, cx| {
            ws.write_active(&bytes);
            cx.notify();
        });
        Some(Ok(Outcome::Done))
    }

    pub(in crate::main_window) fn unescape(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                continue;
            }
            match chars.next() {
                Some('r') => out.push(b'\r'),
                Some('n') => out.push(b'\n'),
                Some('e') => out.push(0x1b),
                Some('t') => out.push(b'\t'),
                // 行尾的空格会被脚本的 trim 吃掉，用 `\s` 写。
                Some('s') => out.push(b' '),
                Some('\\') => out.push(b'\\'),
                Some(other) => {
                    out.push(b'\\');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                }
                None => out.push(b'\\'),
            }
        }
        out
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
