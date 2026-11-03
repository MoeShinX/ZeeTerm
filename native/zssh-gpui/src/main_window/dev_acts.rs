//! 调试驱动（`ZSSH_MAIN_SCRIPT` 里的 `act:` 步骤）能调的主机页 / 面板 / 对话框动作。只在 debug 构建里
//! 通过 `dev_hooks` 读到脚本，release 里这条路径不存在。
//!
//! 写法 `act:<动作>[:参数…]`。主机、分组可以用名字引用：`@web-1`（主机）、`#生产环境`（分组），
//! `none` 是没有（未分组 / 顶层）。动作都走和界面按钮同一批方法；`assert_*` 检查结果，不对就让
//! 脚本失败。日志里只写动作名和结果，不写字段内容。

use ::gpui::Window;
use gpui_kit::*;
use zssh_core::model::{AuthMethod, IdentityKind, ProxyKind, ProxyMode};
use zssh_core::store::forwards::ForwardKind;

use super::MainWindow;
use super::dev_script::Outcome;
use super::drag::DropHint;
use super::modal::{ConfirmAction, Modal};
use super::page_logic::{PageOptions, ViewMode, page_hosts};

const KNOWN: &[&str] = &[
    "new_host",
    "select_host",
    "edit_host",
    "panel",
    "close_panel",
    "set",
    "host_group",
    "host_auth",
    "host_identity",
    "host_proxy_mode",
    "host_proxy",
    "host_jump",
    "host_legacy",
    "host_save",
    "ask_delete_host",
    "confirm",
    "modal_cancel",
    "new_group",
    "rename_group",
    "ask_delete_group",
    "adopt_ungrouped",
    "move_host",
    "drop_host",
    "step_host",
    "step_group",
    "reset_order",
    "view_mode",
    "density",
    "sort",
    "search",
    "vault_enter",
    "select_group",
    "keychain_new",
    "keychain_pick",
    "keychain_kind",
    "keychain_save",
    "keychain_ask_delete",
    "proxy_new",
    "proxy_pick",
    "proxy_save",
    "proxy_ask_delete",
    "proxy_default",
    "forward_new",
    "forward_edit",
    "forward_kind",
    "forward_save",
    "forward_toggle",
    "forward_ask_delete",
    "generate_key",
    "install_key",
    "assert_order",
    "assert_host_group",
    "assert_groups",
    "assert_panel",
    "assert_modal",
    "assert_forward",
    "wait_forward",
    "assert_form",
    "assert_count",
    "wait_count",
    "forward_auto",
    "host_reveal",
    "set_memory",
    "memory_save",
    "memory_clear",
    "assert_memory",
    "assert_memory_hides",
    "assert_secret",
    "assert_layout",
    "assert_hint",
    "assert_error_free",
    "assert_host_field",
    "assert_panel_width",
    "assert_info",
    "assert_tree_expanded",
    "tcp_expect",
    "tcp_refused",
];

/// 解析脚本时只检查动作名认不认识，参数在执行时检查。
pub fn validate(spec: &str) -> Result<(), String> {
    let name = spec.split(':').next().unwrap_or("");
    if super::dev_acts_dock::is_known(name)
        || super::dev_acts_edit::is_known(name)
        || super::dev_acts_agent::is_known(name)
        || super::dev_acts_a4::is_known(name)
        || super::dev_acts_a5::is_known(name)
        || super::dev_acts_about::is_known(name)
        || super::dev_acts_pane::is_known(name)
        || super::dev_acts_u1::is_known(name)
        || super::dev_acts_u2::is_known(name)
        || super::dev_acts_u4::is_known(name)
        || super::dev_acts_u6::is_known(name)
        || super::dev_acts_u7::is_known(name)
        || super::dev_acts_u12::is_known(name)
        || super::dev_acts_u13::is_known(name)
        || super::dev_acts_u18::is_known(name)
        || super::dev_acts_u19::is_known(name)
        || super::dev_acts_u20::is_known(name)
        || super::dev_acts_n4::is_known(name)
        || super::dev_acts_sync::is_known(name)
        || KNOWN.contains(&name)
    {
        Ok(())
    } else {
        Err(format!("不认识的动作：{name}"))
    }
}

fn fail<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

/// `@名字` 或数字 → 主机 id。
fn host_ref(this: &MainWindow, raw: &str) -> Result<i64, String> {
    if let Some(name) = raw.strip_prefix('@') {
        return this
            .store
            .hosts()
            .iter()
            .find(|h| h.name == name)
            .map(|h| h.id)
            .ok_or_else(|| "没有这个名字的主机".to_owned());
    }
    raw.parse::<i64>()
        .map_err(|_| "主机引用不是数字也不是 @名字".to_owned())
}

/// `#名字` / 数字 / `none` → 分组 id。
fn group_ref(this: &MainWindow, raw: &str) -> Result<Option<i64>, String> {
    if raw == "none" {
        return Ok(None);
    }
    if let Some(name) = raw.strip_prefix('#') {
        return this
            .store
            .groups()
            .iter()
            .find(|g| g.name == name)
            .map(|g| Some(g.id))
            .ok_or_else(|| "没有这个名字的分组".to_owned());
    }
    raw.parse::<i64>()
        .map(Some)
        .map_err(|_| "分组引用不是数字也不是 #名字".to_owned())
}

fn state_name(this: &MainWindow) -> &'static str {
    use crate::settings_nav::SettingsSection;
    // 凭据 / 代理 / 端口转发 / 快捷命令现在是「管理 + 设置」页的分类：页面上显示着就算「开着」，
    // 旧脚本里的 `assert_panel:keychain` 之类断言仍然有意义。
    if this.nav.view == super::nav::MainView::Settings {
        match this.settings_section {
            SettingsSection::Credentials => return "keychain",
            SettingsSection::Proxies => return "proxies",
            SettingsSection::Forwards => return "forwards",
            SettingsSection::QuickCommands => return "quick",
            _ => {}
        }
    }
    match this.panels.visible() {
        Some(_) => "other",
        None => "none",
    }
}

pub(super) fn modal_name(this: &MainWindow) -> &'static str {
    match &this.modal {
        None => "none",
        Some(Modal::Confirm(_)) => "confirm",
        Some(Modal::Group(_)) => "group",
        Some(Modal::Quick(_)) => "quick",
        Some(Modal::QuickTarget(_)) => "quick_target",
        Some(Modal::QuickEdit(_)) => "quick_edit",
        Some(Modal::HostProps(_)) => "host_props",
        Some(Modal::GenerateKey(_)) => "generate",
        Some(Modal::InstallKey(_)) => "install",
        Some(Modal::Unsaved(_)) => "unsaved",
        Some(Modal::Recovery(_)) => "recovery",
        Some(Modal::X11Needed(_)) => "x11",
        Some(Modal::Connections(_)) => "connections",
        Some(Modal::Processes(_)) => "processes",
        Some(Modal::AiEndpoint(_)) => "ai_endpoint",
        Some(Modal::AiPresets) => "ai_presets",
        Some(Modal::Tokenway(_)) => "tokenway",
    }
}

fn set_input(
    state: &::gpui::Entity<gpui_kit::component::input::InputState>,
    value: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) {
    let value = value.to_owned();
    state.update(cx, |input, cx| input.set_value(value, window, cx));
}

pub(super) fn run(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Outcome {
    match run_inner(this, spec, window, cx) {
        Ok(outcome) => outcome,
        Err(reason) => Outcome::Failed(format!(
            "{}：{reason}",
            spec.split(':').next().unwrap_or("")
        )),
    }
}

fn run_inner(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    // 编辑器 / 恢复提示 / 外部编辑的动作在 `dev_acts_edit`。
    if let Some(result) = super::dev_acts_edit::run(this, spec, window, cx) {
        return result;
    }
    // AI 智能体（面板里换智能体 / 模型 / 强度，设置页的路径和下载）的动作在 `dev_acts_agent`。
    if let Some(result) = super::dev_acts_agent::run(this, spec, window, cx) {
        return result;
    }
    // V2-A4（设置页内置助手接口管理、添加 / 编辑接口对话框）的动作在 `dev_acts_a4`。
    if let Some(result) = super::dev_acts_a4::run(this, spec, window, cx) {
        return result;
    }
    // V2-A5（Tokenway 一键连接：预设列表、连接对话框、假 Tokenway）的动作在 `dev_acts_a5`。
    if let Some(result) = super::dev_acts_a5::run(this, spec, window, cx) {
        return result;
    }
    // 设置页分类导航、连接 / SFTP / 窗口 / 高亮 / 会话日志设置的动作在 `dev_acts_u2`。
    if let Some(result) = super::dev_acts_u2::run(this, spec, window, cx) {
        return result;
    }
    // V2-U4（字体与排版）的动作在 `dev_acts_u4`。
    if let Some(result) = super::dev_acts_u4::run(this, spec, window, cx) {
        return result;
    }
    // V2-U6（标题栏精简）的动作在 `dev_acts_u6`。
    if let Some(result) = super::dev_acts_u6::run(this, spec, window, cx) {
        return result;
    }
    // V2-U19（从 Z-SSH 迁移）的动作在 `dev_acts_u19`。
    if let Some(result) = super::dev_acts_u19::run(this, spec, window, cx) {
        return result;
    }
    // V2-U20（主机的系统图标、主机树连接线）的动作在 `dev_acts_u20`。
    if let Some(result) = super::dev_acts_u20::run(this, spec, window, cx) {
        return result;
    }
    // V2-U13（主机属性单栏表单、测试连接）的动作在 `dev_acts_u13`。
    if let Some(result) = super::dev_acts_u13::run(this, spec, window, cx) {
        return result;
    }
    // V2-U18（进程管理对话框）的动作在 `dev_acts_u18`。
    if let Some(result) = super::dev_acts_u18::run(this, spec, window, cx) {
        return result;
    }
    // V2-U12（概览面板调宽、所有连接对话框）的动作在 `dev_acts_u12`。
    if let Some(result) = super::dev_acts_u12::run(this, spec, window, cx) {
        return result;
    }
    // V2-U7（主机属性对话框）的动作在 `dev_acts_u7`。
    if let Some(result) = super::dev_acts_u7::run(this, spec, window, cx) {
        return result;
    }
    // V2-N4（主机协议的界面、X11）的动作在 `dev_acts_n4`。
    if let Some(result) = super::dev_acts_n4::run(this, spec, window, cx) {
        return result;
    }
    // 设置页「关于」一节的动作在 `dev_acts_about`。
    if let Some(result) = super::dev_acts_about::run(this, spec, window, cx) {
        return result;
    }
    // 分屏窗格 / 回滚行数 / 外观设置的动作在 `dev_acts_pane`。
    if let Some(result) = super::dev_acts_pane::run(this, spec, window, cx) {
        return result;
    }
    // V2-U1（用户手测反馈）的动作在 `dev_acts_u1`。
    if let Some(result) = super::dev_acts_u1::run(this, spec, window, cx) {
        return result;
    }
    // 同步 / 备份 / 服务器状态的动作在 `dev_acts_sync`。
    if let Some(result) = super::dev_acts_sync::run(this, spec, window, cx) {
        return result;
    }
    // 命令栏 / 撰写栏 / AI / 文件面板的动作在 `dev_acts_dock`。
    if let Some(result) = super::dev_acts_dock::run(this, spec, window, cx) {
        return result;
    }
    // 参数里可能有 `:`（比如要填进输入框的文字），只拆前两段。
    let mut parts = spec.splitn(3, ':');
    let name = parts.next().unwrap_or("");
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or("");
    let done = Ok(Outcome::Done);
    match name {
        "new_host" => {
            let group = if a.is_empty() {
                None
            } else {
                group_ref(this, a)?
            };
            this.open_host_props(None, group, window, cx);
        }
        "select_host" => {
            let id = host_ref(this, a)?;
            this.page_click_host(id, cx);
        }
        "edit_host" => {
            let id = host_ref(this, a)?;
            this.edit_host(id, window, cx);
        }
        "panel" => match a {
            "keychain" => this.open_settings_section(
                crate::settings_nav::SettingsSection::Credentials,
                window,
                cx,
            ),
            "proxies" => this.open_settings_section(
                crate::settings_nav::SettingsSection::Proxies,
                window,
                cx,
            ),
            "forwards" => {
                let host = if b.is_empty() || b == "all" {
                    None
                } else {
                    Some(host_ref(this, b)?)
                };
                this.open_forwards(host, window, cx);
            }
            _ => return fail("面板只能是 keychain / proxies / forwards"),
        },
        "close_panel" => this.close_panel(window, cx),
        "set" => {
            // set:<表单.字段>:<值>
            let (form, field) = a.split_once('.').ok_or("写成 set:表单.字段:值")?;
            let target = match (form, field) {
                ("host", "name") => this.host_form.as_ref().map(|f| f.name.clone()),
                ("host", "address") => this.host_form.as_ref().map(|f| f.address.clone()),
                ("host", "port") => this.host_form.as_ref().map(|f| f.port.clone()),
                ("host", "username") => this.host_form.as_ref().map(|f| f.username.clone()),
                ("host", "password") => this.host_form.as_ref().map(|f| f.password.clone()),
                ("keychain", "name") => Some(this.keychain.name.clone()),
                ("keychain", "key_path") => Some(this.keychain.key_path.clone()),
                ("keychain", "secret") => Some(this.keychain.secret.clone()),
                ("keychain", "passphrase") => Some(this.keychain.passphrase.clone()),
                ("proxy", "name") => Some(this.proxy_ui.name.clone()),
                ("proxy", "host") => Some(this.proxy_ui.host.clone()),
                ("proxy", "port") => Some(this.proxy_ui.port.clone()),
                ("proxy", "username") => Some(this.proxy_ui.username.clone()),
                ("proxy", "secret") => Some(this.proxy_ui.secret.clone()),
                ("forward", "name") => this.forward_ui.draft.as_ref().map(|d| d.name.clone()),
                ("forward", "bind_host") => {
                    this.forward_ui.draft.as_ref().map(|d| d.bind_host.clone())
                }
                ("forward", "bind_port") => {
                    this.forward_ui.draft.as_ref().map(|d| d.bind_port.clone())
                }
                ("forward", "dest_host") => {
                    this.forward_ui.draft.as_ref().map(|d| d.dest_host.clone())
                }
                ("forward", "dest_port") => {
                    this.forward_ui.draft.as_ref().map(|d| d.dest_port.clone())
                }
                ("group", "name") => match &this.modal {
                    Some(Modal::Group(dialog)) => Some(dialog.name.clone()),
                    _ => None,
                },
                ("quick_target", "text") => Some(this.quick_target.clone()),
                ("manage", "search") => Some(this.manage_search.clone()),
                ("quick", "password") => match &this.modal {
                    Some(Modal::Quick(dialog)) => Some(dialog.password.clone()),
                    _ => None,
                },
                _ => return fail("不认识的字段"),
            };
            let Some(state) = target else {
                return fail("这个表单现在没开着");
            };
            set_input(&state, b, window, cx);
        }
        "host_group" => {
            let group = group_ref(this, a)?;
            if let Some(form) = &mut this.host_form {
                form.group = group;
            }
        }
        "host_auth" => {
            let method = match a {
                "password" => AuthMethod::Password,
                "key" => AuthMethod::Key,
                "agent" => AuthMethod::Agent,
                _ => return fail("认证方式只能是 password / key / agent"),
            };
            this.host_set_auth(method, window, cx);
        }
        "host_identity" => {
            let id = if a == "none" {
                None
            } else {
                Some(a.parse::<i64>().map_err(|_| "凭据 id 不是数字")?)
            };
            this.host_set_identity(id, window, cx);
        }
        "host_proxy_mode" => {
            let mode = match a {
                "default" => ProxyMode::Default,
                "none" => ProxyMode::None,
                "custom" => ProxyMode::Custom,
                _ => return fail("代理模式只能是 default / none / custom"),
            };
            if let Some(form) = &mut this.host_form {
                form.proxy_mode = mode;
            }
        }
        "host_proxy" => {
            let id = if a == "none" {
                None
            } else {
                Some(a.parse::<i64>().map_err(|_| "代理 id 不是数字")?)
            };
            if let Some(form) = &mut this.host_form {
                form.proxy = id;
            }
        }
        "host_jump" => {
            let id = if a == "none" {
                None
            } else {
                Some(host_ref(this, a)?)
            };
            if let Some(form) = &mut this.host_form {
                form.jump = id;
            }
        }
        "host_legacy" => {
            if let Some(form) = &mut this.host_form {
                form.legacy = a == "on";
            }
        }
        "host_save" => this.host_form_save(window, cx),
        "host_reveal" => this.host_toggle_password(window, cx),
        "set_memory" => {
            let input = this
                .host_form
                .as_ref()
                .and_then(|f| f.memory.as_ref())
                .map(|m| m.input.clone())
                .ok_or("这台主机没有记忆小节")?;
            let text = if b.is_empty() {
                a.to_owned()
            } else {
                format!("{a}:{b}")
            };
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        "memory_save" => this.host_memory_save(window, cx, false),
        "memory_clear" => this.host_memory_save(window, cx, true),
        "assert_memory" => {
            let id = host_ref(this, a)?;
            let stored = this
                .store
                .db()
                .and_then(|db| db.host_memory_get(id).map_err(|e| e.to_string()))?
                .map(|m| m.content)
                .unwrap_or_default();
            if stored != b {
                return fail("库里的记忆和预期不一样");
            }
        }
        "assert_memory_hides" => {
            // assert_memory_hides:<主机>:<不该出现的文字>；同时要求记忆不是空的
            let id = host_ref(this, a)?;
            let stored = this
                .store
                .db()
                .and_then(|db| db.host_memory_get(id).map_err(|e| e.to_string()))?
                .map(|m| m.content)
                .unwrap_or_default();
            if stored.is_empty() {
                return fail("记忆是空的");
            }
            if stored.contains(b) {
                return fail("记忆里还留着不该出现的文字");
            }
        }
        "assert_secret" => {
            // assert_secret:<凭据 id>:<密码>（只检查密码类凭据）
            let id = a.parse::<i64>().map_err(|_| "凭据 id 不是数字")?;
            let stored = this
                .store
                .db()
                .and_then(|db| db.identity_saved_password(id).map_err(|e| e.to_string()))?;
            if stored.as_str() != b {
                return fail("保险库里的密码和预期不一样");
            }
        }
        "ask_delete_host" => {
            let id = host_ref(this, a)?;
            this.ask_delete_host(id, cx);
        }
        "confirm" => this.confirm_modal(window, cx),
        "modal_cancel" => this.close_modal(window, cx),
        "new_group" => {
            let parent = if a.is_empty() {
                None
            } else {
                group_ref(this, a)?
            };
            this.new_group(parent, window, cx);
        }
        "rename_group" => {
            let id = group_ref(this, a)?.ok_or("要给出分组")?;
            this.rename_group(id, window, cx);
        }
        "ask_delete_group" => {
            let id = group_ref(this, a)?.ok_or("要给出分组")?;
            this.ask_delete_group(id, cx);
        }
        "adopt_ungrouped" => this.adopt_ungrouped(window, cx),
        "move_host" => {
            let id = host_ref(this, a)?;
            let group = group_ref(this, b)?;
            this.move_host_ui(id, group, window, cx);
        }
        "drop_host" => {
            // drop_host:<主机>:<目标主机>:<before|after> ——参数里只有三段，目标和位置用逗号分开。
            let id = host_ref(this, a)?;
            let (target, place) = b
                .split_once(',')
                .ok_or("写成 drop_host:主机:目标,before|after")?;
            let target = host_ref(this, target)?;
            this.apply_host_drop(id, target, place == "after", window, cx);
        }
        "step_host" => {
            let id = host_ref(this, a)?;
            this.step_host(
                id,
                b.parse::<i32>().map_err(|_| "步数不是数字")?,
                window,
                cx,
            );
        }
        "step_group" => {
            let id = group_ref(this, a)?.ok_or("要给出分组")?;
            this.step_group(
                id,
                b.parse::<i32>().map_err(|_| "步数不是数字")?,
                window,
                cx,
            );
        }
        "reset_order" => {
            let group = group_ref(this, a)?;
            this.reset_layer_order(group, window, cx);
        }
        "view_mode" => {
            this.set_view_mode(
                if a == "list" {
                    ViewMode::List
                } else {
                    ViewMode::Grid
                },
                window,
                cx,
            );
        }
        "density" => this.toggle_density(window, cx),
        "sort" => this.cycle_sort_mode(window, cx),
        "search" => {
            let query = this.vault_query.clone();
            let text = if b.is_empty() {
                a.to_owned()
            } else {
                format!("{a}:{b}")
            };
            set_input(&query, &text, window, cx);
        }
        "vault_enter" => this.search_enter(window, cx),
        "select_group" => {
            this.tree.selected_group = if a == "all" {
                super::GroupSel::All
            } else {
                match group_ref(this, a)? {
                    Some(id) => super::GroupSel::Group(id),
                    None => super::GroupSel::Ungrouped,
                }
            };
        }
        "keychain_new" => this.keychain_new(window, cx),
        "keychain_pick" => {
            let id = a.parse::<i64>().map_err(|_| "凭据 id 不是数字")?;
            this.keychain_select(id, window, cx);
        }
        "keychain_kind" => {
            this.keychain.kind = match a {
                "password" => IdentityKind::Password,
                "key_file" => IdentityKind::KeyFile,
                "key_inline" => IdentityKind::KeyInline,
                "agent" => IdentityKind::Agent,
                _ => return fail("类型只能是 password / key_file / key_inline / agent"),
            };
        }
        "keychain_save" => this.keychain_save(window, cx),
        "keychain_ask_delete" => this.keychain_ask_delete(cx),
        "proxy_new" => {
            this.open_settings_section(crate::settings_nav::SettingsSection::Proxies, window, cx);
            this.proxy_new(window, cx);
        }
        "proxy_pick" => {
            let id = a.parse::<i64>().map_err(|_| "代理 id 不是数字")?;
            let proxy = this
                .store
                .proxies()
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or("没有这个代理")?;
            this.proxy_pick(&proxy, window, cx);
        }
        "proxy_save" => this.proxy_save(window, cx),
        "proxy_ask_delete" => {
            let id = a.parse::<i64>().map_err(|_| "代理 id 不是数字")?;
            this.proxy_ask_delete(id, cx);
        }
        "proxy_default" => {
            let id = if a == "none" {
                None
            } else {
                Some(a.parse::<i64>().map_err(|_| "代理 id 不是数字")?)
            };
            this.store
                .db()
                .and_then(|db| db.set_default_proxy_id(id).map_err(|e| e.to_string()))?;
        }
        "forward_new" => {
            this.forward_new(window, cx);
        }
        "forward_edit" => {
            let id = a.parse::<i64>().map_err(|_| "规则 id 不是数字")?;
            let rule = this
                .store
                .forwards()
                .iter()
                .find(|r| r.id == id)
                .cloned()
                .ok_or("没有这条规则")?;
            this.forward_edit(&rule, window, cx);
        }
        "forward_auto" => {
            if let Some(draft) = this.forward_ui.draft.as_mut() {
                draft.auto_start = a == "on";
            }
        }
        "forward_kind" => {
            if let Some(draft) = this.forward_ui.draft.as_mut() {
                draft.kind = if a == "remote" {
                    ForwardKind::Remote
                } else {
                    ForwardKind::Local
                };
            }
        }
        "forward_save" => this.forward_save(window, cx),
        "forward_toggle" => {
            let id = a.parse::<i64>().map_err(|_| "规则 id 不是数字")?;
            let rule = this
                .store
                .forwards()
                .iter()
                .find(|r| r.id == id)
                .cloned()
                .ok_or("没有这条规则")?;
            this.forward_toggle(&rule);
        }
        "forward_ask_delete" => this.forward_ask_delete(cx),
        "generate_key" => this.open_generate_key_dialog(window, cx),
        "install_key" => {
            let id = host_ref(this, a)?;
            this.open_install_key_dialog(id, window, cx);
        }
        // ---- 检查 ----
        "assert_order" => {
            let layout = this.store.layout().clone();
            let query = this.vault_query.read(cx).value().to_string();
            let shown = page_hosts(
                this.store.groups(),
                this.store.hosts(),
                &PageOptions {
                    query: &query,
                    selected: this.tree.selected_group,
                    mode: layout.sort_mode,
                    recent: &layout.recent,
                    flat: this.store.flat(),
                },
            );
            let names: Vec<&str> = shown.iter().map(|h| h.name.as_str()).collect();
            let wanted: Vec<&str> = a.split(',').filter(|s| !s.is_empty()).collect();
            if names != wanted {
                return fail(format!("页面顺序是 {names:?}，不是 {wanted:?}"));
            }
        }
        "assert_host_group" => {
            let id = host_ref(this, a)?;
            let wanted = group_ref(this, b)?;
            let actual = this.store.host(id).and_then(|h| h.group_id);
            if actual != wanted {
                return fail(format!("分组是 {actual:?}，不是 {wanted:?}"));
            }
        }
        "assert_groups" => {
            let names = super::page_logic::flatten_groups(this.store.groups())
                .iter()
                .map(|(g, depth)| format!("{}{}", "-".repeat(*depth), g.name))
                .collect::<Vec<_>>();
            let wanted: Vec<String> = a
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if names != wanted {
                return fail(format!("分组是 {names:?}，不是 {wanted:?}"));
            }
        }
        "assert_panel" => {
            let now = state_name(this);
            if now != a {
                return fail(format!("现在显示的面板是 {now}，不是 {a}"));
            }
        }
        "assert_modal" => {
            let now = modal_name(this);
            if now != a {
                return fail(format!("现在的对话框是 {now}，不是 {a}"));
            }
        }
        "assert_forward" | "wait_forward" => {
            let id = a.parse::<i64>().map_err(|_| "规则 id 不是数字")?;
            let status = crate::forward_runtime::manager()
                .statuses()
                .into_iter()
                .find(|s| s.id == id);
            let now = match status.as_ref().map(|s| s.phase) {
                None => "stopped",
                Some(crate::forward_runtime::Phase::Starting) => "starting",
                Some(crate::forward_runtime::Phase::Running) => "running",
                Some(crate::forward_runtime::Phase::Failed) => "failed",
            };
            if now != b {
                if name == "wait_forward" {
                    return Ok(Outcome::Pending);
                }
                return fail(format!("转发状态是 {now}，不是 {b}"));
            }
        }
        "assert_form" => {
            let (form, field) = a.split_once('.').ok_or("写成 assert_form:表单.字段:值")?;
            let value = match (form, field) {
                ("host", "name") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.name.read(cx).value().to_string()),
                ("host", "address") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.address.read(cx).value().to_string()),
                ("host", "port") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.port.read(cx).value().to_string()),
                ("host", "username") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.username.read(cx).value().to_string()),
                ("host", "proxy_mode") => this
                    .host_form
                    .as_ref()
                    .map(|f| format!("{:?}", f.proxy_mode).to_lowercase()),
                ("host", "proxy") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.proxy.map_or("none".to_owned(), |id| id.to_string())),
                ("host", "group") => this.host_form.as_ref().map(|f| {
                    f.group
                        .and_then(|id| this.store.groups().iter().find(|g| g.id == id))
                        .map_or("none".to_owned(), |g| g.name.clone())
                }),
                ("host", "password") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.password.read(cx).value().to_string()),
                ("host", "shown") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.password_shown.to_string()),
                ("host", "auth") => this
                    .host_form
                    .as_ref()
                    .map(|f| format!("{:?}", f.auth).to_lowercase()),
                ("host", "mismatch") => this
                    .host_form
                    .as_ref()
                    .map(|f| f.mismatch_note.is_some().to_string()),
                _ => return fail("不认识的字段"),
            };
            if value.as_deref() != Some(b) {
                return fail("字段的值不对");
            }
        }
        "assert_count" | "wait_count" => {
            let (what, wanted) = (a, b.parse::<usize>().map_err(|_| "个数不是数字")?);
            let now = match what {
                "hosts" => this.store.hosts().len(),
                "groups" => this.store.groups().len(),
                "identities" => this.store.identities().len(),
                "proxies" => this.store.proxies().len(),
                "forwards" => this.store.forwards().len(),
                _ => return fail("只能数 hosts / groups / identities / proxies / forwards"),
            };
            if now != wanted {
                if name == "wait_count" {
                    this.store.reload();
                    return Ok(Outcome::Pending);
                }
                return fail(format!("{what} 有 {now} 个，不是 {wanted}"));
            }
        }
        "assert_layout" => {
            let layout = this.store.layout();
            let now = format!(
                "{}:{}:{}",
                layout.view_mode.as_str(),
                layout.density.as_str(),
                layout.sort_mode.as_str()
            );
            let wanted = if b.is_empty() {
                a.to_owned()
            } else {
                format!("{a}:{b}")
            };
            if now != wanted {
                return fail(format!("布局设置是 {now}，不是 {wanted}"));
            }
        }
        "assert_panel_width" => {
            // assert_panel_width:<最小>:<最大>
            let (min, max) = (
                a.parse::<f32>().map_err(|_| "最小值不是数字")?,
                b.parse::<f32>().map_err(|_| "最大值不是数字")?,
            );
            let now = this.aside_width;
            if now < min || now > max {
                return fail(format!("面板宽度是 {now}，不在 {min}~{max}"));
            }
        }
        "assert_info" => {
            // assert_info:<主机|none>：树底部属性面板显示哪台
            let now = this
                .info_host
                .and_then(|id| this.store.host(id))
                .map(|h| h.name.clone());
            let wanted = if a == "none" {
                None
            } else {
                Some(a.trim_start_matches('@').to_owned())
            };
            if now != wanted {
                return fail(format!("属性面板显示 {now:?}，不是 {wanted:?}"));
            }
        }
        "assert_tree_expanded" => {
            // assert_tree_expanded:<#分组>:<on|off>
            let id = group_ref(this, a)?.ok_or("要给出分组")?;
            let on = this
                .tree
                .expanded
                .contains(&super::host_tree::group_key(Some(id)));
            if on != (b == "on") {
                return fail(format!("展开状态是 {on}"));
            }
        }
        "assert_hint" => {
            let now = match this.drop_hint {
                None => "none",
                Some(DropHint::InsertHost { .. }) => "insert_host",
                Some(DropHint::InsertGroup { .. }) => "insert_group",
                Some(DropHint::IntoGroup(_)) => "into_group",
                Some(DropHint::IntoRoot) => "into_root",
            };
            if now != a {
                return fail(format!("落点提示是 {now}，不是 {a}"));
            }
        }
        "assert_host_field" => {
            // assert_host_field:<主机>:<字段>=<值>
            let id = host_ref(this, a)?;
            let (field, wanted) = b
                .split_once('=')
                .ok_or("写成 assert_host_field:主机:字段=值")?;
            let host = this.store.host(id).ok_or("主机已经不在库里")?;
            let actual = match field {
                "tags" => host.tags.join(","),
                "port" => host.port.to_string(),
                "name" => host.name.clone(),
                "username" => host.username.clone(),
                "legacy" => host.legacy_algorithms.to_string(),
                "identity" => host
                    .identity_id
                    .map_or("none".to_owned(), |i| i.to_string()),
                "sort" => host.sort.to_string(),
                _ => return fail("不认识的字段"),
            };
            if actual != wanted {
                return fail(format!("{field} 是 {actual}，不是 {wanted}"));
            }
        }
        "tcp_expect" | "tcp_refused" => {
            // tcp_expect:<本机端口>:<回应里要有的文字> / tcp_refused:<本机端口>
            // 在后台线程里连：界面线程不能卡在网络上。最多等 3 秒，没到就稍后再看。
            let port = a.parse::<u16>().map_err(|_| "端口不是数字")?;
            let expect_text = b.to_owned();
            let refused_wanted = name == "tcp_refused";
            let key = format!("{name}:{port}:{expect_text}");
            return Ok(super::dev_acts_tcp::poll(&key, move || {
                super::dev_acts_tcp::probe(port, &expect_text, refused_wanted)
            }));
        }
        "assert_error_free" => {
            if let Some(error) = this.store.error() {
                return fail(error.to_owned());
            }
        }
        _ => return fail("还没有实现"),
    }
    let _ = (ProxyKind::Socks5, ConfirmAction::DeleteHost(0));
    done
}
