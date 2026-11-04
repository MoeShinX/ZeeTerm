//! 调试驱动里和同步、备份、服务器状态有关的 `act:` 动作（G11）。动作都走和界面按钮同一批方法；
//! `*_assert` 检查结果，不对就让脚本失败；`*_wait` 条件还没成立就等。日志里只写动作名和结果，
//! 不写任何连接信息（本机桩的端口只在进程里，脚本里不出现）。
//!
//! - 服务器状态：`status_mark` / `assert_status_polls`；
//! - 设置页：`sync_open` / `backup_open`；
//! - 同步：`sync_set:<字段>:<值>`、`sync_do:<动作>[:<参数>]`、`sync_assert|sync_wait:<项>:<值>`、`sync_idle`；
//! - 备份：`backup_set` / `backup_do` / `backup_assert` / `backup_wait` / `backup_idle`；
//! - 本机桩：`stub_start`、`stub_reset`、`stub_fill:<存储>`、`stub_knob:<开关>[:<值>]`、`stub_assert:<项>:<值>`；
//! - 另一台电脑：`peer:<存储>:<操作>`、`local_hosts:<n>`、`local_rename:<旧>:<新>`、`assert_local_hosts:<n>`、
//!   `assert_local_host:<名字>:yes|no`；
//! - 底部同步条：`sync_bar:<项>:<值>`（等到成立）、`sync_bar_click`。
//!
//! 查询项的含义见 `sync_demo/dev.rs` / `backup_demo/dev.rs`。比较规则：项名以 `_has` 结尾的是「包含」，
//! 其余是「相等」；项名以 `_not` 结尾的取反。

use std::sync::Mutex;

use ::gpui::Window;
use gpui_kit::*;

use super::super::MainWindow;
use super::super::dev_script::Outcome;

pub const KNOWN: &[&str] = &[
    "status_mark",
    "assert_status_polls",
    "sync_open",
    "backup_open",
    "sync_set",
    "sync_do",
    "sync_assert",
    "sync_wait",
    "sync_idle",
    "backup_set",
    "backup_do",
    "backup_assert",
    "backup_wait",
    "backup_idle",
    "stub_start",
    "stub_reset",
    "stub_fill",
    "stub_knob",
    "stub_assert",
    "stub_mark",
    "peer",
    "local_hosts",
    "local_rename",
    "local_corrupt_baseline",
    "assert_local_hosts",
    "assert_local_host",
    "sync_bar",
    "sync_bar_click",
    "settings_scroll_by",
    "overview_scroll",
    "overview_assert",
];

pub fn is_known(name: &str) -> bool {
    KNOWN.contains(&name)
}

/// `status_mark` 时的（拨号次数, 采集次数）。
static MARK: Mutex<(u64, u64)> = Mutex::new((0, 0));
/// `stub_mark` 时桩已经收到的请求数。
static STUB_MARK: Mutex<usize> = Mutex::new(0);

fn fail<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

/// 不认识的动作返回 `None`，让上一层报错。
pub(in crate::main_window) fn run(
    this: &mut MainWindow,
    spec: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Option<Result<Outcome, String>> {
    let name = spec.split(':').next().unwrap_or("");
    if !is_known(name) {
        return None;
    }
    // 值里可能有 `:`（比如要填进输入框的地址），所以只拆前两段，剩下的整体当值。
    let mut parts = spec.splitn(3, ':');
    let _ = parts.next();
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or("");
    Some(run_inner(this, name, a, b, window, cx))
}

/// 比较一项查询结果和期望值：`_has` 是包含，`_not` 取反，其余相等。
fn matches(key: &str, have: &str, want: &str) -> bool {
    let (key, negate) = key
        .strip_suffix("_not")
        .map_or((key, false), |base| (base, true));
    let ok = if key.ends_with("_has") {
        have.contains(want)
    } else {
        have == want
    };
    ok != negate
}

/// 去掉查询项名字里的 `_has` / `_not` 后缀，得到真正的查询名。
fn query_name(key: &str) -> &str {
    let key = key.strip_suffix("_not").unwrap_or(key);
    key.strip_suffix("_has").unwrap_or(key)
}

fn run_inner(
    this: &mut MainWindow,
    name: &str,
    a: &str,
    b: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<Outcome, String> {
    let sync = |this: &MainWindow| {
        this.sync_settings
            .clone()
            .ok_or_else(|| "设置页还没有打开过（先 view:settings）".to_owned())
    };
    let backup = |this: &MainWindow| {
        this.backup_settings
            .clone()
            .ok_or_else(|| "设置页还没有打开过（先 view:settings）".to_owned())
    };
    match name {
        // 记下现在的拨号 / 采集次数，之后用 assert_status_polls 看这段时间新增了多少。
        "status_mark" => {
            *MARK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                crate::status_demo::poll_counters();
        }
        // assert_status_polls:<最少采集>:<最多采集>:<最多拨号>：同一台主机不管开了几个窗格，
        // 状态栏背后只该有一个轮询器（2 秒一次）、一条连接。
        "assert_status_polls" => {
            let (min_collects, max_collects, max_dials) = polls_args(a, b)?;
            let (dials, collects) = crate::status_demo::poll_counters();
            let (mark_dials, mark_collects) =
                *MARK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let (dials, collects) = (
                dials.saturating_sub(mark_dials),
                collects.saturating_sub(mark_collects),
            );
            if collects < min_collects {
                return Ok(Outcome::Pending);
            }
            if collects > max_collects || dials > max_dials {
                return fail(format!(
                    "这段时间采集 {collects} 次、拨号 {dials} 次，超出预期（采集 ≤ {max_collects}，拨号 ≤ {max_dials}）"
                ));
            }
            // 要求有采集时，订阅只来自状态栏这一处；要求「一次都没有」（隐藏后）时反过来：不能还订着。
            if max_collects > 0 && this.status_host.is_none() {
                return fail("状态栏没有订阅任何主机");
            }
            if max_collects == 0 && this.status_host.is_some() {
                return fail("预期已经退订，状态栏还订着主机");
            }
        }
        "sync_open" => {
            this.open_sync_settings(window, cx);
        }
        "backup_open" => {
            this.show_view(super::super::nav::MainView::Settings, window, cx);
            this.set_settings_section(crate::settings_nav::SettingsSection::Sync, cx);
            this.settings_scroll
                .scroll_to_top_of_item(super::super::settings_page::BACKUP_ITEM);
            cx.notify();
        }
        "sync_set" => {
            let entity = sync(this)?;
            entity.update(cx, |sync, cx| sync.dev_set(a, b, window, cx))?;
        }
        "sync_do" => {
            let entity = sync(this)?;
            entity.update(cx, |sync, cx| sync.dev_do(a, b, window, cx))?;
        }
        "sync_idle" => {
            let entity = sync(this)?;
            if !entity.read(cx).dev_idle() {
                return Ok(Outcome::Pending);
            }
        }
        "sync_assert" | "sync_wait" => {
            let entity = sync(this)?;
            let have = entity.read(cx).dev_query(query_name(a), cx)?;
            if !matches(a, &have, b) {
                if name == "sync_wait" {
                    return Ok(Outcome::Pending);
                }
                return fail(format!("{a} 和预期不同"));
            }
        }
        "backup_set" => {
            let entity = backup(this)?;
            entity.update(cx, |backup, cx| backup.dev_set(a, b, window, cx))?;
        }
        "backup_do" => {
            let entity = backup(this)?;
            entity.update(cx, |backup, cx| backup.dev_do(a, b, cx))?;
        }
        "backup_idle" => {
            let entity = backup(this)?;
            if !entity.read(cx).dev_idle() {
                return Ok(Outcome::Pending);
            }
        }
        "backup_assert" | "backup_wait" => {
            let entity = backup(this)?;
            let have = entity.read(cx).dev_query(query_name(a), cx)?;
            if !matches(a, &have, b) {
                if name == "backup_wait" {
                    return Ok(Outcome::Pending);
                }
                return fail(format!("{a} 和预期不同"));
            }
        }
        "stub_start" => crate::sync_stub::start()?,
        "stub_reset" => crate::sync_stub::reset_cloud()?,
        // stub_fill:webdav|s3|github|onedrive：把桩的地址和固定凭据填进对应表单（端口不出现在脚本里）。
        "stub_fill" => stub_fill(this, a, window, cx)?,
        // stub_knob:touch|rate_limit|delay:<毫秒>|od:approve|deny|hang
        "stub_knob" => stub_knob(a, b)?,
        // stub_mark：记下桩现在收到的请求数，之后用 stub_assert:since_mark:=0 看这段时间有没有新请求。
        "stub_mark" => {
            let count = crate::sync_stub::with_state(|state| state.requests())?;
            *STUB_MARK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = count;
        }
        // stub_assert:requests>=n 之类，见 `stub_assert`。
        "stub_assert" => return stub_assert(a, b),
        // peer:<存储>:<操作>：另一台电脑做一件事再同步（可能要几秒，期间界面不动）。
        "peer" => {
            let runtime = crate::ssh_backend::runtime()?;
            let handle = runtime.handle().clone();
            crate::sync_stub::peer_run(&handle, a, b)?;
        }
        "local_hosts" => {
            let count: usize = a.parse().map_err(|_| "local_hosts:<个数>".to_owned())?;
            crate::sync_stub::local_hosts(count)?;
        }
        "local_rename" => crate::sync_stub::local_rename(a, b)?,
        "local_corrupt_baseline" => crate::sync_stub::corrupt_baseline()?,
        "assert_local_hosts" => {
            let want: usize = a
                .parse()
                .map_err(|_| "assert_local_hosts:<个数>".to_owned())?;
            let have = crate::sync_stub::local_host_count()?;
            if have != want {
                return fail(format!("本机有 {have} 台主机，预期 {want}"));
            }
        }
        "assert_local_host" => {
            let present = crate::sync_stub::local_has_host(a)?;
            match (b, present) {
                ("yes", true) | ("no", false) => {}
                _ => return fail("本机主机和预期不同"),
            }
        }
        // sync_bar:visible:true|false / has:<文字> / tone:<idle|ok|busy|warn|error>：等到成立。
        "sync_bar" => {
            let (_, model) = crate::sync_demo::bar_model();
            let ok = match a {
                "visible" => model.visible.to_string() == b,
                "has" => model.visible && model.text.contains(b),
                "tone" => model.visible && format!("{:?}", model.tone).to_lowercase() == b,
                _ => return fail("sync_bar 只能是 visible / has / tone"),
            };
            if !ok {
                return Ok(Outcome::Pending);
            }
        }
        "sync_bar_click" => this.open_sync_settings(window, cx),
        // overview_assert:<项>:<值>：概览里有没有这些内容（不截图，避免把服务器上的进程名拍进图里）。
        // 项：cores / mounts / ifaces / procs 是个数（`>=n`），os / kernel / cpu_model 是「非空」，
        // cpu / net_rate 是「有值」；条件还没成立就等。
        "overview_assert" => {
            let Some(summary) = this.status.as_ref().and_then(|s| s.summary.as_ref()) else {
                return Ok(Outcome::Pending);
            };
            let stats = &summary.stats;
            let at_least = |have: usize| -> Result<bool, String> {
                let want: usize = b
                    .trim_start_matches(">=")
                    .parse()
                    .map_err(|_| "写成 >=3".to_owned())?;
                Ok(have >= want)
            };
            let ok = match a {
                "cores" => at_least(stats.per_core_percent.len())?,
                "mounts" => at_least(stats.mounts.len())?,
                "ifaces" => at_least(stats.net_interfaces.len())?,
                "procs" => at_least(stats.top_processes.len())?,
                "os" => !stats.os_name.is_empty(),
                "kernel" => !stats.kernel.is_empty(),
                "cpu_model" => stats.cpu_model.as_deref().is_some_and(|m| !m.is_empty()),
                "cpu" => stats.cpu_percent.is_some(),
                "net_rate" => summary.net_rate.is_some(),
                _ => return fail("overview_assert 不认识这一项"),
            };
            if !ok {
                return Ok(Outcome::Pending);
            }
        }
        // overview_scroll:<像素>：服务器概览面板的内容区往下滚到这个位置（0 = 回到顶）。
        "overview_scroll" => {
            let at: f32 = a.parse().map_err(|_| "overview_scroll:<像素>".to_owned())?;
            this.docks
                .overview_scroll
                .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(-at)));
            cx.notify();
        }
        // settings_scroll_by:<像素>：设置页在现在的位置上再往下滚 N 像素（截图时把某张卡放进视野）。
        "settings_scroll_by" => {
            let delta: f32 = a
                .parse()
                .map_err(|_| "settings_scroll_by:<像素>".to_owned())?;
            let mut offset = this.settings_scroll.offset();
            offset.y -= ::gpui::px(delta);
            this.settings_scroll.set_offset(offset);
            cx.notify();
        }
        _ => return fail("还没有实现"),
    }
    Ok(Outcome::Done)
}

/// `assert_status_polls` 的三个参数：（最少采集, 最多采集, 最多拨号）。`a`:`b` 是前两个，第三个写在
/// `b` 的后面（`最多采集:最多拨号`）。
fn polls_args(a: &str, b: &str) -> Result<(u64, u64, u64), String> {
    let usage = || "参数写成 assert_status_polls:最少采集:最多采集:最多拨号".to_owned();
    let (max_collects, max_dials) = b.split_once(':').ok_or_else(usage)?;
    let number = |text: &str| text.trim().parse::<u64>().map_err(|_| usage());
    Ok((number(a)?, number(max_collects)?, number(max_dials)?))
}

fn stub_fill(
    this: &mut MainWindow,
    kind: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Result<(), String> {
    use crate::sync_stub as stub;
    let entity = this
        .sync_settings
        .clone()
        .ok_or("设置页还没有打开过（先 view:settings）")?;
    let fields: Vec<(&str, String)> = match kind {
        "webdav" => vec![
            ("url", stub::webdav_url()?),
            ("directory", "zssh".to_owned()),
            ("username", stub::DAV_USER.to_owned()),
            ("password", stub::DAV_PASS.to_owned()),
        ],
        "s3" => vec![
            ("s3_endpoint", stub::s3_endpoint()?),
            ("s3_region", "us-east-1".to_owned()),
            ("s3_bucket", stub::S3_BUCKET.to_owned()),
            ("s3_access", stub::S3_ACCESS.to_owned()),
            ("s3_secret", stub::S3_SECRET.to_owned()),
            ("s3_prefix", "z-ssh".to_owned()),
        ],
        "github" => vec![
            ("gh_api", stub::github_api()?),
            ("gh_token", stub::GH_TOKEN.to_owned()),
        ],
        "onedrive" => vec![("od_client", stub::OD_CLIENT_ID.to_owned())],
        _ => return Err("存储只能是 webdav / s3 / github / onedrive".to_owned()),
    };
    entity.update(cx, |sync, cx| {
        sync.dev_do("provider", kind, window, cx)?;
        for (field, value) in &fields {
            sync.dev_set(field, value, window, cx)?;
        }
        Ok(())
    })
}

fn stub_knob(name: &str, value: &str) -> Result<(), String> {
    use crate::sync_stub::{OneDriveMode, with_state};
    match name {
        "touch" => with_state(|state| state.touch_after_next_read = true),
        // rate_limit:<n>：接下来 n 个请求回 429（不写 n 就是 50，写 0 关掉）。
        "rate_limit" => {
            let count: u32 = if value.is_empty() {
                50
            } else {
                value.parse().map_err(|_| "rate_limit:<个数>".to_owned())?
            };
            with_state(|state| state.rate_limit_left = count)
        }
        "delay" => {
            let millis: u64 = value.parse().map_err(|_| "delay:<毫秒>".to_owned())?;
            with_state(|state| state.delay_ms = millis)
        }
        // auth_revoked:on|off：云端把凭据撤销了（本地设置没变，请求一律按凭据无效回）。
        "auth_revoked" => {
            let on = match value {
                "on" => true,
                "off" => false,
                _ => return Err("auth_revoked:on|off".to_owned()),
            };
            with_state(|state| state.auth_revoked = on)
        }
        "od" => {
            let mode = match value {
                "approve" => OneDriveMode::Approve,
                "deny" => OneDriveMode::Deny,
                "hang" => OneDriveMode::Hang,
                _ => return Err("od:approve|deny|hang".to_owned()),
            };
            with_state(|state| state.set_onedrive_mode(mode))
        }
        _ => Err("开关只能是 touch / rate_limit / delay / od".to_owned()),
    }
}

/// stub_assert:<项>:<值>。项：
/// - `package_gets` / `package_puts` / `requests` / `since_mark`：对同步包的请求数、全部请求数、`stub_mark` 之后的
///   请求数（`>=n` / `<=n` / `=n`）；
/// - `backup_puts`：对云端备份对象（`zeeterm-backup.json`）的写请求数；
/// - `first_put_if_none_match`：第一次写同步包带的是 `If-None-Match: *`；
/// - `later_puts_if_match`：之后每次写都带 `If-Match`；
/// - `client_id_seen`：授权页收到过这个 client ID（`default` = 内置 / 桩的那个）；
/// - `opened`：打开过的授权网址个数（`>=n`）。
fn stub_assert(key: &str, value: &str) -> Result<Outcome, String> {
    let number = |text: &str| -> Result<(char, usize), String> {
        let (op, digits) = match text.chars().next() {
            Some(op @ ('>' | '<' | '=')) => (op, &text[1..]),
            _ => ('=', text),
        };
        let digits = digits.trim_start_matches('=');
        digits
            .parse::<usize>()
            .map(|n| (op, n))
            .map_err(|_| "写成 >=3 / <=2 / =1".to_owned())
    };
    let check = |have: usize, spec: &str| -> Result<bool, String> {
        let (op, want) = number(spec)?;
        Ok(match op {
            '>' => have >= want,
            '<' => have <= want,
            _ => have == want,
        })
    };
    let ok = crate::sync_stub::with_state(|state| -> Result<bool, String> {
        Ok(match key {
            "package_gets" => check(state.package_requests("GET"), value)?,
            "package_puts" => check(state.package_requests("PUT"), value)?,
            "requests" => check(state.requests(), value)?,
            "since_mark" => {
                let mark = *STUB_MARK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                check(state.requests().saturating_sub(mark), value)?
            }
            "backup_puts" => check(state.object_requests("PUT", "zeeterm-backup.json"), value)?,
            "first_put_if_none_match" => state
                .package_puts()
                .first()
                .is_some_and(|(_, none)| none.as_deref() == Some("*")),
            "later_puts_if_match" => {
                let puts = state.package_puts();
                puts.len() > 1 && puts.iter().skip(1).all(|(matched, _)| matched.is_some())
            }
            "client_id_seen" => {
                state
                    .client_ids_seen()
                    .iter()
                    .any(|seen| seen == crate::sync_stub::OD_CLIENT_ID)
                    == (value == "stub")
            }
            "opened" => check(state.opened_urls().len(), value)?,
            _ => return Err("不认识的桩断言".to_owned()),
        })
    })??;
    if ok {
        Ok(Outcome::Done)
    } else {
        // 桩的请求是异步到的：条件还没成立就等，等不到由单步上限判失败。
        Ok(Outcome::Pending)
    }
}
