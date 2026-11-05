//! 调试驱动里 V2-U21（「所有网络连接」右键菜单：断开连接、结束进程）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里只剩空结构和几个空函数。
//!
//! 没有真实服务器时，用假数据喂界面（假数据走和真数据同一条解析 / 绘制路径）。地址只有文档保留段
//! （192.0.2.x / 198.51.100.x / 203.0.113.x / 2001:db8::），进程名是 web-server / app-worker 这类通用占位：
//! - `fake_u21:<root|nonroot|netstat|none>`：连接表和进程表一起换成 U21 的夹具（连接里的 PID 和进程表对得上；
//!   `192.0.2.10:22 ← 198.51.100.7:51234` 是 ZeeTerm 自己的那条 SSH 连接，`$SSH_CONNECTION` 指向它；
//!   `none` = 取消假数据）；同时把「断开」的假结果设成成功、把「结束进程」的假结果设成成功；
//! - `fake_ss:<ok|unsupported|denied|nomatch|gone|nossh|failed>`：下一次「断开这条连接」的结果（`ok` 会把这一行从
//!   假的连接表里删掉，确认刷新时就看不到它了）；
//! - `conn21:<select|ask_disconnect|ask_kill|confirm|confirm_force|cancel>[:<本地>,<远端>[,<pid>]]`：对话框的操作
//!   （右键菜单和按钮走真点击，这里给键盘 / 鼠标够不着的，也用来直接模拟"有人强行调用"）；
//! - `assert_conn21:<ask|notice|selected|verify|menu|procs|processes_selected>:<值>`：对话框状态。
//!   `menu:<本地>,<远端>,<标签前缀>,<on|off>[/原因里的字]` = 这一行菜单里某一项能不能点。

use zssh_core::ssh::{DisconnectError, NetConn};

use super::dev_acts_u12::DevFake;

/// 下一次「断开」的假结果。
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SsPlan {
    /// 成功（连接从假的列表里消失）。
    #[default]
    Ok,
    NotSupported,
    Denied,
    NoMatch,
    Gone,
    NoSs,
    Failed,
}

/// 假的「断开」结果（debug 构建有内容，release 是空的）。
#[derive(Default)]
pub struct FakeDisconnect {
    #[cfg(debug_assertions)]
    pub plan: Option<SsPlan>,
}

#[cfg(debug_assertions)]
impl DevFake {
    /// 假的断开结果；没有启用假数据时是 `None`（走真实路径）。
    pub fn disconnect_result(&mut self, row: &NetConn) -> Option<Result<(), DisconnectError>> {
        let plan = self.disconnect.plan?;
        Some(match plan {
            SsPlan::Ok => {
                // 删掉假输出里这一条连接的那一行（本地、远端两个词相邻）。
                if let Some(Ok(output)) = self.connections.as_mut() {
                    let (local, remote) = (row.local.display(), row.remote.display());
                    let kept: Vec<&str> = output
                        .lines()
                        .filter(|line| {
                            let words: Vec<&str> = line.split_whitespace().collect();
                            !words
                                .windows(2)
                                .any(|pair| pair[0] == local && pair[1] == remote)
                        })
                        .collect();
                    *output = kept.join("\n") + "\n";
                }
                Ok(())
            }
            SsPlan::NotSupported => Err(DisconnectError::NotSupported),
            SsPlan::Denied => Err(DisconnectError::NoPermission),
            SsPlan::NoMatch => Err(DisconnectError::NoMatch),
            SsPlan::Gone => Err(DisconnectError::Gone),
            SsPlan::NoSs => Err(DisconnectError::NoSs),
            SsPlan::Failed => Err(DisconnectError::Failed("ss: 没有成功".to_owned())),
        })
    }
}

#[cfg(not(debug_assertions))]
impl DevFake {
    pub fn disconnect_result(&mut self, _row: &NetConn) -> Option<Result<(), DisconnectError>> {
        None
    }
}

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::ssh::KillSignal;

    use super::super::MainWindow;
    use super::super::connections_ui::{RowKey, row_menu};
    use super::super::dev_acts_u18::KillPlan;
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use super::super::processes_ui::NoticeLevel;
    use super::SsPlan;

    const KNOWN: &[&str] = &["fake_u21", "fake_ss", "conn21", "assert_conn21"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    fn check(ok: bool, what: String) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else {
            Err(format!("断言没有成立：{what}"))
        }
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(match name {
            "fake_u21" => fake_u21(this, a),
            "fake_ss" => fake_ss(this, a),
            "conn21" => conn21(this, a, b, window, cx),
            _ => assert_conn21(this, a, b),
        })
    }

    // ---- 假数据 ----

    /// 一台 root 登录的服务器（`ss -tunap`）：覆盖监听 / 已建立 / 各种半关状态 / UDP / IPv6 / 多 PID / 受保护进程 /
    /// 自己的 SSH 连接。连接里的 PID 在 [`PROCS_ROOT`] 里都有。
    const SS_ROOT: &str = "\
@@ZSSH uid 0
@@ZSSH ssh_connection 198.51.100.7 51234 192.0.2.10 22
@@ZSSH source ss
udp   UNCONN 0      0       127.0.0.53%lo:53         0.0.0.0:*     users:((\"resolver\",pid=620,fd=13))
tcp   LISTEN 0      511           0.0.0.0:80         0.0.0.0:*     users:((\"web-server\",pid=1290,fd=6),(\"web-server\",pid=124,fd=6))
tcp   LISTEN 0      4096          0.0.0.0:22         0.0.0.0:*     users:((\"sshd\",pid=800,fd=3))
tcp   LISTEN 0      128         127.0.0.1:5432       0.0.0.0:*     users:((\"db-main\",pid=950,fd=5),(\"db-main\",pid=905,fd=5),(\"db-main\",pid=906,fd=5))
tcp   ESTAB  0      0          192.0.2.10:22      198.51.100.7:51234 users:((\"sshd\",pid=3090,fd=4),(\"sshd\",pid=3100,fd=4))
tcp   ESTAB  0      0          192.0.2.10:22      198.51.100.8:40000 users:((\"sshd\",pid=1200,fd=4))
tcp   ESTAB  0      0          192.0.2.10:443     198.51.100.21:40112 users:((\"web-server\",pid=124,fd=11))
tcp   ESTAB  0      36    [2001:db8::10]:443     [2001:db8::77]:40000 users:((\"web-server\",pid=124,fd=13))
tcp   ESTAB  0      0        127.0.0.1:41022      127.0.0.1:5432  users:((\"app-worker\",pid=812,fd=9))
tcp   ESTAB  0      0          192.0.2.10:514     198.51.100.40:50514 users:((\"rsyslogd\",pid=777,fd=5))
tcp   ESTAB  0      0          192.0.2.10:9000    198.51.100.50:50900 users:((\"stubborn\",pid=900,fd=3))
tcp   TIME-WAIT 0    0          192.0.2.10:8080    198.51.100.9:33333
tcp   SYN-SENT  0    1          192.0.2.10:45678   203.0.113.5:443  users:((\"job-runner\",pid=1500,fd=8))
tcp   CLOSE-WAIT 1   0          192.0.2.10:8081    198.51.100.30:50000 users:((\"app-worker\",pid=812,fd=14))
";

    /// 非 root（app）登录：只有自己（app）的进程拿得到，别的行的进程列是「—」。
    fn ss_nonroot() -> String {
        let mut out = String::new();
        for line in SS_ROOT.lines() {
            // 自己（app）的进程，加上组可读的 web-server：这几行拿得到进程，别的行进程列是「—」。
            let line = if line.contains("app-worker")
                || line.contains("job-runner")
                || line.contains("stubborn")
                || line.contains("web-server")
            {
                line.to_owned()
            } else if let Some((before, _)) = line.split_once(" users:(") {
                before.to_owned()
            } else {
                line.to_owned()
            };
            out.push_str(&line);
            out.push('\n');
        }
        out.replace("@@ZSSH uid 0", "@@ZSSH uid 1000")
    }

    const SS_NETSTAT: &str = "\
@@ZSSH uid 0
@@ZSSH ssh_connection 198.51.100.7 51234 192.0.2.10 22
@@ZSSH source netstat
Active Internet connections (servers and established)
Proto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name
tcp        0      0 0.0.0.0:80              0.0.0.0:*               LISTEN      1290/web-server
tcp        0      0 192.0.2.10:22           198.51.100.7:51234      ESTABLISHED 3090/sshd: root [pr
tcp        0      0 192.0.2.10:443          198.51.100.21:40112     ESTABLISHED 124/web-server
tcp        0      0 192.0.2.10:8081         198.51.100.30:50000     CLOSE_WAIT  812/app-worker
";

    /// 进程表：沿用进程管理（U18）的假进程表，再补一个别人的 sshd 会话（1200）。
    fn procs_root() -> String {
        let mut out = super::super::dev_acts_u18::full_listing().to_owned();
        out.push_str(
            "   1200     800 root      0.0  0.1   11000 Ss       3600 sshd: other [priv]\n",
        );
        out
    }

    fn procs_nonroot() -> String {
        procs_root()
            .replace("@@ZSSH uid 0", "@@ZSSH uid 1000")
            .replace("@@ZSSH user root", "@@ZSSH user app")
    }

    fn fake_u21(this: &mut MainWindow, what: &str) -> Result<Outcome, String> {
        match what {
            "root" => {
                this.dev_fake.connections = Some(Ok(SS_ROOT.to_owned()));
                this.dev_fake.procs.output = Some(procs_root());
            }
            "nonroot" => {
                this.dev_fake.connections = Some(Ok(ss_nonroot()));
                this.dev_fake.procs.output = Some(procs_nonroot());
            }
            "netstat" => {
                this.dev_fake.connections = Some(Ok(SS_NETSTAT.to_owned()));
                this.dev_fake.procs.output = Some(procs_root());
            }
            "none" => {
                this.dev_fake.connections = None;
                this.dev_fake.procs.output = None;
                this.dev_fake.disconnect.plan = None;
                return Ok(Outcome::Done);
            }
            _ => return Err("fake_u21:root|nonroot|netstat|none".to_owned()),
        }
        this.dev_fake.procs.error = None;
        this.dev_fake.procs.kill = KillPlan::Ok;
        this.dev_fake.disconnect.plan = Some(SsPlan::Ok);
        Ok(Outcome::Done)
    }

    fn fake_ss(this: &mut MainWindow, what: &str) -> Result<Outcome, String> {
        this.dev_fake.disconnect.plan = Some(match what {
            "ok" => SsPlan::Ok,
            "unsupported" => SsPlan::NotSupported,
            "denied" => SsPlan::Denied,
            "nomatch" => SsPlan::NoMatch,
            "gone" => SsPlan::Gone,
            "nossh" => SsPlan::NoSs,
            "failed" => SsPlan::Failed,
            _ => return Err("fake_ss:ok|unsupported|denied|nomatch|gone|nossh|failed".to_owned()),
        });
        Ok(Outcome::Done)
    }

    // ---- 对话框 ----

    /// `<本地>,<远端>[,<pid>]`：找到这一行的身份（地址里有冒号和方括号，所以用逗号分隔）。
    fn row_key(this: &MainWindow, args: &str) -> Result<(RowKey, Option<u32>), String> {
        let mut words = args.split(',');
        let (local, remote) = (
            words.next().ok_or("要写 <本地>,<远端>")?,
            words.next().ok_or("要写 <本地>,<远端>")?,
        );
        let pid = match words.next() {
            Some(p) => Some(p.parse::<u32>().map_err(|_| "pid 不是数字".to_owned())?),
            None => None,
        };
        let Some(Modal::Connections(dialog)) = &this.modal else {
            return Err("连接对话框没开着".to_owned());
        };
        let report = dialog.report.as_ref().ok_or("还没有数据")?;
        let row = report
            .rows
            .iter()
            .find(|r| r.local.display() == local && r.remote.display() == remote)
            .ok_or_else(|| format!("表里没有 {local} → {remote}"))?;
        Ok((RowKey::of(row), pid))
    }

    fn conn21(
        this: &mut MainWindow,
        what: &str,
        args: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "select" => {
                let (key, _) = row_key(this, args)?;
                this.connections_select(key, cx);
            }
            "ask_disconnect" => {
                let (key, _) = row_key(this, args)?;
                this.connections_ask_disconnect(&key, cx);
            }
            "ask_kill" => {
                let (key, pid) = row_key(this, args)?;
                this.connections_ask_kill(
                    &key,
                    pid.ok_or("ask_kill 要写 <本地>,<远端>,<pid>")?,
                    cx,
                );
            }
            "confirm" => this.connections_confirm(false, window, cx),
            "confirm_force" => this.connections_confirm(true, window, cx),
            "cancel" => this.connections_cancel_ask(cx),
            _ => {
                return Err(
                    "conn21:select|ask_disconnect|ask_kill|confirm|confirm_force|cancel".to_owned(),
                );
            }
        }
        Ok(Outcome::Done)
    }

    fn assert_conn21(this: &MainWindow, what: &str, want: &str) -> Result<Outcome, String> {
        if what == "processes_selected" {
            let Some(Modal::Processes(dialog)) = &this.modal else {
                return Err("进程管理对话框没开着".to_owned());
            };
            if dialog.loading {
                return Ok(Outcome::Pending);
            }
            let have = dialog
                .selected
                .map_or_else(|| "none".to_owned(), |pid| pid.to_string());
            return check(
                have == want,
                format!("进程管理里选中的是 {have}，不是 {want}"),
            );
        }
        let Some(Modal::Connections(dialog)) = &this.modal else {
            return Err("连接对话框没开着".to_owned());
        };
        // 还在等采集结果时先等一等；结果提示要等动作之后的确认刷新回来。
        if dialog.loading || dialog.procs_loading || (what == "notice" && dialog.verify.is_some()) {
            return Ok(Outcome::Pending);
        }
        match what {
            "ask" => {
                let have = match dialog.ask.as_ref().map(|a| &a.kind) {
                    None => "none",
                    Some(super::super::connections_ui::ConnAskKind::Disconnect(_)) => "disconnect",
                    Some(super::super::connections_ui::ConnAskKind::Kill { signal, .. }) => {
                        match signal {
                            KillSignal::Term => "kill",
                            KillSignal::Kill => "force",
                        }
                    }
                };
                check(have == want, format!("确认框是 {have}，不是 {want}"))
            }
            "notice" => {
                // 写法：`<info|success|warn|error|none>[/文字片段]`
                let (level_want, needle) = want.split_once('/').unwrap_or((want, ""));
                let have = dialog.notice.as_ref().map(|n| match n.level {
                    NoticeLevel::Info => "info",
                    NoticeLevel::Success => "success",
                    NoticeLevel::Warn => "warn",
                    NoticeLevel::Error => "error",
                });
                if level_want == "none" {
                    return check(have.is_none(), format!("不该有结果提示：{have:?}"));
                }
                let text = dialog
                    .notice
                    .as_ref()
                    .map(|n| n.text.as_str())
                    .unwrap_or("");
                check(
                    have == Some(level_want) && text.contains(needle),
                    format!("结果提示是 {have:?}「{text}」，不是 {want}"),
                )
            }
            "selected" => {
                let have = dialog.selected_row().map_or_else(
                    || "none".to_owned(),
                    |r| format!("{},{}", r.local.display(), r.remote.display()),
                );
                check(have == want, format!("选中的是 {have}，不是 {want}"))
            }
            "verify" => check(
                dialog.verify.is_some() == (want == "on"),
                "等待确认的状态不对".to_owned(),
            ),
            "procs" => {
                let have = dialog
                    .procs
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |p| p.rows.len().to_string());
                check(have == want, format!("进程表有 {have} 行，不是 {want}"))
            }
            // menu:<本地>,<远端>,<标签前缀>,<on|off>[/原因里的字]
            "menu" => {
                let mut words = want.splitn(4, ',');
                let (local, remote, label, state) = (
                    words.next().unwrap_or(""),
                    words.next().unwrap_or(""),
                    words.next().unwrap_or(""),
                    words.next().unwrap_or(""),
                );
                let (state, reason) = state.split_once('/').unwrap_or((state, ""));
                let report = dialog.report.as_ref().ok_or("还没有数据")?;
                let row = report
                    .rows
                    .iter()
                    .find(|r| r.local.display() == local && r.remote.display() == remote)
                    .ok_or_else(|| format!("表里没有 {local} → {remote}"))?;
                let entries = row_menu(report, dialog.procs.as_ref(), row);
                let item = entries
                    .iter()
                    .flatten()
                    .find(|i| i.label.starts_with(label))
                    .ok_or_else(|| format!("菜单里没有 {label}"))?;
                let enabled = item.off.is_none();
                check(
                    enabled == (state == "on")
                        && (reason.is_empty()
                            || item.off.as_deref().is_some_and(|r| r.contains(reason))),
                    format!(
                        "「{}」可用 = {enabled}，原因 {:?}，期望 {want}",
                        item.label, item.off
                    ),
                )
            }
            _ => Err(
                "assert_conn21:ask|notice|selected|verify|menu|procs|processes_selected".to_owned(),
            ),
        }
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;
