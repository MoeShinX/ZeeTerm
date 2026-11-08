//! ZeeTerm（Z-SSH 的 GPUI 原生版）入口：GPUI Kit 0.8.1 + gpui-pre 0.3.10。
//! 默认打开主窗口 `MainWindow`（标题栏标签、主机树、终端、设置、状态栏）和托盘；
//! 各功能的独立演示窗口只在 debug 钩子下打开（见 `dev_hooks`），G07b/G07c 逐个收编。
//! 数据目录按 `ZSSH_DATA_DIR` 决定。

// 发行版用 GUI 子系统：双击 exe 不再多出一个控制台窗口。调试构建保留控制台，方便看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about_view;
mod agent_chat;
#[cfg(test)]
mod agent_flow_tests;
mod agent_host;
mod agent_setup;
mod agent_text;
mod agent_tools;
mod agents;
mod ai_terminals;
mod app_lock;
#[cfg(any(target_os = "macos", test))]
mod app_menu;
mod app_prefs;
mod app_theme;
mod armed;
mod backup;
mod chat_copy;
#[cfg(unix)]
mod cli_args;
mod conn_pool;
mod datetime;
mod dev_hooks;
mod error_text;
mod ops_text;
mod reason_text;
mod update_text;
// 编辑器演示窗口只有 dev 钩子开得到（ZSSH_OPEN_EDITOR / terminal_demo 内部），release 不编译。
#[cfg(debug_assertions)]
mod editor_demo;
#[cfg(debug_assertions)]
mod fake_telnet;
mod file_window;
mod fonts;
mod format;
mod forward_runtime;
mod highlight;
mod highlight_editor;
mod host_activity;
mod host_db;
mod host_os;
mod hosts;
mod i18n;
mod icons;
mod identities;
mod ime;
mod input_activity;
mod install_state;
#[cfg(test)]
mod installer_manifest;
mod links;
#[cfg(test)]
mod live_support;
mod main_window;
mod menu_kit;
mod os_icons;
mod panic_log;
mod portable;
mod product;
mod protocol_backend;
mod proxies;
mod remote_editor;
mod selection;
mod settings;
mod settings_nav;
mod settings_store;
mod settings_view;
mod sftp;
mod split_tree;
mod ssh_backend;
mod ssh_interact;
mod ssh_prompt;
mod ssh_prompt_ui;
mod status;
mod sync;
#[cfg(debug_assertions)]
mod sync_stub;
mod tab_drag;
mod tab_set;
mod term_window;
// 旧的终端演示窗口只有 dev 钩子开得到（ZSSH_DEMO_TERMINAL / ZSSH_CONNECT_HOST_WINDOW /
// ZSSH_EXTRA_TERMINAL / hosts 演示窗的连接按钮），release 不编译。
#[cfg(debug_assertions)]
mod terminal_demo;
mod terminal_menu;
mod terminal_workspace;
#[cfg(test)]
mod test_sftp_server;
#[cfg(test)]
mod test_ssh_server;
#[cfg(windows)]
mod tray;
#[cfg(target_os = "linux")]
#[path = "tray_linux.rs"]
mod tray;
// macOS 还没有菜单栏图标：同一套接口，行为同 Linux 托盘不可用时（见 `tray_macos.rs`）。
#[cfg(target_os = "macos")]
#[path = "tray_macos.rs"]
mod tray;
#[cfg(any(windows, target_os = "linux"))]
mod tray_menu;
// 只为在别的平台上也编译、测试 macOS 那份托盘替身（它不依赖任何 macOS 专有的东西）；
// 这里没人调用它的接口，所以放过"从未使用"。
#[cfg(all(test, not(target_os = "macos")))]
#[path = "tray_macos.rs"]
#[allow(dead_code)]
mod tray_macos_check;
mod typography;
mod update;
mod update_auto;
mod window_frame;
mod window_state;
mod x11_glue;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use main_window::MainWindow;
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Mutex;
#[cfg(debug_assertions)]
use terminal_demo::TerminalDemo;
use zssh_platform_windows::single_instance::{self, InstanceGuard};

const INSTANCE_NAME: &str = "ZeeTerm-instance";
const MAIN_WINDOW_PROP: &str = "ZeeTerm-main";

static INSTANCE: Mutex<Option<InstanceGuard>> = Mutex::new(None);

fn main() {
    // `--version`：打印版本就退出，放在任何初始化（数据目录、钥匙串、窗口）之前。
    #[cfg(unix)]
    if let Some(line) = cli_args::version_line(std::env::args_os()) {
        println!("{line}");
        return;
    }
    // 没有图形显示时 GPUI 会进无头模式、永远不出窗口：直接说清楚再退出，别让人以为卡住了。
    #[cfg(target_os = "linux")]
    if !cli_args::has_display(
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("DISPLAY"),
    ) {
        eprintln!("{}", cli_args::no_display_message());
        std::process::exit(1);
    }
    // 免安装版自更新后的重启：新进程先等旧进程退出（释放单实例互斥体），否则会被当成"已经有一个在运行"而退出。
    // 放在最前面，早于数据目录和单实例的任何动作；等不到（超时）也照常往下走。
    #[cfg(windows)]
    if let Some(pid) = zssh_update::portable::wait_pid_from_args(std::env::args_os()) {
        zssh_platform_windows::process::wait_for_exit(pid, std::time::Duration::from_secs(30));
    }
    // 先确认有正式的数据目录：拿不到就提示退出，不让凭据库、主机库落进会被清理的临时目录。
    settings::ensure_data_dir_or_exit();
    // 出货版 panic 之后进程直接没了：先装 hook，把 panic 的位置和消息记到数据目录的 logs/panic.log。
    panic_log::install();
    // Linux / macOS 的本机密钥：后备密钥文件放进数据目录，并在后台先把密钥取出来缓存好，
    // 免得界面线程上第一次保存 / 查看密码时去等钥匙串（见 `zssh_core::store::secrets`）。
    #[cfg(unix)]
    {
        zssh_core::store::secrets::init_vault(&settings::data_dir());
        zssh_core::store::secrets::warm_up_vault();
    }
    // macOS：在后台先问一次登录 shell 的 PATH，给智能体的探测和启动用（其他平台什么也不做）。
    zssh_core::ai::agent::discovery::warm_up_agent_path();
    let app = gpui_kit::application().with_assets(icons::AppAssets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // 编辑器的快捷键要在组件的键位之后注册，同深度的绑定后注册的优先。
        remote_editor::init_commands(cx);

        // 应用级设置：全进程一份，开任何窗口之前建好（主题在这里定下来：`Theme::change` 会把颜色
        // 重置成组件库默认值，之后统一由 `app_theme` 写入自己的色板）。主窗口、独立终端窗口都共用它，
        // 落盘和"跟随系统主题"的轮询也都归它。
        settings_store::AppSettings::init(cx);

        // X11 失败写进终端的那一行跟随界面语言（核心库不挑语言）。
        zssh_core::x11::set_notice_renderer(error_text::x11_notice_line);

        // 空闲自动锁定要知道“ZeeTerm 窗口里最近一次键盘 / 鼠标操作”（V2-U26）：Windows 给界面线程装消息钩子；
        // Unix 没有钩子，由 GPUI 层把键盘 / 鼠标事件喂给 `activity::touch()`。
        #[cfg(windows)]
        if !zssh_platform_windows::activity::install_on_current_thread() {
            tracing::warn!("没能安装输入活动钩子，空闲自动锁定不会生效");
        }
        #[cfg(not(windows))]
        input_activity::install(cx);

        // 单实例：已有实例就把它的主窗口带到前台，然后退出自己。
        // 调试构建里自动化可以用 `ZSSH_INSTANCE_NAME` 换一个互斥体名：用户自己开着的 ZeeTerm
        // 占着默认名时，自动化起的程序不然会把用户的窗口拉到前台再退出（release 里读不到这个变量）。
        // 免安装版的互斥体名、窗口属性名带数据目录的短哈希，这样它和安装版、和别的目录里的免安装版可以同时开；
        // 安装版的名字不变（老版本升级上来仍是同一个名字）。
        let instance_names = portable::instance_names(
            INSTANCE_NAME,
            MAIN_WINDOW_PROP,
            portable::is_portable().then(settings::data_dir).as_deref(),
        );
        let instance_name = dev_hooks::var("ZSSH_INSTANCE_NAME").unwrap_or(instance_names.mutex);
        let window_prop = instance_names.window_prop;
        // Unix：第二个实例通过套接字叫醒这里；监听线程只负责转发，真正动窗口要回到界面线程。
        #[cfg(unix)]
        let mut activation_rx = None;
        match single_instance::acquire(&instance_name) {
            Some(guard) => {
                #[cfg(unix)]
                let guard = {
                    let mut guard = guard;
                    let (tx, rx) = futures::channel::mpsc::unbounded::<()>();
                    match guard.listen_for_activation(move || {
                        let _ = tx.unbounded_send(());
                    }) {
                        Ok(()) => activation_rx = Some(rx),
                        Err(err) => {
                            tracing::warn!(%err, "没能监听激活请求，再次启动时叫不醒这个窗口")
                        }
                    }
                    guard
                };
                if let Ok(mut slot) = INSTANCE.lock() {
                    *slot = Some(guard);
                }
                // 免安装版：上次自更新留下的 `*.old`（运行中的旧主程序当时删不掉）在这里清掉。放后台线程，不拖慢启动；
                // 拿到单实例锁之后才做，旧进程一定已经退出了。
                if let Some(layout) = portable::layout() {
                    std::thread::spawn(move || {
                        let removed = zssh_update::portable::cleanup_leftovers(&layout);
                        if !removed.is_empty() {
                            tracing::info!(count = removed.len(), "已清理上次更新留下的旧文件");
                        }
                    });
                }
            }
            None => {
                #[cfg(windows)]
                single_instance::focus_existing(&window_prop);
                #[cfg(unix)]
                single_instance::activate_existing(&instance_name);
                // 还没建窗口和托盘，直接退出；cx.quit() 在空事件循环里不一定收得住。
                std::process::exit(0);
            }
        }

        // 退出后安装：更新窗口只把安装计划排进进程级队列，真正启动安装器放在
        // 退出回调里。订阅一被丢弃就会注销，所以这里 forget 让它活到进程结束；
        // 进程退出时本来也不需要再回收这点内存。
        std::mem::forget(cx.on_app_quit(|_| async {
            // 会话日志的写线程各自在收尾；这里有限等一下（最多 2 秒）让排着的写完，超时 / 写失败
            // 记进设置，下次打开设置里的会话日志时看得到。
            app_prefs::finish_session_logs_at_exit();
            update::run_pending_install();
            // 设置要求时，一并关掉本次运行拉起的 X 服务器（用户自己开的不动）。
            x11_glue::shutdown_owned();
            // 从 Dock 右键「退出」、注销关机这类退出不经过主窗口的退出流程，也要把智能体的共用进程
            // （按进程组）收掉；走过 `finalize_quit` 的再调一次只是空转。各平台都一样。
            agent_host::shutdown_global();
        }));

        // 自动同步的调度器：一个进程一个，和同步设置窗口的开关无关。
        sync::spawn_auto_sync(cx);
        // 后台自动检查更新：启动后延迟一会儿查一次，之后每 6 小时一次（设置里可关）。
        update_auto::spawn(cx);
        // 底部同步状态条的数据：每 2 秒读一次本机的同步设置（0.2.14 的 `SyncStatusBar`）。
        sync::spawn_status_refresher(cx);

        // 调试用：往（独立的）数据目录里写种子主机，必须在主窗口读主机库之前。
        // dev_seed 整个模块只编进 debug 构建，入口一起门掉。
        #[cfg(debug_assertions)]
        {
            if let Err(err) = main_window::dev_seed::seed_from_env() {
                tracing::warn!(%err, "写种子主机失败");
            }
        }

        // 无边框窗口：标题栏自己画（`TitleBar::title_bar_options` 让系统不画标题栏）。
        let window_options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                // 标题栏自己画，但系统标题（任务栏、Alt+Tab）要有名字。
                title: Some(product::NAME.into()),
                ..TitleBar::title_bar_options()
            }),
            window_bounds: Some(
                window_state::restore_main(
                    &window_state::WindowState::load(),
                    &window_state::display_rects(cx),
                )
                .unwrap_or_else(|| WindowBounds::centered(size(px(1180.), px(780.)), cx)),
            ),
            window_min_size: Some(size(
                px(window_state::MAIN_MIN.0),
                px(window_state::MAIN_MIN.1),
            )),
            ..window_frame::client_options()
        };

        let opened = gpui_kit::open_window(window_options, cx, |window, cx| {
            // 无边框窗口的系统标题默认是空的：任务栏、Alt+Tab 里看不到名字，这里设成产品名。
            window.set_window_title(product::NAME);
            let view = cx.new(|cx| MainWindow::new(window, cx));
            window.focus(&view.read(cx).focus_handle(), cx);
            #[cfg(windows)]
            if let Ok(handle) = window.window_handle()
                && let RawWindowHandle::Win32(win32) = handle.as_raw()
            {
                single_instance::set_window_prop(win32.hwnd.get(), &window_prop);
            }
            #[cfg(not(windows))]
            let _ = &window_prop;
            // 关闭按钮收进托盘，进程和后台任务继续跑；托盘没建起来时没有入口能叫回窗口，
            // 这时必须真正退出。两种情况都记住窗口位置和布局。
            let close_view = view.clone();
            window_frame::on_should_close(window, cx, move |window, cx| {
                match tray::close_action() {
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
                        close_view.update(cx, |this, cx| this.finalize_quit(window, cx));
                        cx.quit();
                        true
                    }
                }
            });
            view
        });
        let (handle, view) = match opened {
            Ok(pair) => pair,
            Err(err) => {
                // 没有主窗口就没有任何入口，留着进程也没用；记一条日志后退出。
                tracing::error!(%err, "打开主窗口失败");
                cx.quit();
                return;
            }
        };

        // 文件窗口把标签交回主窗口时要找到它。
        file_window::register_main(cx, handle, view.downgrade());

        // macOS：应用菜单（关于 / 设置 / 隐藏 / 退出）和 Cmd+Q 等应用级快捷键，任何窗口里都走主窗口的流程。
        #[cfg(target_os = "macos")]
        app_menu::install(cx, handle, view.clone());

        tray::spawn(cx, handle, view.clone());

        // 跟随系统主题：Linux 上 GPUI 在后台读 xdg-desktop-portal 的 color-scheme，系统切换明暗时窗口的外观
        // 随之变化，这里立刻对齐一次（定时轮询仍在，作后备，也负责启动时门户的第一次回答）。
        #[cfg(not(windows))]
        let _ = handle.update(cx, |_, window, _| {
            window
                .observe_window_appearance(|_window, cx| {
                    settings_store::AppSettings::global(cx)
                        .update(cx, |settings, cx| settings.sync_system_theme(cx));
                })
                .detach();
        });

        // 再次启动 ZeeTerm 时把这个窗口叫到前台（在界面线程上做；窗口收起 / 最小化时由
        // `activate_window` 请求合成器 / 窗口管理器恢复）。
        #[cfg(unix)]
        if let Some(mut rx) = activation_rx {
            use futures::StreamExt as _;
            cx.spawn(async move |cx| {
                while rx.next().await.is_some() {
                    let _ = handle.update(cx, |_, window, _| tray::bring_to_front(window));
                }
            })
            .detach();
        }

        // 启动隐藏：等窗口真正显示后再收进托盘，否则会被平台的首次显示覆盖。
        if settings_store::snapshot(cx).start_hidden {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                // Linux 的托盘要先在 D-Bus 上登记好才算可用；等不到（没有托盘宿主）时 `tray::hide`
                // 什么都不做，窗口留在屏幕上，不会让用户找不到它。
                #[cfg(not(windows))]
                for _ in 0..16 {
                    if tray::available() {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                }
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
    // 驱动脚本整套（dev_script + dev_acts_*）只编进 debug 构建。
    #[cfg(debug_assertions)]
    main_window::dev_script::spawn_from_env(view.clone(), handle, cx);

    // 手动验证用：再开旧的终端演示窗口（原来的主窗口），它带着原来那一批演示钩子。
    // 演示窗口（terminal_demo）只编进 debug 构建。
    #[cfg(debug_assertions)]
    {
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
    }

    // 手动验证用：直接打开主机库窗口。
    if dev_hooks::var_os("ZSSH_HOSTS").is_some() {
        hosts::open_hosts_window(cx);
    }

    // 手动/截图验证用：带一个主机库 id 时启动就开 SFTP 浏览窗口。
    if let Some(raw) = dev_hooks::var("ZSSH_SFTP_HOST")
        && let Ok(host_id) = raw.trim().parse::<i64>()
    {
        sftp::open_sftp_window(cx, host_id);
    }

    // 手动/截图验证用：带这个环境变量启动时再开一个独立终端窗口（terminal_demo 只编进 debug）。
    #[cfg(debug_assertions)]
    {
        if dev_hooks::var_os("ZSSH_EXTRA_TERMINAL").is_some() {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
                ..crate::window_frame::app_options()
            };
            let _ = cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| TerminalDemo::new(window, cx, "terminal-1".to_owned()));
                window.focus(&view.read(cx).focus_handle(), cx);
                view
            });
        }

        // 手动/截图验证用：带这个环境变量启动时直接开一个编辑器窗口（editor_demo 只编进 debug）。
        if dev_hooks::var_os("ZSSH_OPEN_EDITOR").is_some() {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(900.), px(620.)), cx)),
                ..crate::window_frame::app_options()
            };
            let _ = cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| editor_demo::EditorDemo::new(window, cx));
                window.focus(&view.read(cx).focus_handle(), cx);
                view
            });
        }
    }

    // 手动验证用：带这个环境变量启动时直接打开更新检查窗口。
    if dev_hooks::var_os("ZSSH_UPDATE").is_some() {
        update::open_update_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开备份与恢复窗口。
    if dev_hooks::var_os("ZSSH_BACKUP").is_some() {
        backup::open_backup_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开同步设置窗口。
    if dev_hooks::var_os("ZSSH_SYNC").is_some() {
        sync::open_sync_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开服务器状态窗口。
    if dev_hooks::var_os("ZSSH_STATUS").is_some() {
        status::open_status_window(cx);
    }

    // 手动验证用：带这个环境变量启动时直接打开 AI 智能体窗口。
    if dev_hooks::var_os("ZSSH_AGENTS").is_some() {
        agents::open_agents_window(cx);
    }

    // 手动验证用（只在调试版）：带 ZSSH_AGENT_CHAT=1 启动时直接打开 AI 对话窗口；
    // ZSSH_AGENT_DEMO=1 时窗口从离线脚本演示开始（产品默认是真实智能体）。
    if dev_hooks::var_os("ZSSH_AGENT_CHAT").is_some()
        || dev_hooks::var_os("ZSSH_AGENT_DEMO").is_some()
    {
        agent_chat::open_agent_chat_window(cx);
    }
}

/// 旧的终端演示窗口（原来的主窗口）：只在 `ZSSH_DEMO_TERMINAL` 钩子下打开，
/// 带着它原来的那一批演示钩子（预开标签、分屏、鼠标上报、高亮、设置面板）。
#[cfg(debug_assertions)]
fn open_demo_terminal(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
        ..crate::window_frame::app_options()
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
