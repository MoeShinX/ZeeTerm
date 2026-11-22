//! V2-U44 A：主窗口关闭标签 / 窗格前的确认（`close_flow`）的窗口测试：每个入口在「有正在使用的连接 / 没有」
//! 两种情况下问不问、批量只问一次、取消什么都不关、「不再询问」、确认框开着时标签变了。
//!
//! 要一个独立的数据目录（`MainWindow::new` 会打开主机库）：没设 `ZEETERM_DATA_DIR` 就跳过并打印原因。

use ::gpui::{AppContext as _, TestAppContext, VisualTestContext, WindowHandle};

use super::modal::Modal;
use super::nav::MainView;
use super::{CloseScope, MainWindow};
use crate::settings_store::AppSettings;
use crate::split_tree::Axis;

struct Harness {
    window: WindowHandle<MainWindow>,
    ids: Vec<u64>,
}

impl Harness {
    /// 开一个主窗口，每个标签是「已连上的主机会话」（`live = true`）或离线演示标签；选中第一个。
    fn open(cx: &mut TestAppContext, tabs: &[(&str, bool)]) -> Option<Self> {
        if std::env::var_os("ZEETERM_DATA_DIR").is_none() {
            eprintln!("跳过关闭确认的窗口测试：需要 ZEETERM_DATA_DIR 指向独立数据目录");
            return None;
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window = cx.add_window(MainWindow::new);
        let ids = window
            .update(cx, |main, window, cx| {
                main.show_view(MainView::Terminal, window, cx);
                main.workspace.update(cx, |ws, cx| {
                    let ids = tabs
                        .iter()
                        .map(|(title, live)| {
                            if *live {
                                ws.push_test_host_tab(title, 7)
                            } else {
                                ws.push_test_tab(title)
                            }
                        })
                        .collect::<Vec<_>>();
                    ws.select_tab(0, cx);
                    ids
                })
            })
            .expect("main window");
        let harness = Self { window, ids };
        harness.draw(cx);
        harness.draw(cx);
        Some(harness)
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    fn order(&self, cx: &mut TestAppContext) -> Vec<u64> {
        self.window
            .read_with(cx, |main, cx| main.workspace.read(cx).tab_ids())
            .expect("order")
    }

    /// 现在开着的是不是关闭确认框；是就返回它的（标题，正文，「不再询问」勾了没有）。
    fn ask(&self, cx: &mut TestAppContext) -> Option<(String, String, bool)> {
        self.window
            .read_with(cx, |main, _| match &main.modal {
                Some(Modal::CloseTabs(ask)) => {
                    Some((ask.text.title.clone(), ask.text.body.clone(), ask.dont_ask))
                }
                _ => None,
            })
            .expect("modal")
    }

    fn any_modal(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .read_with(cx, |main, _| main.modal.is_some())
            .expect("modal")
    }

    fn close_tab(&self, cx: &mut TestAppContext, index: usize) {
        self.window
            .update(cx, |main, window, cx| main.close_tab(index, window, cx))
            .expect("close");
        self.draw(cx);
    }

    fn close_tabs(&self, cx: &mut TestAppContext, index: usize, scope: CloseScope) {
        self.window
            .update(cx, |main, window, cx| {
                main.close_tabs(index, scope, window, cx)
            })
            .expect("close");
        self.draw(cx);
    }

    fn confirm_enabled(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .read_with(cx, |main, cx| {
                main.workspace.read(cx).settings().confirm_close_tab
            })
            .expect("settings")
    }
}

#[gpui::test]
fn closing_a_live_tab_asks_first_and_cancelling_closes_nothing(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true), ("web-2", true), ("web-3", true)]) else {
        return;
    };
    let before = h.order(cx);
    h.close_tab(cx, 1);
    let (title, body, dont_ask) = h.ask(cx).expect("有正在使用的连接要先问");
    assert!(title.contains("web-2"), "{title}");
    assert!(body.contains('1'), "{body}");
    assert!(!dont_ask, "每次弹出都是不勾");
    assert_eq!(h.order(cx), before, "问的时候还没关");

    // 确认框开着：键盘不在终端上（敲的字不会进还连着的会话）。
    let terminal_focused = h
        .window
        .update(cx, |main, window, cx| {
            main.workspace.read(cx).focus_handle().is_focused(window)
        })
        .expect("focus");
    assert!(!terminal_focused);

    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("escape");
    h.draw(cx);
    assert!(!h.any_modal(cx), "Esc 取消");
    assert_eq!(h.order(cx), before, "取消后什么都没关");
}

#[gpui::test]
fn enter_confirms_and_closes_the_tab_that_was_asked_about(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true), ("web-2", true), ("web-3", true)]) else {
        return;
    };
    let [a, b, c] = [h.ids[0], h.ids[1], h.ids[2]];
    h.close_tab(cx, 1);
    assert!(h.ask(cx).is_some());
    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("enter");
    h.draw(cx);
    assert!(!h.any_modal(cx));
    assert_eq!(h.order(cx), [a, c], "关的是 web-2（{b}）");
}

#[gpui::test]
fn tabs_without_a_connection_in_use_close_without_asking(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("demo", false), ("web-1", true)]) else {
        return;
    };
    let live = h.ids[1];
    h.close_tab(cx, 0);
    assert!(!h.any_modal(cx), "离线演示标签直接关");
    assert_eq!(h.order(cx), [live]);

    // 会话自己断开了：关它也不问。
    h.window
        .update(cx, |main, _, cx| {
            main.workspace.update(cx, |ws, _| {
                ws.dev_mark_disconnected_for_test(0);
            });
        })
        .expect("disconnect");
    h.close_tab(cx, 0);
    assert!(!h.any_modal(cx), "已断开的标签直接关");
    assert!(h.order(cx).is_empty());
}

#[gpui::test]
fn with_the_setting_off_a_live_tab_closes_without_asking(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true), ("web-2", true)]) else {
        return;
    };
    h.window
        .update(cx, |main, _, cx| {
            main.workspace
                .update(cx, |ws, cx| ws.toggle_confirm_close_tab(cx));
        })
        .expect("setting");
    assert!(!h.confirm_enabled(cx));
    h.close_tab(cx, 0);
    assert!(!h.any_modal(cx));
    assert_eq!(h.order(cx), [h.ids[1]]);
}

#[gpui::test]
fn a_batch_close_asks_once_with_the_count_and_the_names(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(
        cx,
        &[
            ("web-1", true),
            ("web-2", true),
            ("web-3", true),
            ("web-4", true),
            ("web-5", true),
        ],
    ) else {
        return;
    };
    h.close_tabs(cx, 0, CloseScope::Right);
    let (title, body, _) = h.ask(cx).expect("批量只问一次");
    assert!(title.contains('4'), "{title}");
    assert!(body.contains('4'), "要断开 4 个连接：{body}");
    for name in ["web-2", "web-3", "web-4"] {
        assert!(body.contains(name), "最多列三个名字：{body}");
    }
    assert!(!body.contains("web-5"), "第四个起不列名：{body}");
    assert!(
        !body.contains("web-1"),
        "被点的标签不关，不在名单里：{body}"
    );

    // 已经有确认框开着：再来一次关闭不叠第二个、也不关任何东西。
    h.close_tabs(cx, 4, CloseScope::Left);
    assert_eq!(h.order(cx).len(), 5);
    let (second_title, _, _) = h.ask(cx).expect("还是第一个框");
    assert_eq!(second_title, title);

    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("enter");
    h.draw(cx);
    assert_eq!(h.order(cx), [h.ids[0]], "只剩被点的那个");
    assert_eq!(
        h.window
            .read_with(cx, |main, cx| main.workspace.read(cx).active_tab_id())
            .expect("active"),
        Some(h.ids[0])
    );
}

#[gpui::test]
fn ticking_do_not_ask_again_turns_the_setting_off_only_when_closing(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true), ("web-2", true), ("web-3", true)]) else {
        return;
    };
    // 勾上又点取消：设置不变。
    h.close_tab(cx, 0);
    h.window
        .update(cx, |main, _, cx| main.set_close_dont_ask(true, cx))
        .expect("tick");
    assert_eq!(h.ask(cx).map(|ask| ask.2), Some(true));
    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("escape");
    h.draw(cx);
    assert!(h.confirm_enabled(cx), "取消不改设置");

    // 勾上并关闭：设置被关掉，之后直接关。
    h.close_tab(cx, 0);
    assert_eq!(h.ask(cx).map(|ask| ask.2), Some(false), "下次弹出重新不勾");
    h.window
        .update(cx, |main, _, cx| main.set_close_dont_ask(true, cx))
        .expect("tick");
    vcx.simulate_keystrokes("enter");
    h.draw(cx);
    assert!(!h.confirm_enabled(cx), "勾了不再询问");
    assert_eq!(h.order(cx).len(), 2);
    h.close_tab(cx, 0);
    assert!(!h.any_modal(cx), "以后直接关");
    assert_eq!(h.order(cx).len(), 1);
}

#[gpui::test]
fn tabs_changing_while_the_question_is_open_never_close_the_wrong_one(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true), ("web-2", true), ("web-3", true)]) else {
        return;
    };
    let [a, b, c] = [h.ids[0], h.ids[1], h.ids[2]];
    h.close_tabs(cx, 0, CloseScope::Right);
    assert!(h.ask(cx).is_some());
    // 问着的时候 web-2 被程序内部关掉（下标全变了），又新开了一个标签。
    let d = h
        .window
        .update(cx, |main, _, cx| {
            main.workspace.update(cx, |ws, cx| {
                ws.close_tab(1, cx);
                ws.push_test_host_tab("web-4", 7)
            })
        })
        .expect("change");
    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("enter");
    h.draw(cx);
    assert_eq!(
        h.order(cx),
        [a, d],
        "只关当初选中的 web-3（{c}）；web-2（{b}）已经没了；新开的 web-4 不在范围里"
    );
}

#[gpui::test]
fn ctrl_shift_w_asks_for_the_pane_when_split_and_for_the_tab_otherwise(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true)]) else {
        return;
    };
    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("ctrl-shift-w");
    h.draw(cx);
    let (title, _, _) = h.ask(cx).expect("关标签要问");
    assert!(title.contains("web-1"), "{title}");
    vcx.simulate_keystrokes("escape");
    h.draw(cx);
    assert_eq!(h.order(cx).len(), 1);

    // 分屏：关的是聚焦的窗格，窗格标题。
    h.window
        .update(cx, |main, _, cx| {
            main.workspace
                .update(cx, |ws, cx| ws.split_active(Axis::X, cx))
        })
        .expect("split");
    h.draw(cx);
    vcx.simulate_keystrokes("ctrl-shift-w");
    h.draw(cx);
    let (pane_title, body, _) = h.ask(cx).expect("关窗格也要问");
    assert_ne!(pane_title, title, "窗格有自己的标题");
    assert!(body.contains('1'));
    vcx.simulate_keystrokes("escape");
    h.draw(cx);
    assert_eq!(h.order(cx).len(), 1, "取消后标签还在");
}

#[gpui::test]
fn the_pane_right_click_close_goes_through_the_same_question(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[("web-1", true)]) else {
        return;
    };
    let pane = h
        .window
        .update(cx, |main, _, cx| {
            main.workspace.update(cx, |ws, cx| {
                ws.split_active(Axis::X, cx);
                ws.dev_focused_pane_id_for_test()
            })
        })
        .expect("split");
    h.window
        .update(cx, |main, _, cx| {
            main.workspace
                .update(cx, |ws, cx| ws.request_close_pane_for_test(pane, cx))
        })
        .expect("menu");
    h.draw(cx);
    assert!(h.ask(cx).is_some(), "右键「关闭窗格」弹的是主窗口的确认框");
    let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
    vcx.simulate_keystrokes("enter");
    h.draw(cx);
    let panes = h
        .window
        .read_with(cx, |main, cx| {
            main.workspace.read(cx).dev_pane_count_for_test()
        })
        .expect("panes");
    assert_eq!(panes, 1, "确认后窗格关掉了");
}
