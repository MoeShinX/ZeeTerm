//! 编辑器命令和快捷键：保存、跳行、切换注释、上下移动 / 复制 / 删除行、选中行、下方插空行、
//! 跳到配对括号、选下一个相同内容。
//!
//! gpui-kit 0.8.1 的 Editor 自带光标移动、选择、撤销重做、缩进、查找替换、行号，但没有这几条
//! （CodeMirror 的 `defaultKeymap` / `searchKeymap` 里有）。键位照 0.2.14：
//!
//! | 键 | 命令 | CodeMirror 的名字 |
//! |---|---|---|
//! | Ctrl+S | 保存 | 自定义 `Mod-s` |
//! | Ctrl+G | 跳到行 | `gotoLine`（0.2.14 把 `Mod-g` 绑给了它） |
//! | Ctrl+/ | 切换注释 | `toggleComment` |
//! | Alt+↑ / Alt+↓ | 上 / 下移动行 | `moveLineUp` / `moveLineDown` |
//! | Shift+Alt+↑ / ↓ | 向上 / 下复制行 | `copyLineUp` / `copyLineDown` |
//! | Ctrl+Shift+K | 删除行 | `deleteLine` |
//! | Alt+L | 选中行 | `selectLine` |
//! | Ctrl+Enter | 下方插入空行 | `insertBlankLine` |
//! | Ctrl+Shift+\ | 跳到配对括号 | `cursorMatchingBracket` |
//! | Ctrl+D | 选下一个相同内容 | `selectNextOccurrence` |
//! | F3 / Shift+F3 | 查找下一个 / 上一个 | `findNext` / `findPrevious` |
//! | Ctrl+Shift+Z | 重做 | `historyKeymap` 的 `Mod-Shift-z` |
//!
//! 绑定的上下文是 `Input && mode == editor`（Editor 元素自带的 key context），所以只对代码编辑器生效；
//! 和组件自己的绑定（Shift+Alt+↑↓ 是「向上 / 下加光标」、Ctrl+Enter 是回车的次要形态）同样深，
//! 后注册的赢，所以这里在组件初始化之后注册。

use ::gpui::{App, KeyBinding, actions};
use gpui_kit::component::input::Redo;

actions!(
    zssh_editor,
    [
        Save,
        GotoLine,
        ToggleComment,
        MoveLineUp,
        MoveLineDown,
        CopyLineUp,
        CopyLineDown,
        DeleteLine,
        SelectLine,
        InsertLineBelow,
        JumpToBracket,
        SelectNextOccurrence,
        FindNext,
        FindPrevious,
        CloseGoto,
    ]
);

/// 只对代码编辑器里的焦点生效。
const CONTEXT: &str = "Input && mode == editor";

/// 主修饰键：macOS 上是 Cmd，其他平台是 Ctrl（表里写的 Ctrl 在 macOS 上读作 Cmd）。
/// 不能用 gpui 的 `secondary` 别名：这个版本的 `KeyBinding::new` 只认具体的修饰键名，用了会让程序启动就 panic。
#[cfg(target_os = "macos")]
macro_rules! primary {
    ($key:literal) => {
        concat!("cmd-", $key)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! primary {
    ($key:literal) => {
        concat!("ctrl-", $key)
    };
}

/// 带主修饰键的键位（`init` 和测试共用，保证测试检查的就是真正注册的字符串）。
const PRIMARY_KEYS: [&str; 8] = [
    primary!("s"),
    primary!("g"),
    primary!("/"),
    primary!("shift-k"),
    primary!("enter"),
    primary!("shift-\\"),
    primary!("d"),
    primary!("shift-z"),
];

/// 注册键位。要在 `gpui_kit::init` 之后调（见 `main.rs`），后注册的同深度绑定优先。
pub fn init(cx: &mut App) {
    let [
        save,
        goto,
        comment,
        delete_line,
        insert_below,
        bracket,
        next_occurrence,
        redo,
    ] = PRIMARY_KEYS;
    cx.bind_keys([
        KeyBinding::new(save, Save, Some(CONTEXT)),
        KeyBinding::new(goto, GotoLine, Some(CONTEXT)),
        KeyBinding::new(comment, ToggleComment, Some(CONTEXT)),
        KeyBinding::new("alt-up", MoveLineUp, Some(CONTEXT)),
        KeyBinding::new("alt-down", MoveLineDown, Some(CONTEXT)),
        KeyBinding::new("shift-alt-up", CopyLineUp, Some(CONTEXT)),
        KeyBinding::new("shift-alt-down", CopyLineDown, Some(CONTEXT)),
        KeyBinding::new(delete_line, DeleteLine, Some(CONTEXT)),
        KeyBinding::new("alt-l", SelectLine, Some(CONTEXT)),
        KeyBinding::new(insert_below, InsertLineBelow, Some(CONTEXT)),
        KeyBinding::new(bracket, JumpToBracket, Some(CONTEXT)),
        KeyBinding::new(next_occurrence, SelectNextOccurrence, Some(CONTEXT)),
        KeyBinding::new("f3", FindNext, Some(CONTEXT)),
        KeyBinding::new("shift-f3", FindPrevious, Some(CONTEXT)),
        KeyBinding::new(redo, Redo, Some(CONTEXT)),
        // 跳行条里的输入框是单行 Input，不是 editor 模式：Esc 单独绑，只在跳行条的上下文里。
        KeyBinding::new("escape", CloseGoto, Some("ZsshGoto")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::PRIMARY_KEYS;

    /// `KeyBinding::new` 解析失败会直接 panic（程序启动就崩），这里提前用同一个解析器把关。
    #[test]
    fn every_primary_binding_parses_with_gpui() {
        for keys in PRIMARY_KEYS {
            assert!(
                ::gpui::Keystroke::parse(keys).is_ok(),
                "gpui 不认这个键位：{keys}"
            );
        }
    }
}
