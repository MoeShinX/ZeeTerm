//! 更新窗口的"画什么"：当前是哪一屏、标题 / 副标题写什么、底部有哪些按钮，以及下载进度的文字。
//!
//! 都是纯函数（不碰 gpui），状态测试不用起窗口；`view.rs` 只管把这里的结果摆到界面上。

use std::time::{Duration, Instant};

use super::{CheckKind, DownloadState, UpdateMsg};
use crate::i18n::t;

/// 窗口当前这一屏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Screen {
    /// 正在取清单。
    Checking,
    /// 取清单 / 选包失败（含"更新签名未配置"）。
    CheckFailed,
    /// 已是最新版本。
    Latest,
    /// 发现新版本，还没开始下载（或下载被取消了）。
    Found,
    Downloading,
    /// 下完了，正在校验签名。
    Verifying,
    /// 签名通过，等用户点安装。
    Verified,
    /// 下载 / 校验失败（含签名对不上）。
    DownloadFailed,
}

/// 底部按钮的动作；`view.rs` 把它们接到窗口的方法上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// 关闭窗口。
    Close,
    /// 重新检查（检查失败时）。
    RecheckAgain,
    /// 重新下载（下载失败时）。
    RetryDownload,
    /// 跳过这个版本。
    Skip,
    /// 稍后：发现新版本时只关窗口；已验证时排队"退出后安装"再关窗口。
    Later,
    Download,
    Cancel,
    Install,
}

/// 一个按钮的外观。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ButtonSpec {
    pub action: Action,
    pub label: &'static str,
    /// 主按钮（强调色）。
    pub primary: bool,
    /// 文字按钮（没有边框和底色），用于"跳过此版本"这类次要动作。
    pub text_only: bool,
}

impl ButtonSpec {
    const fn plain(action: Action, label: &'static str) -> Self {
        Self {
            action,
            label,
            primary: false,
            text_only: false,
        }
    }

    const fn primary(action: Action, label: &'static str) -> Self {
        Self {
            action,
            label,
            primary: true,
            text_only: false,
        }
    }

    const fn text(action: Action, label: &'static str) -> Self {
        Self {
            action,
            label,
            primary: false,
            text_only: true,
        }
    }
}

/// 由"检查结果 + 下载阶段"决定的这一屏。
pub(super) fn screen(check: Option<CheckKind>, phase: &DownloadState) -> Screen {
    match check {
        None => Screen::Checking,
        Some(CheckKind::Failed) => Screen::CheckFailed,
        Some(CheckKind::Latest) => Screen::Latest,
        Some(CheckKind::Found) => match phase {
            DownloadState::Idle | DownloadState::Cancelled => Screen::Found,
            DownloadState::Downloading { .. } => Screen::Downloading,
            DownloadState::Verifying => Screen::Verifying,
            DownloadState::Verified { .. } => Screen::Verified,
            DownloadState::Failed(_) => Screen::DownloadFailed,
        },
    }
}

/// 这一屏要不要显示"更新内容"卡片。
pub(super) fn shows_notes(screen: Screen) -> bool {
    matches!(
        screen,
        Screen::Found
            | Screen::Downloading
            | Screen::Verifying
            | Screen::Verified
            | Screen::DownloadFailed
    )
}

/// 底部左侧要不要放"安装后重启 ZeeTerm"复选框。
pub(super) fn shows_restart_choice(screen: Screen) -> bool {
    matches!(screen, Screen::Found | Screen::Verified)
}

/// 主标题。`version` 是新版本号（没有新版本时不用）。
pub(super) fn title(screen: Screen, version: &str) -> String {
    match screen {
        Screen::Checking => t(UpdateMsg::CheckingTitle).to_owned(),
        Screen::CheckFailed => t(UpdateMsg::CheckFailedTitle).to_owned(),
        Screen::Latest => t(UpdateMsg::LatestVersion).to_owned(),
        Screen::Found => format!("{}{version}", t(UpdateMsg::FoundVersionPrefix)),
        Screen::Downloading => format!("{}{version}", t(UpdateMsg::DownloadingTitlePrefix)),
        Screen::Verifying => t(UpdateMsg::VerifyingSignature).to_owned(),
        Screen::Verified => format!("{}{version}", t(UpdateMsg::ReadyToInstallPrefix)),
        Screen::DownloadFailed => format!(
            "{}{version}{}",
            t(UpdateMsg::UpdateToVersionPrefix),
            t(UpdateMsg::UpdateToVersionSuffix)
        ),
    }
}

/// 副标题：当前版本，有发布日期就带上（只有发现了新版本的几屏才有日期）。
pub(super) fn subtitle(screen: Screen, current: &str, published: Option<&str>) -> String {
    let mut text = format!("{}{current}", t(UpdateMsg::CurrentVersionPrefix));
    if shows_notes(screen)
        && let Some(date) = published.and_then(publish_date)
    {
        text.push_str(&format!("{}{date}", t(UpdateMsg::PublishedSuffix)));
    }
    text
}

/// `pub_date`（RFC 3339，例如 `2026-11-03T08:00:00Z`）取日期部分；格式不对就当没有。
pub(super) fn publish_date(raw: &str) -> Option<String> {
    let head = raw.trim().get(..10)?;
    let bytes = head.as_bytes();
    let shaped = bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        _ => b.is_ascii_digit(),
    });
    shaped.then(|| head.to_owned())
}

/// 底部右侧的按钮，从左到右。
pub(super) fn buttons(screen: Screen) -> Vec<ButtonSpec> {
    match screen {
        Screen::Checking => vec![ButtonSpec::plain(Action::Close, t(UpdateMsg::CloseButton))],
        Screen::CheckFailed => vec![
            ButtonSpec::plain(Action::Close, t(UpdateMsg::CloseButton)),
            ButtonSpec::primary(Action::RecheckAgain, t(UpdateMsg::RetryButton)),
        ],
        Screen::Latest => vec![ButtonSpec::primary(
            Action::Close,
            t(UpdateMsg::CloseButton),
        )],
        Screen::Found => vec![
            ButtonSpec::text(Action::Skip, t(UpdateMsg::SkipVersionButton)),
            ButtonSpec::plain(Action::Later, t(UpdateMsg::LaterButton)),
            ButtonSpec::primary(Action::Download, t(UpdateMsg::DownloadAndInstallButton)),
        ],
        Screen::Downloading => vec![ButtonSpec::plain(
            Action::Cancel,
            t(UpdateMsg::CancelButton),
        )],
        // 校验只有一瞬间，没有可点的东西（取消只对下载中有效）。
        Screen::Verifying => Vec::new(),
        Screen::Verified => vec![
            ButtonSpec::plain(Action::Later, t(UpdateMsg::LaterButton)),
            ButtonSpec::primary(Action::Install, t(UpdateMsg::InstallNowButton)),
        ],
        Screen::DownloadFailed => vec![
            ButtonSpec::plain(Action::Close, t(UpdateMsg::CloseButton)),
            ButtonSpec::primary(Action::RetryDownload, t(UpdateMsg::RetryButton)),
        ],
    }
}

/// 选一个合适的单位（`progress_line` 要让两个数用同一个单位）。
fn unit_for(bytes: u64) -> (f64, &'static str) {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        (KIB * KIB * KIB, "GB")
    } else if value >= KIB * KIB {
        (KIB * KIB, "MB")
    } else if value >= KIB {
        (KIB, "KB")
    } else {
        (1.0, "B")
    }
}

/// 下载进度那一行：`12.3 / 14.8 MB · 2.1 MB/s`；总大小未知时 `12.3 MB`；速度还没测出来就不写。
pub(super) fn progress_line(received: u64, total: Option<u64>, rate: Option<f64>) -> String {
    let mut text = match total {
        Some(total) if total > 0 => {
            // 两个数用同一个单位，读起来才对得上。
            let (divisor, unit) = unit_for(total);
            if divisor == 1.0 {
                format!("{received} / {total} {unit}")
            } else {
                format!(
                    "{:.1} / {:.1} {unit}",
                    received as f64 / divisor,
                    total as f64 / divisor
                )
            }
        }
        _ => crate::format::size(received),
    };
    if let Some(rate) = rate.filter(|rate| rate.is_finite() && *rate > 0.0) {
        text.push_str(&format!(" · {}/s", crate::format::size(rate as u64)));
    }
    text
}

/// 进度条的百分比（0–100）；总大小未知返回 `None`（界面改画"不确定进度"）。
pub(super) fn progress_percent(received: u64, total: Option<u64>) -> Option<f32> {
    let total = total.filter(|total| *total > 0)?;
    Some((received as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32)
}

/// 下载速度：隔一小段时间取一次差，再做平滑，免得数字一直跳。
#[derive(Debug, Default)]
pub(super) struct SpeedMeter {
    last: Option<(Instant, u64)>,
    rate: Option<f64>,
}

/// 两次取样之间至少隔这么久（上报是 100ms 一次，太密的差值抖得厉害）。
const SPEED_WINDOW: Duration = Duration::from_millis(400);

impl SpeedMeter {
    /// 开始新一轮下载。
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// 记一次进度；`now` 由调用方给，测试里可以快进。
    pub(super) fn record(&mut self, now: Instant, received: u64) {
        let Some((at, bytes)) = self.last else {
            self.last = Some((now, received));
            return;
        };
        let elapsed = now.saturating_duration_since(at);
        if elapsed < SPEED_WINDOW {
            return;
        }
        let instant = received.saturating_sub(bytes) as f64 / elapsed.as_secs_f64();
        self.rate = Some(match self.rate {
            Some(previous) => previous * 0.6 + instant * 0.4,
            None => instant,
        });
        self.last = Some((now, received));
    }

    /// 平滑后的速度（字节 / 秒）；还没测出来为 `None`。
    pub(super) fn rate(&self) -> Option<f64> {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Action, Screen, SpeedMeter, buttons, progress_line, progress_percent, publish_date, screen,
        shows_notes, shows_restart_choice, subtitle, title,
    };
    use crate::update::{CheckKind, DownloadState};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn verified() -> DownloadState {
        DownloadState::Verified {
            path: PathBuf::from("setup.exe"),
            size: 1,
            sha256: "0".repeat(64),
        }
    }

    fn actions(screen: Screen) -> Vec<Action> {
        buttons(screen)
            .into_iter()
            .map(|button| button.action)
            .collect()
    }

    fn labels(screen: Screen) -> Vec<&'static str> {
        buttons(screen)
            .into_iter()
            .map(|button| button.label)
            .collect()
    }

    #[test]
    fn every_state_maps_to_its_screen() {
        let idle = DownloadState::Idle;
        assert_eq!(screen(None, &idle), Screen::Checking);
        assert_eq!(screen(Some(CheckKind::Failed), &idle), Screen::CheckFailed);
        assert_eq!(screen(Some(CheckKind::Latest), &idle), Screen::Latest);
        assert_eq!(screen(Some(CheckKind::Found), &idle), Screen::Found);
        assert_eq!(
            screen(Some(CheckKind::Found), &DownloadState::Cancelled),
            Screen::Found
        );
        let downloading = DownloadState::Downloading {
            received: 1,
            total: Some(2),
        };
        assert_eq!(
            screen(Some(CheckKind::Found), &downloading),
            Screen::Downloading
        );
        assert_eq!(
            screen(Some(CheckKind::Found), &DownloadState::Verifying),
            Screen::Verifying
        );
        assert_eq!(
            screen(Some(CheckKind::Found), &verified()),
            Screen::Verified
        );
        assert_eq!(
            screen(Some(CheckKind::Found), &DownloadState::Failed("x".into())),
            Screen::DownloadFailed
        );
        // 没有新版本时，下载阶段的残留状态不影响画面。
        assert_eq!(screen(Some(CheckKind::Latest), &verified()), Screen::Latest);
    }

    #[test]
    fn titles_follow_the_state() {
        assert_eq!(title(Screen::Checking, "0.3.1"), "正在检查更新…");
        assert_eq!(title(Screen::CheckFailed, "0.3.1"), "检查更新失败");
        assert_eq!(title(Screen::Latest, "0.3.1"), "已是最新版本");
        assert_eq!(title(Screen::Found, "0.3.1"), "发现新版本 0.3.1");
        assert_eq!(title(Screen::Downloading, "0.3.1"), "正在下载 0.3.1");
        assert_eq!(title(Screen::Verified, "0.3.1"), "已准备好安装 0.3.1");
        assert!(title(Screen::DownloadFailed, "0.3.1").contains("0.3.1"));
    }

    #[test]
    fn subtitle_shows_the_current_version_and_the_release_date_when_known() {
        assert_eq!(
            subtitle(Screen::Found, "0.2.14", Some("2026-11-03T08:00:00Z")),
            "当前版本 0.2.14 · 发布于 2026-11-03"
        );
        assert_eq!(subtitle(Screen::Found, "0.2.14", None), "当前版本 0.2.14");
        // 日期格式不对就省略，不显示乱码。
        assert_eq!(
            subtitle(Screen::Found, "0.2.14", Some("昨天")),
            "当前版本 0.2.14"
        );
        // 没有新版本的几屏不带日期。
        assert_eq!(
            subtitle(Screen::Latest, "0.3.1", Some("2026-11-03")),
            "当前版本 0.3.1"
        );
        assert_eq!(publish_date("2026-11-03"), Some("2026-11-03".to_owned()));
        assert_eq!(publish_date("2026/11/03"), None);
        assert_eq!(publish_date(""), None);
        assert_eq!(publish_date("2026-1-3T00"), None);
    }

    #[test]
    fn found_offers_skip_later_and_download() {
        assert_eq!(
            actions(Screen::Found),
            vec![Action::Skip, Action::Later, Action::Download]
        );
        assert_eq!(
            labels(Screen::Found),
            vec!["跳过此版本", "稍后", "下载并安装"]
        );
        let found = buttons(Screen::Found);
        assert!(found[0].text_only, "跳过此版本是文字按钮");
        assert!(found[2].primary, "下载并安装是主按钮");
        assert!(shows_restart_choice(Screen::Found));
    }

    #[test]
    fn downloading_only_offers_cancel() {
        assert_eq!(labels(Screen::Downloading), vec!["取消"]);
        assert!(!shows_restart_choice(Screen::Downloading));
        assert_eq!(actions(Screen::Verifying), Vec::<Action>::new());
    }

    #[test]
    fn verified_offers_later_and_install_in_one_step() {
        assert_eq!(labels(Screen::Verified), vec!["稍后", "立即安装"]);
        assert_eq!(
            actions(Screen::Verified),
            vec![Action::Later, Action::Install]
        );
        assert!(buttons(Screen::Verified)[1].primary);
        assert!(shows_restart_choice(Screen::Verified));
    }

    #[test]
    fn latest_and_failures_close_and_failures_can_retry() {
        assert_eq!(labels(Screen::Latest), vec!["关闭"]);
        assert_eq!(labels(Screen::CheckFailed), vec!["关闭", "重试"]);
        assert_eq!(
            actions(Screen::CheckFailed),
            vec![Action::Close, Action::RecheckAgain]
        );
        assert_eq!(labels(Screen::DownloadFailed), vec!["关闭", "重试"]);
        assert_eq!(
            actions(Screen::DownloadFailed),
            vec![Action::Close, Action::RetryDownload]
        );
        for screen in [Screen::Latest, Screen::CheckFailed, Screen::Checking] {
            assert!(!shows_restart_choice(screen), "{screen:?}");
            assert!(!shows_notes(screen), "{screen:?} 不画空卡片");
        }
    }

    #[test]
    fn notes_card_follows_the_found_screens() {
        for screen in [
            Screen::Found,
            Screen::Downloading,
            Screen::Verifying,
            Screen::Verified,
            Screen::DownloadFailed,
        ] {
            assert!(shows_notes(screen), "{screen:?}");
        }
    }

    #[test]
    fn bytes_are_shown_in_binary_units_with_one_decimal() {
        assert_eq!(crate::format::size(0), "0 B");
        assert_eq!(crate::format::size(512), "512 B");
        assert_eq!(crate::format::size(1536), "1.5 KB");
        assert_eq!(
            crate::format::size(15 * 1024 * 1024 + 512 * 1024),
            "15.5 MB"
        );
        assert_eq!(crate::format::size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn progress_line_uses_one_unit_and_shows_the_speed_when_known() {
        let mb = 1024.0 * 1024.0;
        let received = (12.3 * mb) as u64;
        let total = (14.8 * mb) as u64;
        assert_eq!(
            progress_line(received, Some(total), Some(2.1 * mb)),
            "12.3 / 14.8 MB · 2.1 MB/s"
        );
        assert_eq!(progress_line(received, Some(total), None), "12.3 / 14.8 MB");
        assert_eq!(progress_line(received, None, None), "12.3 MB");
        assert_eq!(progress_line(received, Some(0), None), "12.3 MB");
        assert_eq!(progress_line(100, Some(200), None), "100 / 200 B");
        // 速度是 0 / 非数就不写。
        assert_eq!(progress_line(100, Some(200), Some(0.0)), "100 / 200 B");
        assert_eq!(progress_line(100, Some(200), Some(f64::NAN)), "100 / 200 B");
    }

    #[test]
    fn progress_percent_is_clamped_and_unknown_without_a_total() {
        assert_eq!(progress_percent(50, Some(200)), Some(25.0));
        assert_eq!(progress_percent(500, Some(200)), Some(100.0));
        assert_eq!(progress_percent(5, None), None);
        assert_eq!(progress_percent(5, Some(0)), None);
    }

    #[test]
    fn speed_meter_waits_for_a_window_then_smooths() {
        let mut meter = SpeedMeter::default();
        let start = Instant::now();
        assert_eq!(meter.rate(), None);
        meter.record(start, 0);
        // 窗口没到：还没有速度。
        meter.record(start + Duration::from_millis(100), 100_000);
        assert_eq!(meter.rate(), None);
        // 1 秒内下了 1,000,000 字节：1 MB/s 量级。
        meter.record(start + Duration::from_secs(1), 1_000_000);
        let first = meter.rate().expect("有速度了");
        assert!((first - 1_000_000.0).abs() < 1.0, "{first}");
        // 下一秒更快：平滑后介于两者之间，不会一下跳到新值。
        meter.record(start + Duration::from_secs(2), 3_000_000);
        let second = meter.rate().expect("有速度");
        assert!(second > first && second < 2_000_000.0, "{second}");
        meter.reset();
        assert_eq!(meter.rate(), None);
    }
}
