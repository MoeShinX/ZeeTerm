//! 服务器概览面板（0.2.14 `ServerOverviewPanel.tsx`）：点底部状态栏打开，盖在终端右边（不挤终端）。
//!
//! 数据就是状态栏那条采集（和状态窗口共用同一个轮询器，core 的 `StatsCollector`），不额外开连接、
//! 不额外跑命令；面板关着不渲染。内容同 0.2.14：CPU / 内存 / 磁盘 / 负载四个圆环、网速曲线、
//! 每个核心、系统信息、全部分区、网卡、占内存最多的进程。

use std::collections::VecDeque;

use ::gpui::{PathBuilder, Pixels, canvas, point, px};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::status_demo::{
    Level, StatusSummary, level_of, mib_gb_label, net_speed_label, traffic_label, uptime_label,
};
use crate::typography::{Role, Typo as _};

/// 网速曲线最多记这么多个点（3 秒一次 ≈ 6 分钟）。
pub const NET_HISTORY_MAX: usize = 120;
/// 概览面板的宽度（0.2.14 默认 360）。
pub const OVERVIEW_WIDTH: f32 = 360.0;

/// 追加一个点，超过上限丢最旧的。
pub fn push_net_point(history: &mut VecDeque<(f64, f64)>, point: (f64, f64)) {
    if history.len() >= NET_HISTORY_MAX {
        history.pop_front();
    }
    history.push_back(point);
}

/// 用量颜色档位：80% 黄、95% 红，其余用主题强调色。
fn ring_color(percent: Option<u8>, tokens: &Tokens) -> ::gpui::Hsla {
    match percent.map(level_of) {
        None => tokens.text_muted,
        Some(Level::Critical) => tokens.danger,
        Some(Level::Warn) => tokens.dot_connecting,
        Some(Level::Normal) => tokens.accent,
    }
}

/// 一个用量圆环：底圈 + 弧，中间是百分数。
fn ring(
    label: &'static str,
    percent: Option<u8>,
    sub: String,
    tokens: &Tokens,
) -> impl IntoElement {
    let color = ring_color(percent, tokens);
    let track = tokens.subtle;
    let shown = percent.map_or(0.0, |p| f32::from(p.min(100)));
    let value = percent.map_or_else(|| "--".to_owned(), |p| format!("{p}%"));
    v_flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .relative()
                .size(px(64.0))
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            let center = bounds.center();
                            let radius = 26.0_f32;
                            let stroke =
                                |fraction: f32, color: ::gpui::Hsla, window: &mut Window| {
                                    if fraction <= 0.0 {
                                        return;
                                    }
                                    // 从 12 点钟方向顺时针画，满圈分成两段半圆避免起终点重合。
                                    let mut path = PathBuilder::stroke(px(6.0));
                                    let steps = ((fraction * 72.0).ceil() as usize).max(2);
                                    for i in 0..=steps {
                                        let angle = -std::f32::consts::FRAC_PI_2
                                            + fraction
                                                * std::f32::consts::TAU
                                                * (i as f32 / steps as f32);
                                        let p = point(
                                            center.x + px(radius * angle.cos()),
                                            center.y + px(radius * angle.sin()),
                                        );
                                        if i == 0 {
                                            path.move_to(p);
                                        } else {
                                            path.line_to(p);
                                        }
                                    }
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, color);
                                    }
                                };
                            stroke(0.9999, track, window);
                            stroke(shown / 100.0, color, window);
                        },
                    )
                    .size_full(),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(value),
                ),
        )
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text_muted)
                .child(label),
        )
        .child(
            div()
                .max_w(px(80.0))
                .truncate()
                .typo(Role::Caption, tokens)
                .child(::gpui::SharedString::from(sub)),
        )
}

/// 网速曲线：下行实线、上行虚线（同 0.2.14），纵轴按这段时间的最大值缩放。
fn net_chart(history: &VecDeque<(f64, f64)>, tokens: &Tokens) -> AnyElement {
    if history.len() < 2 {
        return div()
            .h(px(56.0))
            .flex()
            .items_center()
            .typo(Role::Caption, tokens)
            .child("正在积累数据…")
            .into_any_element();
    }
    let points: Vec<(f64, f64)> = history.iter().copied().collect();
    let down = tokens.dot_connected;
    let up = tokens.accent;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let max = points
                .iter()
                .fold(1.0_f64, |acc, (rx, tx)| acc.max(*rx).max(*tx));
            // 点少的时候按 30 个点的宽度铺开，多了再往左延伸。
            let slots = points.len().max(30);
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            let step = width / (slots as f32 - 1.0);
            let offset = width - step * (points.len() as f32 - 1.0);
            let at = |i: usize, value: f64| {
                point(
                    bounds.origin.x + px(offset + i as f32 * step),
                    bounds.origin.y + px(height - 2.0 - (value / max) as f32 * (height - 4.0)),
                )
            };
            // 下行：实线。
            let mut path = PathBuilder::stroke(px(1.5));
            for (i, p) in points.iter().enumerate() {
                if i == 0 {
                    path.move_to(at(i, p.0));
                } else {
                    path.line_to(at(i, p.0));
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, down);
            }
            // 上行：虚线（3 画 2 空，沿折线按长度切，同 0.2.14 的 `strokeDasharray="3 2"`）。
            let mut drawing = true;
            let mut left = 3.0_f32;
            for i in 1..points.len() {
                let (from, to) = (at(i - 1, points[i - 1].1), at(i, points[i].1));
                let (dx, dy) = (f32::from(to.x - from.x), f32::from(to.y - from.y));
                let length = (dx * dx + dy * dy).sqrt();
                let mut travelled = 0.0_f32;
                while travelled < length {
                    let run = left.min(length - travelled);
                    if drawing && run > 0.0 {
                        let start = point(
                            from.x + px(dx * travelled / length),
                            from.y + px(dy * travelled / length),
                        );
                        let end = point(
                            from.x + px(dx * (travelled + run) / length),
                            from.y + px(dy * (travelled + run) / length),
                        );
                        let mut dash = PathBuilder::stroke(px(1.2));
                        dash.move_to(start);
                        dash.line_to(end);
                        if let Ok(dash) = dash.build() {
                            window.paint_path(dash, up);
                        }
                    }
                    travelled += run;
                    left -= run;
                    if left <= f32::EPSILON {
                        drawing = !drawing;
                        left = if drawing { 3.0 } else { 2.0 };
                    }
                }
            }
            let _: Pixels = bounds.size.width;
        },
    )
    .h(px(56.0))
    .w_full()
    .into_any_element()
}

fn field(label: &'static str, value: String, tokens: &Tokens) -> impl IntoElement {
    v_flex()
        .min_w_0()
        .child(div().text_xs().text_color(tokens.text_muted).child(label))
        .child(
            div()
                .truncate()
                .text_sm()
                .child(::gpui::SharedString::from(value)),
        )
}

fn section(
    title: &'static str,
    extra: Option<String>,
    body: AnyElement,
    tokens: &Tokens,
) -> impl IntoElement {
    v_flex()
        .gap_2()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(tokens.border)
        .child(
            h_flex()
                .justify_between()
                .typo(Role::Caption, tokens)
                .child(title)
                .children(extra.map(::gpui::SharedString::from)),
        )
        .child(body)
}

/// 用量条（0.2.14 的 `Bar`）。
fn bar(percent: u8, tokens: &Tokens) -> impl IntoElement {
    div()
        .h(px(6.0))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(tokens.subtle)
        .child(
            div()
                .h_full()
                .w(::gpui::relative(f32::from(percent.min(100)) / 100.0))
                .rounded_full()
                .bg(ring_color(Some(percent), tokens)),
        )
}

fn usage_text(used: Option<f64>, total: Option<f64>, digits: usize) -> String {
    format!(
        "{}/{}",
        mib_gb_label(used, digits),
        mib_gb_label(total, digits)
    )
}

/// 每个核心：一小格竖条，高度跟着使用率（0.2.14 的「核心」一节）。
fn cores_section(cores: &[Option<u8>], tokens: &Tokens) -> AnyElement {
    let cells = cores.iter().enumerate().map(|(index, percent)| {
        let fraction = f32::from(percent.unwrap_or(0).min(100)) / 100.0;
        div()
            .id(("overview-core", index))
            .relative()
            .w(px(22.0))
            .h(px(32.0))
            .rounded_sm()
            .overflow_hidden()
            .bg(tokens.subtle)
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(::gpui::relative(fraction))
                    .bg(ring_color(*percent, tokens)),
            )
    });
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .max_h(px(160.0))
        .overflow_hidden()
        .children(cells)
        .into_any_element()
}

fn mounts_section(summary: &StatusSummary, tokens: &Tokens) -> AnyElement {
    v_flex()
        .gap_2p5()
        .children(summary.stats.mounts.iter().map(|mount| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .justify_between()
                        .gap_2()
                        .text_xs()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(::gpui::SharedString::from(mount.mount.clone())),
                        )
                        .child(div().flex_none().child(format!("{}%", mount.percent))),
                )
                .child(bar(mount.percent, tokens))
                .child(
                    h_flex()
                        .justify_between()
                        .gap_2()
                        .text_color(tokens.text_muted)
                        .child(div().min_w_0().truncate().text_size(px(10.0)).child(
                            ::gpui::SharedString::from(format!(
                                "{} · {}",
                                mount.fs_type, mount.source
                            )),
                        ))
                        .child(div().flex_none().text_size(px(10.0)).child(format!(
                            "{} / {}",
                            mib_gb_label(Some(mount.used_mib), 1),
                            mib_gb_label(Some(mount.total_mib), 1)
                        ))),
                )
        }))
        .into_any_element()
}

fn interfaces_section(summary: &StatusSummary, tokens: &Tokens) -> AnyElement {
    v_flex()
        .gap_1()
        .children(summary.stats.net_interfaces.iter().map(|iface| {
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .child(div().size(px(6.0)).rounded_full().bg(tokens.dot_connected))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(::gpui::SharedString::from(iface.name.clone())),
                        )
                        .child(
                            div()
                                .text_color(tokens.dot_connected)
                                .child(format!("↓ {}", net_speed_label(iface.rx_speed))),
                        )
                        .child(
                            div()
                                .text_color(tokens.accent)
                                .child(format!("↑ {}", net_speed_label(iface.tx_speed))),
                        ),
                )
                .child(
                    h_flex()
                        .justify_between()
                        .pl(px(14.0))
                        .text_size(px(10.0))
                        .text_color(tokens.text_muted)
                        .child("开机以来")
                        .child(format!(
                            "↓ {} · ↑ {}",
                            traffic_label(iface.rx_total),
                            traffic_label(iface.tx_total)
                        )),
                )
        }))
        .into_any_element()
}

fn processes_section(summary: &StatusSummary, tokens: &Tokens) -> AnyElement {
    let head = h_flex()
        .py_1()
        .gap_2()
        .text_size(px(11.0))
        .text_color(tokens.text_muted)
        .child(div().w(px(56.0)).child("PID"))
        .child(div().flex_1().child("命令"))
        .child(div().w(px(64.0)).flex_none().text_right().child("内存"));
    v_flex()
        .child(head)
        .children(summary.stats.top_processes.iter().map(|proc| {
            let fraction = (proc.mem_percent as f32 / 100.0).clamp(0.0, 1.0);
            h_flex()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .text_size(px(11.0))
                .child(
                    div()
                        .w(px(56.0))
                        .text_color(tokens.text_muted)
                        .child(::gpui::SharedString::from(proc.pid.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(::gpui::SharedString::from(proc.command.clone())),
                )
                .child(
                    div()
                        .relative()
                        .w(px(64.0))
                        .h(px(16.0))
                        .flex_none()
                        .rounded_sm()
                        .overflow_hidden()
                        .bg(tokens.subtle)
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(::gpui::relative(fraction))
                                .bg(tokens.accent_soft),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .pr_1()
                                .flex()
                                .items_center()
                                .justify_end()
                                .text_size(px(10.0))
                                .child(format!("{:.1}%", proc.mem_percent)),
                        ),
                )
        }))
        .into_any_element()
}

/// 「系统」一节的格子：系统 / 内核 / 运行时间 / 负载 / 交换空间 / CPU 型号。
fn system_section(s: &StatusSummary, tokens: &Tokens) -> AnyElement {
    let stats = &s.stats;
    let load = |value: Option<f64>| value.map_or_else(|| "--".to_owned(), |v| format!("{v:.2}"));
    let dash = |text: &str| {
        if text.is_empty() {
            "--".to_owned()
        } else {
            text.to_owned()
        }
    };
    let swap = match stats.swap_total_mib {
        Some(total) if total > 0 => {
            usage_text(stats.swap_used_mib.map(|v| v as f64), Some(total as f64), 1)
        }
        _ => "--".to_owned(),
    };
    let mut grid = div()
        .grid()
        .grid_cols(2)
        .gap_x_3()
        .gap_y_2()
        .child(field("系统", dash(&stats.os_name), tokens))
        .child(field("内核", dash(&stats.kernel), tokens))
        .child(field(
            "运行时间",
            stats
                .uptime_secs
                .map_or_else(|| "--".to_owned(), uptime_label),
            tokens,
        ))
        .child(field(
            "负载 1 / 5 / 15m",
            format!(
                "{} / {} / {}",
                load(stats.load_1m),
                load(stats.load_5m),
                load(stats.load_15m)
            ),
            tokens,
        ))
        .child(field("交换空间", swap, tokens));
    if let Some(model) = stats
        .cpu_model
        .as_deref()
        .map(|m| m.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|m| !m.is_empty())
    {
        grid = grid.child(field("CPU", model, tokens));
    }
    grid.into_any_element()
}

/// 四个圆环。
fn rings_section(s: &StatusSummary, tokens: &Tokens) -> AnyElement {
    let stats = &s.stats;
    div()
        .grid()
        .grid_cols(4)
        .gap_1()
        .px_3()
        .py_4()
        .child(ring(
            "CPU",
            stats.cpu_percent,
            stats
                .cpu_cores
                .map_or_else(String::new, |cores| format!("{cores} 核")),
            tokens,
        ))
        .child(ring(
            "内存",
            s.mem_percent(),
            usage_text(
                stats.mem_used_mib.map(|v| v as f64),
                stats.mem_total_mib.map(|v| v as f64),
                1,
            ),
            tokens,
        ))
        .child(ring(
            "磁盘",
            stats.disk_percent,
            usage_text(stats.disk_used_mib, stats.disk_total_mib, 0),
            tokens,
        ))
        .child(ring(
            "负载",
            s.load_percent(),
            stats
                .load_1m
                .map_or_else(String::new, |load| format!("{load:.2}")),
            tokens,
        ))
        .into_any_element()
}

/// 面板内容（有数据时）。每一节拆成单独的函数，debug 构建里栈帧才不会撑爆。
fn overview_body(
    s: &StatusSummary,
    history: &VecDeque<(f64, f64)>,
    scroll: &::gpui::ScrollHandle,
    tokens: &Tokens,
) -> AnyElement {
    let stats = &s.stats;
    let net_extra = s
        .net_rate
        .map(|(rx, tx)| format!("↓ {}  ↑ {}", net_speed_label(rx), net_speed_label(tx)));
    let mut body = v_flex()
        .id("overview-scroll")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(scroll)
        .child(rings_section(s, tokens))
        .child(section(
            "网络",
            net_extra,
            net_chart(history, tokens),
            tokens,
        ));
    if !stats.per_core_percent.is_empty() {
        body = body.child(section(
            "核心",
            Some(stats.per_core_percent.len().to_string()),
            cores_section(&stats.per_core_percent, tokens),
            tokens,
        ));
    }
    body = body.child(section("系统", None, system_section(s, tokens), tokens));
    if !stats.mounts.is_empty() {
        body = body.child(section(
            "存储",
            Some(stats.mounts.len().to_string()),
            mounts_section(s, tokens),
            tokens,
        ));
    }
    if !stats.net_interfaces.is_empty() {
        body = body.child(section("网卡", None, interfaces_section(s, tokens), tokens));
    }
    if !stats.top_processes.is_empty() {
        body = body.child(section(
            "内存占用进程",
            None,
            processes_section(s, tokens),
            tokens,
        ));
    }
    body.into_any_element()
}

impl MainWindow {
    pub(crate) fn toggle_overview(&mut self, cx: &mut Context<Self>) {
        self.docks.overview_open = !self.docks.overview_open;
        cx.notify();
    }

    /// 概览面板（没有活动会话或没开就是 `None`）。盖在终端区域右边，底部让出两条栏。
    pub(super) fn render_overview(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.docks.overview_open {
            return None;
        }
        let host_id = self.status_host?;
        let title = self
            .store
            .host(host_id)
            .map(|host| host.name.clone())
            .unwrap_or_else(|| "服务器概览".to_owned());
        let entity = cx.entity();
        let summary: Option<StatusSummary> = self
            .status
            .as_ref()
            .and_then(|status| status.summary.clone());
        let error = self.status.as_ref().and_then(|status| status.error.clone());

        let close = {
            let entity = entity.clone();
            div()
                .id("overview-close")
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .rounded_md()
                .cursor_pointer()
                .text_color(tokens.text_muted)
                .hover({
                    let hover = tokens.hover;
                    move |style| style.bg(hover)
                })
                .child(Icon::new(IconName::Close).size_3p5())
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| this.toggle_overview(cx));
                })
        };
        let header = h_flex()
            .flex_none()
            .h(px(40.0))
            .px_4()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(tokens.border)
            .child(
                Icon::new(Ico::CircleGauge)
                    .size_3p5()
                    .text_color(tokens.accent),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(::gpui::SharedString::from(title)),
            )
            .child(close);

        let body: AnyElement = match &summary {
            None => div()
                .p_4()
                .text_sm()
                .text_color(if error.is_some() {
                    tokens.danger
                } else {
                    tokens.text_muted
                })
                .child(match error {
                    Some(error) => format!("状态采集失败：{error}"),
                    None => "状态采集等待中…".to_owned(),
                })
                .into_any_element(),
            Some(s) => overview_body(s, &self.docks.net, &self.docks.overview_scroll, tokens),
        };

        Some(
            v_flex()
                .id("server-overview")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(OVERVIEW_WIDTH))
                .border_l_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .shadow_lg()
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{NET_HISTORY_MAX, push_net_point};
    use std::collections::VecDeque;

    #[test]
    fn the_net_history_keeps_the_newest_points() {
        let mut history = VecDeque::new();
        for n in 0..(NET_HISTORY_MAX + 5) {
            push_net_point(&mut history, (n as f64, 0.0));
        }
        assert_eq!(history.len(), NET_HISTORY_MAX);
        assert_eq!(history.front().map(|p| p.0), Some(5.0));
        assert_eq!(
            history.back().map(|p| p.0),
            Some((NET_HISTORY_MAX + 4) as f64)
        );
    }
}
