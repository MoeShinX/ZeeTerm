//! 调试驱动里 V2-U18（进程管理对话框）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩空结构和
//! 几个空函数。
//!
//! 没有真实服务器时，用假数据喂界面（假数据走和真数据同一条解析 / 绘制路径）：
//! - `fake_proc:<full|nonroot|busybox|minimal|proc|many|unsupported|timeout|none>`：进程管理下一次采集拿到的
//!   **脚本原始输出**（`none` = 取消假数据）；用户列只有 root / www-data / app 这类通用名字；
//! - `fake_kill:<ok|ignored|denied|gone|changed|failed>`：下一次「结束进程」的结果（`ok` 会把这个进程从假的列表里
//!   删掉，所以确认刷新时能看到「已结束」；`ignored` 是信号发出去了但进程还在）；
//! - `proc:<open|refresh|close|mode|search|sort|select|ask|confirm|cancel|auto>[:值]`：对话框的操作（按钮走真点击，
//!   这里给键盘 / 鼠标够不着的）；
//! - `assert_proc:<open|count|total|source|error|loading|selected|ask|notice|mode|sort|first_pid|auto|hint|
//!   verify|lock>:<值>`：对话框状态（`lock:<pid>:<原因|none>` = 这个进程有没有被保护）；
//! - `overview_procs:<cpu|mem>` / `assert_overview_procs:<cpu|mem>`：概览面板「进程」一节的分段；
//!   `overview_scroll:<top|bottom>`：把概览面板滚到顶 / 底。

use zssh_core::ssh::{KillError, ProcessReport, ProcessesError};

use super::dev_acts_u12::DevFake;

/// 下一次「结束进程」的假结果。
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KillPlan {
    /// 信号发出去了，进程结束（从假列表里删掉）。
    #[default]
    Ok,
    /// 信号发出去了，进程不理会（还在）。
    Ignored,
    Denied,
    Gone,
    Changed,
    Failed,
}

/// 假的进程管理数据（debug 构建有内容，release 是空的）。
#[derive(Default)]
pub struct FakeProcs {
    #[cfg(debug_assertions)]
    pub output: Option<String>,
    #[cfg(debug_assertions)]
    pub error: Option<ProcessesError>,
    #[cfg(debug_assertions)]
    pub kill: KillPlan,
}

#[cfg(debug_assertions)]
impl DevFake {
    /// 假的采集结果（已解析）。
    pub fn processes_result(&self) -> Option<Result<ProcessReport, ProcessesError>> {
        if let Some(err) = &self.procs.error {
            return Some(Err(err.clone()));
        }
        let output = self.procs.output.as_ref()?;
        Some(zssh_core::ssh::parse_processes(output))
    }

    /// 假的结束结果；没有启用假数据时是 `None`（走真实路径）。
    pub fn kill_result(&mut self, pid: u32) -> Option<Result<(), KillError>> {
        let plan = self.procs.kill;
        let output = self.procs.output.as_mut()?;
        Some(match plan {
            KillPlan::Ok => {
                // 删掉这个进程的那一行（第一个词是 PID）。
                let kept: Vec<&str> = output
                    .lines()
                    .filter(|line| line.split_whitespace().next() != Some(&pid.to_string()))
                    .collect();
                *output = kept.join("\n") + "\n";
                Ok(())
            }
            KillPlan::Ignored => Ok(()),
            KillPlan::Denied => Err(KillError::NoPermission),
            KillPlan::Gone => Err(KillError::NoSuchProcess),
            KillPlan::Changed => Err(KillError::Changed),
            KillPlan::Failed => Err(KillError::Failed("kill: 没有成功".to_owned())),
        })
    }
}

#[cfg(not(debug_assertions))]
impl DevFake {
    pub fn processes_result(&self) -> Option<Result<ProcessReport, ProcessesError>> {
        None
    }

    pub fn kill_result(&mut self, _pid: u32) -> Option<Result<(), KillError>> {
        None
    }
}

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::ssh::{
        KillSignal, PROCESSES_MAX_ROWS, ProcMode, ProcSort, ProcSortKey, ProcessesError,
    };

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use super::super::processes_ui::NoticeLevel;
    use super::KillPlan;

    const KNOWN: &[&str] = &[
        "fake_proc",
        "fake_kill",
        "proc",
        "assert_proc",
        "overview_procs",
        "assert_overview_procs",
        "overview_scroll",
    ];

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
            "fake_proc" => fake_proc(this, a),
            "fake_kill" => fake_kill(this, a),
            "proc" => proc_act(this, a, b, window, cx),
            "assert_proc" => assert_proc(this, a, b),
            "overview_procs" => overview_procs(this, a, cx),
            "overview_scroll" => overview_scroll(this, a, cx),
            _ => assert_overview_procs(this, a),
        })
    }

    // ---- 假数据 ----

    /// 一台 root 登录的服务器（procps 的 `ps`）。4242 是采集脚本的 shell，挂在承载这条 SSH 连接的 sshd 下面
    /// （3100 → 3090 → 800）。
    const FULL: &str = "\
@@ZSSH uid 0
@@ZSSH user root
@@ZSSH self 4242
@@ZSSH memtotal 16000000
@@ZSSH source procps
    PID    PPID USER     %CPU %MEM     RSS STAT ELAPSED COMMAND
    812       1 app      62.3 12.4 1984000 Ssl    93784 /usr/bin/app-worker --queue=high --workers 4
    123       1 app      18.4  3.7  592000 Sl    172700 /usr/bin/cache-store --port 6379
    950       1 db       11.0  9.8 1568000 Ssl   172650 /usr/lib/db-main/db-main -D /var/lib/db-main/data
   1500       1 app       7.5  4.3  688000 S       12000 /usr/bin/job-runner --concurrency 8 --log-level info
    124     123 www-data  3.2  2.5  400000 S       86400 web-server: worker process
   1290       1 www-data  1.8  6.1  976000 S        5000 /usr/sbin/web-server -g daemon on; master_process on;
    620       1 root      0.9  1.2  192000 Ss     172800 /lib/systemd/resolver --foreground
    701       1 avahi     0.4  0.4   64000 Ss     172800 mdns-agent: running [web-1.local]
    800       1 root      0.1  0.6   96000 Ss     172800 sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups
   3090     800 root      0.0  0.1   11000 Ss       3600 sshd: root [priv]
   3100    3090 root      0.0  0.1    9000 S        3600 sshd: root@notty
   4242    3100 root      0.4  0.0    3000 Ss          0 sh -c exec
    900       1 app       0.0  0.2   32000 S         5000 /usr/bin/stubborn --ignore-term
   2000     812 app       0.0  0.0       0 Z         1200 [worker] <defunct>
      1       0 root      0.0  0.3   48000 Ss     172800 /sbin/init splash
      2       0 root      0.0  0.0       0 S      172800 [kthreadd]
     14       2 root      0.0  0.0       0 I      172800 [rcu_sched]
    330       1 root      0.0  0.2   30000 Ss     172800 /lib/systemd/systemd-journald
    402       1 root      0.0  0.1   17000 Ss     172800 /lib/systemd/systemd-udevd
    655       1 root      0.0  0.1   12000 Ss     172800 /usr/sbin/cron -f
    703       1 root      0.0  0.3   42000 Ssl    172800 /usr/lib/unattended-upgrades/unattended-upgrade-shutdown
    777       1 root      0.0  0.2   28000 Ssl    172800 /usr/sbin/rsyslogd -n
    905       1 db        0.0  0.1   18000 S      172650 db-main: checkpointer
    906       1 db        0.0  0.1   17000 S      172650 db-main: background writer
   1801       1 app       0.0  0.0    4000 S        3000 /bin/sh /opt/app/run-healthcheck.sh
";

    /// 非 root 登录（app）：别人的进程只能看、没权限结束。
    fn nonroot() -> String {
        FULL.replace("@@ZSSH uid 0", "@@ZSSH uid 1000")
            .replace("@@ZSSH user root", "@@ZSSH user app")
            .replace("sshd: root [priv]", "sshd: app [priv]")
            .replace("sshd: root@notty", "sshd: app@notty")
            .replace("   3100    3090 root", "   3100    3090 app ")
            .replace("   4242    3100 root", "   4242    3100 app ")
    }

    /// BusyBox 的 `ps -o`：没有 CPU%，内存% 用 RSS 除以总内存算。
    const BUSYBOX: &str = "\
@@ZSSH uid 0
@@ZSSH user root
@@ZSSH self 400
@@ZSSH memtotal 262144
@@ZSSH source procps
@@ZSSH source bb
  PID  PPID USER     STAT   RSS ELAPSED COMMAND
    1     0 root     S      900 12-03:04:05 init
  120     1 root     S     1500 12-03:04:00 /sbin/syslogd -n
  210     1 root     S     5200 12-03:03:58 /usr/sbin/dropbear -F -p 22
  305     1 app      S    24000    03:40:11 /usr/bin/app-worker --queue=high
  311     1 www-data S     8800    03:40:09 web-server: master process
  399   210 root     S     2100       10:02 dropbear
  400   399 root     S     1200       00:00 sh -c exec
  402   400 root     R      900       00:00 ps -o pid,ppid,user,stat,rss,etime,args
";

    /// 最简的 `ps w`：只有 PID / 用户 / 状态 / 命令。
    const MINIMAL: &str = "\
@@ZSSH uid 0
@@ZSSH source bb
PID   USER     VSZ STAT COMMAND
    1 root     1356 S    init
  120 root     3144 S    /sbin/syslogd -n
  210 root     5100 S    /usr/sbin/dropbear -F -p 22
  305 app     24000 S    /usr/bin/app-worker --queue=high
  402 root     1356 R    ps w
";

    /// `ps` 都不能用、读 /proc/*/stat 的回退（命令只有进程名）。
    const PROC: &str = "\
@@ZSSH uid 1000
@@ZSSH user app
@@ZSSH source proc
PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
1 0 root 0.0 0.1 4812 S 86400 systemd
23 1 app 12.5 3.0 31000 R 120 app-worker
24 1 app 0.3 1.1 11000 S 120 tmux: server
";

    fn many() -> String {
        let mut out = String::from(
            "@@ZSSH uid 0\n@@ZSSH user root\n@@ZSSH memtotal 16000000\n@@ZSSH source procps\nPID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND\n",
        );
        for i in 0..(PROCESSES_MAX_ROWS + 300) {
            out.push_str(&format!(
                "{} 1 app {:.1} {:.1} {} S {} app-worker --shard {}\n",
                1000 + i,
                (i % 97) as f64 / 3.0,
                (i % 31) as f64 / 4.0,
                20_000 + (i % 5000) * 7,
                60 + i,
                i
            ));
        }
        out
    }

    fn fake_proc(this: &mut MainWindow, what: &str) -> Result<Outcome, String> {
        let procs = &mut this.dev_fake.procs;
        procs.error = None;
        match what {
            "full" => procs.output = Some(FULL.to_owned()),
            "nonroot" => procs.output = Some(nonroot()),
            "busybox" => procs.output = Some(BUSYBOX.to_owned()),
            "minimal" => procs.output = Some(MINIMAL.to_owned()),
            "proc" => procs.output = Some(PROC.to_owned()),
            "many" => procs.output = Some(many()),
            "unsupported" => procs.error = Some(ProcessesError::Unsupported),
            "timeout" => procs.error = Some(ProcessesError::Timeout),
            "none" => procs.output = None,
            _ => {
                return Err(
                    "fake_proc:full|nonroot|busybox|minimal|proc|many|unsupported|timeout|none"
                        .to_owned(),
                );
            }
        }
        Ok(Outcome::Done)
    }

    fn fake_kill(this: &mut MainWindow, what: &str) -> Result<Outcome, String> {
        this.dev_fake.procs.kill = match what {
            "ok" => KillPlan::Ok,
            "ignored" => KillPlan::Ignored,
            "denied" => KillPlan::Denied,
            "gone" => KillPlan::Gone,
            "changed" => KillPlan::Changed,
            "failed" => KillPlan::Failed,
            _ => return Err("fake_kill:ok|ignored|denied|gone|changed|failed".to_owned()),
        };
        Ok(Outcome::Done)
    }

    // ---- 概览面板 ----

    fn parse_mode(raw: &str) -> Result<ProcMode, String> {
        match raw {
            "cpu" => Ok(ProcMode::Cpu),
            "mem" => Ok(ProcMode::Mem),
            _ => Err("分段写 cpu 或 mem".to_owned()),
        }
    }

    fn overview_procs(
        this: &mut MainWindow,
        raw: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        this.docks.overview_procs = parse_mode(raw)?;
        cx.notify();
        Ok(Outcome::Done)
    }

    /// 把概览面板滚到底（进程一节在最下面）/ 回到顶。
    fn overview_scroll(
        this: &mut MainWindow,
        to: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match to {
            "bottom" => this.docks.overview_scroll.scroll_to_bottom(),
            "top" => this
                .docks
                .overview_scroll
                .set_offset(::gpui::point(::gpui::px(0.0), ::gpui::px(0.0))),
            _ => return Err("overview_scroll:top|bottom".to_owned()),
        }
        cx.notify();
        Ok(Outcome::Done)
    }

    fn assert_overview_procs(this: &MainWindow, raw: &str) -> Result<Outcome, String> {
        let want = parse_mode(raw)?;
        check(
            this.docks.overview_procs == want,
            format!(
                "概览「进程」分段是 {:?}，不是 {raw}",
                this.docks.overview_procs
            ),
        )
    }

    // ---- 对话框 ----

    fn proc_act(
        this: &mut MainWindow,
        what: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "open" => this.open_processes_dialog(window, cx),
            "refresh" => this.refresh_processes(window, cx),
            "close" => this.close_modal(window, cx),
            "mode" => this.processes_set_mode(parse_mode(value)?, cx),
            "search" => {
                let input = this.proc_query.clone();
                let text = value.to_owned();
                input.update(cx, |state, cx| state.set_value(text, window, cx));
                // 程序里写入文字不一定发 Change 事件：直接重算（真实键入走 key: 步骤）。
                this.processes_refilter(cx);
            }
            "sort" => {
                let key = match value {
                    "pid" => ProcSortKey::Pid,
                    "user" => ProcSortKey::User,
                    "cpu" => ProcSortKey::Cpu,
                    "mem" => ProcSortKey::Mem,
                    "rss" => ProcSortKey::Rss,
                    "state" => ProcSortKey::State,
                    "time" => ProcSortKey::Elapsed,
                    "cmd" => ProcSortKey::Command,
                    _ => {
                        return Err(
                            "排序写 pid / user / cpu / mem / rss / state / time / cmd".to_owned()
                        );
                    }
                };
                this.processes_sort_by(key, cx);
            }
            "select" => {
                let pid: u32 = value.parse().map_err(|_| "select:<pid>".to_owned())?;
                this.processes_select(pid, cx);
            }
            "ask" => {
                let Some(Modal::Processes(dialog)) = &this.modal else {
                    return Err("进程管理对话框没开着".to_owned());
                };
                let pid = dialog.selected.ok_or("先 proc:select:<pid>")?;
                let signal = match value {
                    "term" => KillSignal::Term,
                    "kill" => KillSignal::Kill,
                    _ => return Err("ask:term|kill".to_owned()),
                };
                this.processes_ask_kill(pid, signal, cx);
            }
            "confirm" => this.processes_confirm_kill(window, cx),
            "cancel" => this.processes_cancel_ask(cx),
            "auto" => {
                let on = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err("参数只能是 on 或 off".to_owned()),
                };
                let Some(Modal::Processes(dialog)) = &mut this.modal else {
                    return Err("进程管理对话框没开着".to_owned());
                };
                dialog.auto = on;
                cx.notify();
            }
            _ => {
                return Err(
                    "proc:open|refresh|close|mode|search|sort|select|ask|confirm|cancel|auto[:值]"
                        .to_owned(),
                );
            }
        }
        Ok(Outcome::Done)
    }

    fn assert_proc(this: &MainWindow, what: &str, want: &str) -> Result<Outcome, String> {
        if what == "open" {
            let have = matches!(this.modal, Some(Modal::Processes(_)));
            return check(
                have == (want == "on"),
                format!("进程管理对话框开着 = {have}"),
            );
        }
        let Some(Modal::Processes(dialog)) = &this.modal else {
            return Err("进程管理对话框没开着".to_owned());
        };
        // 还在等采集结果时，除了 loading 本身，其余断言先等一等；结果提示要等结束后的确认刷新回来。
        if (dialog.loading && what != "loading") || (dialog.verify.is_some() && what == "notice") {
            return Ok(Outcome::Pending);
        }
        match what {
            "count" => check(
                dialog.shown.len().to_string() == want,
                format!("显示 {} 个，不是 {want}", dialog.shown.len()),
            ),
            "total" => {
                let have = dialog.report.as_ref().map_or(0, |r| r.rows.len());
                check(have.to_string() == want, format!("共 {have} 个，不是 {want}"))
            }
            "source" => {
                let have = dialog.report.as_ref().map(|r| r.source.label());
                check(
                    have.unwrap_or("none") == want,
                    format!("来源是 {have:?}，不是 {want}"),
                )
            }
            "error" => {
                let have = dialog.error.clone().unwrap_or_default();
                if want == "none" {
                    check(have.is_empty(), "不该有错误".to_owned())
                } else {
                    check(have.contains(want), "错误文字里没有预期的内容".to_owned())
                }
            }
            "loading" => check(dialog.loading == (want == "on"), "loading 状态不对".to_owned()),
            "selected" => {
                let have = dialog
                    .selected
                    .map_or_else(|| "none".to_owned(), |pid| pid.to_string());
                check(have == want, format!("选中的是 {have}，不是 {want}"))
            }
            "ask" => {
                let have = match &dialog.ask {
                    None => "none".to_owned(),
                    Some(ask) => match ask.signal {
                        KillSignal::Term => "term".to_owned(),
                        KillSignal::Kill => "kill".to_owned(),
                    },
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
                let text = dialog.notice.as_ref().map(|n| n.text.as_str()).unwrap_or("");
                check(
                    have == Some(level_want) && text.contains(needle),
                    format!("结果提示是 {have:?}「{text}」，不是 {want}"),
                )
            }
            "mode" => check(
                dialog.mode == parse_mode(want)?,
                format!("分段是 {:?}，不是 {want}", dialog.mode),
            ),
            "sort" => {
                let (key, desc) = dialog.sort.effective(dialog.mode);
                let explicit = dialog.sort != ProcSort::default();
                let have = format!(
                    "{key:?}:{}{}",
                    if desc { "desc" } else { "asc" },
                    if explicit { "" } else { ":default" }
                );
                check(
                    have.eq_ignore_ascii_case(want),
                    format!("排序是 {have}，不是 {want}"),
                )
            }
            "first_pid" => {
                let have = dialog
                    .shown
                    .first()
                    .and_then(|i| dialog.report.as_ref().map(|r| r.rows[*i].pid.to_string()))
                    .unwrap_or_default();
                check(have == want, format!("第一行是 PID {have}，不是 {want}"))
            }
            "auto" => check(dialog.auto == (want == "on"), "自动刷新开关不对".to_owned()),
            "hint" => {
                let have = dialog
                    .report
                    .as_ref()
                    .and_then(super::super::processes_ui::report_hint)
                    .unwrap_or("none");
                check(
                    (want == "none" && have == "none") || (want != "none" && have.contains(want)),
                    format!("提示是「{have}」，不是 {want}"),
                )
            }
            // lock:<pid>:<原因里的字 | none>：这个进程有没有被保护（锁图标 + 按钮置灰）。
            "lock" => {
                let (pid, reason) = want.split_once(':').ok_or("lock:<pid>:<原因|none>")?;
                let pid: u32 = pid.parse().map_err(|_| "lock:<pid>:<原因|none>".to_owned())?;
                let have = dialog
                    .report
                    .as_ref()
                    .and_then(|r| r.protection(pid))
                    .map_or("none", |p| p.reason());
                check(
                    (reason == "none" && have == "none") || (reason != "none" && have.contains(reason)),
                    format!("PID {pid} 的保护原因是「{have}」，不是 {reason}"),
                )
            }
            "verify" => check(
                dialog.verify.is_some() == (want == "on"),
                "等待确认的状态不对".to_owned(),
            ),
            _ => Err(
                "assert_proc:open|count|total|source|error|loading|selected|ask|notice|mode|sort|first_pid|auto|hint|verify"
                    .to_owned(),
            ),
        }
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
