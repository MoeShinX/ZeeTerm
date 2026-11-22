//! V2-U48：命令历史面板的窗口测试。真的画出主窗口和右侧侧栏，用模拟的鼠标走一遍：状态栏按钮开关、
//! 按活动窗格选桶（按主机 / 共用、切标签）、从新到旧、搜索、单击填入（不回车）、悬停的「运行」（回车）、
//! 多行确认、窗格没连着的提示、各种空状态、新记录靠节拍出现、复制走可替换的写入口。
//!
//! 命令历史在测试里是每个测试线程各一份（`assist_ops::shared_history`），互不污染；开关 / 范围直接改窗口自己的
//! `prefs`（不写库、不动进程级的设置缓存）。要一个独立的数据目录：没设 `ZSSH_DATA_DIR` 就跳过并打印原因。

use ::gpui::{AppContext as _, Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use zssh_core::terminal_assist::history::HistoryScope;

use super::MainWindow;
use super::bars::BarsMsg;
use super::modal::{ConfirmAction, Modal};
use super::nav::MainView;
use super::side_panel::{COMMAND_HISTORY, QUICK_COMMANDS};
use crate::settings_store::AppSettings;
use crate::terminal_workspace::TerminalWorkspace;

struct Harness {
    window: WindowHandle<MainWindow>,
}

/// 一个标签：离线演示窗格（本地回显的「已连上」）挂上主机编号，或没有编号（不是主机会话），
/// 或「没连上」的主机窗格。
#[derive(Clone, Copy)]
enum Tab {
    Live(i64),
    Local,
    Dead(i64),
}

impl Harness {
    fn open(cx: &mut TestAppContext, tabs: &[Tab]) -> Option<Self> {
        if std::env::var_os("ZSSH_DATA_DIR").is_none() {
            eprintln!("跳过命令历史面板的窗口测试：需要 ZSSH_DATA_DIR 指向独立数据目录");
            return None;
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        // 本进程里别的测试可能留着「历史开关关着」的缓存设置之类：这里只信窗口自己的 prefs。
        crate::chat_copy::capture_start();
        let window = cx.add_window(MainWindow::new);
        window
            .update(cx, |main, window, cx| {
                main.show_view(MainView::Terminal, window, cx);
                main.prefs.terminal_assist_enabled = true;
                main.prefs.terminal_assist_history = true;
                main.prefs.terminal_assist_history_scope = HistoryScope::Host;
                main.workspace.update(cx, |ws, cx| {
                    for tab in tabs {
                        match *tab {
                            Tab::Live(id) => {
                                ws.dev_offline_tab(cx);
                                let last = ws.tab_count() - 1;
                                ws.select_tab(last, cx);
                                ws.dev_set_host_id(id);
                            }
                            Tab::Local => {
                                ws.dev_offline_tab(cx);
                                let last = ws.tab_count() - 1;
                                ws.select_tab(last, cx);
                            }
                            Tab::Dead(id) => {
                                ws.push_test_host_tab("web-1", id);
                                let last = ws.tab_count() - 1;
                                ws.select_tab(last, cx);
                                ws.set_test_pane_disconnected();
                            }
                        }
                    }
                    ws.select_tab(0, cx);
                });
                window.focus(&main.focus, cx);
            })
            .expect("main window");
        let harness = Self { window };
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

    fn vcx(&self, cx: &mut TestAppContext) -> VisualTestContext {
        VisualTestContext::from_window(self.window.into(), cx)
    }

    fn has(&self, cx: &mut TestAppContext, name: &'static str) -> bool {
        self.vcx(cx).debug_bounds(name).is_some()
    }

    fn click(&self, cx: &mut TestAppContext, name: &'static str) {
        let mut vcx = self.vcx(cx);
        let bounds = vcx
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("画面上没有 {name}"));
        vcx.simulate_click(bounds.center(), Modifiers::none());
        self.draw(cx);
    }

    fn hover(&self, cx: &mut TestAppContext, name: &'static str) {
        let mut vcx = self.vcx(cx);
        let bounds = vcx
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("画面上没有 {name}"));
        vcx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
        self.draw(cx);
        self.draw(cx);
    }

    fn open_panel(&self, cx: &mut TestAppContext) {
        self.click(cx, "history-switch");
        assert!(self.panel_open(cx), "点状态栏按钮应该打开命令历史面板");
        self.draw(cx);
    }

    fn panel_open(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .read_with(cx, |main, _| main.history_panel_visible())
            .expect("panel")
    }

    fn select_tab(&self, cx: &mut TestAppContext, index: usize) {
        self.window
            .update(cx, |main, window, cx| main.select_tab(index, window, cx))
            .expect("select");
        self.draw(cx);
    }

    /// 面板缓存里现在的命令（从新到旧）。
    fn cached(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.window
            .read_with(cx, |main, _| {
                main.history_ui
                    .entries()
                    .iter()
                    .map(|hit| hit.text.clone())
                    .collect()
            })
            .expect("cache")
    }

    fn rows(&self, cx: &mut TestAppContext) -> usize {
        let mut vcx = self.vcx(cx);
        (0..256)
            .take_while(|index| {
                let name: &'static str = Box::leak(format!("history-row-{index}").into_boxed_str());
                vcx.debug_bounds(name).is_some()
            })
            .count()
    }

    fn click_row(&self, cx: &mut TestAppContext, index: usize) {
        let name: &'static str = Box::leak(format!("history-row-{index}").into_boxed_str());
        self.click(cx, name);
    }

    fn click_run(&self, cx: &mut TestAppContext, index: usize) {
        let row: &'static str = Box::leak(format!("history-row-{index}").into_boxed_str());
        let run: &'static str = Box::leak(format!("history-run-{index}").into_boxed_str());
        self.hover(cx, row);
        self.click(cx, run);
    }

    fn tick(&self, cx: &mut TestAppContext) -> bool {
        let changed = self
            .window
            .update(cx, |main, _, cx| main.history_tick(cx))
            .expect("tick");
        self.draw(cx);
        changed
    }

    fn set_search(&self, cx: &mut TestAppContext, text: &str) {
        let text = text.to_owned();
        self.window
            .update(cx, |main, window, cx| {
                main.history_ui
                    .search
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            })
            .expect("search");
        self.draw(cx);
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

    fn unfocus_terminal(&self, cx: &mut TestAppContext) {
        self.window
            .update(cx, |main, window, cx| window.focus(&main.focus, cx))
            .expect("focus");
    }

    fn notices(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.window
            .read_with(cx, |main, _| main.test_notices.clone())
            .expect("notices")
    }

    fn set_prefs(&self, cx: &mut TestAppContext, assist: bool, history: bool, scope: HistoryScope) {
        self.window
            .update(cx, |main, _, cx| {
                main.prefs.terminal_assist_enabled = assist;
                main.prefs.terminal_assist_history = history;
                main.prefs.terminal_assist_history_scope = scope;
                cx.notify();
            })
            .expect("prefs");
        self.draw(cx);
    }

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
}

fn record(bucket: &str, commands: &[&str]) {
    for command in commands {
        TerminalWorkspace::dev_record_history(bucket, command);
    }
}

// ---- 入口：状态栏按钮 ----

#[gpui::test]
fn the_status_bar_button_toggles_the_panel_and_it_replaces_quick_commands(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    assert!(!h.panel_open(cx), "默认不开");
    assert!(
        h.has(cx, "history-switch"),
        "状态栏右边那组开关里多了一个按钮"
    );
    let width = h
        .window
        .read_with(cx, |main, _| main.aside_width)
        .expect("width");
    h.click(cx, "history-switch");
    assert!(h.panel_open(cx));
    h.click(cx, "history-switch");
    assert!(!h.panel_open(cx), "再点一次关掉");

    // 侧栏同一时间只显示一个面板：开历史会换掉快捷命令管理，反过来也一样。
    h.window
        .update(cx, |main, window, cx| main.toggle_quick_panel(window, cx))
        .expect("quick");
    h.draw(cx);
    assert_eq!(
        h.window
            .read_with(cx, |main, _| main.panels.visible())
            .expect("v"),
        Some(QUICK_COMMANDS),
        "快捷命令管理面板照旧能开"
    );
    h.click(cx, "history-switch");
    assert_eq!(
        h.window
            .read_with(cx, |main, _| main.panels.visible())
            .expect("v"),
        Some(COMMAND_HISTORY)
    );
    h.window
        .update(cx, |main, window, cx| main.toggle_quick_panel(window, cx))
        .expect("quick");
    h.draw(cx);
    assert_eq!(
        h.window
            .read_with(cx, |main, _| main.panels.visible())
            .expect("v"),
        Some(QUICK_COMMANDS)
    );
    // 侧栏宽度沿用，没被改。
    assert_eq!(
        h.window
            .read_with(cx, |main, _| main.aside_width)
            .expect("width"),
        width
    );
}

// ---- 桶的选择、排序、刷新 ----

#[gpui::test]
fn rows_come_from_the_active_panes_bucket_newest_first_and_follow_tab_switches(
    cx: &mut TestAppContext,
) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7), Tab::Live(8), Tab::Local]) else {
        return;
    };
    record("host:7", &["ls", "pwd", "whoami"]);
    record("host:8", &["uptime"]);
    h.open_panel(cx);
    assert_eq!(h.cached(cx), ["whoami", "pwd", "ls"], "从新到旧");
    assert_eq!(h.rows(cx), 3);

    h.select_tab(cx, 1);
    assert_eq!(h.cached(cx), ["uptime"], "切标签换成那台主机的桶");
    assert_eq!(h.rows(cx), 1);

    h.select_tab(cx, 2);
    assert!(h.cached(cx).is_empty(), "不是主机会话没有桶");
    assert!(h.has(cx, "history-empty-not-host"));
    assert_eq!(h.rows(cx), 0);

    h.select_tab(cx, 0);
    assert_eq!(h.cached(cx).len(), 3, "切回来又是第一台主机的");
}

#[gpui::test]
fn the_shared_scope_shows_one_bucket_on_every_host(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7), Tab::Live(8)]) else {
        return;
    };
    record("host:7", &["only-on-seven"]);
    record("global:*", &["g1", "g2"]);
    h.set_prefs(cx, true, true, HistoryScope::Global);
    h.open_panel(cx);
    assert_eq!(
        h.cached(cx),
        ["g2", "g1"],
        "共用时看的是共用桶，不含按主机记的"
    );
    h.select_tab(cx, 1);
    assert_eq!(h.cached(cx), ["g2", "g1"], "换一台主机还是同一桶");
    // 改回按主机：马上换桶（面板渲染时跟着范围走）。
    h.set_prefs(cx, true, true, HistoryScope::Host);
    h.select_tab(cx, 0);
    assert_eq!(h.cached(cx), ["only-on-seven"]);
}

#[gpui::test]
fn a_repeated_command_is_listed_once_at_the_top(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    record("host:7", &["a", "b"]);
    h.open_panel(cx);
    record("host:7", &["a"]);
    assert!(h.tick(cx));
    assert_eq!(h.cached(cx), ["a", "b"], "合并成一条并排到最前");
    assert_eq!(h.rows(cx), 2);
}

#[gpui::test]
fn new_commands_show_up_on_the_beat_and_the_beat_is_quiet_otherwise(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    record("host:7", &["first"]);
    // 面板没开：节拍什么都不做。
    assert!(!h.tick(cx), "面板关着不读历史");
    h.open_panel(cx);
    assert_eq!(h.cached(cx), ["first"]);
    assert!(!h.tick(cx), "没有新记录：不重读、不重绘");

    record("host:7", &["second"]);
    // 只重画几帧不会去读锁里的历史（没有每帧全量读取）。
    h.draw(cx);
    h.draw(cx);
    assert_eq!(h.cached(cx), ["first"], "新记录要等节拍");
    assert!(h.tick(cx), "节拍发现修订号变了");
    assert_eq!(h.cached(cx), ["second", "first"]);
    assert_eq!(h.rows(cx), 2);
    // 别的桶的新记录也让修订号动一下，但这个桶的内容不变；重读无害。
    record("host:8", &["elsewhere"]);
    let _ = h.tick(cx);
    assert_eq!(h.cached(cx), ["second", "first"]);
}

// ---- 搜索 ----

#[gpui::test]
fn searching_filters_rows_by_substring_ignoring_case(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    record(
        "host:7",
        &[
            "systemctl restart Nginx",
            "ls /var/log",
            "tail /var/log/nginx/error.log",
        ],
    );
    h.open_panel(cx);
    assert_eq!(h.rows(cx), 3);
    h.set_search(cx, "NGINX");
    assert_eq!(h.rows(cx), 2);
    h.set_search(cx, "/var");
    assert_eq!(h.rows(cx), 2, "子串，不只是前缀");
    h.set_search(cx, "不存在");
    assert_eq!(h.rows(cx), 0);
    assert!(h.has(cx, "history-empty-no-matches"));
    assert!(h.has(cx, "history-search"), "没命中时搜索框还在，能改词");
    h.set_search(cx, "");
    assert_eq!(h.rows(cx), 3);
    assert_eq!(h.cached(cx).len(), 3, "搜索只是过滤，不动缓存");
}

// ---- 操作：填入 / 运行 / 复制 ----

#[gpui::test]
fn a_click_fills_without_enter_and_the_hover_run_button_adds_enter(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    // 最近的在最上面：行 0 = WWWW，行 1 = ZZZZ，行 2 = YYYY，行 3 = XXXX。
    record(
        "host:7",
        &["echo XXXX", "echo YYYY", "echo ZZZZ", "echo WWWW"],
    );
    h.open_panel(cx);
    h.unfocus_terminal(cx);
    assert!(!h.terminal_focused(cx));

    // 单击填入：不回车，两次填入接在同一行上。
    h.click_row(cx, 3);
    h.click_row(cx, 2);
    let text = h.pane_text(cx);
    let joined = text
        .lines()
        .find(|line| line.contains("XXXX"))
        .unwrap_or_else(|| panic!("屏幕上没有填入的命令：{text}"));
    assert!(
        joined.contains("YYYY"),
        "没回车，第二条接在第一条后面：{text}"
    );
    assert!(h.terminal_focused(cx), "填入后键盘回到终端");

    // 悬停出现「运行」：填入并回车，只发一次（不会连带触发整行的单击填入）。
    h.click_run(cx, 1);
    let text = h.pane_text(cx);
    assert_eq!(text.matches("ZZZZ").count(), 1, "只发了一次：{text}");
    // 运行带了回车：离线演示窗格的本地回显把回车当「回到行首」，所以下一次填入会从行首盖过去；
    // 要是运行没带回车，WWWW 就会接在 ZZZZ 后面。
    h.click_row(cx, 0);
    let text = h.pane_text(cx);
    let line = text
        .lines()
        .find(|line| line.contains("WWWW"))
        .unwrap_or_else(|| panic!("没有 WWWW：{text}"));
    assert!(
        line.starts_with("echo WWWW") && !line.contains("XXXX"),
        "回车把光标带回了行首，后面的从行首写起：{text}"
    );
}

#[gpui::test]
fn a_multi_line_command_asks_first_like_a_quick_command(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    // 历史里本来不会有多行命令（采集时就拒了），所以直接走发送入口：它和快捷命令是同一条路。
    h.window
        .update(cx, |main, window, cx| {
            main.history_send("echo a\necho b", false, window, cx)
        })
        .expect("send");
    h.draw(cx);
    let (all, data) = h.send_confirm(cx).expect("多行先确认");
    assert!(!all, "只发活动窗格");
    assert_eq!(data, "echo a\recho b", "和快捷命令「不回车」的内容一样");
    assert!(!h.pane_text(cx).contains("echo a"), "确认前什么都没发");
}

#[gpui::test]
fn a_pane_that_is_not_connected_gets_the_quick_command_notice(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Dead(7)]) else {
        return;
    };
    record("host:7", &["echo hi"]);
    h.open_panel(cx);
    assert_eq!(h.rows(cx), 1);
    h.click_row(cx, 0);
    assert_eq!(
        h.notices(cx).last().map(String::as_str),
        Some(crate::i18n::t(BarsMsg::PaneNotConnected)),
        "和快捷命令一样的提示"
    );
}

#[gpui::test]
fn copying_goes_through_the_replaceable_writer(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    // `open` 已经进了抓取模式：写进内存缓冲，不碰系统剪贴板。
    crate::chat_copy::capture_start();
    let _ = crate::chat_copy::capture_take();
    h.window
        .update(cx, |main, window, cx| {
            main.history_copy("echo copied-token-48", window, cx)
        })
        .expect("copy");
    assert_eq!(
        crate::chat_copy::capture_take(),
        ["echo copied-token-48".to_owned()],
        "走的是可替换的写入口"
    );
    assert_eq!(h.notices(cx).len(), 1, "给了一句「已复制」");
}

// ---- 空状态 ----

#[gpui::test]
fn each_empty_state_explains_why_and_hides_the_list(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7), Tab::Local]) else {
        return;
    };
    record("host:7", &["ls"]);
    h.open_panel(cx);
    assert_eq!(h.rows(cx), 1, "对照：正常有一行");
    assert!(!h.has(cx, "history-empty-no-records"));

    // 命令建议总开关关着。
    h.set_prefs(cx, false, true, HistoryScope::Host);
    assert!(h.has(cx, "history-empty-assist-off"));
    assert!(h.has(cx, "history-open-settings"), "给了去设置的按钮");
    assert_eq!(h.rows(cx), 0);
    assert!(!h.has(cx, "history-search"));
    // 只有「记录命令历史」关着。
    h.set_prefs(cx, true, false, HistoryScope::Host);
    assert!(h.has(cx, "history-empty-history-off"));
    assert!(h.has(cx, "history-open-settings"));
    assert_eq!(h.rows(cx), 0);
    // 都开着但当前不是主机会话。
    h.set_prefs(cx, true, true, HistoryScope::Host);
    h.select_tab(cx, 1);
    assert!(h.has(cx, "history-empty-not-host"));
    assert!(
        !h.has(cx, "history-open-settings"),
        "这种情形去设置里改不了什么"
    );
    // 主机会话但还没有记录。
    h.select_tab(cx, 0);
    h.window
        .update(cx, |main, _, cx| {
            // 换一个没有记录的主机编号。
            main.workspace.update(cx, |ws, _| ws.dev_set_host_id(99));
            cx.notify();
        })
        .expect("host");
    h.draw(cx);
    assert!(h.has(cx, "history-empty-no-records"));
    assert_eq!(h.rows(cx), 0);
    assert!(h.has(cx, "history-search"));
}

#[gpui::test]
fn the_empty_state_button_opens_the_terminal_settings_page(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    h.set_prefs(cx, false, true, HistoryScope::Host);
    h.open_panel(cx);
    h.click(cx, "history-open-settings");
    let on_settings = h
        .window
        .read_with(cx, |main, _| main.nav.view == MainView::Settings)
        .expect("view");
    assert!(on_settings, "跳到设置页（终端一页里有命令建议卡片）");
}

// ---- 回归：不碰别的 ----

#[gpui::test]
fn opening_and_closing_the_panel_leaves_the_terminal_and_other_panels_alone(
    cx: &mut TestAppContext,
) {
    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    record("host:7", &["ls"]);
    let before = h.pane_text(cx);
    h.open_panel(cx);
    h.click(cx, "history-switch");
    h.draw(cx);
    assert!(!h.panel_open(cx));
    assert_eq!(h.pane_text(cx), before, "开关面板不往终端里写任何东西");
    // 历史本身没被面板读坏：采集那一头照旧能用（recent 不算一次访问、不改记录）。
    let after = h
        .window
        .read_with(cx, |main, cx| {
            main.workspace.read(cx).history_snapshot("host:7")
        })
        .expect("snapshot");
    assert_eq!(after.1.len(), 1);
    assert_eq!(after.1[0].uses, 1);
}

// ---- 性能测量（V2-U54，不进门禁）----

/// 满桶（每个桶最多 200 条）时命令历史面板一次重绘要多久：只造元素的那部分，和整帧（造元素 + 布局 + 绘制）。
/// 用法：`cargo test --profile gate -p zssh-gpui --bin zssh-gpui a_full_bucket -- --ignored --nocapture`
/// （这些测试要 debug_assertions，gate 配置下用环境变量 `CARGO_PROFILE_GATE_DEBUG_ASSERTIONS=true` 打开）。
/// 测试平台没有真正的字形排版和 GPU，整帧的数字比真机偏小，只拿来比较「前后」。
#[gpui::test]
#[ignore = "性能测量，手动运行"]
fn a_full_bucket_redraw_cost(cx: &mut TestAppContext) {
    use std::time::Instant;

    fn per_frame(h: &Harness, cx: &mut TestAppContext, frames: usize) -> f64 {
        let started = Instant::now();
        for _ in 0..frames {
            h.window.update(cx, |_, _, cx| cx.notify()).expect("notify");
            h.draw(cx);
        }
        started.elapsed().as_secs_f64() * 1000.0 / frames as f64
    }

    let Some(h) = Harness::open(cx, &[Tab::Live(7)]) else {
        return;
    };
    for i in 0..200 {
        TerminalWorkspace::dev_record_history(
            "host:7",
            &format!("systemctl status service-{i} --no-pager --lines=50 | grep -i error"),
        );
    }
    per_frame(&h, cx, 20);
    let closed = per_frame(&h, cx, 100);
    h.open_panel(cx);
    assert_eq!(h.cached(cx).len(), 200, "桶里 200 条");
    per_frame(&h, cx, 20);
    let open = per_frame(&h, cx, 100);

    let frames = 200;
    let started = Instant::now();
    for _ in 0..frames {
        h.window
            .update(cx, |main, window, cx| {
                let tokens = crate::app_theme::Tokens::current(cx);
                drop(main.history_panel_content(&tokens, window, cx));
            })
            .expect("content");
    }
    let content = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    let (filter_ms, found) = h
        .window
        .read_with(cx, |main, _| {
            let entries = main.history_ui.entries();
            let started = Instant::now();
            let mut found = 0;
            for _ in 0..2000 {
                found = super::history_panel::filter(entries, "service-1").len();
            }
            (started.elapsed().as_secs_f64() * 1000.0 / 2000.0, found)
        })
        .expect("filter");

    h.set_search(cx, "service-1");
    per_frame(&h, cx, 20);
    let searching = per_frame(&h, cx, 100);

    println!("命令历史面板（桶里 200 条）：");
    println!("  整帧，面板关着                  {closed:.3} ms");
    println!(
        "  整帧，面板开着（200 行）        {open:.3} ms   面板多出 {:.3} ms",
        open - closed
    );
    println!("  只造面板元素 history_panel_content {content:.3} ms");
    println!("  过滤一次（200 条，命中 {found} 条）  {filter_ms:.4} ms");
    println!("  整帧，搜索「service-1」（111 行） {searching:.3} ms");
}
