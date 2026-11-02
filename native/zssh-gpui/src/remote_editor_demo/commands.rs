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

/// 注册键位。要在 `gpui_kit::init` 之后调（见 `main.rs`），后注册的同深度绑定优先。
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-s", Save, Some(CONTEXT)),
        KeyBinding::new("ctrl-g", GotoLine, Some(CONTEXT)),
        KeyBinding::new("ctrl-/", ToggleComment, Some(CONTEXT)),
        KeyBinding::new("alt-up", MoveLineUp, Some(CONTEXT)),
        KeyBinding::new("alt-down", MoveLineDown, Some(CONTEXT)),
        KeyBinding::new("shift-alt-up", CopyLineUp, Some(CONTEXT)),
        KeyBinding::new("shift-alt-down", CopyLineDown, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-k", DeleteLine, Some(CONTEXT)),
        KeyBinding::new("alt-l", SelectLine, Some(CONTEXT)),
        KeyBinding::new("ctrl-enter", InsertLineBelow, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-\\", JumpToBracket, Some(CONTEXT)),
        KeyBinding::new("ctrl-d", SelectNextOccurrence, Some(CONTEXT)),
        KeyBinding::new("f3", FindNext, Some(CONTEXT)),
        KeyBinding::new("shift-f3", FindPrevious, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(CONTEXT)),
        // 跳行条里的输入框是单行 Input，不是 editor 模式：Esc 单独绑，只在跳行条的上下文里。
        KeyBinding::new("escape", CloseGoto, Some("ZsshGoto")),
    ]);
}
