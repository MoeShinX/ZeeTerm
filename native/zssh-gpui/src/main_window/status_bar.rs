//! 底部状态栏：当前活动会话的服务器状态（CPU / 内存 / 磁盘 / 运行时间）。
//!
//! 数据来自 `status_demo` 的进程级 hub，和状态窗口共用同一个轮询器，所以同一台主机不会因为
//! 开了状态栏又多一条采集；没有活动会话时整条隐藏。网速由 hub 用前后两次 `/proc/net/dev`
//! 的累计流量算出来，刚连上还没有第二个样本时显示「—」。

use ::gpui::{Hsla, px};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
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

impl MainWindow {
    /// 没有活动会话（或不在终端页）时返回 `None`，整条不占位置。
    pub(super) fn render_status_bar(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.status_host?;
        let overview_open = self.docks.overview_open;
        let bar = h_flex()
            .id("status-bar")
            .h(px(26.0))
            .w_full()
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(tokens.border)
            .bg(if overview_open {
                tokens.hover
            } else {
                tokens.surface
            })
            .typo(Role::Caption, tokens)
            // 点状态栏打开 / 关上服务器概览（同 0.2.14）。
            .cursor_pointer()
            .on_click({
                let entity = cx.entity();
                move |_, _window, cx| {
                    entity.update(cx, |this, cx| this.toggle_overview(cx));
                }
            });
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
        Some(match summary {
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
                bar.child(metric(
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
            None => bar
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
        })
    }
}
