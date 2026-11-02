//! X11 转发的原生版转接：把核心库的 X11 能力接到"连接并开终端"的流程上。
//!
//! 业务都在 `zssh_core::x11`；这里只做三件事：
//! - 看这台主机开没开 X11 转发（`wanted`）；
//! - 连上之后、开 shell 之前准备本机 X 服务器（`attempt`）：用现成的、没有就拉起 VcXsrv，
//!   都不行就带着原因继续，终端里会有提示（X11 失败从不阻止终端打开）；
//! - 退出时按设置关掉自己拉起的 X 服务器（`shutdown_owned`）。
//!
//! 一键安装 VcXsrv 和检测状态的入口（`install_vcxsrv`、`local_status`）已经备好，
//! 界面按钮在后续界面卡里接。

use std::path::PathBuf;
use std::sync::Arc;

use zssh_core::ai::terminal::SharedTerminalRing;
use zssh_core::error::ZsshError;
use zssh_core::ssh::{Connection, DEFAULT_BUFFER_LIMIT, ShellSession};
use zssh_core::store::Database;
use zssh_core::x11::local::{self, X11Prefs, prefs};
use zssh_core::x11::{X11Failure, X11Outcome, X11Request};

use crate::settings::data_dir;

/// 一次 X11 尝试：要么带着本机 X 服务器的目标去请求，要么带着原因跳过。
pub type X11Attempt = Result<X11Request, X11Failure>;

fn x11_dir() -> PathBuf {
    data_dir().join("x11")
}

fn open_db() -> Option<Database> {
    Database::open(&data_dir().join("zssh.db")).ok()
}

/// 这台主机开了 X11 转发吗。读不了主机库按"没开"。
pub fn wanted(host_id: i64) -> bool {
    open_db().is_some_and(|db| wanted_in(&db, host_id))
}

fn wanted_in(db: &Database, host_id: i64) -> bool {
    db.host_protocol_get(host_id)
        .map(|settings| settings.x11_enabled())
        .unwrap_or(false)
}

/// 准备本机 X 服务器。`wanted` 为 false 返回 `None`（普通终端）。
pub async fn attempt(wanted: bool) -> Option<X11Attempt> {
    if !wanted {
        return None;
    }
    let prefs = open_db()
        .map(|db| prefs::load_prefs(&db))
        .unwrap_or_default();
    Some(
        local::prepare(prefs, x11_dir())
            .await
            .map(|prepared| X11Request {
                route: prepared.route,
                on_failure: None,
            }),
    )
}

/// 开 shell：没有 X11 要求就是普通终端，否则走核心库的 X11 流程（被拒会自动换普通通道）。
pub async fn open_shell(
    connection: &Connection,
    cols: u16,
    rows: u16,
    ring: Option<SharedTerminalRing>,
    x11: Option<X11Attempt>,
) -> Result<ShellSession, ZsshError> {
    let Some(x11) = x11 else {
        return connection
            .open_shell_with_buffer_limit(cols, rows, DEFAULT_BUFFER_LIMIT, ring)
            .await;
    };
    let (shell, outcome) = connection
        .open_shell_x11(cols, rows, DEFAULT_BUFFER_LIMIT, ring, x11)
        .await?;
    match outcome {
        X11Outcome::Active { display: x_server } => tracing::info!(%x_server, "X11 转发已开启"),
        X11Outcome::Skipped(failure) => tracing::info!(code = failure.code(), "X11 转发没有开启"),
        X11Outcome::NotRequested => {}
    }
    Ok(shell)
}

/// 程序退出时调用：设置里要求关闭时，结束本次运行亲手拉起的 X 服务器。
pub fn shutdown_owned() {
    let prefs: X11Prefs = open_db()
        .map(|db| prefs::load_prefs(&db))
        .unwrap_or_default();
    if local::shutdown_owned_if_configured(&prefs, &x11_dir()) {
        tracing::info!("已关闭 ZeeTerm 拉起的 X 服务器");
    }
}

/// 界面用：本机 X 服务器的状态（已装的 VcXsrv、正在运行的、ZeeTerm 自己拉起的）。
#[allow(dead_code)] // 界面入口在后续界面卡
pub fn local_status() -> local::LocalXStatus {
    local::status(&local::RealSystem, local::OwnedRegistry::global())
}

/// 界面用：一键安装 VcXsrv。下载走软件网络代理，校验固定的 SHA-256，静默安装由系统 UAC 确认。
/// `on_progress(已下载, 总量)`；`cancel` 置位即中断。
#[allow(dead_code)] // 界面入口在后续界面卡
pub async fn install_vcxsrv(
    on_progress: impl FnMut(u64, u64) + Send,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<local::VcXsrvInstall, local::install::InstallError> {
    let data = data_dir();
    let proxy = crate::agent_setup::download::proxy_url(&data);
    let client = zssh_core::ai::agent::download::build_client(proxy.as_deref())
        .map_err(local::install::InstallError::Download)?;
    local::install::install_vcxsrv(
        &client,
        &local::install::pinned_source(),
        &x11_dir().join("download"),
        Arc::new(local::install::UacRunner),
        Arc::new(local::RealSystem),
        on_progress,
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use zssh_core::model::{AuthMethod, HostDraft, ProtocolSettings, ProxyMode};

    use super::*;

    fn host(db: &Database, x11: bool) -> i64 {
        let draft = HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: "gui".to_owned(),
            host: "example.internal".to_owned(),
            port: 22,
            username: "deploy".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        };
        let settings = ProtocolSettings {
            x11_forwarding: x11,
            ..ProtocolSettings::default()
        };
        db.host_save_with_protocol(&draft, &settings)
            .expect("存主机")
            .0
            .id
    }

    #[test]
    fn only_hosts_with_x11_switched_on_ask_for_it() {
        let db = Database::open_in_memory().expect("内存库");
        let with = host(&db, true);
        let without = host(&db, false);
        assert!(wanted_in(&db, with));
        assert!(!wanted_in(&db, without));
        assert!(!wanted_in(&db, 9999), "不存在的主机按没开");
    }

    #[tokio::test]
    async fn a_host_without_x11_never_touches_the_local_x_server() {
        assert!(attempt(false).await.is_none());
    }
}
