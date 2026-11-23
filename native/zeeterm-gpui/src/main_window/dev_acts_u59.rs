//! 调试驱动里 V2-U59（从 SSH 配置导入主机）要用的 `act:` 动作。只在 debug 构建里编进去。
//! 配置文件用 `ZEETERM_SSH_CONFIG_PATH` 指向自己造的文件（见 `ssh_import`），从不碰真实的 `~/.ssh`。
//!
//! - `u59_import_open`：点「从 SSH 配置导入」入口（读默认路径）；
//! - `u59_import_load:<路径>`：像在文件选择对话框里选了这个文件（对话框要已经开着）；
//! - `u59_import_toggle:<别名>`、`u59_import_select:all|none`、`u59_import_group:<#分组名|none>`：改勾选 / 分组；
//! - `u59_import_confirm`：点「导入」；
//! - `u59_assert_import:<共几台>:<已选几台>`、`u59_assert_import_state:ready|missing|unreadable|too_large`；
//! - `u59_seed_host:<名字>:<地址>:<端口>:<用户名>`：往库里直接建一台密码认证的主机（造「已存在」用）；
//! - `u59_assert_host:<名字>:<地址>:<端口>:<用户名>:key|password:<跳板机名|none>`：检查导入出来的主机；
//! - `u59_assert_hosts:<台数>`：库里一共几台主机。

use ::gpui::Window;
use gpui_kit::*;
use zeeterm_core::model::{AuthMethod, HostDraft, ProxyMode};

use super::MainWindow;
use super::dev_script::Outcome;
use super::modal::Modal;
use super::ssh_import::ImportState;

const KNOWN: &[&str] = &[
    "u59_import_open",
    "u59_import_load",
    "u59_import_toggle",
    "u59_import_select",
    "u59_import_group",
    "u59_import_confirm",
    "u59_assert_import",
    "u59_assert_import_state",
    "u59_seed_host",
    "u59_assert_host",
    "u59_assert_hosts",
];

pub fn is_known(name: &str) -> bool {
    KNOWN.contains(&name)
}

pub(super) fn run(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Option<Result<Outcome, String>> {
    let (name, rest) = spec.split_once(':').unwrap_or((spec, ""));
    if !is_known(name) {
        return None;
    }
    Some(run_inner(this, name, rest, window, cx))
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Result<Outcome, String> {
    if ok {
        Ok(Outcome::Done)
    } else {
        Err(format!("断言没有成立：{}", what()))
    }
}

fn run_inner(
    this: &mut MainWindow,
    name: &str,
    rest: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    match name {
        "u59_import_open" => this.open_ssh_import(window, cx),
        "u59_import_load" => this.ssh_import_load_file(std::path::PathBuf::from(rest), cx),
        "u59_import_confirm" => this.ssh_import_confirm(window, cx),
        "u59_import_toggle" => {
            let index = row_index(this, rest)?;
            let now = selected(this, index)?;
            this.ssh_import_toggle(index, !now, cx);
        }
        "u59_import_select" => match rest {
            "all" => this.ssh_import_select_all(true, cx),
            "none" => this.ssh_import_select_all(false, cx),
            _ => return Err("u59_import_select:all|none".to_owned()),
        },
        "u59_import_group" => {
            let group = if rest == "none" {
                None
            } else {
                let wanted = rest.strip_prefix('#').ok_or("分组写 #名字 或 none")?;
                Some(
                    this.store
                        .groups()
                        .iter()
                        .find(|group| group.name == wanted)
                        .map(|group| group.id)
                        .ok_or("没有这个名字的分组")?,
                )
            };
            this.ssh_import_set_group(group, cx);
        }
        "u59_assert_import" => {
            let (total, chosen) = rest
                .split_once(':')
                .ok_or("u59_assert_import:<共几台>:<已选几台>")?;
            let (want_total, want_chosen): (usize, usize) = (
                total.parse().map_err(|_| "共几台要是数字")?,
                chosen.parse().map_err(|_| "已选几台要是数字")?,
            );
            let preview = preview(this)?;
            let have = (
                preview.rows.len(),
                preview.selected.iter().filter(|on| **on).count(),
            );
            return check(have == (want_total, want_chosen), || {
                format!("共 {} 台、已选 {} 台", have.0, have.1)
            });
        }
        "u59_assert_import_state" => {
            let Some(Modal::SshImport(dialog)) = &this.modal else {
                return Err("导入对话框没开着".to_owned());
            };
            let have = match &dialog.state {
                ImportState::Missing => "missing",
                ImportState::TooLarge => "too_large",
                ImportState::Unreadable(_) => "unreadable",
                ImportState::Ready(_) => "ready",
            };
            return check(have == rest, || format!("状态是 {have}"));
        }
        "u59_seed_host" => {
            let parts: Vec<&str> = rest.split(':').collect();
            let [host_name, address, port, user] = parts.as_slice() else {
                return Err("u59_seed_host:<名字>:<地址>:<端口>:<用户名>".to_owned());
            };
            let draft = HostDraft {
                id: None,
                group_id: None,
                name: (*host_name).to_owned(),
                host: (*address).to_owned(),
                port: port.parse().map_err(|_| "端口要是数字")?,
                username: (*user).to_owned(),
                auth_method: AuthMethod::Password,
                identity_id: None,
                proxy_id: None,
                jump_host_id: None,
                proxy_mode: ProxyMode::Default,
                tags: Vec::new(),
                sort: 0,
                legacy_algorithms: false,
                protocol: None,
            };
            this.store.save_host(&draft)?;
        }
        "u59_assert_hosts" => {
            let want: usize = rest.parse().map_err(|_| "台数要是数字")?;
            let have = this.store.hosts().len();
            return check(have == want, || format!("库里有 {have} 台"));
        }
        "u59_assert_host" => return assert_host(this, rest),
        _ => return Err("不认识的 u59 动作".to_owned()),
    }
    Ok(Outcome::Done)
}

fn preview(this: &MainWindow) -> Result<&super::ssh_import::Preview, String> {
    let Some(Modal::SshImport(dialog)) = &this.modal else {
        return Err("导入对话框没开着".to_owned());
    };
    match &dialog.state {
        ImportState::Ready(preview) => Ok(preview),
        _ => Err("导入对话框里没有预览".to_owned()),
    }
}

fn row_index(this: &MainWindow, alias: &str) -> Result<usize, String> {
    preview(this)?
        .rows
        .iter()
        .position(|row| row.alias == alias)
        .ok_or_else(|| "预览里没有这个别名".to_owned())
}

fn selected(this: &MainWindow, index: usize) -> Result<bool, String> {
    preview(this)?
        .selected
        .get(index)
        .copied()
        .ok_or_else(|| "行号越界".to_owned())
}

fn assert_host(this: &MainWindow, rest: &str) -> Result<Outcome, String> {
    let parts: Vec<&str> = rest.split(':').collect();
    let [name, address, port, user, auth, jump] = parts.as_slice() else {
        return Err(
            "u59_assert_host:<名字>:<地址>:<端口>:<用户名>:key|password:<跳板机名|none>".to_owned(),
        );
    };
    let hosts = this.store.hosts();
    let Some(host) = hosts.iter().find(|host| host.name == *name) else {
        return Err(format!("库里没有叫 {name} 的主机"));
    };
    let jump_name = host
        .jump_host_id
        .and_then(|id| hosts.iter().find(|other| other.id == id))
        .map(|other| other.name.as_str())
        .unwrap_or("none");
    let want_auth = if *auth == "key" {
        AuthMethod::Key
    } else {
        AuthMethod::Password
    };
    let ok = host.host == *address
        && host.port.to_string() == *port
        && host.username == *user
        && host.auth_method == want_auth
        && jump_name == *jump
        // 密钥认证必须带凭据；密码认证不带。
        && host.identity_id.is_some() == (want_auth == AuthMethod::Key);
    check(ok, || {
        format!(
            "{name}：{}@{}:{} 认证 {} 跳板机 {jump_name}",
            host.username,
            host.host,
            host.port,
            host.auth_method.as_str()
        )
    })
}
