//! 主视图切换（主机页 / 终端 / 设置）和设置页的进出规则。
//!
//! 规则取自 0.2.14 的 `ui/src/state/settingsView.ts`：齿轮是开关，关掉设置要回到进来之前
//! 的那一页（`settingsReturn`），不是一律回主机页。

/// 主区现在显示哪一页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainView {
    Vault,
    Terminal,
    Settings,
}

impl MainView {
    /// 窗口状态文件里的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vault => "vault",
            Self::Terminal => "terminal",
            Self::Settings => "settings",
        }
    }

    /// 读回窗口状态；不认识的值（文件被手改或版本不同）按 `None` 处理。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "vault" => Some(Self::Vault),
            "terminal" => Some(Self::Terminal),
            "settings" => Some(Self::Settings),
            _ => None,
        }
    }
}

/// 离开设置页能回去的那两页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnView {
    Vault,
    Terminal,
}

impl ReturnView {
    fn view(self) -> MainView {
        match self {
            Self::Vault => MainView::Vault,
            Self::Terminal => MainView::Terminal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewNav {
    pub view: MainView,
    /// 进设置之前在哪一页；不在设置页时没有意义。
    pub return_to: ReturnView,
}

impl Default for ViewNav {
    /// 启动时没有终端标签，只显示主机页。
    fn default() -> Self {
        Self {
            view: MainView::Vault,
            return_to: ReturnView::Vault,
        }
    }
}

impl ViewNav {
    pub fn on(view: MainView) -> Self {
        let return_to = match view {
            MainView::Terminal => ReturnView::Terminal,
            _ => ReturnView::Vault,
        };
        Self { view, return_to }
    }

    /// 切到主机页或终端页。在设置页里切走，等于这次设置之旅结束，回去的页也跟着更新。
    pub fn show(self, view: MainView) -> Self {
        match view {
            MainView::Settings => self.open_settings(),
            other => Self::on(other),
        }
    }

    /// 打开设置：记住从哪来。已经在设置页就不动（别把 `return_to` 改成设置页）。
    pub fn open_settings(self) -> Self {
        match self.view {
            MainView::Settings => self,
            MainView::Vault => Self {
                view: MainView::Settings,
                return_to: ReturnView::Vault,
            },
            MainView::Terminal => Self {
                view: MainView::Settings,
                return_to: ReturnView::Terminal,
            },
        }
    }

    /// 关掉设置：回进来之前那一页。进来时在终端、待在设置页期间标签全关了（标题栏还能关标签），
    /// 这时回终端只会看到一片空白，退回主机页。
    pub fn close_settings(self, has_active_tab: bool) -> Self {
        if self.view != MainView::Settings {
            return self;
        }
        let back = if self.return_to == ReturnView::Terminal && !has_active_tab {
            ReturnView::Vault
        } else {
            self.return_to
        };
        Self {
            view: back.view(),
            return_to: back,
        }
    }

    /// 齿轮按钮：在设置页就关，不在就开。
    pub fn toggle_settings(self, has_active_tab: bool) -> Self {
        if self.view == MainView::Settings {
            self.close_settings(has_active_tab)
        } else {
            self.open_settings()
        }
    }

    /// 最后一个标签关掉以后：终端页没有东西可看，回主机页（0.2.14 `closeTab`）。
    pub fn tabs_emptied(self) -> Self {
        match self.view {
            MainView::Terminal => Self::on(MainView::Vault),
            MainView::Settings if self.return_to == ReturnView::Terminal => Self {
                view: MainView::Settings,
                return_to: ReturnView::Vault,
            },
            _ => self,
        }
    }

    /// 重启后恢复：主机页和设置页原样回来（设置页关掉回主机页）；终端页恢复不了——
    /// 会话不跨进程，重启后没有标签，退回主机页。
    pub fn restored(saved: Option<MainView>) -> Self {
        match saved {
            Some(MainView::Settings) => Self::default().open_settings(),
            Some(MainView::Vault) | Some(MainView::Terminal) | None => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_returns_to_the_page_it_was_opened_from() {
        let from_terminal = ViewNav::on(MainView::Terminal).open_settings();
        assert_eq!(from_terminal.view, MainView::Settings);
        assert_eq!(from_terminal.return_to, ReturnView::Terminal);
        assert_eq!(from_terminal.close_settings(true).view, MainView::Terminal);

        let from_vault = ViewNav::on(MainView::Vault).open_settings();
        assert_eq!(from_vault.close_settings(true).view, MainView::Vault);
    }

    #[test]
    fn opening_settings_twice_keeps_the_first_return_page() {
        let nav = ViewNav::on(MainView::Terminal)
            .open_settings()
            .open_settings();
        assert_eq!(nav.return_to, ReturnView::Terminal);
    }

    #[test]
    fn returning_to_a_terminal_with_no_tabs_falls_back_to_the_vault() {
        let nav = ViewNav::on(MainView::Terminal).open_settings();
        let closed = nav.close_settings(false);
        assert_eq!(closed.view, MainView::Vault);
        assert_eq!(closed.return_to, ReturnView::Vault);
    }

    #[test]
    fn the_gear_toggles() {
        let nav = ViewNav::on(MainView::Terminal);
        let open = nav.toggle_settings(true);
        assert_eq!(open.view, MainView::Settings);
        assert_eq!(open.toggle_settings(true).view, MainView::Terminal);
        // 不在设置页时不会动 return_to 以外的东西。
        assert_eq!(
            ViewNav::on(MainView::Vault)
                .toggle_settings(false)
                .return_to,
            ReturnView::Vault
        );
    }

    #[test]
    fn closing_outside_settings_does_nothing() {
        let nav = ViewNav::on(MainView::Vault);
        assert_eq!(nav.close_settings(true), nav);
    }

    #[test]
    fn switching_pages_inside_settings_ends_the_settings_trip() {
        let nav = ViewNav::on(MainView::Terminal).open_settings();
        let vault = nav.show(MainView::Vault);
        assert_eq!(vault.view, MainView::Vault);
        // 之后再开设置，回的是主机页，不是更早的终端页。
        assert_eq!(vault.open_settings().return_to, ReturnView::Vault);
        assert_eq!(nav.show(MainView::Terminal).view, MainView::Terminal);
    }

    #[test]
    fn emptying_the_tabs_leaves_the_terminal_page() {
        assert_eq!(
            ViewNav::on(MainView::Terminal).tabs_emptied().view,
            MainView::Vault
        );
        // 在设置页里把标签关光：关掉设置时回主机页。
        let nav = ViewNav::on(MainView::Terminal)
            .open_settings()
            .tabs_emptied();
        assert_eq!(nav.view, MainView::Settings);
        assert_eq!(nav.close_settings(false).view, MainView::Vault);
        assert_eq!(
            ViewNav::on(MainView::Vault).tabs_emptied().view,
            MainView::Vault
        );
    }

    #[test]
    fn view_names_round_trip_and_garbage_is_ignored() {
        for view in [MainView::Vault, MainView::Terminal, MainView::Settings] {
            assert_eq!(MainView::parse(view.as_str()), Some(view));
        }
        assert_eq!(MainView::parse("nope"), None);
        assert_eq!(MainView::parse(""), None);
    }

    #[test]
    fn restart_restores_what_can_exist_and_falls_back_for_the_rest() {
        assert_eq!(
            ViewNav::restored(Some(MainView::Vault)).view,
            MainView::Vault
        );
        // 设置页原样回来，关掉回主机页。
        let settings = ViewNav::restored(Some(MainView::Settings));
        assert_eq!(settings.view, MainView::Settings);
        assert_eq!(settings.close_settings(false).view, MainView::Vault);
        // 会话不跨进程：存的是终端页，重启后落在主机页。
        assert_eq!(
            ViewNav::restored(Some(MainView::Terminal)).view,
            MainView::Vault
        );
        assert_eq!(ViewNav::restored(None).view, MainView::Vault);
    }
}
