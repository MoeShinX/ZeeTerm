//! V2-U44 A：关闭前确认在工作区一侧的行为——什么算「正在使用的连接」、范围怎么圈、确认后按编号关、
//! 确认框开着时标签变了怎么处理、右键「关闭窗格」把请求交给外壳。

use super::{Axis, Pane, PaneState, Size, Tab, TerminalWorkspace, next_generation};
use crate::close_confirm::{CloseRequest, CloseScope};
use crate::settings_store::AppSettings;
use gpui::{TestAppContext, WindowHandle};

const SIZE: Size = Size { rows: 8, cols: 40 };

fn open(cx: &mut TestAppContext) -> WindowHandle<TerminalWorkspace> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        AppSettings::init_for_test(cx);
    });
    cx.add_window(|window, cx| TerminalWorkspace::new(window, cx, "close-tests".to_owned()))
}

/// 一个主机会话的窗格，状态由调用方指定（没有真实连接：占位后端丢掉所有输入）。
fn host_pane(state: PaneState) -> Pane {
    Pane::placeholder(SIZE, 100, next_generation(), Some(7), state)
}

fn push(ws: &mut TerminalWorkspace, title: &str, pane: Pane) -> u64 {
    let tab = Tab::new(title.to_owned(), pane);
    let id = tab.id;
    ws.tabs.push(tab);
    id
}

fn tabs_request(ids: &[u64], anchor: Option<u64>) -> CloseRequest {
    CloseRequest::Tabs {
        ids: ids.to_vec(),
        anchor,
    }
}

#[gpui::test]
fn only_connected_or_connecting_sessions_count_as_in_use(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let live = push(ws, "live", host_pane(PaneState::Live));
            let connecting = push(ws, "connecting", host_pane(PaneState::Connecting));
            let dropped = push(ws, "dropped", host_pane(PaneState::Disconnected));
            let blank = push(ws, "blank", host_pane(PaneState::Empty));
            let offline = ws.push_test_tab("offline-demo");

            let asked =
                |ws: &TerminalWorkspace, id: u64| ws.close_confirmation(&tabs_request(&[id], None));
            let summary = asked(ws, live).expect("已连上要问");
            assert_eq!((summary.connections, summary.tab_total), (1, 1));
            assert_eq!(summary.busy_tabs, ["live"]);
            assert!(asked(ws, connecting).is_some(), "正在连接也算在用");
            assert!(asked(ws, dropped).is_none(), "已断开的直接关");
            assert!(asked(ws, blank).is_none(), "空白标签直接关");
            assert!(asked(ws, offline).is_none(), "离线演示窗格直接关");
        })
        .expect("window");
}

#[gpui::test]
fn the_setting_turns_the_question_off(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let live = push(ws, "live", host_pane(PaneState::Live));
            let request = tabs_request(&[live], None);
            assert!(ws.close_confirmation(&request).is_some(), "默认开");
            ws.toggle_confirm_close_tab(cx);
            assert!(!ws.settings().confirm_close_tab);
            assert!(ws.close_confirmation(&request).is_none(), "关掉后不再问");
            ws.toggle_confirm_close_tab(cx);
            assert!(ws.close_confirmation(&request).is_some(), "再打开又问");
            // 「不再询问」走的是同一个设置字段：只关不开。
            ws.disable_confirm_close_tab(cx);
            assert!(ws.close_confirmation(&request).is_none());
        })
        .expect("window");
}

#[gpui::test]
fn a_batch_counts_every_in_use_pane_and_names_the_tabs_that_have_one(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let a = push(ws, "web-1", host_pane(PaneState::Live));
            // web-2 分屏：两个窗格都连着，算两个连接、一个标签。
            let b = push(ws, "web-2", host_pane(PaneState::Live));
            ws.tabs
                .items_mut()
                .iter_mut()
                .find(|tab| tab.id == b)
                .expect("tab")
                .split_active(Axis::X, host_pane(PaneState::Live));
            let c = push(ws, "idle", host_pane(PaneState::Disconnected));
            let summary = ws
                .close_confirmation(&tabs_request(&[a, b, c], Some(a)))
                .expect("有在用的");
            assert_eq!(summary.connections, 3);
            assert_eq!(summary.tab_total, 3, "标题数的是范围里所有标签");
            assert_eq!(summary.busy_tabs, ["web-1", "web-2"], "断开的标签不点名");
        })
        .expect("window");
}

#[gpui::test]
fn scope_requests_follow_the_clicked_tab_and_empty_scopes_are_no_request(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let ids: Vec<u64> = ["a", "b", "c"]
                .iter()
                .map(|name| push(ws, name, host_pane(PaneState::Live)))
                .collect();
            assert_eq!(
                ws.close_request_for_scope(1, CloseScope::Others),
                Some(tabs_request(&[ids[0], ids[2]], Some(ids[1])))
            );
            assert_eq!(
                ws.close_request_for_scope(1, CloseScope::Right),
                Some(tabs_request(&[ids[2]], Some(ids[1])))
            );
            assert_eq!(ws.close_request_for_scope(2, CloseScope::Right), None);
            assert_eq!(ws.close_request_for_scope(0, CloseScope::Left), None);
            assert_eq!(ws.close_request_for_scope(9, CloseScope::All), None);
            assert_eq!(
                ws.close_request_for_window(),
                Some(tabs_request(&ids, None))
            );
        })
        .expect("window");
}

#[gpui::test]
fn confirming_closes_only_the_tabs_chosen_when_the_question_opened(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let a = push(ws, "a", host_pane(PaneState::Live));
            let b = push(ws, "b", host_pane(PaneState::Live));
            let c = push(ws, "c", host_pane(PaneState::Live));
            let request = ws
                .close_request_for_scope(0, CloseScope::Right)
                .expect("request");
            assert_eq!(request, tabs_request(&[b, c], Some(a)));

            // 确认框开着时：b 被别处关掉、又新开了一个标签 d、c 挪到了最前面——下标全变了。
            let id_b = ws.tab_ids().iter().position(|id| *id == b).expect("b");
            ws.close_tab(id_b, cx);
            let d = push(ws, "d", host_pane(PaneState::Live));
            let id_c = ws.tab_ids().iter().position(|id| *id == c).expect("c");
            ws.move_tab(id_c, 0, cx);

            assert_eq!(ws.perform_close(&request, cx), 1, "b 已经不在，只关 c");
            assert_eq!(ws.tab_ids(), [a, d], "没有顺手关掉 a 也没有带上新开的 d");
        })
        .expect("window");
}

#[gpui::test]
fn closing_by_id_keeps_the_selection_rules(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let ids: Vec<u64> = ["a", "b", "c", "d"]
                .iter()
                .map(|name| push(ws, name, host_pane(PaneState::Live)))
                .collect();
            // 活动标签是 d；「关闭其他」点 b：只剩 b，并且 b 成为活动标签。
            ws.select_tab(3, cx);
            let request = ws
                .close_request_for_scope(1, CloseScope::Others)
                .expect("request");
            assert_eq!(ws.perform_close(&request, cx), 3);
            assert_eq!(ws.tab_ids(), [ids[1]]);
            assert_eq!(ws.active_tab_id(), Some(ids[1]));
        })
        .expect("window");
}

#[gpui::test]
fn a_pane_request_describes_one_pane_and_closing_it_leaves_the_siblings(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let tab_id = push(ws, "web-1", host_pane(PaneState::Live));
            let tab = ws
                .tabs
                .items_mut()
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .expect("tab");
            let first = tab.panes[0].id;
            tab.split_active(Axis::X, host_pane(PaneState::Disconnected));
            let second = tab.panes[1].id;

            let first_request = CloseRequest::Pane {
                tab_id,
                pane_id: first,
            };
            let summary = ws
                .close_confirmation(&first_request)
                .expect("连着的窗格要问");
            assert!(summary.pane);
            assert_eq!(summary.connections, 1);
            let second_request = CloseRequest::Pane {
                tab_id,
                pane_id: second,
            };
            assert!(
                ws.close_confirmation(&second_request).is_none(),
                "已断开的窗格直接关"
            );

            assert_eq!(ws.perform_close(&second_request, cx), 1);
            // 只剩一个窗格：不顺手把整个标签关掉。
            assert_eq!(ws.perform_close(&first_request, cx), 0);
            assert_eq!(ws.tab_ids(), [tab_id]);
            // 窗格已经不在：什么也不做。
            assert_eq!(ws.perform_close(&second_request, cx), 0);
        })
        .expect("window");
}

#[gpui::test]
fn the_pane_menu_hands_the_question_to_the_shell_instead_of_closing(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let tab_id = push(ws, "web-1", host_pane(PaneState::Live));
            let tab = ws
                .tabs
                .items_mut()
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .expect("tab");
            let live = tab.panes[0].id;
            tab.split_active(Axis::X, host_pane(PaneState::Disconnected));
            let dropped = tab.panes[1].id;

            ws.request_close_pane(live, cx);
            assert_eq!(ws.tabs.active().expect("tab").panes.len(), 2, "还没关");
            assert_eq!(
                ws.take_close_request(),
                Some(CloseRequest::Pane {
                    tab_id,
                    pane_id: live
                })
            );
            assert_eq!(ws.take_close_request(), None, "只取一次");

            // 已断开的窗格不用问：直接关。
            ws.request_close_pane(dropped, cx);
            assert_eq!(ws.tabs.active().expect("tab").panes.len(), 1);
            assert_eq!(ws.take_close_request(), None);

            // 只剩一个窗格：没有「关闭窗格」可言，也不发请求。
            ws.request_close_pane(live, cx);
            assert_eq!(ws.take_close_request(), None);
        })
        .expect("window");
}

#[gpui::test]
fn the_focused_request_is_the_pane_when_split_and_the_tab_otherwise(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            assert_eq!(ws.focused_close_request(), None, "没有标签");
            let tab_id = push(ws, "web-1", host_pane(PaneState::Live));
            assert_eq!(
                ws.focused_close_request(),
                Some(tabs_request(&[tab_id], None))
            );
            let tab = ws
                .tabs
                .items_mut()
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .expect("tab");
            tab.split_active(Axis::Y, host_pane(PaneState::Live));
            let focused = tab.active().id;
            assert_eq!(
                ws.focused_close_request(),
                Some(CloseRequest::Pane {
                    tab_id,
                    pane_id: focused
                })
            );
        })
        .expect("window");
}
