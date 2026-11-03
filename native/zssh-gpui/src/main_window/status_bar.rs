//! 终端页底部的状态栏：左边是当前活动会话的服务器状态（CPU / 内存 / 磁盘 / 运行时间），右边常驻
//! 快捷命令栏、撰写栏两个开关（V2-U6 从标题栏挪来；任何终端标签都有，SSH / Telnet / 串口一样）。
//!
//! 服务器状态的数据来自 `status_demo` 的进程级 hub，和状态窗口共用同一个轮询器，所以同一台主机不会因为
//! 开了状态栏又多一条采集；Telnet / 串口、没有活动会话或设置里关掉时左边是空的，条本身照常画。
//! 网速由 hub 用前后两次 `/proc/net/dev` 的累计流量算出来，刚连上还没有第二个样本时显示「—」。

use ::gpui::{Hsla, Stateful, px};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::status_demo::{
    Level, StatusSummary, level_of, mib_gb_label, net_speed_label, uptime_label,
};
use crate::typography::{Role, Typo as _};

/// 数值颜色：80% 黄、95% 红，其余用次级文字色（同 0.2.14 的 `usageClass`）。
fn level_color(percent: Option<u8>, tokens: &Tokens) -> Hsla {
    match percent.map(level_of) {
        Some(Level::Critical) => tokens.danger,
        Some(Level::Warn) => tokens.dot_connecting,
        _ => tokens.text_muted,
    }
}

/// 一格宽的迷你条（0.2.14 `MiniBar`：宽 36、高 6），宽度跟着用量走。
fn mini_bar(percent: u8, tokens: &Tokens) -> impl IntoElement {
    let fill = match level_of(percent) {
        Level::Critical => tokens.danger,
        Level::Warn => tokens.dot_connecting,
        Level::Normal => tokens.text_muted,
    };
    div()
        .w(px(36.0))
        .h(px(6.0))
        .flex_none()
        .rounded_full()
        .overflow_hidden()
        .bg(tokens.subtle)
        .child(
            div()
                .h_full()
                .w(::gpui::relative(f32::from(percent.min(100)) / 100.0))
                .rounded_full()
                .bg(fill),
        )
}

/// 状态栏里的一项：图标 + 数值 + 迷你条。没有读数时数值显示 `--`。
fn metric(icon: Icon, value: String, percent: Option<u8>, tokens: &Tokens) -> impl IntoElement {
    let color = level_color(percent, tokens);
    h_flex()
        .flex_none()
        .gap_1()
        .items_center()
        .child(icon.size_3().text_color(color))
        .child(div().text_color(color).child(value))
        .children(percent.map(|p| mini_bar(p, tokens)))
}

/// 网速一项的文字：收 / 发；还没有两个样本（刚连上）显示「—」。
fn net_label(summary: &StatusSummary) -> (String, String) {
    match summary.net_rate {
        Some((rx, tx)) => (net_speed_label(rx), net_speed_label(tx)),
        None => ("—".to_owned(), "—".to_owned()),
    }
}

/// 命令栏 / 撰写栏开关所在的这一条要不要画开关：只在终端页、有标签、不是 SFTP / 编辑器标签时
/// （这几种情况和底部两条栏本身出现的条件一致）。欢迎页、主机页、设置页没有终端。
pub(super) fn bar_switches_visible(view: MainView, tab_count: usize, file_tab: bool) -> bool {
    view == MainView::Terminal && tab_count > 0 && !file_tab
}

/// 状态栏的高度：固定值，开关常驻时终端区域不会因为服务器状态出现 / 消失而跳动。
pub(super) const STATUS_BAR_HEIGHT: f32 = 26.0;

/// 状态栏右侧的小图标开关（开着时高亮）；`occlude` 让点它不会同时触发底下「点状态栏开概览」。
fn switch_button(
    id: &'static str,
    icon: Ico,
    tooltip: &'static str,
    on: bool,
    tokens: &Tokens,
) -> Stateful<Div> {
    let hover = tokens.hover;
    let text = tokens.text;
    div()
        .id(id)
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.0))
        .flex_none()
        .rounded_md()
        .cursor_pointer()
        .text_color(if on { tokens.accent } else { tokens.text_muted })
        .when(on, |d| d.bg(tokens.accent_soft))
        .hover(move |style| style.bg(hover).text_color(text))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .child(Icon::new(icon).size_4())
}

impl MainWindow {
    /// 现在这一帧状态栏上有没有两个开关。
    pub(super) fn bar_switches_now(&self, cx: &App) -> bool {
        let tab_count = self.workspace.read(cx).tab_count();
        let file_tab = self.docks.active_sftp.is_some() || self.docks.active_editor.is_some();
        bar_switches_visible(self.nav.view, tab_count, file_tab)
    }

    /// 终端页底部的状态栏：左边是服务器状态（只在 SSH 连接且设置里开着时有内容），右边常驻
    /// 快捷命令栏、撰写栏两个开关。条的高度固定，所以服务器状态出现 / 消失终端区域不跳；
    /// 没有终端的页面（欢迎页、主机页、设置页）整条不画。
    pub(super) fn render_status_bar(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let switches_on = self.bar_switches_now(cx);
        if self.status_host.is_none() && !switches_on {
            return None;
        }
        let overview_open = self.overview_open(cx) && self.status_host.is_some();
        let entity = cx.entity();
        let bar = h_flex()
            .id("status-bar")
            .h(px(STATUS_BAR_HEIGHT))
            .w_full()
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(tokens.divider)
            .bg(if overview_open {
                tokens.hover
            } else {
                tokens.background
            })
            .typo(Role::Caption, tokens);
        // 左边这块（服务器状态）可点：打开 / 关上服务器概览（同 0.2.14）。
        let stats_area = h_flex()
            .id("status-stats")
            .flex_1()
            .min_w_0()
            .h_full()
            // 窗口很窄时服务器状态各项放不下：裁掉，不压到右边的开关上。
            .overflow_hidden()
            .gap_3()
            .items_center()
            .when(self.status_host.is_some(), |d| {
                let entity = entity.clone();
                d.cursor_pointer().on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| this.toggle_overview(cx));
                })
            });
        let stats_area = match self.status_host {
            Some(_) => self.fill_status_stats(stats_area, overview_open, tokens),
            None => stats_area,
        };
        let switches = switches_on.then(|| {
            let (quick_entity, compose_entity) = (entity.clone(), entity.clone());
            h_flex()
                .flex_none()
                .gap_0p5()
                .items_center()
                .child(
                    switch_button(
                        "quick-switch",
                        Ico::Command,
                        "快捷命令栏",
                        self.quick_bar_open(),
                        tokens,
                    )
                    .on_click(move |_, window, cx| {
                        quick_entity.update(cx, |this, cx| this.toggle_quick_bar(window, cx));
                    }),
                )
                .child(
                    switch_button(
                        "compose-switch",
                        Ico::SquarePen,
                        "撰写栏",
                        self.compose_bar_open(),
                        tokens,
                    )
                    .on_click(move |_, window, cx| {
                        compose_entity.update(cx, |this, cx| this.toggle_compose_bar(window, cx));
                    }),
                )
        });
        Some(bar.child(stats_area).children(switches))
    }

    /// 服务器状态那一块的内容：有读数时 CPU / 内存 / 磁盘 / 网速 / 运行时间，没有读数时说明原因，
    /// 最右是「概览」入口。
    fn fill_status_stats(
        &self,
        area: Stateful<Div>,
        overview_open: bool,
        tokens: &Tokens,
    ) -> Stateful<Div> {
        let status = self.status.as_ref();
        let summary: Option<&StatusSummary> = status.and_then(|s| s.summary.as_ref());
        let error = status.and_then(|s| s.error.as_deref());
        let overview_entry = h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .text_color(if overview_open {
                tokens.accent
            } else {
                tokens.text_muted
            })
            .child(Icon::new(Ico::CircleGauge).size_3())
            .child("概览");
        match summary {
            Some(s) => {
                let stats = &s.stats;
                let (rx, tx) = net_label(s);
                let mem_text = match (stats.mem_used_mib, stats.mem_total_mib) {
                    (Some(used), Some(total)) => format!(
                        "{}/{}",
                        mib_gb_label(Some(used as f64), 1),
                        mib_gb_label(Some(total as f64), 1)
                    ),
                    _ => "--".to_owned(),
                };
                let disk_text = match (stats.disk_used_mib, stats.disk_total_mib) {
                    (Some(used), Some(total)) => format!(
                        "{}/{}",
                        mib_gb_label(Some(used), 0),
                        mib_gb_label(Some(total), 0)
                    ),
                    _ => "--".to_owned(),
                };
                area.child(metric(
                    Icon::new(IconName::Cpu),
                    stats
                        .cpu_percent
                        .map_or_else(|| "--".to_owned(), |p| format!("{p}%")),
                    stats.cpu_percent,
                    tokens,
                ))
                .child(metric(
                    Icon::new(IconName::MemoryStick),
                    mem_text,
                    s.mem_percent(),
                    tokens,
                ))
                .child(metric(
                    Icon::new(IconName::HardDrive),
                    disk_text,
                    stats.disk_percent,
                    tokens,
                ))
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .items_center()
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .size_3()
                                .text_color(tokens.dot_connected),
                        )
                        .child(div().min_w(px(52.0)).child(rx))
                        .child(
                            Icon::new(IconName::ArrowUp)
                                .size_3()
                                .text_color(tokens.accent),
                        )
                        .child(div().min_w(px(52.0)).child(tx)),
                )
                .when_some(stats.uptime_secs, |d, secs| {
                    d.child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(Icon::new(Ico::Clock).size_3())
                            .child(format!("运行 {}", uptime_label(secs))),
                    )
                })
                // 采集失败但还留着上一次的数据：标出来，别让旧数字冒充实时。
                .when_some(error, |d, error| {
                    d.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(tokens.danger)
                            .child(format!("已断开：{error}")),
                    )
                })
                .child(div().flex_1())
                .child(overview_entry)
            }
            None => area
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(if error.is_some() {
                            tokens.danger
                        } else {
                            tokens.text_muted
                        })
                        .child(match error {
                            Some(error) => format!("状态采集失败：{error}"),
                            None => "状态采集等待中…".to_owned(),
                        }),
                )
                .child(div().flex_1())
                .child(overview_entry),
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{STATUS_BAR_HEIGHT, bar_switches_visible};
    use crate::main_window::nav::MainView;

    #[test]
    fn the_bar_switches_follow_the_terminal() {
        assert!(bar_switches_visible(MainView::Terminal, 1, false));
        assert!(
            bar_switches_visible(MainView::Terminal, 3, false),
            "任何终端标签都有开关，和协议、服务器状态栏设置无关"
        );
        for view in [MainView::Welcome, MainView::Vault, MainView::Settings] {
            assert!(!bar_switches_visible(view, 2, false), "{view:?} 没有终端");
        }
        assert!(!bar_switches_visible(MainView::Terminal, 0, false));
        assert!(
            !bar_switches_visible(MainView::Terminal, 2, true),
            "SFTP / 编辑器标签里不画底部两条栏，开关也不出现"
        );
    }

    #[test]
    fn the_status_bar_height_is_fixed() {
        // 栏高的上限（`bars::area_height`）按这个高度扣；改它要一起改。
        assert_eq!(STATUS_BAR_HEIGHT, 26.0);
    }
}
