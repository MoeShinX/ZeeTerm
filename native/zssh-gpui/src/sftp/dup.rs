//! 复制 SFTP 标签（V2-U24）：对同一台主机再开一个文件视图，目录定位到原视图当前所在的位置，
//! 复用原视图已认证的 SSH 连接（不重新握手）。文件窗口和主窗口的标签双击、右键「复制标签」都走这里。
//!
//! 能不能复制、复制出来从哪个目录开始是纯规则（有单测）；真正建视图在 [`SftpDemo::new_clone`]。

use std::sync::Arc;

use ::gpui::Window;
use gpui_kit::*;

use super::{NEXT_SFTP, Phase, SftpDemo, StartDirs};
use crate::i18n::t;
use crate::ssh_backend::SshSessionFactory;
use std::sync::atomic::Ordering;

/// 不能复制的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneRefusal {
    /// 原视图自己还在连接：没有已认证的连接可复用，也没有"当前目录"可定位。
    Connecting,
    /// 连接已经断了（连接失败、对端断开、keepalive 超时、视图已被收尾）。
    Disconnected,
}

impl CloneRefusal {
    /// 给用户看的一句话。
    pub fn message(self) -> &'static str {
        match self {
            Self::Connecting => t(super::SftpMsg::CloneNotConnected),
            Self::Disconnected => t(super::SftpMsg::CloneDisconnected),
        }
    }
}

/// 复制一个视图要带上的东西。
pub struct CloneSpec {
    pub(super) host_id: i64,
    /// 原视图已认证的连接；调试夹具（没有真实连接）为空。
    pub(super) factory: Option<Arc<SshSessionFactory>>,
    pub(super) dirs: StartDirs,
}

/// 能不能复制。`connection_closed` 是原视图手里那条连接现在是不是已经断了：`None` = 手里没有连接
/// （视图已经收尾），按断开处理；`fixture` 是调试夹具视图（没有连接也允许复制）。
pub(super) fn gate(
    phase: Phase,
    connection_closed: Option<bool>,
    fixture: bool,
) -> Result<(), CloneRefusal> {
    match phase {
        Phase::Connecting => Err(CloneRefusal::Connecting),
        Phase::Failed => Err(CloneRefusal::Disconnected),
        Phase::Ready if fixture => Ok(()),
        Phase::Ready => match connection_closed {
            Some(false) => Ok(()),
            Some(true) | None => Err(CloneRefusal::Disconnected),
        },
    }
}

impl SftpDemo {
    /// 复制这个视图要带的东西；不能复制时给原因（调用方负责告诉用户，不开新标签）。
    pub fn clone_spec(&self) -> Result<CloneSpec, CloneRefusal> {
        let closed = self
            .factory
            .as_ref()
            .map(|factory| factory.connection().is_closed());
        gate(self.phase, closed, self.fixture)?;
        Ok(CloneSpec {
            host_id: self.host_id,
            factory: self.factory.clone(),
            dirs: StartDirs {
                remote: self.remote.path.clone(),
                local: self.local.path.clone(),
            },
        })
    }

    /// 按 [`CloneSpec`] 建一个新的文件视图（标签里的双栏布局）。
    pub fn new_clone(window: &mut Window, cx: &mut Context<Self>, spec: CloneSpec) -> Self {
        let n = NEXT_SFTP.fetch_add(1, Ordering::Relaxed);
        Self::new(
            window,
            cx,
            spec.host_id,
            format!("sftp-embedded-{n}"),
            spec.factory,
            Some(spec.dirs),
        )
    }

    /// 状态栏上现在的话（驱动脚本和测试用）。
    #[cfg(debug_assertions)]
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// 在这个视图的状态栏上放一句警示（复制被拒绝时用）。
    pub fn show_warning(&mut self, text: &str, cx: &mut Context<Self>) {
        self.warn(text);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{CloneRefusal, Phase, gate};

    #[test]
    fn a_connected_view_with_a_live_connection_can_be_cloned() {
        assert_eq!(gate(Phase::Ready, Some(false), false), Ok(()));
    }

    #[test]
    fn a_dropped_connection_refuses_with_the_disconnected_message() {
        assert_eq!(
            gate(Phase::Ready, Some(true), false),
            Err(CloneRefusal::Disconnected)
        );
        // 视图手里已经没有连接（被收尾过）：同样当作断开。
        assert_eq!(
            gate(Phase::Ready, None, false),
            Err(CloneRefusal::Disconnected)
        );
        assert_eq!(
            gate(Phase::Failed, None, false),
            Err(CloneRefusal::Disconnected)
        );
        assert_eq!(CloneRefusal::Disconnected.message(), "连接已断开，不能复制");
    }

    #[test]
    fn a_view_that_is_still_connecting_has_nothing_to_clone_yet() {
        assert_eq!(
            gate(Phase::Connecting, None, false),
            Err(CloneRefusal::Connecting)
        );
        assert_ne!(
            CloneRefusal::Connecting.message(),
            CloneRefusal::Disconnected.message()
        );
    }

    #[test]
    fn the_debug_fixture_has_no_connection_but_is_still_cloneable() {
        assert_eq!(gate(Phase::Ready, None, true), Ok(()));
        // 夹具也不能越过"没连上"。
        assert_eq!(
            gate(Phase::Connecting, None, true),
            Err(CloneRefusal::Connecting)
        );
    }
}

#[cfg(test)]
mod load_tests {
    //! 复制标签的首屏：用进程内的假 SFTP 服务器验证"起始目录读得出来就落在那儿，读不出来退回家目录"。

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::super::{SftpEvent, load_first};
    use crate::ssh_backend::{SshSessionFactory, runtime};
    use crate::test_ssh_server::{SFTP_HOME, connect_trusting, spawn_sftp};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 连上假服务器，用同一条连接跑一次首屏，返回 (落在哪个目录, 里面的名字)。
    fn first_screen(start: Option<&str>) -> (String, Vec<String>) {
        let runtime = runtime().expect("运行时");
        let dir = std::env::temp_dir().join(format!(
            "zssh-u24-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let start = start.map(str::to_owned);
        let known_hosts_dir = dir.clone();
        let event = runtime.block_on(async move {
            let addr = spawn_sftp().await;
            let connection = connect_trusting(addr, &known_hosts_dir).await;
            let factory: Arc<SshSessionFactory> = SshSessionFactory::from_connection(connection);
            load_first(1, Some(factory), start).await
        });
        let _ = std::fs::remove_dir_all(&dir);
        match event {
            SftpEvent::Ready { cwd, entries, .. } => {
                (cwd, entries.into_iter().map(|e| e.name).collect())
            }
            SftpEvent::ConnectFailed(message) => panic!("首屏失败：{message}"),
            _ => panic!("首屏给了意料之外的事件"),
        }
    }

    #[test]
    fn without_a_start_dir_the_first_screen_is_the_home_directory() {
        let (cwd, names) = first_screen(None);
        assert_eq!(cwd, SFTP_HOME);
        assert!(names.contains(&"notes.txt".to_owned()));
    }

    #[test]
    fn a_clone_lands_in_the_original_tabs_directory() {
        let (cwd, names) = first_screen(Some("/srv/www/releases"));
        assert_eq!(cwd, "/srv/www/releases");
        assert!(names.contains(&"deploy.sh".to_owned()));
        assert!(!names.contains(&"notes.txt".to_owned()));
    }

    #[test]
    fn a_start_dir_that_is_gone_falls_back_to_home_instead_of_failing() {
        let (cwd, _) = first_screen(Some("/deleted/since"));
        assert_eq!(cwd, SFTP_HOME);
        // 空白的起始目录同样当作没给。
        let (cwd, _) = first_screen(Some("   "));
        assert_eq!(cwd, SFTP_HOME);
    }
}
