//! V2-U59：「从 SSH 配置导入」对话框的窗口测试：读一份自己造的配置（临时目录里，绝不碰真实的 `~/.ssh`）→
//! 预览 → 勾选 → 导入，以及「没有找到文件」和「全部取消勾选」的状态。
//!
//! 要一个独立的数据目录（`MainWindow::new` 会打开主机库）：没设 `ZEETERM_DATA_DIR` 就跳过并打印原因。
//! 主机别名和地址里带一个本次运行独有的记号，数据目录里留着上一次的主机也不会互相当成「已存在」。

use ::gpui::{AppContext as _, TestAppContext, WindowHandle};

use super::MainWindow;
use super::modal::Modal;
use super::ssh_import::ImportState;
use crate::settings_store::AppSettings;

fn open(cx: &mut TestAppContext) -> Option<WindowHandle<MainWindow>> {
    if std::env::var_os("ZEETERM_DATA_DIR").is_none() {
        eprintln!("跳过 SSH 配置导入的窗口测试：需要 ZEETERM_DATA_DIR 指向独立数据目录");
        return None;
    }
    cx.update(|cx| {
        gpui_kit::init(cx);
        AppSettings::init_for_test(cx);
    });
    let window = cx.add_window(MainWindow::new);
    // 主机库换成私有的内存库：并行的窗口测试共用数据目录里的库，同时迁移会互相踩，这里也不该往它里面写东西。
    window
        .update(cx, |main, _, _| {
            main.store = super::host_store::HostStore::in_memory()
        })
        .expect("换库");
    Some(window)
}

fn draw(cx: &mut TestAppContext, window: WindowHandle<MainWindow>) {
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("draw");
}

/// 本次运行独有的记号。
fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

fn load(
    cx: &mut TestAppContext,
    window: WindowHandle<MainWindow>,
    path: &std::path::Path,
) -> ImportSummary {
    let path = path.to_path_buf();
    window
        .update(cx, |main, _, cx| {
            let options = zeeterm_core::ssh_config::custom_config_location(&path);
            main.load_ssh_config(path.clone(), options, true, cx);
        })
        .expect("load");
    draw(cx, window);
    window
        .update(cx, |main, _, _| match &main.modal {
            Some(Modal::SshImport(dialog)) => match &dialog.state {
                ImportState::Ready(preview) => ImportSummary::Ready {
                    aliases: preview.rows.iter().map(|row| row.alias.clone()).collect(),
                    selected: preview.selected.clone(),
                },
                ImportState::Missing => ImportSummary::Missing,
                _ => ImportSummary::Other,
            },
            _ => ImportSummary::Closed,
        })
        .expect("state")
}

#[derive(Debug, PartialEq, Eq)]
enum ImportSummary {
    Ready {
        aliases: Vec<String>,
        selected: Vec<bool>,
    },
    Missing,
    Other,
    Closed,
}

#[gpui::test]
fn the_preview_lists_the_hosts_and_importing_creates_the_checked_ones(cx: &mut TestAppContext) {
    let Some(window) = open(cx) else {
        return;
    };
    let token = token();
    let dir = tempfile::Builder::new()
        .prefix("zeeterm-u59-")
        .tempdir()
        .expect("临时目录");
    let config = dir.path().join("config");
    std::fs::write(
        &config,
        format!(
            "Host *\n  User ops\n\nHost web-{token}\n  HostName web-{token}.example.com\n  IdentityFile keys/web_key\n  \
             ProxyJump gate-{token}\n  ForwardAgent yes\n\nHost gate-{token}\n  HostName gate-{token}.example.net\n  Port 2222\n\
             \nMatch host nothing\n  User ignored\n\nHost bad-{token}\n  Port 99999\n"
        ),
    )
    .expect("写配置");

    let summary = load(cx, window, &config);
    assert_eq!(
        summary,
        ImportSummary::Ready {
            aliases: vec![format!("web-{token}"), format!("gate-{token}")],
            selected: vec![true, true],
        },
        "两台都能导入、默认全勾；端口不合法的那台被跳过（列在底部）"
    );

    // 取消勾选再勾回来，计数跟着走。
    window
        .update(cx, |main, _, cx| {
            main.ssh_import_toggle(0, false, cx);
        })
        .expect("toggle");
    draw(cx, window);
    window
        .update(cx, |main, _, cx| {
            main.ssh_import_toggle(0, true, cx);
        })
        .expect("toggle");

    window
        .update(cx, |main, window, cx| main.ssh_import_confirm(window, cx))
        .expect("confirm");
    draw(cx, window);
    window
        .update(cx, |main, _, _| {
            assert!(main.modal.is_none(), "导入成功后关掉对话框");
            assert!(
                main.test_notices
                    .iter()
                    .any(|text| text == "已导入 2 台主机"),
                "要弹出「已导入 N 台主机」：{:?}",
                main.test_notices
            );
            let hosts = main.store.hosts();
            let web = hosts
                .iter()
                .find(|host| host.name == format!("web-{token}"))
                .expect("web 已建");
            let gate = hosts
                .iter()
                .find(|host| host.name == format!("gate-{token}"))
                .expect("gate 已建");
            assert_eq!(web.username, "ops", "Host * 里的 User 成了默认值");
            assert_eq!(web.jump_host_id, Some(gate.id), "跳板机先建后挂");
            assert_eq!(gate.port, 2222);
            assert_eq!(web.auth_method, zeeterm_core::model::AuthMethod::Key);
            let identity = main
                .store
                .identities()
                .iter()
                .find(|item| Some(item.id) == web.identity_id)
                .expect("私钥凭据已建");
            assert_eq!(identity.name, "web_key");
            assert!(!identity.has_secret, "只记路径，没有读私钥");
        })
        .expect("check");
}

#[gpui::test]
fn a_missing_file_shows_the_not_found_state_and_nothing_can_be_imported(cx: &mut TestAppContext) {
    let Some(window) = open(cx) else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("zeeterm-u59-")
        .tempdir()
        .expect("临时目录");
    let summary = load(cx, window, &dir.path().join("config"));
    assert_eq!(summary, ImportSummary::Missing);
    // 没有预览时点「导入」什么都不会发生。
    let before = window
        .update(cx, |main, _, _| main.store.hosts().len())
        .expect("count");
    window
        .update(cx, |main, window, cx| main.ssh_import_confirm(window, cx))
        .expect("confirm");
    window
        .update(cx, |main, _, _| {
            assert_eq!(main.store.hosts().len(), before);
            assert!(matches!(main.modal, Some(Modal::SshImport(_))));
        })
        .expect("check");
}

#[gpui::test]
fn existing_hosts_are_unchecked_by_default_and_nothing_selected_cannot_import(
    cx: &mut TestAppContext,
) {
    let Some(window) = open(cx) else {
        return;
    };
    let token = token();
    let dir = tempfile::Builder::new()
        .prefix("zeeterm-u59-")
        .tempdir()
        .expect("临时目录");
    let config = dir.path().join("config");
    std::fs::write(
        &config,
        format!("Host dup-{token}\n  HostName dup-{token}.example.com\n  User ops\n"),
    )
    .expect("写配置");
    window
        .update(cx, |main, _, _| {
            main.store
                .save_host(&zeeterm_core::model::HostDraft {
                    id: None,
                    group_id: None,
                    name: "already here".into(),
                    host: format!("dup-{token}.example.com"),
                    port: 22,
                    username: "ops".into(),
                    auth_method: zeeterm_core::model::AuthMethod::Password,
                    identity_id: None,
                    proxy_id: None,
                    jump_host_id: None,
                    proxy_mode: zeeterm_core::model::ProxyMode::Default,
                    tags: Vec::new(),
                    sort: 0,
                    legacy_algorithms: false,
                    protocol: None,
                })
                .expect("先建一台");
        })
        .expect("seed");
    let summary = load(cx, window, &config);
    assert_eq!(
        summary,
        ImportSummary::Ready {
            aliases: vec![format!("dup-{token}")],
            selected: vec![false],
        },
        "地址、端口、用户名都相同：默认不勾"
    );
    let count = window
        .update(cx, |main, _, _| main.store.hosts().len())
        .expect("count");
    window
        .update(cx, |main, window, cx| main.ssh_import_confirm(window, cx))
        .expect("confirm");
    window
        .update(cx, |main, _, _| {
            assert_eq!(main.store.hosts().len(), count, "一台都没勾，导入不动库");
            assert!(matches!(main.modal, Some(Modal::SshImport(_))));
        })
        .expect("check");
}
