//! 文件栏之间的拖动，和从文件栏拖到终端（0.2.14 `state/pointerDrag.ts` + `SftpFilePane` 的拖放 +
//! `PaneView` 的落点 + `state/pathQuote.ts`）。
//!
//! - 本地栏 ↔ 远端栏拖动 = 上传 / 下载：落在对面那一栏的目录行上就传进那个目录，没指到目录行就是这一栏
//!   的当前目录；拖到同一栏里什么也不做。条目取自按下时的选择（拖的是没选中的条目就换成它）。
//! - 从**远端**栏拖到终端：把远端路径（逐个按 POSIX 单引号规则引用，空格隔开）插到输入行，不执行、不加
//!   回车；路径里有终端控制字符就整个不插。本地栏的条目不接受。
//! - 从资源管理器拖进来：远端栏 = 上传（落在目录行上就传进那个目录），本地栏什么也不做。

use std::path::PathBuf;

use ::gpui::{ExternalPaths, SharedString, WeakEntity};
use gpui_kit::component::*;
use gpui_kit::*;

use super::SftpDemo;
use super::menu::Side;
use super::model::Entry;
use crate::app_theme::Tokens;
use crate::i18n::t;

/// 正在拖的文件栏条目。条目本身不放在载荷里：松手时按"按下时的选择"现取，所以多选拖动拖的是整个选择。
#[derive(Clone)]
pub struct SftpDrag {
    pub(super) view: WeakEntity<SftpDemo>,
    pub(super) view_id: u64,
    pub(super) side: Side,
    /// 按下的那一行（路径）：它在选择里就拖整个选择，不在就只拖它。
    pub(super) anchor: String,
}

impl SftpDrag {
    /// 拖的是远端栏的条目吗（终端只接这种）。
    pub fn is_remote(&self) -> bool {
        self.side == Side::Remote
    }

    /// 拖的是远端栏的条目时，它们的远端路径（给终端落点用）；本地栏的条目、视图已经关了都是 `None`。
    pub fn remote_paths(&self, cx: &App) -> Option<Vec<String>> {
        if self.side != Side::Remote {
            return None;
        }
        let view = self.view.upgrade()?;
        let paths: Vec<String> = view
            .read(cx)
            .drag_entries(self.side, &self.anchor)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        (!paths.is_empty()).then_some(paths)
    }
}

/// 几个路径插进终端的文字（0.2.14 `joinQuoted(paths, quoteRemotePath)`）：逐个引用，空格隔开，不加回车。
/// 路径里有终端控制字符时给出原因。
pub fn terminal_insert_text(paths: &[String]) -> Result<String, &'static str> {
    if paths.iter().any(|path| has_control_chars(path)) {
        return Err(t(super::SftpMsg::DragControlChars));
    }
    Ok(paths
        .iter()
        .map(|path| quote_remote_path(path))
        .collect::<Vec<_>>()
        .join(" "))
}

/// 远端路径给 POSIX shell 用：安全字符原样，其余用单引号包起来（`'` → `'\''`）。双引号里 shell 照样展开
/// `$()`、反引号和 `$变量`，不能用来装不可信的文件名。
pub(super) fn quote_remote_path(path: &str) -> String {
    let safe = !path.is_empty()
        && path.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '_' | '/' | '.' | ',' | ':' | '@' | '%' | '+' | '=' | '~' | '-'
                )
        });
    if safe {
        path.to_owned()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// 路径里有没有终端控制字符（C0 和 DEL）：单引号只管 shell 语法，终端行编辑在 shell 解析之前就会处理
/// 这些字符，所以带它们的文件名不能往终端里插。
pub(super) fn has_control_chars(text: &str) -> bool {
    text.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
}

/// 松手后传到哪一栏、用什么方式：只有拖到**对面**那一栏才算。
pub(super) fn transfer_side(dragged_from: Side, dropped_on: Side) -> Option<Side> {
    (dragged_from != dropped_on).then_some(dropped_on)
}

impl SftpDemo {
    /// 这一栏里按下 `anchor` 那一行时要拖的条目：它在选择里就是整个选择，不在就只有它；
    /// 名字不是 UTF-8 的（不可操作）不带。
    pub(super) fn drag_entries(&self, side: Side, anchor: &str) -> Vec<Entry> {
        let pane = self.pane(side);
        let selected = pane.selected();
        let picked: Vec<Entry> = if selected.iter().any(|entry| entry.path == anchor) {
            selected
        } else {
            pane.rows
                .iter()
                .filter(|entry| entry.path == anchor)
                .cloned()
                .collect()
        };
        picked
            .into_iter()
            .filter(|entry| entry.name_is_utf8)
            .collect()
    }

    /// 起一个拖动载荷（行上 `on_drag` 用）。
    pub(super) fn drag_payload(&self, cx: &Context<Self>, side: Side, anchor: &str) -> SftpDrag {
        SftpDrag {
            view: cx.entity().downgrade(),
            view_id: self.view_id,
            side,
            anchor: anchor.to_owned(),
        }
    }

    /// 跟着指针走的小标签上的字：单个条目写名字，多个写「N 项」。
    pub(super) fn drag_label(&self, side: Side, anchor: &str) -> String {
        let entries = self.drag_entries(side, anchor);
        match entries.as_slice() {
            [one] => one.name.clone(),
            many => format!("{}{}", many.len(), t(super::SftpMsg::ItemCountSuffix)),
        }
    }

    /// 拖动落在 `target` 这一栏上：`onto` 是落点的目录行（没指到目录行就是这一栏的当前目录）。
    /// 本地 → 远端是上传，远端 → 本地是下载，一次拖放一个批次；同一栏里拖什么也不做。
    pub(super) fn drop_pane_drag(
        &mut self,
        target: Side,
        onto: Option<String>,
        drag: &SftpDrag,
        cx: &mut Context<Self>,
    ) {
        // 别的视图（比如停靠面板和标签页各有一个）里拖来的不收：条目的选择在那个视图里。
        if drag.view_id != self.view_id || super::drag::transfer_side(drag.side, target).is_none() {
            return;
        }
        let entries = self.drag_entries(drag.side, &drag.anchor);
        if entries.is_empty() {
            self.warn(t(super::SftpMsg::DragNotUtf8));
            cx.notify();
            return;
        }
        let dir = onto.unwrap_or_else(|| self.pane(target).path.clone());
        match target {
            Side::Remote => self.upload_entries(&entries, &dir),
            Side::Local => self.download_entries(&entries, PathBuf::from(dir)),
        }
        cx.notify();
    }

    /// 从资源管理器拖进来落在远端栏：`onto` 是落点的目录行。
    pub(super) fn drop_external(
        &mut self,
        paths: &ExternalPaths,
        onto: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.handle_drop(paths, onto, cx);
    }
}

/// 拖动时跟着指针走的小标签：「N 项」或者单个名字。
pub struct SftpDragChip {
    text: SharedString,
}

impl SftpDragChip {
    pub(super) fn new(text: String) -> Self {
        Self { text: text.into() }
    }
}

impl Render for SftpDragChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        h_flex()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(tokens.accent)
            .bg(tokens.background)
            .shadow_lg()
            .text_xs()
            .text_color(tokens.text)
            .child(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]`。
    use super::{Side, has_control_chars, quote_remote_path, terminal_insert_text, transfer_side};

    #[test]
    fn quoting_follows_the_posix_single_quote_rule() {
        assert_eq!(quote_remote_path("/var/log/syslog"), "/var/log/syslog");
        assert_eq!(quote_remote_path("/a b/c"), "'/a b/c'");
        assert_eq!(quote_remote_path("it's"), r"'it'\''s'");
        assert_eq!(quote_remote_path("/tmp/$(rm -rf x)"), "'/tmp/$(rm -rf x)'");
        assert_eq!(quote_remote_path("/tmp/`id`"), "'/tmp/`id`'");
        assert_eq!(quote_remote_path("/tmp/中文 目录"), "'/tmp/中文 目录'");
        assert_eq!(quote_remote_path("/tmp/a;b"), "'/tmp/a;b'");
        assert_eq!(quote_remote_path("/tmp/a\\b"), r"'/tmp/a\b'");
        assert_eq!(quote_remote_path(""), "''");
    }

    #[test]
    fn terminal_text_refuses_control_characters() {
        let two = vec!["/a b/c".to_owned(), "/d".to_owned()];
        assert_eq!(terminal_insert_text(&two).as_deref(), Ok("'/a b/c' /d"));
        for bad in ["/a\nb", "/a\rb", "/a\x1bb", "/a\x7fb", "/a\x00b"] {
            assert!(terminal_insert_text(&[bad.to_owned()]).is_err(), "{bad:?}");
        }
        // 一个坏的就整个不插，别的好路径也不插。
        assert!(terminal_insert_text(&["/ok".to_owned(), "/a\nb".to_owned()]).is_err());
        assert!(has_control_chars("tab\there"));
        assert!(!has_control_chars("/中文 空格"));
    }

    #[test]
    fn only_the_opposite_pane_takes_a_drop() {
        assert_eq!(transfer_side(Side::Local, Side::Remote), Some(Side::Remote));
        assert_eq!(transfer_side(Side::Remote, Side::Local), Some(Side::Local));
        assert_eq!(transfer_side(Side::Remote, Side::Remote), None);
        assert_eq!(transfer_side(Side::Local, Side::Local), None);
    }
}
