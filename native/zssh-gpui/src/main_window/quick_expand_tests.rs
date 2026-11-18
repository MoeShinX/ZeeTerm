//! V2-U47：快捷命令栏展开成面板的窗口测试。真的画出主窗口，用模拟的鼠标 / 键盘走一遍：
//! 收起时命令栏和以前一样（高度、命令块、单击 / Ctrl+单击 / 广播的发送路径、焦点），展开后分组列、搜索、
//! 和栏共用当前分组、发送走同一条路径、高度拖动和持久化、搜索框拿着焦点时按键不进终端。
//!
//! 要一个独立的数据目录（`MainWindow::new` 会打开主机库）：没设 `ZSSH_DATA_DIR` 就跳过并打印原因。
//! 几个测试共用同一个库里的栏设置，所以每个测试开头先拿锁、再把栏设置写成已知状态。

use std::sync::{Mutex, MutexGuard};

use ::gpui::Focusable as _;
use ::gpui::{
    AppContext as _, Modifiers, MouseButton, MouseDownEvent, TestAppContext, VisualTestContext,
    WindowHandle, point, px,
};

use super::MainWindow;
use super::bar_height;
use super::bar_settings;
use super::modal::{ConfirmAction, Modal};
use super::nav::MainView;
use super::quick_commands::{self, QuickCommand};
use crate::settings_store::AppSettings;

static STATE: Mutex<()> = Mutex::new(());

fn cmd(id: &str, name: &str, command: &str, group: Option<&str>, enter: bool) -> QuickCommand {
    QuickCommand {
        id: id.into(),
        name: name.into(),
        command: command.into(),
        group: group.map(Into::into),
        enter,
        color: None,
        no_suggest: false,
    }
}

/// 三个分组（排查 2、服务 1）+ 两条没分组；第一条是单行回车执行，第二条是多行。
fn sample() -> Vec<QuickCommand> {
    vec![
        cmd("1", "端口", "echo ports", Some("排查"), true),
        cmd("2", "两行", "echo a\necho b", None, true),
        cmd("3", "磁盘", "echo disk", Some("排查"), false),
        cmd("4", "重启", "echo restart", Some("服务"), true),
        cmd("5", "时间", "echo time", None, true),
    ]
}

struct Harness {
    window: WindowHandle<MainWindow>,
}

impl Harness {
    /// 开主窗口、一个离线演示标签（本地回显的「已连上」窗格），栏设置写成已知状态：命令栏开着，
    /// 默认高度，分组「全部」，`expanded` 指定展开与否。
    fn open(
        cx: &mut TestAppContext,
        commands: &[QuickCommand],
        expanded: bool,
    ) -> Option<(MutexGuard<'static, ()>, Self)> {
        if std::env::var_os("ZSSH_DATA_DIR").is_none() {
            eprintln!("跳过快捷命令面板的窗口测试：需要 ZSSH_DATA_DIR 指向独立数据目录");
            return None;
        }
        let guard = STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window = cx.add_window(MainWindow::new);
        window
            .update(cx, |main, window, cx| {
                main.show_view(MainView::Terminal, window, cx);
                main.workspace.update(cx, |ws, cx| {
                    ws.dev_offline_tab(cx);
                    let last = ws.tab_count() - 1;
                    ws.select_tab(last, cx);
                });
                main.store
                    .write_bar_setting(|db| {
                        bar_settings::write_commands(db, commands)?;
                        bar_settings::write_bool(db, bar_settings::QUICK_BAR, true)?;
                        bar_settings::write_bool(db, bar_settings::QUICK_EXPANDED, expanded)?;
                        bar_settings::write_text(db, bar_settings::QUICK_GROUP, "")?;
                        bar_settings::write_number(
                            db,
                            bar_settings::QUICK_HEIGHT,
                            bar_height::QUICK_DEFAULT,
                        )?;
                        bar_settings::write_number(
                            db,
                            bar_settings::QUICK_PANEL_HEIGHT,
                            bar_height::PANEL_DEFAULT,
                        )
                    })
                    .expect("写栏设置");
                window.focus(&main.focus, cx);
            })
            .expect("main window");
        let harness = Self { window };
        harness.draw(cx);
        harness.draw(cx);
        Some((guard, harness))
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    fn vcx(&self, cx: &mut TestAppContext) -> VisualTestContext {
        VisualTestContext::from_window(self.window.into(), cx)
    }

    fn settings(&self, cx: &mut TestAppContext) -> bar_settings::BarSettings {
        self.window
            .read_with(cx, |main, _| main.store.bars().clone())
            .expect("settings")
    }

    fn click(&self, cx: &mut TestAppContext, name: &'static str, mods: Modifiers) {
        let mut vcx = self.vcx(cx);
        let bounds = vcx
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("画面上没有 {name}"));
        vcx.simulate_click(bounds.center(), mods);
        self.draw(cx);
    }

    fn has(&self, cx: &mut TestAppContext, name: &'static str) -> bool {
        self.vcx(cx).debug_bounds(name).is_some()
    }

    fn height_of(&self, cx: &mut TestAppContext, name: &'static str) -> f32 {
        let bounds = self
            .vcx(cx)
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("画面上没有 {name}"));
        f32::from(bounds.size.height)
    }

    /// 画出来的命令块有几个（`quick-command-0`、`-1`……连续数到没有为止）。
    fn buttons(&self, cx: &mut TestAppContext) -> usize {
        let mut vcx = self.vcx(cx);
        (0..64)
            .take_while(|index| {
                let name: &'static str =
                    Box::leak(format!("quick-command-{index}").into_boxed_str());
                vcx.debug_bounds(name).is_some()
            })
            .count()
    }

    fn click_nth_command(&self, cx: &mut TestAppContext, index: usize, mods: Modifiers) {
        let name: &'static str = Box::leak(format!("quick-command-{index}").into_boxed_str());
        self.click(cx, name, mods);
    }

    fn click_nth_group(&self, cx: &mut TestAppContext, index: usize) {
        let name: &'static str = Box::leak(format!("quick-group-row-{index}").into_boxed_str());
        self.click(cx, name, Modifiers::none());
    }

    fn pane_text(&self, cx: &mut TestAppContext) -> String {
        self.window
            .read_with(cx, |main, cx| {
                main.workspace.read(cx).pane_texts().join("\n")
            })
            .expect("pane text")
    }

    fn terminal_focused(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .update(cx, |main, window, cx| {
                main.workspace.read(cx).focus_handle().is_focused(window)
            })
            .expect("focus")
    }

    fn focus_search(&self, cx: &mut TestAppContext) {
        self.window
            .update(cx, |main, window, cx| {
                let handle = main.bars.quick_search.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            })
            .expect("focus search");
        self.draw(cx);
    }

    fn search_focused(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .update(cx, |main, window, cx| main.quick_search_focused(window, cx))
            .expect("focus")
    }

    fn set_search(&self, cx: &mut TestAppContext, text: &str) {
        let text = text.to_owned();
        self.window
            .update(cx, |main, window, cx| {
                main.bars
                    .quick_search
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            })
            .expect("set search");
        self.draw(cx);
    }

    fn search_value(&self, cx: &mut TestAppContext) -> String {
        self.window
            .read_with(cx, |main, cx| {
                main.bars.quick_search.read(cx).value().to_string()
            })
            .expect("search")
    }

    /// 现在开着的是不是「发送多行命令」的确认框；是就返回（发到所有分屏吗，要发的内容）。
    fn send_confirm(&self, cx: &mut TestAppContext) -> Option<(bool, String)> {
        self.window
            .read_with(cx, |main, _| match &main.modal {
                Some(Modal::Confirm(confirm)) => match &confirm.action {
                    ConfirmAction::SendCommand { data, all, .. } => Some((*all, data.clone())),
                    _ => None,
                },
                _ => None,
            })
            .expect("modal")
    }

    fn cancel_modal(&self, cx: &mut TestAppContext) {
        self.window
            .update(cx, |main, _, cx| {
                main.modal = None;
                cx.notify();
            })
            .expect("modal");
        self.draw(cx);
    }
}

// ---- 收起：和以前一样 ----

#[gpui::test]
fn collapsed_bar_looks_and_sizes_like_before(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), false) else {
        return;
    };
    assert!(h.has(cx, "quick-bar"), "收起时画的是原来的命令栏");
    assert!(!h.has(cx, "quick-panel"), "收起时没有面板");
    assert!(!h.has(cx, "quick-search"), "收起时没有搜索框");
    assert!(h.has(cx, "quick-expand"), "栏上多了「展开」按钮");
    assert!(!h.has(cx, "quick-collapse"));
    for name in ["quick-manage", "quick-create", "quick-broadcast"] {
        assert!(h.has(cx, name), "{name} 还在栏上");
    }
    assert_eq!(h.height_of(cx, "quick-bar"), bar_height::QUICK_DEFAULT);
    assert_eq!(h.buttons(cx), 5, "「全部」分组下五条都在栏上");

    // 拉高后的高度规则不变：存的高度照旧生效、夹取照旧。
    h.window
        .update(cx, |main, _, cx| {
            main.store
                .write_bar_setting(|db| {
                    bar_settings::write_number(db, bar_settings::QUICK_HEIGHT, 76.0)
                })
                .expect("写高度");
            cx.notify();
        })
        .expect("update");
    h.draw(cx);
    assert_eq!(h.height_of(cx, "quick-bar"), 76.0);
    // 展开面板的高度设置不影响栏高。
    assert_eq!(h.settings(cx).quick_height, 76.0);
}

#[gpui::test]
fn collapsed_bar_follows_the_shared_group_and_ignores_the_panel_height(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), false) else {
        return;
    };
    h.window
        .update(cx, |main, window, cx| {
            main.set_quick_group("排查", window, cx)
        })
        .expect("group");
    h.draw(cx);
    assert_eq!(h.buttons(cx), 2, "选了「排查」栏上只剩它的两条");
    // 选「未分组」（面板里才能选，栏的下拉里没有这一项）：栏上也只显示没分组的。
    h.window
        .update(cx, |main, window, cx| {
            main.set_quick_group(quick_commands::UNGROUPED_KEY, window, cx)
        })
        .expect("group");
    h.draw(cx);
    assert_eq!(h.buttons(cx), 2);
    // 分组被清空（或指向不存在的分组）回到全部，和以前一样。
    h.window
        .update(cx, |main, window, cx| {
            main.set_quick_group("没有的组", window, cx)
        })
        .expect("group");
    h.draw(cx);
    assert_eq!(h.buttons(cx), 5);
}

#[gpui::test]
fn collapsed_click_sends_through_the_same_path_as_before(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), false) else {
        return;
    };
    // 单击单行命令：写进窗格（离线演示窗格本地回显）并把键盘还给终端。
    h.window
        .update(cx, |main, window, cx| window.focus(&main.focus, cx))
        .expect("focus");
    assert!(!h.terminal_focused(cx));
    h.click_nth_command(cx, 0, Modifiers::none());
    assert!(
        h.pane_text(cx).contains("echo ports"),
        "单击发到活动窗格：{}",
        h.pane_text(cx)
    );
    assert!(h.terminal_focused(cx), "发完键盘回到终端");
    assert!(h.send_confirm(cx).is_none(), "单行不用确认");

    // 单击多行命令：先确认，不发到所有分屏。
    h.click_nth_command(cx, 1, Modifiers::none());
    let (all, data) = h.send_confirm(cx).expect("多行命令先确认");
    assert!(!all);
    assert_eq!(data, "echo a\recho b\r");
    h.cancel_modal(cx);

    // Ctrl+单击：发到所有分屏。
    h.click_nth_command(
        cx,
        1,
        Modifiers {
            control: true,
            ..Modifiers::none()
        },
    );
    assert!(
        h.send_confirm(cx).expect("多行命令先确认").0,
        "Ctrl+单击 = 所有分屏"
    );
    h.cancel_modal(cx);

    // 标签开着输入广播：不按 Ctrl 也发到所有分屏。
    h.window
        .update(cx, |main, _, cx| {
            main.workspace.update(cx, |ws, cx| ws.toggle_broadcast(cx))
        })
        .expect("broadcast");
    h.draw(cx);
    h.click_nth_command(cx, 1, Modifiers::none());
    assert!(
        h.send_confirm(cx).expect("多行命令先确认").0,
        "广播开着 = 所有分屏"
    );
}

// ---- 展开 ----

#[gpui::test]
fn expanding_swaps_the_bar_for_a_panel_and_collapsing_brings_the_bar_back(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), false) else {
        return;
    };
    h.click(cx, "quick-expand", Modifiers::none());
    assert!(h.settings(cx).quick_expanded, "展开状态写进了本机设置");
    assert!(h.has(cx, "quick-panel"));
    assert!(!h.has(cx, "quick-bar"), "展开后原来的一行栏不画");
    assert!(h.has(cx, "quick-search"));
    assert!(h.has(cx, "quick-collapse"));
    for name in ["quick-manage", "quick-create", "quick-broadcast"] {
        assert!(h.has(cx, name), "{name} 保留在面板顶部");
    }
    assert_eq!(h.height_of(cx, "quick-panel"), bar_height::PANEL_DEFAULT);

    h.click(cx, "quick-collapse", Modifiers::none());
    assert!(!h.settings(cx).quick_expanded);
    assert!(h.has(cx, "quick-bar"));
    assert!(!h.has(cx, "quick-panel"));
    assert_eq!(
        h.height_of(cx, "quick-bar"),
        bar_height::QUICK_DEFAULT,
        "收起后回到原来的高度"
    );
}

#[gpui::test]
fn an_expanded_panel_comes_back_after_a_restart(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    assert!(h.has(cx, "quick-panel"));
    // 「重启」：另开一个主窗口，只从库里读设置。
    let second = cx.add_window(MainWindow::new);
    second
        .update(cx, |main, window, cx| {
            main.show_view(MainView::Terminal, window, cx);
            main.workspace.update(cx, |ws, cx| {
                ws.dev_offline_tab(cx);
            });
        })
        .expect("second");
    let restarted = Harness { window: second };
    restarted.draw(cx);
    restarted.draw(cx);
    assert!(restarted.settings(cx).quick_expanded);
    assert!(restarted.has(cx, "quick-panel"), "重启后仍是展开的");
    assert!(!restarted.has(cx, "quick-bar"));
}

#[gpui::test]
fn the_panel_is_hidden_when_the_command_bar_switch_is_off(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    h.window
        .update(cx, |main, window, cx| main.toggle_quick_bar(window, cx))
        .expect("toggle");
    h.draw(cx);
    assert!(
        !h.has(cx, "quick-panel"),
        "命令栏开关关着，展开面板也不显示"
    );
    assert!(!h.has(cx, "quick-bar"));
    assert!(h.settings(cx).quick_expanded, "展开状态本身不丢");
    h.window
        .update(cx, |main, window, cx| main.toggle_quick_bar(window, cx))
        .expect("toggle");
    h.draw(cx);
    assert!(h.has(cx, "quick-panel"), "开关再打开，面板回来");
}

#[gpui::test]
fn the_group_column_lists_all_groups_and_ungrouped_with_counts_and_filters(
    cx: &mut TestAppContext,
) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    assert_eq!(h.buttons(cx), 5, "「全部」右边是所有命令");
    // 分组列：全部、排查、服务、未分组（见 quick_commands 的列表测试）：这里只核对点击效果。
    h.click_nth_group(cx, 1);
    assert_eq!(h.settings(cx).group, "排查");
    assert_eq!(h.buttons(cx), 2);
    h.click_nth_group(cx, 2);
    assert_eq!(h.settings(cx).group, "服务");
    assert_eq!(h.buttons(cx), 1);
    h.click_nth_group(cx, 3);
    assert_eq!(h.settings(cx).group, quick_commands::UNGROUPED_KEY);
    assert_eq!(h.buttons(cx), 2, "没分组的两条");
    h.click_nth_group(cx, 0);
    assert_eq!(h.settings(cx).group, "");
    assert_eq!(h.buttons(cx), 5);
}

#[gpui::test]
fn panel_and_bar_share_the_current_group_both_ways(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    // 面板里选 → 收起后栏上就是这个分组。
    h.click_nth_group(cx, 1);
    h.click(cx, "quick-collapse", Modifiers::none());
    assert_eq!(h.buttons(cx), 2, "栏上显示的也是「排查」");
    // 栏的下拉里选（走同一个方法）→ 展开后面板左边高亮的是它。
    h.window
        .update(cx, |main, window, cx| {
            main.set_quick_group("服务", window, cx)
        })
        .expect("group");
    h.click(cx, "quick-expand", Modifiers::none());
    assert_eq!(h.buttons(cx), 1, "展开后右边显示「服务」的一条");
    assert_eq!(h.settings(cx).group, "服务");
}

#[gpui::test]
fn searching_filters_by_name_and_command_text_ignoring_case(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    h.set_search(cx, "ECHO DISK");
    assert_eq!(h.buttons(cx), 1, "命令内容、大小写不敏感");
    h.set_search(cx, "端口");
    assert_eq!(h.buttons(cx), 1, "名称");
    h.set_search(cx, "不存在的东西");
    assert_eq!(h.buttons(cx), 0);
    assert!(h.has(cx, "quick-no-match"), "没命中时说没有匹配的命令");
    h.set_search(cx, "");
    assert_eq!(h.buttons(cx), 5);
    // 搜索和分组一起用。
    h.click_nth_group(cx, 1);
    h.set_search(cx, "disk");
    assert_eq!(h.buttons(cx), 1);
    h.set_search(cx, "restart");
    assert_eq!(h.buttons(cx), 0, "「排查」里没有 restart");
}

#[gpui::test]
fn the_panel_sends_through_the_same_path_as_the_bar(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    h.window
        .update(cx, |main, window, cx| window.focus(&main.focus, cx))
        .expect("focus");
    h.click_nth_command(cx, 0, Modifiers::none());
    assert!(
        h.pane_text(cx).contains("echo ports"),
        "{}",
        h.pane_text(cx)
    );
    assert!(h.terminal_focused(cx), "发完键盘回到终端，和栏一样");

    // 「不回车」的命令只填入。
    let before = h.pane_text(cx);
    h.click_nth_command(cx, 2, Modifiers::none());
    let after = h.pane_text(cx);
    assert!(after.contains("echo disk"), "{after}");
    assert_ne!(before, after);

    // 多行：照旧先确认；Ctrl+单击 = 所有分屏；广播开着也是。
    h.click_nth_command(cx, 1, Modifiers::none());
    let (all, data) = h.send_confirm(cx).expect("多行命令先确认");
    assert!(!all);
    assert_eq!(data, "echo a\recho b\r");
    h.cancel_modal(cx);
    h.click_nth_command(
        cx,
        1,
        Modifiers {
            control: true,
            ..Modifiers::none()
        },
    );
    assert!(h.send_confirm(cx).expect("先确认").0);
    h.cancel_modal(cx);
    h.window
        .update(cx, |main, _, cx| {
            main.workspace.update(cx, |ws, cx| ws.toggle_broadcast(cx))
        })
        .expect("broadcast");
    h.draw(cx);
    h.click_nth_command(cx, 1, Modifiers::none());
    assert!(h.send_confirm(cx).expect("先确认").0);
}

#[gpui::test]
fn broadcast_button_in_the_panel_toggles_the_same_tab_flag(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    // 只有一个窗格时广播按钮是灰的（和栏一样），点了没反应。
    h.click(cx, "quick-broadcast", Modifiers::none());
    let on = h
        .window
        .read_with(cx, |main, cx| main.workspace.read(cx).broadcast_on())
        .expect("broadcast");
    assert!(!on, "没分屏时不能开广播");
}

#[gpui::test]
fn an_empty_list_shows_the_existing_hint_and_the_plus_button(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &[], true) else {
        return;
    };
    assert!(h.has(cx, "quick-panel"));
    assert!(h.has(cx, "quick-empty"), "沿用栏上的空状态说明");
    assert!(h.has(cx, "quick-create"), "「+」还在");
    assert!(!h.has(cx, "quick-no-match"));
    assert_eq!(h.buttons(cx), 0);
    h.click(cx, "quick-empty", Modifiers::none());
    let editing = h
        .window
        .read_with(cx, |main, _| {
            matches!(main.modal, Some(Modal::QuickEdit(_)))
        })
        .expect("modal");
    assert!(editing, "点说明 = 新建一条（和栏一样）");
}

// ---- 搜索框的键盘 ----

#[gpui::test]
fn typing_in_the_search_box_does_not_reach_the_terminal(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    h.focus_search(cx);
    assert!(h.search_focused(cx));
    assert!(!h.terminal_focused(cx), "搜索框拿着键盘，终端没有");
    let before = h.pane_text(cx);
    let mut vcx = h.vcx(cx);
    vcx.simulate_input("zzqq");
    h.draw(cx);
    assert_eq!(h.search_value(cx), "zzqq", "字进了搜索框");
    assert_eq!(h.pane_text(cx), before, "终端屏幕一点没变（没有收到按键）");
    assert!(!h.pane_text(cx).contains("zzqq"));
}

#[gpui::test]
fn escape_clears_the_search_then_hands_the_keyboard_back(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    h.focus_search(cx);
    h.set_search(cx, "disk");
    let mut vcx = h.vcx(cx);
    vcx.simulate_keystrokes("escape");
    h.draw(cx);
    assert_eq!(h.search_value(cx), "", "第一次 Esc 清空搜索");
    assert!(h.search_focused(cx), "还在搜索框里");
    assert_eq!(h.buttons(cx), 5);
    let mut vcx = h.vcx(cx);
    vcx.simulate_keystrokes("escape");
    h.draw(cx);
    assert!(!h.search_focused(cx), "第二次 Esc 离开搜索框");
    assert!(h.terminal_focused(cx), "键盘回到终端");
}

#[gpui::test]
fn collapsing_clears_the_search_and_gives_the_keyboard_back(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    h.focus_search(cx);
    h.set_search(cx, "disk");
    h.click(cx, "quick-collapse", Modifiers::none());
    assert_eq!(h.search_value(cx), "");
    assert!(h.terminal_focused(cx), "搜索框随面板消失，键盘回到终端");
    h.click(cx, "quick-expand", Modifiers::none());
    assert_eq!(h.buttons(cx), 5, "再展开时是干净的搜索");
}

// ---- 高度拖动 ----

fn drag_handle(h: &Harness, cx: &mut TestAppContext, dy: f32) {
    let mut vcx = h.vcx(cx);
    let start = vcx
        .debug_bounds("quick-panel-resize")
        .expect("拖动条")
        .center();
    vcx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    h.draw(cx);
    let mut vcx = h.vcx(cx);
    // 目标点要落在窗口里（窗口外的鼠标移动不会派给窗口），拖过头的情形就拖到窗口的上 / 下边缘。
    let window_height = vcx.update(|window, _| f32::from(window.viewport_size().height));
    let moved = point(
        start.x,
        px((f32::from(start.y) + dy).clamp(2.0, window_height - 2.0)),
    );
    vcx.simulate_mouse_move(moved, MouseButton::Left, Modifiers::none());
    h.draw(cx);
    let mut vcx = h.vcx(cx);
    vcx.simulate_mouse_up(moved, MouseButton::Left, Modifiers::none());
    h.draw(cx);
}

fn area_of(h: &Harness, cx: &mut TestAppContext) -> f32 {
    h.window
        .read_with(cx, |main, _| main.area_height())
        .expect("area")
}

#[gpui::test]
fn dragging_the_top_edge_resizes_the_panel_and_remembers_it(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    let area = area_of(&h, cx);
    drag_handle(&h, cx, -40.0);
    let want = bar_height::clamp_panel(bar_height::PANEL_DEFAULT + 40.0, area);
    assert_eq!(h.settings(cx).quick_panel_height, want, "松手写进设置");
    assert_eq!(h.height_of(cx, "quick-panel"), want);
    assert_eq!(
        h.settings(cx).quick_height,
        bar_height::QUICK_DEFAULT,
        "收起时的栏高不受影响"
    );

    // 拖过头：夹在上限 / 下限。
    drag_handle(&h, cx, -5000.0);
    assert_eq!(
        h.settings(cx).quick_panel_height,
        bar_height::panel_max(area)
    );
    drag_handle(&h, cx, 5000.0);
    assert_eq!(h.settings(cx).quick_panel_height, bar_height::PANEL_MIN);
    assert_eq!(h.height_of(cx, "quick-panel"), bar_height::PANEL_MIN);
    // 下限的高度里分组列、搜索框、至少一行命令都还在。
    assert!(h.has(cx, "quick-group-row-0"));
    assert!(h.has(cx, "quick-search"));
    assert_eq!(h.buttons(cx), 5);
}

#[gpui::test]
fn double_clicking_the_handle_restores_the_default_height(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), true) else {
        return;
    };
    drag_handle(&h, cx, -30.0);
    assert_ne!(h.settings(cx).quick_panel_height, bar_height::PANEL_DEFAULT);
    let mut vcx = h.vcx(cx);
    let at = vcx
        .debug_bounds("quick-panel-resize")
        .expect("拖动条")
        .center();
    vcx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    h.draw(cx);
    assert_eq!(h.settings(cx).quick_panel_height, bar_height::PANEL_DEFAULT);
}

#[gpui::test]
fn dragging_the_collapsed_bar_edge_still_resizes_only_the_bar(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx, &sample(), false) else {
        return;
    };
    let mut vcx = h.vcx(cx);
    let start = vcx
        .debug_bounds("quick-bar-resize")
        .expect("栏的拖动条")
        .center();
    vcx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    h.draw(cx);
    let mut vcx = h.vcx(cx);
    let moved = point(start.x, start.y - px(24.0));
    vcx.simulate_mouse_move(moved, MouseButton::Left, Modifiers::none());
    h.draw(cx);
    let mut vcx = h.vcx(cx);
    vcx.simulate_mouse_up(moved, MouseButton::Left, Modifiers::none());
    h.draw(cx);
    assert_eq!(h.settings(cx).quick_height, 52.0, "栏高 28 + 24");
    assert_eq!(
        h.settings(cx).quick_panel_height,
        bar_height::PANEL_DEFAULT,
        "展开面板的高度没被动"
    );
    assert_eq!(h.height_of(cx, "quick-bar"), 52.0);
}
