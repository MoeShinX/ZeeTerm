//! 终端｜AI｜SFTP 三块同屏的宽度分配（0.2.14 `state/dockLayout.ts`）：纯函数，不依赖 GPUI。
//!
//! 规则：
//! - 顺序固定（终端、AI、SFTP），这里只算宽度；
//! - 终端至少保留 360px（左边展开的主机树占的宽度先扣掉，见 `reserved`）；
//! - 放不下时先把两个面板往各自的最小宽度（320px）收，按「各自还能收多少」的比例分摊；
//! - 收到最小还放不下，就收起 AI 面板（SFTP 是用户刚打开的，优先保留）。窗口变宽后不自动再展开。

use super::panel_width::{PANEL_MIN, PanelKind, TERMINAL_MIN, bounds};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Input {
    pub window_width: f32,
    pub ai_open: bool,
    pub sftp_open: bool,
    /// 用户记住的宽度（设置表里的值，已经体检过）。
    pub ai_desired: f32,
    pub sftp_desired: f32,
    /// 左边已经占掉的宽度（展开的主机树），不能分给终端和面板。
    pub reserved: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockLayout {
    /// 实际宽度；没开或者被收起的是 0。
    pub ai_width: f32,
    pub sftp_width: f32,
    /// 两个都开、窗口又太窄：AI 面板必须收起。调用方负责真的关掉并提示。
    pub collapse_ai: bool,
    pub terminal_width: f32,
}

fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.round().clamp(min, max.max(min))
}

pub fn compute(input: Input) -> DockLayout {
    let Input {
        window_width,
        ai_open,
        sftp_open,
        ai_desired,
        sftp_desired,
        reserved,
    } = input;
    let content = (window_width - reserved.max(0.0)).max(0.0);
    let single = |kind: PanelKind, desired: f32| -> f32 {
        let b = bounds(kind, window_width, reserved);
        clamp(desired, b.min, b.max)
    };
    match (ai_open, sftp_open) {
        (false, false) => DockLayout {
            ai_width: 0.0,
            sftp_width: 0.0,
            collapse_ai: false,
            terminal_width: content,
        },
        (true, false) => {
            let ai = single(PanelKind::Ai, ai_desired);
            DockLayout {
                ai_width: ai,
                sftp_width: 0.0,
                collapse_ai: false,
                terminal_width: content - ai,
            }
        }
        (false, true) => {
            let sftp = single(PanelKind::Sftp, sftp_desired);
            DockLayout {
                ai_width: 0.0,
                sftp_width: sftp,
                collapse_ai: false,
                terminal_width: content - sftp,
            }
        }
        (true, true) => {
            let available = (content - TERMINAL_MIN).floor().max(0.0);
            let mut ai = clamp(
                ai_desired,
                PANEL_MIN,
                (window_width * 0.6).floor().max(PANEL_MIN),
            );
            let mut sftp = clamp(
                sftp_desired,
                PANEL_MIN,
                (window_width * 0.6).floor().max(PANEL_MIN),
            );
            let excess = ai + sftp - available;
            if excess <= 0.0 {
                return DockLayout {
                    ai_width: ai,
                    sftp_width: sftp,
                    collapse_ai: false,
                    terminal_width: content - ai - sftp,
                };
            }
            let slack_ai = ai - PANEL_MIN;
            let slack_sftp = sftp - PANEL_MIN;
            if slack_ai + slack_sftp >= excess {
                let cut_ai = slack_ai.min((excess * slack_ai / (slack_ai + slack_sftp)).round());
                ai -= cut_ai;
                sftp -= excess - cut_ai;
                return DockLayout {
                    ai_width: ai,
                    sftp_width: sftp,
                    collapse_ai: false,
                    terminal_width: content - ai - sftp,
                };
            }
            // 两个都收到最小也放不下：收起 AI，SFTP 单独排。
            let alone = single(PanelKind::Sftp, sftp_desired);
            DockLayout {
                ai_width: 0.0,
                sftp_width: alone,
                collapse_ai: true,
                terminal_width: content - alone,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(window: f32, ai: bool, sftp: bool) -> Input {
        Input {
            window_width: window,
            ai_open: ai,
            sftp_open: sftp,
            ai_desired: 400.0,
            sftp_desired: 420.0,
            reserved: 0.0,
        }
    }

    #[test]
    fn nothing_open_leaves_everything_to_the_terminal() {
        let layout = compute(input(1200.0, false, false));
        assert_eq!((layout.ai_width, layout.sftp_width), (0.0, 0.0));
        assert_eq!(layout.terminal_width, 1200.0);
    }

    #[test]
    fn a_single_panel_gets_its_desired_width_within_bounds() {
        let layout = compute(input(1200.0, true, false));
        assert_eq!(layout.ai_width, 400.0);
        assert_eq!(layout.terminal_width, 800.0);
        let narrow = compute(input(700.0, false, true));
        assert_eq!(narrow.sftp_width, 340.0, "终端要留 360");
        assert_eq!(narrow.terminal_width, 360.0);
    }

    #[test]
    fn both_open_share_the_excess_in_proportion_to_what_each_can_give() {
        // 窗口 1000：可用 640，期望 400 + 420 = 820，超出 180。
        let layout = compute(input(1000.0, true, true));
        assert!(!layout.collapse_ai);
        assert_eq!(layout.ai_width + layout.sftp_width, 640.0);
        assert!(layout.ai_width >= PANEL_MIN && layout.sftp_width >= PANEL_MIN);
        assert_eq!(layout.terminal_width, 360.0);
    }

    #[test]
    fn too_narrow_for_two_collapses_the_ai_panel() {
        // 窗口 900：可用 540 < 320 + 320。
        let layout = compute(input(900.0, true, true));
        assert!(layout.collapse_ai);
        assert_eq!(layout.ai_width, 0.0);
        assert_eq!(layout.sftp_width, 420.0);
        assert_eq!(layout.terminal_width, 480.0);
    }

    #[test]
    fn the_host_tree_comes_off_the_top_first() {
        let mut with_tree = input(1200.0, true, false);
        with_tree.reserved = 300.0;
        let layout = compute(with_tree);
        assert_eq!(layout.ai_width, 400.0);
        assert_eq!(layout.terminal_width, 500.0);
        with_tree.ai_desired = 900.0;
        let wide = compute(with_tree);
        assert_eq!(wide.terminal_width, TERMINAL_MIN, "终端不低于 360");
    }
}
