//! 主区的终端页，以及空白标签里的「连接到主机」。欢迎页在 `welcome`，主机页在 `vault_page`，
//! 设置与管理页在 `settings_page`。

use ::gpui::{MouseButton, SharedString, px};
use gpui_kit::component::input::Input;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::Host;

use super::MainWindow;
use super::host_tree::{compare_names, host_matches};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::typography::{Role, Typo as _};

/// 本文件（终端页 / 空白标签的启动页）的界面文案。主机名、地址是用户数据，不进这张表。
#[derive(Clone, Copy, Debug)]
enum PagesMsg {
    NoTabs,
    QuickConnect,
    LauncherHint,
    NoHosts,
    NoMatch,
}

impl Msg for PagesMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::NoTabs => "没有打开的标签。按 Ctrl+Shift+T 新建，或者在左侧双击一台主机。",
            Self::QuickConnect => "快速连接",
            Self::LauncherHint => "搜索已保存的会话，回车即可连接",
            Self::NoHosts => "主机库里还没有主机，点侧栏上方的 + 新建一台。",
            Self::NoMatch => "没有匹配的主机",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::NoTabs => {
                "No open tabs. Press Ctrl+Shift+T to create one, or double-click a host on the left."
            }
            Self::QuickConnect => "Quick Connect",
            Self::LauncherHint => "Search your saved sessions, then press Enter to connect",
            Self::NoHosts => {
                "The host library is empty; click + at the top of the sidebar to add one."
            }
            Self::NoMatch => "No matching hosts",
        }
    }
}

/// 空白标签里「连接到主机」要列出的主机：按名字排序，搜索框里的字匹配名称 / 地址 /
/// 用户名 / 标签（同 0.2.14 的 `filterHosts`）。
pub fn launcher_hosts(hosts: &[Host], query: &str) -> Vec<i64> {
    let mut shown: Vec<&Host> = hosts
        .iter()
        .filter(|host| host_matches(host, query))
        .collect();
    shown.sort_by(|a, b| compare_names(&a.name, &b.name));
    shown.into_iter().map(|host| host.id).collect()
}

impl MainWindow {
    // ---- 终端页 ----

    pub(super) fn render_terminal(
        &mut self,
        tokens: &Tokens,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // SFTP 标签：主视图里显示文件视图（双栏），不画终端和底部两条栏。
        if let Some(view) = self.active_sftp_view() {
            return div().size_full().child(view).into_any_element();
        }
        // 编辑器标签：主视图里显示编辑器（不画终端和底部两条栏）。
        if let Some(view) = self.active_editor_view() {
            return div().size_full().child(view).into_any_element();
        }
        let count = self.workspace.read(cx).tab_count();
        if count == 0 {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(t(PagesMsg::NoTabs))
                .into_any_element();
        }
        let blank = self.workspace.read(cx).active_tab_is_blank();
        let bars = self.render_bars(tokens, window, cx);
        // 服务器概览盖在终端右边（不挤终端），底部让出两条栏（它们在这个区域下面）。
        let overview = self.render_overview(tokens, cx);
        v_flex()
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.workspace.clone())
                    .when(blank, |d| d.child(self.render_launcher(tokens, cx)))
                    .children(overview),
            )
            .children(bars)
            .into_any_element()
    }

    /// 空白标签：搜索 + 主机列表，点一行（或回车）就在这个标签里连上。
    fn render_launcher(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let query = self.launcher_query.read(cx).value().to_string();
        let shown = launcher_hosts(self.store.hosts(), &query);
        let cursor = self.launcher_cursor.min(shown.len().saturating_sub(1));
        let hover = tokens.hover;
        let rows = shown.iter().enumerate().filter_map(|(at, id)| {
            let host = self.store.host(*id)?;
            let host_id = *id;
            let open = entity.clone();
            let name: SharedString = host.name.clone().into();
            let detail: SharedString = super::protocol_form::host_detail(host).into();
            Some(
                h_flex()
                    .id(("launcher-row", at))
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(at == cursor, |d| d.bg(tokens.accent_soft))
                    .when(at != cursor, |d| d.hover(move |style| style.bg(hover)))
                    .child(
                        match self.store.os_icon_of(host).and_then(|os| {
                            crate::os_icons::badge(os, crate::os_icons::OsIconSize::Row)
                        }) {
                            Some(badge) => badge,
                            None => super::widgets::protocol_icon(host.protocol.protocol)
                                .size_4()
                                .text_color(tokens.accent)
                                .into_any_element(),
                        },
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(div().truncate().typo(Role::ItemName, tokens).child(name))
                            .child(div().truncate().typo(Role::Caption, tokens).child(detail)),
                    )
                    .on_click(move |_, window, cx| {
                        open.update(cx, |this, cx| this.open_host(host_id, window, cx));
                    }),
            )
        });
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(80.0))
            .bg(tokens.background)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(448.0))
                    .px_4()
                    .gap_2()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(t(PagesMsg::QuickConnect)),
                    )
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .child(t(PagesMsg::LauncherHint)),
                    )
                    .child(Input::new(&self.launcher_query))
                    .child(if self.store.hosts().is_empty() {
                        div()
                            .py_6()
                            .text_center()
                            .typo(Role::Caption, tokens)
                            .text_size(px(14.0))
                            .child(t(PagesMsg::NoHosts))
                            .into_any_element()
                    } else if shown.is_empty() {
                        div()
                            .py_6()
                            .text_center()
                            .typo(Role::Caption, tokens)
                            .text_size(px(14.0))
                            .child(t(PagesMsg::NoMatch))
                            .into_any_element()
                    } else {
                        v_flex()
                            .id("launcher-list")
                            .max_h(px(360.0))
                            .overflow_y_scroll()
                            .gap_0p5()
                            .children(rows)
                            .into_any_element()
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::launcher_hosts;
    use zssh_core::model::{AuthMethod, Host, ProxyMode};

    fn host(id: i64, group: Option<i64>, name: &str, address: &str) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: group,
            name: name.to_owned(),
            host: address.to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: vec!["blue".to_owned()],
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn launcher_lists_by_name_and_filters_on_every_field() {
        let hosts = [
            host(1, None, "web-10", "192.0.2.10"),
            host(2, None, "web-2", "192.0.2.20"),
            host(3, None, "db", "198.51.100.7"),
        ];
        assert_eq!(launcher_hosts(&hosts, ""), vec![3, 2, 1], "按名字自然序");
        assert_eq!(launcher_hosts(&hosts, "198.51"), vec![3], "地址");
        assert_eq!(launcher_hosts(&hosts, "WEB"), vec![2, 1], "不分大小写");
        assert_eq!(launcher_hosts(&hosts, "blue").len(), 3, "标签");
        assert!(launcher_hosts(&hosts, "nothing").is_empty());
    }
}
