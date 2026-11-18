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

    /// 验收用：终端区（上一帧交给工作区的那块区域）的宽度，和聚焦窗格现在的列数。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_area_and_cols(&self) -> Option<(f32, usize)> {
        let width = f32::from(self.area.get()?.size.width);
        let cols = self.pane()?.snapshot.size.cols;
        Some((width, cols))
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

    // ---- 模拟 AI 在活动窗格的终端里执行命令（V2-U50 的截图 / 单元测试用，不连任何服务器、不拉智能体） ----
    //
    // 和真实流程走同一条路：先 `ai_arm`（会话任务写命令之前发的那个事件），再把 shell 的回显、开始标记、输出、
    // 结束标记当作远端数据喂进泵——所以显示过滤器、合成回显、标记行都是真的，只是「远端」是我们自己。

    /// 第一步：AI 命令开始（`ai_arm` + 回显 + 开始标记行）。`agent` 是开头标记里写的智能体名。
    #[cfg(any(debug_assertions, test))]
    pub(crate) fn dev_ai_begin(&mut self, agent: Option<&str>, command: &str) {
        use zssh_core::ai::pty::{build_clear_prefix, build_wrapped_command};
        // 真实流程里这一步是下一次轮询做的；演示 / 测试直接做，不依赖定时器。
        self.sync_ai_marks();
        let Some(pane) = self.pane_mut() else {
            return;
        };
        let wrapped = build_wrapped_command(command, DEV_AI_MARKER);
        let payload = format!("{}{}\r", build_clear_prefix(), wrapped);
        let pump = pane.pump_mut();
        let generation = pump.generation();
        pump.ai_arm(generation, DEV_AI_MARKER, &payload, command, agent);
        let echo = format!(
            "\x07{}\r\n\x1b[?2004l\r\r\n{DEV_AI_MARKER}_S\r\n",
            wrapped.trim_end()
        );
        pump.on_data(generation, echo.as_bytes());
        pane.refresh();
    }

    /// 第二步（可多次）：命令的输出。
    #[cfg(any(debug_assertions, test))]
    pub(crate) fn dev_ai_output(&mut self, text: &str) {
        let Some(pane) = self.pane_mut() else {
            return;
        };
        let pump = pane.pump_mut();
        let generation = pump.generation();
        pump.on_data(generation, text.as_bytes());
        pane.refresh();
    }

    /// 第三步：命令正常结束（结束标记行 + 新提示符）。
    #[cfg(any(debug_assertions, test))]
    pub(crate) fn dev_ai_end(&mut self, exit_code: i32) {
        let Some(pane) = self.pane_mut() else {
            return;
        };
        let pump = pane.pump_mut();
        let generation = pump.generation();
        let tail = format!("{DEV_AI_MARKER}_E:{exit_code}\r\n\x1b[?2004h$ ");
        pump.on_data(generation, tail.as_bytes());
        pane.refresh();
    }

    /// 第三步的另一种：AI 等不及了（超时返回），命令还在终端里跑。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_ai_timeout(&mut self) {
        let Some(pane) = self.pane_mut() else {
            return;
        };
        let pump = pane.pump_mut();
        let generation = pump.generation();
        pump.ai_release(generation, DEV_AI_MARKER);
        pane.refresh();
    }

    /// 活动窗格现在屏幕上的文字（一行一个，去掉行尾空白）：测试和截图脚本的断言用。
    #[cfg(any(debug_assertions, test))]
    pub(crate) fn dev_screen_lines(&self) -> Vec<String> {
        self.pane()
            .map(|pane| {
                pane.snapshot
                    .rows
                    .iter()
                    .map(|row| row.text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 模拟 AI 命令用的固定标记（一次只模拟一条）。
#[cfg(any(debug_assertions, test))]
const DEV_AI_MARKER: &str = "__ZSSH_DEMO0000000000__";
