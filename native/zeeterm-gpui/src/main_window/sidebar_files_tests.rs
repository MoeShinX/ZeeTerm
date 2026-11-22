//! V2-U46：左侧边栏单栏文件列表（`sidebar_files`）的窗口测试：入口、页切换、和右侧停靠共用同一个实体、
//! 强制单栏不改设置、跟随终端目录的条件、运行中切换停靠位置、非 SSH 标签 / 没有标签时的说明。
//!
//! 要一个独立的数据目录（`MainWindow::new` 会打开主机库）：没设 `ZEETERM_DATA_DIR` 就跳过并打印原因。
//! 不连任何服务器：标签是「记在主机 7 名下、但不真连」的测试标签，主机库里没有主机 7，所以文件视图
//! 建出来马上因为找不到主机而连接失败——这里只关心视图实体本身（有没有、是不是同一个、画在哪、单栏没有）。
//! 本地栏的起始目录指向临时目录（不去读用户的下载文件夹）。

use ::gpui::{AppContext as _, Context, TestAppContext, Window, WindowHandle};

use super::MainWindow;
use super::nav::MainView;
use super::sidebar_files::SidePage;
use crate::settings::{SftpDockSide, SftpLocalDir};
use crate::settings_store::AppSettings;

const HOST: i64 = 7;

struct Harness {
    window: WindowHandle<MainWindow>,
    /// 第一个标签的编号。
    tab: Option<u64>,
    _local: tempfile::TempDir,
}

impl Harness {
    /// 开一个主窗口，停在终端页。`hosts` 里每项是一个标签：`Some(host)` = 记在这台主机名下的 SSH 标签，
    /// `None` = 空白标签。选中第一个。
    fn open(cx: &mut TestAppContext, hosts: &[Option<i64>], side: SftpDockSide) -> Option<Self> {
        if std::env::var_os("ZEETERM_DATA_DIR").is_none() {
            eprintln!("跳过侧边栏文件列表的窗口测试：需要 ZEETERM_DATA_DIR 指向独立数据目录");
            return None;
        }
        let local = tempfile::Builder::new()
            .prefix("zssh-u46-local-")
            .tempdir()
            .expect("临时目录");
        let local_path = local.path().to_string_lossy().into_owned();
        cx.update(|cx| {
            gpui_kit::init(cx);
            let settings = AppSettings::init_for_test(cx);
            settings.update(cx, |model, mcx| {
                model.update_with(mcx, |s| {
                    s.sftp_local_dir = SftpLocalDir::Custom;
                    s.sftp_local_dir_custom = local_path.clone();
                    s.sftp_dock_side = side;
                });
            });
        });
        let window = cx.add_window(MainWindow::new);
        let tab = window
            .update(cx, |main, window, cx| {
                main.show_view(MainView::Terminal, window, cx);
                main.workspace.update(cx, |ws, cx| {
                    let ids: Vec<u64> = hosts
                        .iter()
                        .enumerate()
                        .map(|(at, host)| match host {
                            Some(host) => ws.push_test_host_tab(&format!("web-{at}"), *host),
                            None => ws.push_test_tab(&format!("blank-{at}")),
                        })
                        .collect();
                    if !ids.is_empty() {
                        ws.select_tab(0, cx);
                    }
                    ids.first().copied()
                })
            })
            .expect("main window");
        let harness = Self {
            window,
            tab,
            _local: local,
        };
        // 第一个节拍只记下「停靠位置」设置的现值（启动时不触发切换）。
        harness.tick(cx);
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

    fn tick(&self, cx: &mut TestAppContext) {
        self.window
            .update(cx, |main, window, cx| main.sync_docks(window, cx))
            .expect("tick");
    }

    /// 在主窗口上做点事（带窗口和上下文）。
    fn with<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut MainWindow, &mut Window, &mut Context<MainWindow>) -> R,
    ) -> R {
        self.window.update(cx, f).expect("main window")
    }

    /// 像设置页上选了一个停靠位置那样改设置，再让节拍收尾。
    fn choose_side(&self, cx: &mut TestAppContext, side: SftpDockSide) {
        self.with(cx, |main, _, cx| {
            main.workspace
                .update(cx, |ws, cx| ws.set_sftp_dock_side(side, cx));
        });
        self.tick(cx);
        self.draw(cx);
    }

    fn page(&self, cx: &mut TestAppContext) -> SidePage {
        self.with(cx, |main, _, _| main.docks.side_page)
    }

    fn right_dock_open(&self, cx: &mut TestAppContext) -> bool {
        self.with(cx, |main, _, cx| {
            main.docks
                .sftp_open_for(main.workspace.read(cx).active_tab_id())
        })
    }

    /// 停靠实体（主机 7 的那一个）的实体号；还没建就是 `None`。
    fn view_id(&self, cx: &mut TestAppContext) -> Option<::gpui::EntityId> {
        self.with(cx, |main, _, _| {
            main.docks.sftp.get(&HOST).map(|view| view.entity_id())
        })
    }

    fn views(&self, cx: &mut TestAppContext) -> usize {
        self.with(cx, |main, _, _| main.docks.sftp.len())
    }

    fn local_hidden(&self, cx: &mut TestAppContext) -> Option<bool> {
        self.with(cx, |main, _, cx| {
            main.docks
                .sftp
                .get(&HOST)
                .map(|view| view.read(cx).local_hidden())
        })
    }

    fn show_local_setting(&self, cx: &mut TestAppContext) -> bool {
        self.with(cx, |main, _, cx| {
            main.workspace.read(cx).settings().sftp_show_local
        })
    }

    fn right_width(&self, cx: &mut TestAppContext) -> f32 {
        self.with(cx, |main, _, cx| main.dock_layout_now(cx).sftp_width)
    }
}

/// 右侧模式（默认）：入口开关右侧停靠，侧边栏没有文件页（回归：行为和改动前一致）。
#[gpui::test]
fn the_right_mode_entries_still_open_and_close_the_right_dock(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Right) else {
        return;
    };
    assert!(!h.with(cx, |main, _, cx| main.sidebar_mode(cx)));
    assert!(!h.right_dock_open(cx));
    assert_eq!(h.right_width(cx), 0.0);
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    h.draw(cx);
    assert!(h.right_dock_open(cx), "入口打开右侧停靠");
    assert_eq!(h.page(cx), SidePage::Hosts, "侧边栏仍是主机页");
    assert!(h.right_width(cx) > 0.0, "右侧停靠有宽度");
    assert!(h.view_id(cx).is_some(), "视图补建出来");
    assert!(h.with(cx, |main, _, cx| main.files_entry_active(cx)));
    assert!(!h.with(cx, |main, _, cx| main.sidebar_files_visible(cx)));
    assert_eq!(
        h.local_hidden(cx),
        Some(false),
        "右侧模式照设置显示本地栏（默认显示）"
    );
    // 「打开」语义：已经开着再要求打开 = 不动；关闭走 toggle。
    h.with(cx, |main, window, cx| main.set_sftp_dock(true, window, cx));
    assert!(h.right_dock_open(cx));
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    assert!(!h.right_dock_open(cx), "再按一次收起");
    assert_eq!(h.right_width(cx), 0.0);
}

/// 左侧模式：入口切侧边栏的页，不开右侧停靠；再按一次切回主机页；折叠着先展开。
#[gpui::test]
fn the_sidebar_mode_entries_switch_the_page_and_never_open_the_right_dock(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Sidebar) else {
        return;
    };
    assert!(h.with(cx, |main, _, cx| main.sidebar_mode(cx)));
    assert_eq!(h.page(cx), SidePage::Hosts, "启动时是主机页");
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    h.draw(cx);
    assert_eq!(h.page(cx), SidePage::Files);
    assert!(!h.right_dock_open(cx), "不开右侧停靠");
    assert_eq!(h.right_width(cx), 0.0, "右侧没有文件停靠的宽度");
    assert!(h.with(cx, |main, _, cx| main.sidebar_files_live(cx)));
    assert!(h.with(cx, |main, _, cx| main.files_entry_active(cx)));
    assert!(h.view_id(cx).is_some(), "文件页补建了当前主机的视图");
    // 再按一次：回主机页。
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    assert_eq!(h.page(cx), SidePage::Hosts);
    assert!(!h.with(cx, |main, _, cx| main.files_entry_active(cx)));
    // 「打开」语义（主机右键「打开 SFTP」走的）：显示文件页，已经在也不会反过来关。
    h.with(cx, |main, window, cx| main.set_sftp_dock(true, window, cx));
    assert_eq!(h.page(cx), SidePage::Files);
    h.with(cx, |main, window, cx| main.set_sftp_dock(true, window, cx));
    assert_eq!(h.page(cx), SidePage::Files);
    h.with(cx, |main, window, cx| main.set_sftp_dock(false, window, cx));
    assert_eq!(h.page(cx), SidePage::Hosts, "关闭 = 回主机页");
    h.with(cx, |main, window, cx| main.set_sftp_dock(false, window, cx));
    assert_eq!(h.page(cx), SidePage::Hosts);
    // 侧边栏折叠着：入口先展开并切到文件页。
    h.with(cx, |main, _, cx| {
        main.tree.collapsed = true;
        cx.notify();
    });
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    assert!(
        !h.with(cx, |main, _, _| main.tree.collapsed),
        "折叠着就展开"
    );
    assert_eq!(h.page(cx), SidePage::Files);
    // 设置页没有侧边栏：入口不动页（由调用方提示）。
    h.with(cx, |main, window, cx| {
        main.show_view(MainView::Settings, window, cx);
        assert!(!main.sidebar_entry(true, window, cx));
    });
}

/// 文件页画的就是右侧停靠的那个实体：右侧 → 左侧 → 右侧，实体一直是同一个、没有重建；
/// 左侧强制单栏但不改「显示本地栏」设置，换回右侧后按设置恢复。
#[gpui::test]
fn the_sidebar_shows_the_same_view_as_the_right_dock_and_switching_never_rebuilds_it(
    cx: &mut TestAppContext,
) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Right) else {
        return;
    };
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    h.draw(cx);
    let view = h.view_id(cx).expect("右侧停靠建了视图");
    assert!(h.show_local_setting(cx));
    assert_eq!(h.local_hidden(cx), Some(false));
    // 视图自己的状态（跟随终端目录的勾选）要跟着实体走。
    h.with(cx, |main, _, cx| {
        let v = main.docks.sftp.get(&HOST).cloned().expect("视图");
        v.update(cx, |v, cx| v.set_follow(true, cx));
    });

    // 运行中改成左侧：右侧停靠关掉、侧边栏切到文件页；实体不重建。
    h.choose_side(cx, SftpDockSide::Sidebar);
    assert!(!h.right_dock_open(cx), "右侧停靠关了");
    assert_eq!(h.right_width(cx), 0.0);
    assert_eq!(h.page(cx), SidePage::Files, "侧边栏切到文件页");
    assert_eq!(h.view_id(cx), Some(view), "同一个实体");
    assert_eq!(h.views(cx), 1, "没有另开一套");
    assert_eq!(h.local_hidden(cx), Some(true), "侧边栏里强制只显示远端一栏");
    assert!(h.show_local_setting(cx), "不改用户的「显示本地」设置");
    let followed = h.with(cx, |main, _, cx| {
        main.docks.sftp.get(&HOST).map(|v| v.read(cx).follow())
    });
    assert_eq!(followed, Some(true), "跟随状态跟着实体走");
    h.draw(cx);
    assert_eq!(h.view_id(cx), Some(view));

    // 再改回右侧：侧边栏回主机页、右侧停靠打开；实体仍是同一个，本地栏按设置恢复。
    h.choose_side(cx, SftpDockSide::Right);
    assert_eq!(h.page(cx), SidePage::Hosts, "侧边栏回到主机页");
    assert!(h.right_dock_open(cx), "右侧停靠重新打开");
    assert!(h.right_width(cx) > 0.0);
    assert_eq!(h.view_id(cx), Some(view), "同一个实体");
    assert_eq!(h.views(cx), 1);
    assert_eq!(h.local_hidden(cx), Some(false), "按设置恢复本地栏");
    assert!(h.show_local_setting(cx));
}

/// 改成左侧时右侧停靠本来就没开：不凭空开文件页；改回右侧时主机页上也不凭空开右侧停靠。
#[gpui::test]
fn switching_the_side_with_nothing_open_opens_nothing(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Right) else {
        return;
    };
    h.choose_side(cx, SftpDockSide::Sidebar);
    assert_eq!(h.page(cx), SidePage::Hosts);
    assert!(!h.right_dock_open(cx));
    assert!(h.view_id(cx).is_none(), "没开过就没有视图");
    h.choose_side(cx, SftpDockSide::Right);
    assert_eq!(h.page(cx), SidePage::Hosts);
    assert!(!h.right_dock_open(cx));
    assert!(h.view_id(cx).is_none());
}

/// 启动时设置就是左侧边栏：第一个节拍只记现值，不当成「刚切换」去动页。
#[gpui::test]
fn starting_with_the_sidebar_setting_does_not_count_as_a_switch(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Sidebar) else {
        return;
    };
    assert_eq!(h.page(cx), SidePage::Hosts);
    assert!(!h.right_dock_open(cx));
    assert_eq!(
        h.with(cx, |m, _, _| m.docks.dock_side_seen),
        Some(SftpDockSide::Sidebar)
    );
}

/// 跟随终端目录：新建视图的初值取设置；轮询在左侧模式下「文件页正显示当前标签」时才跑，
/// 右侧模式的条件不变（当前标签的右侧停靠开着）。
#[gpui::test]
fn following_the_terminal_directory_is_polled_where_the_files_are_shown(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Sidebar) else {
        return;
    };
    let polls =
        |h: &Harness, cx: &mut TestAppContext| h.with(cx, |m, _, cx| m.follow_polls_now(cx));
    assert!(!polls(&h, cx), "主机页：不轮询");
    // 设置里默认勾选「跟随终端目录」：之后建的视图初值是开。
    h.with(cx, |main, _, cx| {
        main.workspace
            .update(cx, |ws, cx| ws.toggle_sftp_follow_cwd(cx));
    });
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    h.draw(cx);
    assert!(polls(&h, cx), "文件页正显示当前标签：轮询");
    let follow = h.with(cx, |main, _, cx| {
        main.docks.sftp.get(&HOST).map(|v| v.read(cx).follow())
    });
    assert_eq!(follow, Some(true), "初值取设置");
    // 侧边栏折叠了（看不见）/ 切回主机页 / 不在终端页：都不轮询。
    h.with(cx, |main, _, cx| {
        main.tree.collapsed = true;
        cx.notify();
    });
    assert!(!polls(&h, cx), "折叠着看不见：不轮询");
    h.with(cx, |main, _, cx| {
        main.tree.collapsed = false;
        cx.notify();
    });
    assert!(polls(&h, cx));
    h.with(cx, |main, window, cx| {
        main.show_view(MainView::Vault, window, cx)
    });
    assert!(!polls(&h, cx), "不在终端页：不轮询");
    h.with(cx, |main, window, cx| {
        main.show_view(MainView::Terminal, window, cx)
    });
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    assert!(!polls(&h, cx), "回到主机页：不轮询");

    // 右侧模式：条件和以前一样，只看当前标签的右侧停靠；侧边栏的页状态不相干。
    h.choose_side(cx, SftpDockSide::Right);
    // 上面已经切回主机页，所以改回右侧时不会凭空打开右侧停靠：不轮询。
    assert!(!h.right_dock_open(cx));
    assert!(!polls(&h, cx), "右侧停靠关着：不轮询");
    h.with(cx, |main, window, cx| main.set_sftp_dock(true, window, cx));
    assert!(polls(&h, cx), "右侧停靠开着：轮询");
}

/// 空白标签（不是 SSH 会话）和没有标签：文件页画一句说明，不建视图、不报错（画几帧不 panic）。
#[gpui::test]
fn the_files_page_explains_itself_for_a_blank_tab_and_when_there_are_no_tabs(
    cx: &mut TestAppContext,
) {
    for hosts in [vec![None], vec![]] {
        let Some(h) = Harness::open(cx, &hosts, SftpDockSide::Sidebar) else {
            return;
        };
        h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
        h.draw(cx);
        h.draw(cx);
        assert_eq!(h.page(cx), SidePage::Files, "{hosts:?}");
        assert_eq!(h.views(cx), 0, "{hosts:?}：不建视图");
        // 再切回主机页也照常画。
        h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
        h.draw(cx);
        assert_eq!(h.page(cx), SidePage::Hosts);
    }
}

/// 活动标签是文件标签 / 不在终端页：文件页不算「正显示当前终端标签」，不建视图、不轮询。
#[gpui::test]
fn the_files_page_is_not_live_off_the_terminal_page(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Sidebar) else {
        return;
    };
    h.with(cx, |main, window, cx| {
        main.show_view(MainView::Vault, window, cx);
        main.show_sidebar_files(window, cx);
    });
    h.draw(cx);
    assert!(
        h.with(cx, |main, _, cx| main.sidebar_files_visible(cx)),
        "主机页上侧边栏也在"
    );
    assert!(!h.with(cx, |main, _, cx| main.sidebar_files_live(cx)));
    assert_eq!(h.views(cx), 0, "不在终端页不建视图");
}

/// 窄窗口 / 窄侧边栏下画文件页和主机页都不 panic（列精简由视图自己按 `compact_columns` 适应）。
#[gpui::test]
fn the_sidebar_pages_draw_at_narrow_widths(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Sidebar) else {
        return;
    };
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    for width in [1200.0, 800.0, 640.0, 520.0] {
        cx.simulate_window_resize(
            h.window.into(),
            ::gpui::size(::gpui::px(width), ::gpui::px(600.0)),
        );
        h.draw(cx);
        h.draw(cx);
    }
    h.with(cx, |main, window, cx| main.toggle_sftp_dock(window, cx));
    h.draw(cx);
    assert_eq!(h.page(cx), SidePage::Hosts);
    let _ = h.tab;
}

/// 左侧模式下右侧停靠的标记就算还留着（比如别的路径写了它），右侧也不画、不占宽度、不建视图、不轮询。
#[gpui::test]
fn a_stale_right_dock_flag_is_ignored_in_the_sidebar_mode(cx: &mut TestAppContext) {
    let Some(h) = Harness::open(cx, &[Some(HOST)], SftpDockSide::Sidebar) else {
        return;
    };
    let tab = h.tab.expect("有标签");
    h.with(cx, |main, _, _| main.docks.set_sftp_open_for(tab, true));
    h.tick(cx);
    h.draw(cx);
    assert_eq!(h.right_width(cx), 0.0, "右侧不占宽度");
    assert!(h.view_id(cx).is_none(), "标记不会让左侧模式建视图");
    assert!(!h.with(cx, |main, _, cx| main.follow_polls_now(cx)));
}

/// 文件标签的「并排到终端」：右侧模式挂到右侧停靠（和以前一样），左侧边栏模式挂到侧边栏的文件页；
/// 两种模式下都是同一个视图实体挪过去，不重建。
#[gpui::test]
fn docking_a_file_tab_goes_to_the_right_dock_or_the_sidebar_files_page(cx: &mut TestAppContext) {
    for side in [SftpDockSide::Right, SftpDockSide::Sidebar] {
        let Some(h) = Harness::open(cx, &[Some(HOST)], side) else {
            return;
        };
        let view = h.with(cx, |main, window, cx| {
            let view =
                cx.new(|cx| crate::sftp::SftpDemo::new_embedded(window, cx, HOST, None, false));
            let id = view.entity_id();
            main.dock_sftp_view(HOST, view, window, cx)
                .expect("可以并排");
            id
        });
        h.draw(cx);
        assert_eq!(h.view_id(cx), Some(view), "{side:?}：同一个实体");
        match side {
            SftpDockSide::Right => {
                assert!(h.right_dock_open(cx));
                assert_eq!(h.page(cx), SidePage::Hosts);
            }
            SftpDockSide::Sidebar => {
                assert!(!h.right_dock_open(cx), "左侧模式不开右侧停靠");
                assert_eq!(h.page(cx), SidePage::Files);
                assert!(h.with(cx, |main, _, cx| main.sidebar_files_live(cx)));
                assert_eq!(h.local_hidden(cx), Some(true), "单栏");
            }
        }
    }
}
