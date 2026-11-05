//! 跟随终端目录（0.2.14 `state/followCwd.ts` + `App.tsx` 里的 `followActive` / `followPath`）。
//!
//! 文件面板（停靠）里有个「跟随终端目录」开关，默认关。开着时主窗口每 1.5 秒问一次焦点终端现在在哪个
//! 目录：走终端连接上的另一条 exec 通道（`zssh_core::ssh::probe_shell_cwds`，读 `/proc/<pid>/cwd`），
//! **不往终端里敲任何东西**；取不到（不是 Linux、对不上号、会话没连上）就保持原样，不报错。
//! 远端栏和终端目录不一样就跳过去；用户在面板里手动换远端目录（进目录、上级、路径栏、后退 / 前进）
//! 会自动关掉开关，免得来回跳。

use gpui_kit::*;

use super::SftpDemo;
use super::model::NavMode;
use crate::i18n::t;

/// 轮询终端目录的间隔：够快，又不会给服务器添负担（每次只是一条 exec 通道跑一下）。
pub(crate) const FOLLOW_POLL_MS: u64 = 1500;

/// 远端栏该不该跳到终端的目录。
///
/// - 开关关着（用户手动换过目录）：不动；
/// - 取不到终端目录（不是 Linux、对不上号、会话没连上）：不动，不报错；
/// - 终端目录和远端栏当前目录一样：不动（不重复列目录）。
pub(super) fn follow_target(
    follow: bool,
    terminal_cwd: Option<&str>,
    current_remote: Option<&str>,
) -> Option<String> {
    let cwd = terminal_cwd.filter(|cwd| !cwd.is_empty())?;
    if !follow || Some(cwd) == current_remote {
        return None;
    }
    Some(cwd.to_owned())
}

impl SftpDemo {
    /// 开关现在开着吗。
    pub fn follow(&self) -> bool {
        self.follow
    }

    pub fn set_follow(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.follow == on {
            return;
        }
        self.follow = on;
        self.say(if on {
            t(super::SftpMsg::FollowOn)
        } else {
            t(super::SftpMsg::FollowOff)
        });
        cx.notify();
    }

    /// 主窗口问到了终端的目录（`None` = 这次没取到）：和远端栏不一样就跳过去。
    /// 这不算用户手动换目录（不会把开关关掉）。
    pub fn follow_to(&mut self, terminal_cwd: Option<&str>, cx: &mut Context<Self>) {
        let current = (!self.remote.path.is_empty()).then_some(self.remote.path.as_str());
        let Some(target) = follow_target(self.follow, terminal_cwd, current) else {
            return;
        };
        self.navigate_remote(&target, NavMode::Visit, None, cx);
    }

    /// 用户手动换了远端目录：停止跟随。
    pub(super) fn manual_remote_navigation(&mut self) {
        if self.follow {
            self.follow = false;
            self.say(t(super::SftpMsg::FollowStopped));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::follow_target;

    #[test]
    fn follow_rules() {
        // 开着、终端目录和当前不同：跳。
        assert_eq!(
            follow_target(true, Some("/srv/app"), Some("/home")),
            Some("/srv/app".to_owned())
        );
        // 一样：不动。
        assert_eq!(follow_target(true, Some("/home"), Some("/home")), None);
        // 开关关着：不动。
        assert_eq!(follow_target(false, Some("/srv"), Some("/home")), None);
        // 取不到（None / 空）：不动。
        assert_eq!(follow_target(true, None, Some("/home")), None);
        assert_eq!(follow_target(true, Some(""), Some("/home")), None);
        // 远端栏还没有目录：照跳。
        assert_eq!(
            follow_target(true, Some("/srv"), None),
            Some("/srv".to_owned())
        );
    }
}
