//! ZeeTerm（Z-SSH 的 GPUI 原生版）入口：GPUI Kit 0.8.1 + gpui-pre 0.3.10。
//! 默认打开主窗口 `MainWindow`（标题栏标签、主机树、终端、设置、状态栏）和托盘；
//! 各功能的独立演示窗口只在 debug 钩子下打开（见 `dev_hooks`），G07b/G07c 逐个收编。
//! 数据目录按 `ZSSH_DATA_DIR` 决定。

// 发行版用 GUI 子系统：双击 exe 不再多出一个控制台窗口。调试构建保留控制台，方便看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about_view;
mod agent_chat_demo;
#[cfg(test)]
mod agent_flow_tests;
mod agent_host;
mod agent_setup;
mod agent_text;
mod agent_tools;
mod agents_demo;
mod ai_terminals;
mod app_lock;
mod app_prefs;
mod app_theme;
mod backup_demo;
mod dev_hooks;
mod editor_demo;
#[cfg(debug_assertions)]
mod fake_telnet;
mod file_window;
mod fonts;
mod forward_runtime;
mod highlight;
mod highlight_editor;
mod host_activity;
mod host_os;
mod hosts_demo;
mod icons;
mod identities_demo;
mod ime;
mod install_state;
#[cfg(test)]
mod installer_manifest;
mod links;
#[cfg(test)]
mod live_support;
mod main_window;
mod menu_kit;
mod migrate_demo;
mod os_icons;
mod product;
mod protocol_backend;
mod proxies_demo;
mod remote_editor_demo;
mod selection;
mod settings;
mod settings_nav;
mod settings_store;
mod settings_view;
mod sftp_demo;
mod split_tree;
mod ssh_backend;
mod ssh_interact;
mod status_demo;
mod sync_demo;
#[cfg(debug_assertions)]
mod sync_stub;
mod tab_drag;
mod tab_set;
mod term_window;
mod terminal_demo;
mod terminal_menu;
mod terminal_workspace;
#[cfg(test)]
mod test_sftp_server;
#[cfg(test)]
mod test_ssh_server;
mod tray;
mod typography;
mod update_auto;
mod update_demo;
mod window_state;
mod x11_glue;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use main_window::MainWindow;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Mutex;
use terminal_demo::TerminalDemo;
use zssh_platform_windows::single_instance::{self, InstanceGuard};

const INSTANCE_NAME: &str = "Z-SSH-native-instance";
const MAIN_WINDOW_PROP: &str = "Z-SSH-native-main";

static INSTANCE: Mutex<Option<InstanceGuard>> = Mutex::new(None);

fn main() {
    let app = gpui_kit::application().with_assets(icons::AppAssets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // OneDrive 登录用的 client ID：构建时从环境变量 `ZSSH_ONEDRIVE_CLIENT_ID` 编进来（和 0.2.14
        // 同一个变量，发布版由工作流从仓库 Variables 取）。自己编译的版本没有，同步设置「高级」里可以填。
        zssh_core::sync::onedrive::config::set_builtin_client_id(option_env!(
            "ZSSH_ONEDRIVE_CLIENT_ID"
        ));
        // 编辑器的快捷键要在组件的键位之后注册，同深度的绑定后注册的优先。
        remote_editor_demo::init_commands(cx);

        // 应用级设置：全进程一份，开任何窗口之前建好（主题在这里定下来：`Theme::change` 会把颜色
        // 重置成组件库默认值，之后统一由 `app_theme` 写入自己的色板）。主窗口、独立终端窗口都共用它，
        // 落盘和"跟随系统主题"的轮询也都归它。
        settings_store::AppSettings::init(cx);

        // 空闲自动锁定要知道“ZeeTerm 窗口里最近一次键盘 / 鼠标操作”（V2-U26）：给界面线程装消息钩子。
        if !zssh_platform_windows::activity::install_on_current_thread() {
            tracing::warn!("没能安装输入活动钩子，空闲自动锁定不会生效");
        }

        // 单实例：已有实例就把它的主窗口带到前台，然后退出自己。
        // 调试构建里自动化可以用 `ZSSH_INSTANCE_NAME` 换一个互斥体名：用户自己开着的 ZeeTerm
        // 占着默认名时，自动化起的程序不然会把用户的窗口拉到前台再退出（release 里读不到这个变量）。
        let instance_name =
            dev_hooks::var("ZSSH_INSTANCE_NAME").unwrap_or_else(|| INSTANCE_NAME.to_owned());
        match single_instance::acquire(&instance_name) {
            Some(guard) => {
                if let Ok(mut slot) = INSTANCE.lock() {
                    *slot = Some(guard);
                }
            }
            None => {
                single_instance::focus_existing(MAIN_WINDOW_PROP);
                // 还没建窗口和托盘，直接退出；cx.quit() 在空事件循环里不一定收得住。
                std::process::exit(0);
            }
        }

        // 退出后安装：更新窗口只把安装计划排进进程级队列，真正启动安装器放在
        // 退出回调里。订阅一被丢弃就会注销，所以这里 forget 让它活到进程结束；
        // 进程退出时本来也不需要再回收这点内存。
        std::mem::forget(cx.on_app_quit(|_| async {
            update_demo::run_pending_install();
            // 设置要求时，一并关掉本次运行拉起的 X 服务器（用户自己开的不动）。
            x11_glue::shutdown_owned();
        }));

        // 自动同步的调度器：一个进程一个，和同步设置窗口的开关无关。
        sync_demo::spawn_auto_sync(cx);
        // 后台自动检查更新：启动后延迟一会儿查一次，之后每 6 小时一次（设置里可关）。
        update_auto::spawn(cx);
        // 底部同步状态条的数据：每 2 秒读一次本机的同步设置（0.2.14 的 `SyncStatusBar`）。
        sync_demo::spawn_status_refresher(cx);

        // 调试用：往（独立的）数据目录里写种子主机，必须在主窗口读主机库之前。
        if let Err(err) = main_window::dev_seed::seed_from_env() {
            tracing::warn!(%err, "写种子主机失败");
        }

        // 无边框窗口：标题栏自己画（`TitleBar::title_bar_options` 让系统不画标题栏）。
        let window_options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                // 标题栏自己画，但系统标题（任务栏、Alt+Tab）要有名字。
                title: Some(product::NAME.into()),
                ..TitleBar::title_bar_options()
            }),
            window_bounds: Some(
                window_state::WindowState::restore(window_state::WindowState::load().main)
                    .unwrap_or_else(|| WindowBounds::centered(size(px(1180.), px(780.)), cx)),
            ),
            window_min_size: Some(size(px(720.), px(480.))),
            ..Default::default()
        };

        let (handle, view) = gpui_kit::open_window(window_options, cx, |window, cx| {
            // 无边框窗口的系统标题默认是空的：任务栏、Alt+Tab 里看不到名字，这里设成产品名。
            window.set_window_title(product::NAME);
            let view = cx.new(|cx| MainWindow::new(window, cx));
            window.focus(&view.read(cx).focus_handle(), cx);
            if let Ok(handle) = window.window_handle()
                && let RawWindowHandle::Win32(win32) = handle.as_raw()
            {
                single_instance::set_window_prop(win32.hwnd.get(), MAIN_WINDOW_PROP);
            }
            // 关闭按钮收进托盘，进程和后台任务继续跑；托盘没建起来时没有入口能叫回窗口，
            // 这时必须真正退出。两种情况都记住窗口位置和布局。
            let close_view = view.clone();
            window.on_window_should_close(cx, move |window, cx| match tray::close_action() {
                tray::CloseAction::HideToTray => {
                    close_view.update(cx, |this, cx| {
                        this.save_state(window);
                        // 设置里选了“关到托盘时锁定”：从托盘再打开时先看到锁屏页。
                        this.lock_for_tray(cx);
                    });
                    tray::hide(window);
                    false
                }
                tray::CloseAction::Quit => {
                    // 有进行中的传输 / AI 回合先问一下；确认后由主窗口自己退出。
                    if !close_view.update(cx, |this, cx| this.request_quit(cx)) {
                        return false;
                    }
                    close_view.update(cx, |this, cx| this.on_close(window, cx));
                    cx.quit();
                    true
                }
            });
            view
        })
        .expect("打开主窗口");

        // 文件窗口把标签交回主窗口时要找到它。
        file_window::register_main(cx, handle, view.downgrade());

        tray::spawn(cx, handle, view.clone());

        // 启动隐藏：等窗口真正显示后再收进托盘，否则会被平台的首次显示覆盖。
        if settings_store::snapshot(cx).start_hidden {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let _ = handle.update(cx, |_, window, _| tray::hide(window));
            })
            .detach();
        }

        // 调试钩子（打开各种演示窗口、驱动脚本）单独成函数，等这个初始化闭包返回、当前这一轮
        // 事件处理完再做：它们开窗口会同步绘制，不让一堆 debug 大栈帧叠在窗口创建的调用链上。
        cx.defer(move |cx| debug_hooks(cx, view, handle));
    });
}

/// 只在 debug 钩子下才会做任何事（见 `dev_hooks`）：打开演示窗口、启动程序内驱动。
fn debug_hooks(cx: &mut App, view: gpui::Entity<MainWindow>, handle: gpui::AnyWindowHandle) {
    // 手动/截图验证用：启动时直接在主窗口里连主机（主机库 id，可逗号分隔多个）。
    // 想开独立的旧终端窗口用 `ZSSH_CONNECT_HOST_WINDOW`。
    if let Some(raw) = dev_hooks::var("ZSSH_CONNECT_HOST") {
        for part in raw.split(',') {
            if let Ok(host_id) = part.trim().parse::<i64>() {
                let view = view.clone();
                let _ = handle.update(cx, |_, window, cx| {
                    view.update(cx, |this, cx| this.open_host(host_id, window, cx));
                });
            }
        }
    }
    // 窗口级场景用：ZSSH_FAKE_TELNET_PORT 起一个回环上的假 Telnet 设备。
    #[cfg(debug_assertions)]
    fake_telnet::spawn_from_env();
    // 手动验证用：按 ZSSH_OPEN_TELNET / ZSSH_OPEN_SERIAL 建一台 Telnet / 串口主机并在主窗口里打开
    // （说明见 `protocol_backend::dev_hook_hosts`）。
    for host_id in protocol_backend::dev_hook_hosts() {
        let view = view.clone();
        let _ = handle.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| this.open_host(host_id, window, cx));
        });
    }
    // 手动/截图验证用：启动时直接切到某一页（vault / terminal / settings）。
    if let Some(name) = dev_hooks::var("ZSSH_MAIN_VIEW")
        && let Some(page) = main_window::nav::MainView::parse(name.trim())
    {
        let view = view.clone();
        let _ = handle.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| this.show_view(page, window, cx));
        });
    }
    // 程序内驱动：按 ZSSH_MAIN_SCRIPT 里的步骤在主窗口里依次操作（见 `main_window::dev_script`）。
    main_window::dev_script::spawn_from_env(view.clone(), handle, cx);

    // 手动验证用：再开旧的终端演示窗口（原来的主窗口），它带着原来那一批演示钩子。
    if dev_hooks::var_os("ZSSH_DEMO_TERMINAL").is_some() {
        open_demo_terminal(cx);
    }

    // 手动验证用：带一个主机库 id 时启动就开独立的终端窗口（旧的 `ZSSH_CONNECT_HOST` 行为）。
    if let Some(raw) = dev_hooks::var("ZSSH_CONNECT_HOST_WINDOW") {
        for part in raw.split(',') {
            if let Ok(host_id) = part.trim().parse::<i64>() {
                terminal_demo::open_ssh_window(cx, host_id);
            }
        }
    }

    // 手动验证用：直接打开主机库窗口。
    if dev_hooks::var_os("ZSSH_HOSTS").is_some() {
        hosts_demo::open_hosts_window(cx);
    }

    // 手动/截图验证用：带一个主机库 id 时启动就开 SFTP 浏览窗口。
    if let Some(raw) = dev_hooks::var("ZSSH_SFTP_HOST")
        && let Ok(host_id) = raw.trim().parse::<i64>()
    {
        sftp_demo::open_sftp_window(cx, host_id);
    }

    // 手动/截图验证用：带这个环境变量启动时再开一个独立终端窗口。
    if dev_hooks::var_os("ZSSH_EXTRA_TERMINAL").is_some() {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
            ..Default::default()
        };
        let _ = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| TerminalDemo::new(window, cx, "terminal-1".to_owned()));
            window.focus(&view.read(cx).focus_handle(), cx);
            view
        });
    }

    // 手动/截图验证用：带这个环境变量启动时直接开一个编辑器窗口。
    if dev_hooks::var_os("ZSSH_OPEN_EDITOR").is_some() {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(900.), px(620.)), cx)),
            ..Default::default()
        };
        let _ = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| editor_demo::EditorDemo::new(window, cx));
            window.focus(&view.read(cx).focus_handle(), cx);
            view
        });
    }

    // 手动验证用：带这个环境变量启动时直接打开更新检查窗口。
    if dev_hooks::var_os("ZSSH_UPDATE").is_some() {
        update_demo::open_update_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开备份与恢复窗口。
    if dev_hooks::var_os("ZSSH_BACKUP").is_some() {
        backup_demo::open_backup_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开同步设置窗口。
    if dev_hooks::var_os("ZSSH_SYNC").is_some() {
        sync_demo::open_sync_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开服务器状态窗口。
    if dev_hooks::var_os("ZSSH_STATUS").is_some() {
        status_demo::open_status_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开 AI 智能体窗口。
    if dev_hooks::var_os("ZSSH_AGENTS").is_some() {
        agents_demo::open_agents_window(cx);
    }

    // 手动验证用（只在调试版）：带 ZSSH_AGENT_CHAT=1 启动时直接打开 AI 对话窗口；
    // ZSSH_AGENT_DEMO=1 时窗口从离线脚本演示开始（产品默认是真实智能体）。
    if dev_hooks::var_os("ZSSH_AGENT_CHAT").is_some()
        || dev_hooks::var_os("ZSSH_AGENT_DEMO").is_some()
    {
        agent_chat_demo::open_agent_chat_window(cx);
    }
}

/// 旧的终端演示窗口（原来的主窗口）：只在 `ZSSH_DEMO_TERMINAL` 钩子下打开，
/// 带着它原来的那一批演示钩子（预开标签、分屏、鼠标上报、高亮、设置面板）。
fn open_demo_terminal(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
        ..Default::default()
    };
    let _ = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| {
            let mut demo = TerminalDemo::new(window, cx, "terminal-demo".to_owned());
            // 截图/手动验证用：带这个环境变量时预开若干标签。
            let extra = dev_hooks::var("ZSSH_TABS")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            for _ in 0..extra {
                demo.new_tab(cx);
            }
            // 截图/手动验证用：带这个环境变量时预先分屏。
            if dev_hooks::var_os("ZSSH_SPLIT").is_some() {
                demo.toggle_split(cx);
            }
            // 截图/手动验证用：带这个环境变量时打开鼠标上报。
            if dev_hooks::var_os("ZSSH_MOUSE").is_some() {
                demo.seed_mouse_reporting(cx);
            }
            // 截图/手动验证用：带这个环境变量时展开高亮面板。
            if dev_hooks::var_os("ZSSH_HIGHLIGHT").is_some() {
                demo.toggle_highlight_panel(cx);
            }
            // 截图/手动验证用：追加一条高亮规则，绕过输入法。
            if let Some(pattern) = dev_hooks::var("ZSSH_HIGHLIGHT_RULE") {
                demo.seed_highlight_rule(pattern, cx);
            }
            // 截图/手动验证用：展开设置面板。
            if dev_hooks::var_os("ZSSH_SETTINGS").is_some() {
                demo.toggle_settings_panel(cx);
            }
            demo
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}
