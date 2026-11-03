//! 服务器概览面板（0.2.14 `ServerOverviewPanel.tsx`）：点底部状态栏打开，盖在终端右边（不挤终端）。
//!
//! 数据就是状态栏那条采集（和状态窗口共用同一个轮询器，core 的 `StatsCollector`），不额外开连接、
//! 不额外跑命令；面板关着不渲染。内容同 0.2.14：CPU / 内存 / 磁盘 / 负载四个圆环、网速曲线、
//! 每个核心、系统信息、全部分区、网卡、占 CPU / 内存最多的进程（可进「进程管理」）。

use std::collections::VecDeque;

use ::gpui::{MouseButton, PathBuilder, Pixels, canvas, point, px};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use zssh_core::ssh::ProcMode;

use super::MainWindow;
use super::panel_width::{self, DragStart, PanelKind};
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::status_demo::{
    Level, StatusSummary, level_of, mib_gb_label, net_speed_label, traffic_label, uptime_label,
};
use crate::typography::{Role, Typo as _};

/// 网速曲线最多记这么多个点（3 秒一次 ≈ 6 分钟）。
pub const NET_HISTORY_MAX: usize = 120;

/// 面板宽度决定的版式（V2-U12）：任何宽度下内容都放得下、不被切掉。
///
/// - 四个圆环一行：每格至少 [`RING_CELL_MIN`] 宽才放得下「0.3GB/3.8GB」这行小字，放不下就换成 2x2；
/// - 「系统」一节的两列：每列至少 [`SYSTEM_CELL_MIN`] 宽，窄了变一列。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverviewLayout {
    pub ring_columns: usize,
    /// 圆环下面小字的最大宽度（格宽）。
    pub ring_cell: f32,
    pub system_columns: usize,
}

/// 圆环区左右各 12 的内边距、格间距 4。
const RING_PAD: f32 = 24.0;
const RING_GAP: f32 = 4.0;
const RING_CELL_MIN: f32 = 76.0;
/// 其他各节左右各 16 的内边距、两列之间 12 的间距。
const SECTION_PAD: f32 = 32.0;
const SYSTEM_GAP: f32 = 12.0;
const SYSTEM_CELL_MIN: f32 = 150.0;

/// `width` 是面板整体宽度（含左边那 1 像素边框）。
pub fn layout_for(width: f32) -> OverviewLayout {
    let inner = width - 1.0;
    let cell4 = (inner - RING_PAD - RING_GAP * 3.0) / 4.0;
    let (ring_columns, ring_cell) = if cell4 >= RING_CELL_MIN {
        (4, cell4)
    } else {
        (2, (inner - RING_PAD - RING_GAP) / 2.0)
    };
    let cell2 = (inner - SECTION_PAD - SYSTEM_GAP) / 2.0;
    OverviewLayout {
        ring_columns,
        ring_cell: ring_cell.floor().max(40.0),
        system_columns: if cell2 >= SYSTEM_CELL_MIN { 2 } else { 1 },
    }
}

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
    cell: f32,
    tokens: &Tokens,
) -> impl IntoElement {
    let color = ring_color(percent, tokens);
    let track = tokens.subtle;
    let shown = percent.map_or(0.0, |p| f32::from(p.min(100)));
    let value = percent.map_or_else(|| "--".to_owned(), |p| format!("{p}%"));
    v_flex()
        .items_center()
        .min_w_0()
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
                .id(::gpui::SharedString::from(format!(
                    "overview-ring-sub-{label}"
                )))
                .max_w(px(cell))
                .truncate()
                .typo(Role::Caption, tokens)
                .tooltip({
                    let full = sub.clone();
                    move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                })
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

/// 「系统」一节的一格：长内容（CPU 型号、内核版本）省略号截断，悬停看全文。
fn field(label: &'static str, value: String, tokens: &Tokens) -> impl IntoElement {
    let full = value.clone();
    v_flex()
        .min_w_0()
        .child(div().text_xs().text_color(tokens.text_muted).child(label))
        .child(
            div()
                .id(::gpui::SharedString::from(format!(
                    "overview-field-{label}"
                )))
                .truncate()
                .text_sm()
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                .child(::gpui::SharedString::from(value)),
        )
}

fn section(
    title: &'static str,
    extra: Option<String>,
    action: Option<AnyElement>,
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
                .gap_2()
                .items_center()
                .justify_between()
                .typo(Role::Caption, tokens)
                .child(div().flex_none().child(title))
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .items_center()
                        .children(extra.map(|text| {
                            div()
                                .min_w_0()
                                .truncate()
                                .child(::gpui::SharedString::from(text))
                        }))
                        .children(action),
                ),
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
        .children(
            summary
                .stats
                .mounts
                .iter()
                .enumerate()
                .map(|(index, mount)| {
                    let mount_name = mount.mount.clone();
                    let detail = format!("{} · {}", mount.fs_type, mount.source);
                    let detail_tip = detail.clone();
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .text_xs()
                                .child(
                                    div()
                                        .id(("overview-mount", index))
                                        .min_w_0()
                                        .truncate()
                                        .font_weight(FontWeight::MEDIUM)
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(mount_name.clone()).build(window, cx)
                                        })
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
                                .child(
                                    div()
                                        .id(("overview-mount-src", index))
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(10.0))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(detail_tip.clone()).build(window, cx)
                                        })
                                        .child(::gpui::SharedString::from(detail)),
                                )
                                .child(div().flex_none().text_size(px(10.0)).child(format!(
                                    "{} / {}",
                                    mib_gb_label(Some(mount.used_mib), 1),
                                    mib_gb_label(Some(mount.total_mib), 1)
                                ))),
                        )
                }),
        )
        .into_any_element()
}

fn interfaces_section(summary: &StatusSummary, tokens: &Tokens) -> AnyElement {
    v_flex()
        .gap_1()
        .children(
            summary
                .stats
                .net_interfaces
                .iter()
                .enumerate()
                .map(|(index, iface)| {
                    let name = iface.name.clone();
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
                                        .id(("overview-iface", index))
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(name.clone()).build(window, cx)
                                        })
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
                }),
        )
        .into_any_element()
}

/// 「进程」一节：CPU / 内存两档，各取前 10（PID、命令、占比 + 细条形）。
fn processes_section(summary: &StatusSummary, mode: ProcMode, tokens: &Tokens) -> AnyElement {
    let stats = &summary.stats;
    let (label, items): (&str, Vec<(String, String, f64)>) = match mode {
        ProcMode::Mem => (
            "内存",
            stats
                .top_processes
                .iter()
                .map(|p| (p.pid.clone(), p.command.clone(), p.mem_percent))
                .collect(),
        ),
        ProcMode::Cpu => (
            "CPU",
            stats
                .top_cpu_processes
                .iter()
                .map(|p| (p.pid.clone(), p.command.clone(), p.cpu_percent))
                .collect(),
        ),
    };
    if items.is_empty() {
        return div()
            .typo(Role::Caption, tokens)
            .child(match mode {
                ProcMode::Cpu => "这台服务器的 ps 不提供 CPU 占用排行",
                ProcMode::Mem => "没有读到内存占用排行",
            })
            .into_any_element();
    }
    let head = h_flex()
        .py_1()
        .gap_2()
        .text_size(px(11.0))
        .text_color(tokens.text_muted)
        .child(div().w(px(56.0)).child("PID"))
        .child(div().flex_1().child("命令"))
        .child(div().w(px(64.0)).flex_none().text_right().child(label));
    v_flex()
        .child(head)
        .children(
            items
                .into_iter()
                .enumerate()
                .map(|(index, (pid, command, percent))| {
                    let tip = command.clone();
                    let fraction = (percent as f32 / 100.0).clamp(0.0, 1.0);
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
                                .child(::gpui::SharedString::from(pid)),
                        )
                        .child(
                            div()
                                .id(("overview-proc", index))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tip.clone()).build(window, cx)
                                })
                                .child(::gpui::SharedString::from(command)),
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
                                        .child(format!("{percent:.1}%")),
                                ),
                        )
                }),
        )
        .into_any_element()
}

/// 「进程」一节标题右侧的小分段：CPU / 内存。
fn proc_mode_switch(entity: &Entity<MainWindow>, mode: ProcMode, tokens: &Tokens) -> AnyElement {
    let (accent, on_accent, hover) = (tokens.accent, tokens.on_accent, tokens.hover);
    h_flex()
        .flex_none()
        .p_0p5()
        .gap_0p5()
        .rounded_md()
        .border_1()
        .border_color(tokens.border)
        .bg(tokens.subtle)
        .children(ProcMode::ALL.iter().enumerate().map(|(index, option)| {
            let option = *option;
            let entity = entity.clone();
            let selected = option == mode;
            div()
                .id(("overview-proc-mode", index))
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .cursor_pointer()
                .when(selected, move |d| d.bg(accent).text_color(on_accent))
                .when(!selected, move |d| d.hover(move |style| style.bg(hover)))
                .child(option.label())
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.docks.overview_procs = option;
                        cx.notify();
                    });
                })
        }))
        .into_any_element()
}

/// 「进程管理」小按钮（同「所有连接」）。
fn process_manager_button(entity: Entity<MainWindow>, tokens: &Tokens) -> AnyElement {
    let hover = tokens.hover;
    div()
        .id("overview-process-manager")
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .text_color(tokens.accent)
        .hover(move |style| style.bg(hover))
        .tooltip(|window, cx| Tooltip::new("查看全部进程，可以结束进程").build(window, cx))
        .child(Icon::new(Ico::ListChecks).size_3p5())
        .child("进程管理")
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.open_processes_dialog(window, cx));
        })
        .into_any_element()
}

/// 「系统」一节的格子：系统 / 内核 / 运行时间 / 负载 / 交换空间 / CPU 型号。
fn system_section(s: &StatusSummary, layout: OverviewLayout, tokens: &Tokens) -> AnyElement {
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
        .grid_cols(layout.system_columns as u16)
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
fn rings_section(s: &StatusSummary, layout: OverviewLayout, tokens: &Tokens) -> AnyElement {
    let stats = &s.stats;
    div()
        .grid()
        .grid_cols(layout.ring_columns as u16)
        .gap_1()
        .gap_y_3()
        .px_3()
        .py_4()
        .child(ring(
            "CPU",
            stats.cpu_percent,
            stats
                .cpu_cores
                .map_or_else(String::new, |cores| format!("{cores} 核")),
            layout.ring_cell,
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
            layout.ring_cell,
            tokens,
        ))
        .child(ring(
            "磁盘",
            stats.disk_percent,
            usage_text(stats.disk_used_mib, stats.disk_total_mib, 0),
            layout.ring_cell,
            tokens,
        ))
        .child(ring(
            "负载",
            s.load_percent(),
            stats
                .load_1m
                .map_or_else(String::new, |load| format!("{load:.2}")),
            layout.ring_cell,
            tokens,
        ))
        .into_any_element()
}

/// 「所有连接」小按钮（网卡一节标题右侧）。
fn connections_button(entity: Entity<MainWindow>, tokens: &Tokens) -> AnyElement {
    let hover = tokens.hover;
    div()
        .id("overview-all-connections")
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .text_color(tokens.accent)
        .hover(move |style| style.bg(hover))
        .tooltip(|window, cx| Tooltip::new("查看所有网络连接").build(window, cx))
        .child(Icon::new(IconName::Network).size_3p5())
        .child("所有连接")
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.open_connections_dialog(window, cx));
        })
        .into_any_element()
}

/// 面板内容（有数据时）。每一节拆成单独的函数，debug 构建里栈帧才不会撑爆。
fn overview_body(
    s: &StatusSummary,
    history: &VecDeque<(f64, f64)>,
    scroll: &::gpui::ScrollHandle,
    layout: OverviewLayout,
    proc_mode: ProcMode,
    entity: &Entity<MainWindow>,
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
        .child(rings_section(s, layout, tokens))
        .child(section(
            "网络",
            net_extra,
            None,
            net_chart(history, tokens),
            tokens,
        ));
    if !stats.per_core_percent.is_empty() {
        body = body.child(section(
            "核心",
            Some(stats.per_core_percent.len().to_string()),
            None,
            cores_section(&stats.per_core_percent, tokens),
            tokens,
        ));
    }
    body = body.child(section(
        "系统",
        None,
        None,
        system_section(s, layout, tokens),
        tokens,
    ));
    if !stats.mounts.is_empty() {
        body = body.child(section(
            "存储",
            Some(stats.mounts.len().to_string()),
            None,
            mounts_section(s, tokens),
            tokens,
        ));
    }
    // 网卡一节永远画：「所有连接」按钮在它的标题上，没有网卡读数时也要能点。
    let nics = if stats.net_interfaces.is_empty() {
        div()
            .typo(Role::Caption, tokens)
            .child("没有读到网卡数据")
            .into_any_element()
    } else {
        interfaces_section(s, tokens)
    };
    body = body.child(section(
        "网卡",
        None,
        Some(connections_button(entity.clone(), tokens)),
        nics,
        tokens,
    ));
    if !stats.top_processes.is_empty() || !stats.top_cpu_processes.is_empty() {
        body = body.child(section(
            "进程",
            None,
            Some(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(proc_mode_switch(entity, proc_mode, tokens))
                    .child(process_manager_button(entity.clone(), tokens))
                    .into_any_element(),
            ),
            processes_section(s, proc_mode, tokens),
            tokens,
        ));
    }
    body.into_any_element()
}

impl MainWindow {
    /// 当前终端标签的概览面板开着没有（按标签记，见 `DockState::overview_open_for`）。
    pub(crate) fn overview_open(&self, cx: &App) -> bool {
        self.docks
            .overview_open_for(self.workspace.read(cx).active_tab_id())
    }

    pub(crate) fn toggle_overview(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.read(cx).active_tab_id() else {
            return;
        };
        let open = self.docks.overview_open_for(Some(tab));
        self.docks.set_overview_open_for(tab, !open);
        cx.notify();
    }

    /// 现在画出来的宽度：设置表里存的（拖动中用临时值）夹进窗口允许的范围。
    pub(super) fn overview_width_now(&self, cx: &App) -> f32 {
        let desired = self
            .docks
            .overview_live
            .unwrap_or(self.store.bars().overview_width);
        panel_width::clamp(
            PanelKind::Overview,
            desired,
            self.window_width,
            self.overview_reserved(cx),
        )
    }

    /// 左边缘拖动中：宽度 = 起始宽度 - 指针移动量。返回 `true` 表示这个鼠标移动事件被它用掉了。
    pub(super) fn overview_mouse_move(
        &mut self,
        event: &::gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(start) = self.docks.overview_drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.commit_overview_width(cx);
            return true;
        }
        let width = panel_width::dragged_width(
            PanelKind::Overview,
            start,
            f32::from(event.position.x),
            self.window_width,
            self.overview_reserved(cx),
        );
        self.docks.overview_live = Some(width);
        cx.notify();
        true
    }

    pub(super) fn overview_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.docks.overview_drag.is_some() {
            self.commit_overview_width(cx);
        }
    }

    /// 松手：把拖出来的宽度写进设置表（本机设置，不同步）。
    fn commit_overview_width(&mut self, cx: &mut Context<Self>) {
        self.docks.overview_drag = None;
        let key = PanelKind::Overview.setting_key();
        if let Some(width) = self.docks.overview_live.take()
            && let Err(err) = self
                .store
                .write_bar_setting(|db| super::bar_settings::write_number(db, key, width))
        {
            tracing::warn!(%err, "保存概览面板宽度失败");
        }
        cx.notify();
    }

    /// 双击把手：恢复默认宽度。
    fn reset_overview_width(&mut self, cx: &mut Context<Self>) {
        let key = PanelKind::Overview.setting_key();
        let width = PanelKind::Overview.default_width();
        if let Err(err) = self
            .store
            .write_bar_setting(|db| super::bar_settings::write_number(db, key, width))
        {
            tracing::warn!(%err, "保存概览面板宽度失败");
        }
        cx.notify();
    }

    /// 概览面板（没有活动会话或没开就是 `None`）。盖在终端区域右边，底部让出两条栏。
    pub(super) fn render_overview(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.overview_open(cx) {
            return None;
        }
        let host_id = self.status_host?;
        let width = self.overview_width_now(cx);
        let layout = layout_for(width);
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
                .child(Icon::new(IconName::Close).size_4())
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
            Some(s) => overview_body(
                s,
                &self.docks.net,
                &self.docks.overview_scroll,
                layout,
                self.docks.overview_procs,
                &entity,
                tokens,
            ),
        };

        // 左边缘的拖动把手（同 AI / 文件面板）：光标变左右箭头，双击恢复默认宽度。
        let handle = {
            let down = entity.clone();
            div()
                .id("overview-resize")
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-3.0))
                .w(px(6.0))
                .cursor_col_resize()
                .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                    cx.stop_propagation();
                    let double = event.click_count >= 2;
                    let start = DragStart {
                        pointer_x: f32::from(event.position.x),
                        width,
                    };
                    down.update(cx, |this, cx| {
                        if double {
                            this.reset_overview_width(cx);
                        } else {
                            this.docks.overview_drag = Some(start);
                            cx.notify();
                        }
                    });
                })
        };

        Some(
            v_flex()
                .id("server-overview")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(width))
                .border_l_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .shadow_lg()
                .child(header)
                .child(body)
                .child(handle)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{NET_HISTORY_MAX, layout_for, push_net_point};
    use crate::main_window::panel_width::{OVERVIEW_DEFAULT, PANEL_MIN};
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

    #[test]
    fn the_default_width_fits_all_four_rings_on_one_row_and_two_system_columns() {
        let layout = layout_for(OVERVIEW_DEFAULT);
        assert_eq!(layout.ring_columns, 4);
        assert_eq!(layout.system_columns, 2);
        // 圆环下面那行小字（「0.3GB/3.8GB」）至少有 76 像素。
        assert!(layout.ring_cell >= 76.0, "{layout:?}");
    }

    #[test]
    fn narrow_widths_wrap_the_rings_and_stack_the_system_fields() {
        let narrow = layout_for(PANEL_MIN);
        assert_eq!(narrow.ring_columns, 2, "320 宽放不下四格");
        assert_eq!(narrow.system_columns, 1);
        assert!(narrow.ring_cell >= 76.0, "两列时每格更宽：{narrow:?}");
        // 临界值：一行放四个圆环要 341，两列系统信息要 345。
        assert_eq!(layout_for(340.0).ring_columns, 2);
        assert_eq!(layout_for(341.0).ring_columns, 4);
        assert_eq!(layout_for(344.0).system_columns, 1);
        assert_eq!(layout_for(345.0).system_columns, 2);
    }

    #[test]
    fn the_layout_never_gets_worse_as_the_panel_gets_wider() {
        let mut last = layout_for(PANEL_MIN);
        for width in 321..1200 {
            let now = layout_for(width as f32);
            assert!(now.ring_columns >= last.ring_columns, "{width}");
            assert!(now.system_columns >= last.system_columns, "{width}");
            // 同一种列数下，格子随宽度变大。
            if now.ring_columns == last.ring_columns {
                assert!(now.ring_cell >= last.ring_cell, "{width}");
            }
            last = now;
        }
        // 再宽也不会超过四个圆环 / 两列。
        assert_eq!(layout_for(1000.0).ring_columns, 4);
        assert_eq!(layout_for(1000.0).system_columns, 2);
    }
}
