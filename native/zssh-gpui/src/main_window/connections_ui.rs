//! 「所有网络连接」对话框（V2-U12）：概览面板「网卡」一节的按钮打开，居中的大对话框，列出这台服务器上
//! `ss` / `netstat` / `/proc/net` 看到的 TCP / UDP 连接。只读，不结束进程、不断开连接。
//!
//! - 采集在 `zssh_core::ssh::connections`（一次 exec，5 秒超时）：点开和点「刷新」各执行一次，**不自动
//!   轮询**；可以打开「每 5 秒自动刷新」开关（默认关）。走当前标签已有的那条 SSH 连接，不新建连接、
//!   不往终端里打字；
//! - 筛选 / 搜索 / 排序都是 core 里的纯函数，这里只画：表格用虚拟列表，两万行也不卡；
//! - 拿不到进程信息时进程列是「—」，表格上方一行淡色提示；服务器不支持（不是 Linux）时对话框里写清原因。

use std::ops::Range;
use std::time::{Duration, Instant};

use ::gpui::{SharedString, UniformListScrollHandle, Window, px, uniform_list};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ssh::{
    ConnFilter, ConnectionsError, ConnectionsReport, NetConn, ProcessHint, SortKey, SortSpec,
    view_indices,
};

use super::MainWindow;
use super::bridge::on_ssh_runtime;
use super::modal::Modal;
use super::widgets::{ButtonLook, segmented, text_button};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 对话框卡片的宽度。
pub const DIALOG_WIDTH: f32 = 940.0;
/// 自动刷新的间隔。
const AUTO_REFRESH: Duration = Duration::from_secs(5);
const ROW_HEIGHT: f32 = 26.0;
/// 表格各列的宽度（最后一列「进程」吃剩下的）。
const COL_PROTO: f32 = 56.0;
const COL_LOCAL: f32 = 250.0;
/// 远端地址列留够一个完整的 IPv6 地址加端口。
const COL_REMOTE: f32 = 310.0;
const COL_STATE: f32 = 100.0;

/// 对话框的状态；搜索框是主窗口自己的 `conn_query`。
pub struct ConnectionsDialog {
    pub host_name: String,
    pub report: Option<ConnectionsReport>,
    /// 最近一次采集失败的原因（已有数据时只在表格上方显示一行，数据还留着）。
    pub error: Option<String>,
    pub loading: bool,
    /// 采集完成的时刻（本地时间 HH:MM:SS）。
    pub fetched: Option<String>,
    pub filter: ConnFilter,
    pub sort: SortSpec,
    /// 「每 5 秒自动刷新」，默认关。
    pub auto: bool,
    /// 筛选 / 搜索 / 排序之后要显示的行（下标指向 `report.rows`）。
    pub shown: Vec<usize>,
    /// 现在在等的那次采集的编号：关掉对话框再打开后，迟到的旧结果不能写进新对话框。
    pub generation: u64,
    last_done: Option<Instant>,
    pub scroll: UniformListScrollHandle,
}

impl ConnectionsDialog {
    pub fn new(host_name: String, generation: u64) -> Self {
        Self {
            host_name,
            report: None,
            error: None,
            loading: false,
            fetched: None,
            filter: ConnFilter::All,
            sort: SortSpec::default(),
            auto: false,
            shown: Vec::new(),
            generation,
            last_done: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    fn refilter(&mut self, query: &str) {
        self.shown = match &self.report {
            Some(report) => view_indices(&report.rows, self.filter, query, self.sort),
            None => Vec::new(),
        };
        self.scroll.scroll_to_item(0, ::gpui::ScrollStrategy::Top);
    }

    /// 该不该再自动采一次：开着自动刷新、没有在采、距离上次完成已满 5 秒。
    pub fn auto_due(&self, now: Instant) -> bool {
        self.auto
            && !self.loading
            && self
                .last_done
                .is_none_or(|done| now.duration_since(done) >= AUTO_REFRESH)
    }
}

/// 「HH:MM:SS」本地时间。
fn clock_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        + crate::sftp_demo::local_utc_offset();
    let in_day = secs.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    )
}

/// 进程列为空时表格上方的淡色提示。
pub fn hint_text(hint: ProcessHint) -> &'static str {
    match hint {
        ProcessHint::NotRoot => "以非 root 用户登录时看不到其他用户进程的信息，进程列显示为「—」。",
        ProcessHint::NoProcessSource => {
            "服务器上没有 ss / netstat，只能读 /proc/net，这种来源不含进程信息。"
        }
    }
}

impl MainWindow {
    /// 概览面板「所有连接」：开对话框并立刻采一次。
    pub(crate) fn open_connections_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host_id) = self.status_host else {
            return;
        };
        let host_name = self
            .store
            .host(host_id)
            .map(|host| host.name.clone())
            .unwrap_or_default();
        self.conn_generation += 1;
        let generation = self.conn_generation;
        let query = self.conn_query.clone();
        query.update(cx, |input, cx| input.set_value("", window, cx));
        window.focus(&query.read(cx).focus_handle(cx), cx);
        self.modal = Some(Modal::Connections(Box::new(ConnectionsDialog::new(
            host_name, generation,
        ))));
        self.refresh_connections(window, cx);
        cx.notify();
    }

    /// 采一次（点开、点「刷新」、自动刷新到点）。上一次还没回来就不再发。
    pub(crate) fn refresh_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.loading {
            return;
        }
        dialog.loading = true;
        let generation = dialog.generation;
        cx.notify();

        // 调试钩子：喂一份假输出（走同一个解析器），没有真实服务器也能截图。
        if let Some(result) = self.dev_fake.connections_result() {
            self.connections_loaded(generation, result, cx);
            return;
        }

        let Some((connection, _, _)) = self.workspace.read(cx).active_shell_position() else {
            self.connections_loaded(
                generation,
                Err(ConnectionsError::Exec(
                    "当前标签没有可用的 SSH 连接（还没连上或已断开）".to_owned(),
                )),
                cx,
            );
            return;
        };
        let rx =
            on_ssh_runtime(async move { zssh_core::ssh::collect_connections(&connection).await });
        match rx {
            Ok(rx) => self.await_result(rx, window, cx, move |this, result, _window, cx| {
                this.connections_loaded(generation, result, cx);
            }),
            Err(reason) => {
                self.connections_loaded(generation, Err(ConnectionsError::Exec(reason)), cx);
            }
        }
    }

    fn connections_loaded(
        &mut self,
        generation: u64,
        result: Result<ConnectionsReport, ConnectionsError>,
        cx: &mut Context<Self>,
    ) {
        let query = self.conn_query.read(cx).value().to_string();
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.loading = false;
        dialog.last_done = Some(Instant::now());
        match result {
            Ok(report) => {
                dialog.report = Some(report);
                dialog.error = None;
                dialog.fetched = Some(clock_now());
                dialog.refilter(&query);
            }
            Err(err) => dialog.error = Some(err.to_string()),
        }
        cx.notify();
    }

    /// 每个节拍：开着自动刷新就到点再采一次。
    pub(super) fn connections_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let due = matches!(&self.modal, Some(Modal::Connections(dialog)) if dialog.auto_due(Instant::now()));
        if due {
            self.refresh_connections(window, cx);
        }
    }

    /// 搜索框文字变了 / 换了筛选或排序：重算要显示的行。
    pub(super) fn connections_refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.conn_query.read(cx).value().to_string();
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.refilter(&query);
            cx.notify();
        }
    }

    fn connections_set_filter(&mut self, filter: ConnFilter, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.filter = filter;
        }
        self.connections_refilter(cx);
    }

    fn connections_sort_by(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.sort = dialog.sort.toggled(key);
        }
        self.connections_refilter(cx);
    }

    fn connections_set_auto(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.auto = on;
            // 刚打开开关：从现在起算 5 秒，不立刻再采一次。
            dialog.last_done = Some(Instant::now());
        }
        cx.notify();
    }

    // ---- 画 ----

    pub(super) fn connections_body(
        &self,
        dialog: &ConnectionsDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        // 对话框高度跟窗口走：卡片外有 24 的外边距、20 的内边距。
        let height = (self.window_height - 48.0 - 40.0).clamp(380.0, 640.0);

        let title = h_flex()
            .gap_2()
            .items_center()
            .child(
                Icon::new(IconName::Network)
                    .size_4()
                    .text_color(tokens.accent),
            )
            .child(div().typo(Role::GroupTitle, tokens).child("所有网络连接"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typo(Role::Caption, tokens)
                    .child(SharedString::from(format!("· {}", dialog.host_name))),
            )
            .child({
                let close = entity.clone();
                div()
                    .id("conn-close")
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
                    .on_click(move |_, window, cx| {
                        close.update(cx, |this, cx| this.close_modal(window, cx));
                    })
            });

        let filter_now = dialog.filter;
        let filters = div().w(px(330.0)).flex_none().child(segmented(
            "conn-filter",
            ConnFilter::ALL
                .iter()
                .map(|f| (f.label().to_owned(), *f == filter_now, None))
                .collect(),
            {
                let entity = entity.clone();
                move |index, _window, cx| {
                    if let Some(filter) = ConnFilter::ALL.get(index).copied() {
                        entity.update(cx, |this, cx| this.connections_set_filter(filter, cx));
                    }
                }
            },
        ));
        let toolbar = h_flex()
            .gap_3()
            .items_center()
            .child(filters)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.conn_query).prefix(Icon::new(IconName::Search))),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .flex_none()
                    .child(Switch::new("conn-auto").checked(dialog.auto).on_click({
                        let entity = entity.clone();
                        let on = !dialog.auto;
                        move |_, _window, cx| {
                            entity.update(cx, |this, cx| this.connections_set_auto(on, cx));
                        }
                    }))
                    .child(div().typo(Role::Caption, tokens).child("每 5 秒自动刷新")),
            )
            .child({
                let entity = entity.clone();
                text_button(
                    "conn-refresh",
                    if dialog.loading {
                        "刷新中…"
                    } else {
                        "刷新"
                    },
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.refresh_connections(window, cx));
                    },
                )
            });

        // 状态行：条数、来源、采集时间。
        let summary = {
            let total = dialog.report.as_ref().map_or(0, |r| r.rows.len());
            let mut text = format!("{} / {} 条", dialog.shown.len(), total);
            if let Some(report) = &dialog.report {
                text.push_str(&format!(" · 来源 {}", report.source.label()));
            }
            if let Some(at) = &dialog.fetched {
                text.push_str(&format!(" · 采集于 {at}"));
            }
            text
        };
        let hint = dialog
            .report
            .as_ref()
            .and_then(ConnectionsReport::process_hint)
            .map(hint_text);
        let info = v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(hint.unwrap_or(""))),
                    )
                    .child(
                        div()
                            .id("conn-summary")
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(summary)),
                    ),
            )
            .children(dialog.report.as_ref().filter(|r| r.truncated).map(|_| {
                div()
                    .text_xs()
                    .text_color(tokens.dot_connecting)
                    .child(format!(
                        "连接太多，只显示前 {} 条。",
                        zssh_core::ssh::CONNECTIONS_MAX_ROWS
                    ))
            }))
            .children(
                dialog
                    .error
                    .as_ref()
                    .filter(|_| dialog.report.is_some())
                    .map(|error| {
                        div()
                            .text_xs()
                            .text_color(tokens.danger)
                            .child(SharedString::from(format!("刷新失败：{error}")))
                    }),
            );

        // 表头：点列名排序。
        let header = {
            let sort = dialog.sort;
            let cell = |id: &'static str, label: &'static str, key: SortKey, width: Option<f32>| {
                let entity = entity.clone();
                let arrow = (sort.key == Some(key)).then_some(if sort.descending {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                });
                let hover = tokens.hover;
                h_flex()
                    .id(id)
                    .gap_1()
                    .items_center()
                    .h_full()
                    .px_2()
                    .cursor_pointer()
                    .when_some(width, |d, w| d.w(px(w)).flex_none())
                    .when(width.is_none(), |d| d.flex_1().min_w_0())
                    .hover(move |style| style.bg(hover))
                    .tooltip(|window, cx| {
                        Tooltip::new("点一下升序，再点降序，第三次恢复").build(window, cx)
                    })
                    .child(div().child(label))
                    .children(arrow.map(|icon| Icon::new(icon).size_3()))
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |this, cx| this.connections_sort_by(key, cx));
                    })
            };
            h_flex()
                .flex_none()
                .h(px(28.0))
                .items_center()
                .border_b_1()
                .border_color(tokens.border)
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text_muted)
                .child(cell(
                    "conn-h-proto",
                    "协议",
                    SortKey::Proto,
                    Some(COL_PROTO),
                ))
                .child(cell(
                    "conn-h-local",
                    "本地地址",
                    SortKey::Local,
                    Some(COL_LOCAL),
                ))
                .child(cell(
                    "conn-h-remote",
                    "远端地址",
                    SortKey::Remote,
                    Some(COL_REMOTE),
                ))
                .child(cell(
                    "conn-h-state",
                    "状态",
                    SortKey::State,
                    Some(COL_STATE),
                ))
                .child(cell("conn-h-proc", "进程", SortKey::Process, None))
        };

        // 表体：没数据 / 失败 / 没有符合条件的 / 虚拟列表。
        let placeholder = |text: String, color: ::gpui::Hsla| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_6()
                .text_sm()
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body: AnyElement = match (&dialog.report, &dialog.error) {
            (None, Some(error)) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .child(
                    div()
                        .text_sm()
                        .text_color(tokens.danger)
                        .child(SharedString::from(error.clone())),
                )
                .child({
                    let entity = entity.clone();
                    text_button(
                        "conn-retry",
                        "重试",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| this.refresh_connections(window, cx));
                        },
                    )
                })
                .into_any_element(),
            (None, None) => placeholder("正在读取网络连接…".to_owned(), tokens.text_muted),
            (Some(_), _) if dialog.shown.is_empty() => {
                placeholder("没有符合条件的连接".to_owned(), tokens.text_muted)
            }
            (Some(_), _) => {
                let row_tokens = *tokens;
                uniform_list(
                    "conn-rows",
                    dialog.shown.len(),
                    cx.processor(move |this, range: Range<usize>, _window, _cx| {
                        this.connection_rows(range, &row_tokens)
                    }),
                )
                .track_scroll(&dialog.scroll)
                .size_full()
                .into_any_element()
            }
        };
        let table = v_flex()
            .flex_1()
            .min_h_0()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .overflow_hidden()
            .child(header)
            .child(div().relative().flex_1().min_h_0().child(body));

        v_flex()
            .gap_3()
            .h(px(height))
            .child(title)
            .child(toolbar)
            .child(info)
            .child(table)
            .into_any_element()
    }

    /// 虚拟列表要画的这一段行。
    fn connection_rows(&self, range: Range<usize>, tokens: &Tokens) -> Vec<AnyElement> {
        let Some(Modal::Connections(dialog)) = &self.modal else {
            return Vec::new();
        };
        let Some(report) = &dialog.report else {
            return Vec::new();
        };
        let hover = tokens.hover;
        range
            .filter_map(|at| {
                let row: &NetConn = report.rows.get(*dialog.shown.get(at)?)?;
                let local = row.local.display();
                let remote = row.remote.display();
                let process = row.process_label();
                let state = row.state.label();
                let state_color = match row.state {
                    zssh_core::ssh::ConnState::Established => tokens.dot_connected,
                    zssh_core::ssh::ConnState::Listen => tokens.accent,
                    zssh_core::ssh::ConnState::TimeWait
                    | zssh_core::ssh::ConnState::CloseWait
                    | zssh_core::ssh::ConnState::LastAck
                    | zssh_core::ssh::ConnState::Closing
                    | zssh_core::ssh::ConnState::Close => tokens.text_muted,
                    _ => tokens.text,
                };
                let cell = |id: &'static str, text: String, width: Option<f32>| {
                    let tip = text.clone();
                    div()
                        .id((id, at))
                        .px_2()
                        .truncate()
                        .when_some(width, |d, w| d.w(px(w)).flex_none())
                        .when(width.is_none(), |d| d.flex_1().min_w_0())
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .child(SharedString::from(text))
                };
                Some(
                    h_flex()
                        .id(("conn-row", at))
                        .h(px(ROW_HEIGHT))
                        .items_center()
                        .text_xs()
                        .hover(move |style| style.bg(hover))
                        .child(cell(
                            "conn-c-proto",
                            row.proto_label().to_owned(),
                            Some(COL_PROTO),
                        ))
                        .child(cell("conn-c-local", local, Some(COL_LOCAL)))
                        .child(cell("conn-c-remote", remote, Some(COL_REMOTE)))
                        .child(cell("conn-c-state", state, Some(COL_STATE)).text_color(state_color))
                        .child(
                            cell("conn-c-proc", process, None)
                                .when(row.processes.is_empty(), |d| {
                                    d.text_color(tokens.text_muted)
                                }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{ConnectionsDialog, clock_now, hint_text};
    use std::time::{Duration, Instant};
    use zssh_core::ssh::{ConnFilter, ProcessHint, SortKey, SortSpec, parse_connections};

    fn loaded() -> ConnectionsDialog {
        let mut dialog = ConnectionsDialog::new("web-1".into(), 1);
        dialog.report = Some(
            parse_connections(
                "@@ZSSH uid 1000\n@@ZSSH source ss\n\
                 tcp LISTEN 0 1 0.0.0.0:80 0.0.0.0:* users:((\"nginx\",pid=1,fd=3))\n\
                 tcp ESTAB 0 0 192.0.2.1:22 198.51.100.2:5000\n\
                 udp UNCONN 0 0 *:5353 *:*\n",
            )
            .expect("解析"),
        );
        dialog.refilter("");
        dialog
    }

    #[test]
    fn a_fresh_dialog_is_empty_idle_and_not_auto_refreshing() {
        let dialog = ConnectionsDialog::new("web-1".into(), 7);
        assert!(dialog.report.is_none() && dialog.shown.is_empty());
        assert!(!dialog.loading && !dialog.auto);
        assert_eq!(dialog.generation, 7);
        assert!(!dialog.auto_due(Instant::now()), "默认不自动刷新");
    }

    #[test]
    fn refiltering_follows_the_filter_the_search_and_the_sort() {
        let mut dialog = loaded();
        assert_eq!(dialog.shown.len(), 3);
        dialog.filter = ConnFilter::Listen;
        dialog.refilter("");
        assert_eq!(dialog.shown.len(), 2, "TCP LISTEN + 没连对端的 UDP");
        dialog.refilter("nginx");
        assert_eq!(dialog.shown.len(), 1);
        dialog.filter = ConnFilter::All;
        dialog.sort = SortSpec::default().toggled(SortKey::Local);
        dialog.refilter("");
        let ports: Vec<String> = dialog
            .shown
            .iter()
            .map(|i| {
                dialog
                    .report
                    .as_ref()
                    .map(|r| r.rows[*i].local.port.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(ports, ["22", "80", "5353"]);
    }

    #[test]
    fn auto_refresh_waits_five_seconds_after_the_last_result_and_never_overlaps() {
        let mut dialog = loaded();
        dialog.auto = true;
        let now = Instant::now();
        // 从没完成过：马上可以采。
        assert!(dialog.auto_due(now));
        dialog.last_done = Some(now);
        assert!(!dialog.auto_due(now + Duration::from_secs(4)));
        assert!(dialog.auto_due(now + Duration::from_secs(5)));
        // 上一次还没回来：不再发。
        dialog.loading = true;
        assert!(!dialog.auto_due(now + Duration::from_secs(60)));
    }

    #[test]
    fn hints_say_what_is_missing() {
        assert!(hint_text(ProcessHint::NotRoot).contains("非 root"));
        assert!(hint_text(ProcessHint::NoProcessSource).contains("/proc/net"));
    }

    #[test]
    fn the_clock_is_hh_mm_ss() {
        let now = clock_now();
        assert_eq!(now.len(), 8);
        assert_eq!(&now[2..3], ":");
        assert_eq!(&now[5..6], ":");
    }
}
