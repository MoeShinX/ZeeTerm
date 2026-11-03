//! 后台自动检查更新：启动后等一会儿查一次，之后程序开着时每 6 小时再查一次。
//!
//! 只在设置里「启动时自动检查」开着时才查（`zssh_core::updater::AUTO_CHECK_KEY`，每轮重新读，
//! 关掉马上生效）；手动「检查更新」不受这个开关影响。查到比当前新的版本，并且用户没对它选过「跳过」，
//! 就打开更新窗口（`update_demo`）——**不抢焦点**：用户可能正在终端里打字。同一个版本一次运行里只
//! 提示一次。下载和安装仍然要用户在窗口里点；后台只做"检查"这一件事。
//!
//! 更新签名公钥是占位值时 `check_for_update` 直接返回"更新签名未配置"，这里什么也不会发生，
//! 也不会发出任何请求。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ::gpui::App;
use zssh_update::Package;

use crate::{product, update_demo};

/// 启动后多久做第一次检查：让界面先起来、避开启动高峰。
pub const STARTUP_DELAY: Duration = Duration::from_secs(45);
/// 程序开着时的检查间隔。
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

static STARTED: AtomicBool = AtomicBool::new(false);
/// 这次运行里已经提示过的版本。
static ANNOUNCED: Mutex<Option<String>> = Mutex::new(None);

/// 调试钩子可以把间隔缩短（毫秒），好在自动化里测；发行版里读不到，恒用默认值。
fn hook_ms(name: &str) -> Option<Duration> {
    crate::dev_hooks::var(name)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}

fn startup_delay() -> Duration {
    hook_ms("ZSSH_UPDATE_AUTO_DELAY_MS").unwrap_or(STARTUP_DELAY)
}

fn check_interval() -> Duration {
    hook_ms("ZSSH_UPDATE_AUTO_INTERVAL_MS").unwrap_or(CHECK_INTERVAL)
}

/// 一次检查的结果要不要提示用户：有包、比当前新、没被「跳过」、这次运行里还没提示过这个版本。
pub fn should_announce_found(
    prefs: &zssh_core::updater::UpdatePrefs,
    current: &str,
    package: &Package,
    announced: Option<&str>,
) -> bool {
    zssh_core::updater::should_announce(prefs, current, &package.version, false)
        && announced != Some(package.version.as_str())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// 读更新偏好；设置库打不开按缺省（自动检查开）。
fn load_prefs() -> zssh_core::updater::UpdatePrefs {
    crate::hosts_demo::open_database()
        .ok()
        .and_then(|db| zssh_core::updater::load_prefs(&db).ok())
        .unwrap_or_default()
}

/// 做一轮：开关关着就什么都不做；开着就查一次，返回需要提示的结果。
async fn check_once() -> Option<(String, Package)> {
    let prefs = load_prefs();
    if !zssh_core::updater::should_auto_check(&prefs) {
        return None;
    }
    let runtime = crate::ssh_backend::runtime().ok()?;
    let current = update_demo::current_version();
    let checked = {
        let current = current.clone();
        runtime
            .spawn(async move { update_demo::check_for_update(&current).await })
            .await
            .ok()?
    };
    if let Ok(db) = crate::hosts_demo::open_database() {
        let _ = zssh_core::updater::record_check(&db, now_secs());
    }
    let (status, package) = checked;
    let package = package?;
    let mut announced = ANNOUNCED.lock().ok()?;
    if !should_announce_found(&prefs, &current, &package, announced.as_deref()) {
        return None;
    }
    *announced = Some(package.version.clone());
    Some((status, package))
}

/// 拉起自动检查：主窗口启动时调一次。任务挂在 `App` 上，进程退出时一起结束。
pub fn spawn(cx: &mut App) {
    if !product::AUTO_UPDATE_ENABLED || STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(startup_delay()).await;
        loop {
            if let Some((status, package)) = check_once().await {
                cx.update(|app| update_demo::open_update_window_for_found(app, status, package));
            }
            cx.background_executor().timer(check_interval()).await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{CHECK_INTERVAL, STARTUP_DELAY, should_announce_found};
    use std::time::Duration;
    use zssh_core::updater::UpdatePrefs;
    use zssh_update::Package;

    fn package(version: &str) -> Package {
        Package {
            platform_key: "windows-x86_64".to_owned(),
            version: version.to_owned(),
            url: "https://example.com/a.exe".to_owned(),
            signature: "sig".to_owned(),
        }
    }

    #[test]
    fn the_schedule_is_a_delayed_first_check_then_every_six_hours() {
        assert!(STARTUP_DELAY >= Duration::from_secs(30), "启动高峰过后再查");
        assert_eq!(CHECK_INTERVAL, Duration::from_secs(6 * 60 * 60));
    }

    #[test]
    fn a_newer_version_is_announced_once_per_run() {
        let prefs = UpdatePrefs::default();
        assert!(should_announce_found(
            &prefs,
            "1.0.0",
            &package("1.1.0"),
            None
        ));
        assert!(
            !should_announce_found(&prefs, "1.0.0", &package("1.1.0"), Some("1.1.0")),
            "同一个版本这次运行里提示过就不再打扰"
        );
        assert!(
            should_announce_found(&prefs, "1.0.0", &package("1.2.0"), Some("1.1.0")),
            "更新的版本照常提示"
        );
    }

    #[test]
    fn same_or_older_versions_and_skipped_versions_are_not_announced() {
        let prefs = UpdatePrefs::default();
        assert!(!should_announce_found(
            &prefs,
            "1.1.0",
            &package("1.1.0"),
            None
        ));
        assert!(!should_announce_found(
            &prefs,
            "1.2.0",
            &package("1.1.0"),
            None
        ));
        let skipped = UpdatePrefs {
            skipped_version: Some("1.1.0".to_owned()),
            ..UpdatePrefs::default()
        };
        assert!(!should_announce_found(
            &skipped,
            "1.0.0",
            &package("1.1.0"),
            None
        ));
        assert!(should_announce_found(
            &skipped,
            "1.0.0",
            &package("1.1.1"),
            None
        ));
    }
}
