//! 离线演示 / 调试钩子用的终端小工具（从 `terminal_workspace.rs` 拆出来）。
//! 都是 `#[cfg(debug_assertions)]`，release 里不存在。

use super::*;

impl TerminalWorkspace {
    /// 截图/手动验证用：让终端进入 1002 + 1006 鼠标上报模式。
    #[cfg(debug_assertions)]
    pub fn seed_mouse_reporting(&mut self, cx: &mut Context<Self>) {
        self.write_active(b"\x1b[?1002h\x1b[?1006h");
        cx.notify();
    }

    /// 所有标签的所有窗格各喂一批：后台标签同样继续消费，切回时不重建。
    #[cfg(debug_assertions)]
    pub(crate) fn feed_batch(&mut self, lines: usize) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                for i in 0..lines {
                    let n = pane.fed + i;
                    let line = format!(
                        "\x1b[32mok\x1b[0m  line {n:04}  \u{4e2d}\u{6587}\u{8f93}\u{51fa} \x1b[31merror\x1b[0m \x1b[33mwarn\x1b[0m \x1b[34mhttps://example.com/{n}\x1b[0m  \x1b[1mbold\x1b[0m\r\n"
                    );
                    pane.write(line.as_bytes());
                }
                pane.fed += lines;
            }
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn refresh_all(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.refresh();
            }
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn feed_more(&mut self) {
        self.feed_batch(20);
        self.refresh_all();
    }
}
