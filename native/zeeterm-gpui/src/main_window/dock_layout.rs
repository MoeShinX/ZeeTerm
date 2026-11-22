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

/// 一帧里终端页横向的全部输入：主机树 / 侧栏占的宽度、AI / 文件停靠、服务器概览。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub window_width: f32,
    /// 终端页左右两边已经占掉的宽度（展开的主机树 + 右侧侧栏），不能分给终端、停靠面板和概览。
    pub outer_reserved: f32,
    pub ai_open: bool,
    pub sftp_open: bool,
    pub ai_desired: f32,
    pub sftp_desired: f32,
    pub overview_open: bool,
    /// 概览想要的宽度：拖动中的临时值，没拖就是设置表里存的。
    pub overview_pref: f32,
}

/// [`resolve`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resolved {
    pub docks: DockLayout,
    /// 概览的实际宽度（没开时也照同一条规则算，调用方按开关决定画不画）。
    pub overview_width: f32,
    /// 终端区（终端 | 概览 里终端那一块）的宽度。
    pub terminal_width: f32,
}

/// 概览停靠在终端右边：它和 AI / 文件面板一样从终端区里拿宽度。顺序是（1.0.5 起不变）：
/// 概览开着时先把它想要的宽度记进 `reserved`，AI / 文件面板据此先让位；停靠面板定下来之后，
/// 概览自己的宽度再按「窗口 - 终端最小宽度 - 已占的全部」夹一次，所以终端永远不会低于 360。
pub fn resolve(frame: Frame) -> Resolved {
    let Frame {
        window_width,
        outer_reserved,
        ai_open,
        sftp_open,
        ai_desired,
        sftp_desired,
        overview_open,
        overview_pref,
    } = frame;
    let overview_want = if overview_open {
        clamp_overview(overview_pref, window_width, 0.0)
    } else {
        0.0
    };
    let docks = compute(Input {
        window_width,
        ai_open,
        sftp_open,
        ai_desired,
        sftp_desired,
        reserved: outer_reserved + overview_want,
    });
    let overview_width = clamp_overview(
        overview_pref,
        window_width,
        outer_reserved + docks.ai_width + docks.sftp_width,
    );
    let terminal_width = (window_width
        - outer_reserved.max(0.0)
        - docks.ai_width
        - docks.sftp_width
        - if overview_open { overview_width } else { 0.0 })
    .max(0.0);
    Resolved {
        docks,
        overview_width,
        terminal_width,
    }
}

fn clamp_overview(desired: f32, window_width: f32, reserved: f32) -> f32 {
    super::panel_width::clamp(PanelKind::Overview, desired, window_width, reserved)
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

    fn frame(window: f32, ai: bool, sftp: bool, overview: Option<f32>) -> Frame {
        Frame {
            window_width: window,
            outer_reserved: 0.0,
            ai_open: ai,
            sftp_open: sftp,
            ai_desired: 400.0,
            sftp_desired: 420.0,
            overview_open: overview.is_some(),
            overview_pref: overview.unwrap_or(360.0),
        }
    }

    /// 改成停靠之前的算法（概览盖在终端上）：概览开着时只把它想要的宽度记进 `reserved`，
    /// 停靠面板的宽度由此而来。回归用：停靠改的只是怎么画，这部分让位规则一个像素都不能变。
    fn docks_before_u45(f: Frame) -> DockLayout {
        let want = if f.overview_open {
            super::super::panel_width::clamp(
                PanelKind::Overview,
                f.overview_pref,
                f.window_width,
                0.0,
            )
        } else {
            0.0
        };
        compute(Input {
            window_width: f.window_width,
            ai_open: f.ai_open,
            sftp_open: f.sftp_open,
            ai_desired: f.ai_desired,
            sftp_desired: f.sftp_desired,
            reserved: f.outer_reserved + want,
        })
    }

    #[test]
    fn the_terminal_gives_up_exactly_the_overview_width_when_it_opens() {
        let closed = resolve(frame(1400.0, false, false, None));
        assert_eq!(closed.terminal_width, 1400.0);
        let open = resolve(frame(1400.0, false, false, Some(360.0)));
        assert_eq!(open.overview_width, 360.0);
        assert_eq!(open.terminal_width, closed.terminal_width - 360.0);
        // 宽度存的是别的值时同样：终端 = 原宽 - 概览宽。
        let wide = resolve(frame(1400.0, false, false, Some(555.0)));
        assert_eq!(wide.overview_width, 555.0);
        assert_eq!(wide.terminal_width, 1400.0 - 555.0);
    }

    #[test]
    fn the_overview_is_clamped_so_the_terminal_keeps_its_minimum() {
        for window in [700.0, 720.0, 800.0, 900.0, 1000.0, 1280.0] {
            for pref in [320.0, 360.0, 540.0, 900.0, 5000.0] {
                let r = resolve(frame(window, false, false, Some(pref)));
                assert!(r.overview_width >= 0.0, "{window}/{pref}");
                assert!(
                    r.terminal_width >= TERMINAL_MIN,
                    "窗口 {window}、想要 {pref}：终端只剩 {}",
                    r.terminal_width
                );
            }
        }
        // 窗口 720：360 给终端，概览只能拿 360。
        assert_eq!(
            resolve(frame(720.0, false, false, Some(540.0))).overview_width,
            360.0
        );
    }

    #[test]
    fn a_window_too_small_for_everything_never_makes_negative_widths() {
        for window in [0.0, 100.0, 359.0, 360.0, 361.0, 400.0] {
            let r = resolve(frame(window, true, true, Some(600.0)));
            assert!(r.overview_width >= 0.0 && r.overview_width.is_finite());
            assert!(r.terminal_width >= 0.0);
            assert!(r.docks.ai_width >= 0.0 && r.docks.sftp_width >= 0.0);
        }
    }

    #[test]
    fn the_three_panels_and_the_terminal_always_add_up_to_the_window() {
        for window in [800.0, 1000.0, 1200.0, 1600.0, 2400.0] {
            for (ai, sftp) in [(false, false), (true, false), (false, true), (true, true)] {
                for overview in [None, Some(320.0), Some(360.0), Some(600.0)] {
                    let mut f = frame(window, ai, sftp, overview);
                    f.outer_reserved = 260.0;
                    let r = resolve(f);
                    let total = f.outer_reserved
                        + r.docks.ai_width
                        + r.docks.sftp_width
                        + if f.overview_open {
                            r.overview_width
                        } else {
                            0.0
                        }
                        + r.terminal_width;
                    assert_eq!(total, window, "{window} {ai} {sftp} {overview:?}");
                    assert!(r.terminal_width >= TERMINAL_MIN.min(window - 260.0));
                }
            }
        }
    }

    #[test]
    fn ai_and_file_docks_yield_exactly_as_they_did_before_the_overview_was_docked() {
        for window in (700..=2400).step_by(50) {
            for outer in [0.0, 224.0, 380.0] {
                for (ai, sftp) in [(true, false), (false, true), (true, true)] {
                    for overview in [None, Some(320.0), Some(360.0), Some(480.0), Some(900.0)] {
                        let mut f = frame(window as f32, ai, sftp, overview);
                        f.outer_reserved = outer;
                        assert_eq!(
                            resolve(f).docks,
                            docks_before_u45(f),
                            "窗口 {window} 外侧 {outer} AI {ai} 文件 {sftp} 概览 {overview:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn closing_the_overview_gives_the_docks_their_stored_widths_back() {
        // 窗口 1000、AI + 文件都开：概览开着时两块面板先让位，关掉后回到不带概览时的宽度。
        let without = resolve(frame(1000.0, true, true, None));
        let with = resolve(frame(1000.0, true, true, Some(360.0)));
        assert!(
            with.docks.ai_width + with.docks.sftp_width
                < without.docks.ai_width + without.docks.sftp_width
                || with.docks.collapse_ai,
            "概览开着时停靠面板要让位"
        );
        let again = resolve(frame(1000.0, true, true, None));
        assert_eq!(again.docks, without.docks);
        assert_eq!(without.docks.ai_width + without.docks.sftp_width, 640.0);
    }

    #[test]
    fn the_three_way_split_keeps_the_terminal_at_its_minimum_on_a_mid_size_window() {
        // 窗口 1280、树 224：AI 400 + 文件 420 + 概览 360 想要 1180 > 1056：AI 面板收起，文件面板和概览各让一点，终端仍是 360。
        let mut f = frame(1280.0, true, true, Some(360.0));
        f.outer_reserved = 224.0;
        let r = resolve(f);
        assert_eq!(r.terminal_width, TERMINAL_MIN);
        assert!(r.overview_width >= 0.0);
    }
}
