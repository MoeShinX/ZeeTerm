//! 调试驱动里 V2-U12（概览面板调宽、「所有连接」对话框）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里只剩空结构和几个空函数。
//!
//! 没有真实服务器时，用假数据喂界面（假数据走和真数据同一条解析 / 绘制路径）：
//! - `fake_status:<@主机名|id>`：给这台主机塞一份状态采集结果（概览面板就有内容、能打开）；
//! - `fake_conn:<ss|ss_nonroot|netstat|proc|many|unsupported|timeout|none>`：「所有连接」下一次采集拿到的东西
//!   （`none` = 取消假数据）；地址全是文档保留段（192.0.2.x / 198.51.100.x / 203.0.113.x / 2001:db8::）；
//! - `assert_overview:<width|layout|stored|open>:<值>`：概览面板现在的宽度 / 版式（圆环列数,系统信息列数）/
//!   设置表里存的宽度 / 开没开；`overview_width:<像素|default>` 直接写设置表里的宽度；
//! - `conn:<open|refresh|close|filter|search|sort|auto>[:值]`：对话框的操作（按钮走真点击，这里给键盘够不着的）；
//! - `assert_conn:<count|total|source|error|loading|hint|truncated|auto|filter|sort|open>:<值>`：对话框状态。

use zssh_core::ssh::ConnectionsError;

/// 假数据（debug 构建有内容，release 是空的）。
#[derive(Default)]
pub struct DevFake {
    #[cfg(debug_assertions)]
    pub status: Option<(i64, crate::status_demo::HostStatus)>,
    /// 下一次采集要拿到的：`Ok(脚本输出)` 或一个失败原因。
    #[cfg(debug_assertions)]
    pub connections: Option<Result<String, ConnectionsError>>,
    /// 进程管理（V2-U18）的假数据，见 `dev_acts_u18`。
    pub procs: super::dev_acts_u18::FakeProcs,
    /// 断开连接（V2-U21）的假结果，见 `dev_acts_u21`。
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub disconnect: super::dev_acts_u21::FakeDisconnect,
}

#[cfg(debug_assertions)]
impl DevFake {
    /// 假的采集结果（已解析）。
    pub fn connections_result(
        &self,
    ) -> Option<Result<zssh_core::ssh::ConnectionsReport, ConnectionsError>> {
        let fake = self.connections.as_ref()?;
        Some(match fake {
            Ok(output) => zssh_core::ssh::parse_connections(output),
            Err(err) => Err(err.clone()),
        })
    }
}

#[cfg(not(debug_assertions))]
impl DevFake {
    pub fn connections_result(
        &self,
    ) -> Option<Result<zssh_core::ssh::ConnectionsReport, ConnectionsError>> {
        None
    }
}

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::ssh::stats::MountUsage;
    use zssh_core::ssh::{
        CONNECTIONS_MAX_ROWS, ConnFilter, ConnSource, ConnectionsError, ProcessHint, SortKey,
        SortSpec,
    };
    use zssh_core::ssh::{CpuProcessInfo, NetInterfaceSpeed, ProcessInfo, ServerStats};

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use super::super::overview;
    use super::super::panel_width::PanelKind;
    use crate::status_demo::{HostStatus, StatusSummary};

    const KNOWN: &[&str] = &[
        "fake_status",
        "fake_conn",
        "assert_overview",
        "overview_width",
        "conn",
        "assert_conn",
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

    fn on_off(raw: &str) -> Result<bool, String> {
        match raw {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(4, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        let c = parts.next().unwrap_or("");
        // 值里可能有 `:`（IPv6 地址、搜索词），b、c 合起来还原。
        let rest = if c.is_empty() {
            b.to_owned()
        } else {
            format!("{b}:{c}")
        };
        Some(match name {
            "fake_status" => fake_status(this, a, cx),
            "fake_conn" => fake_conn(this, a),
            "assert_overview" => assert_overview(this, a, &rest, cx),
            "overview_width" => overview_width(this, a, cx),
            "conn" => conn(this, a, &rest, window, cx),
            _ => assert_conn(this, a, &rest, cx),
        })
    }

    // ---- 假数据 ----

    fn sample_stats() -> ServerStats {
        let mib = |gb: f64| gb * 1024.0;
        ServerStats {
            cpu_percent: Some(37),
            per_core_percent: vec![
                Some(12),
                Some(64),
                Some(33),
                Some(88),
                Some(5),
                Some(41),
                Some(97),
                Some(20),
            ],
            cpu_cores: Some(8),
            mem_total_mib: Some(15_948),
            mem_used_mib: Some(6_210),
            mem_free_mib: Some(2_048),
            mem_buffers_mib: Some(512),
            mem_cached_mib: Some(7_178),
            swap_total_mib: Some(2_048),
            swap_used_mib: Some(120),
            disk_total_mib: Some(mib(58.0)),
            disk_used_mib: Some(mib(21.4)),
            disk_percent: Some(37),
            net_rx_speed: 48_000.0,
            net_tx_speed: 15_000.0,
            net_rx_total: 7_400_000_000,
            net_tx_total: 2_100_000_000,
            net_interfaces: vec![
                NetInterfaceSpeed {
                    name: "eth0".to_owned(),
                    rx_speed: 48_000.0,
                    tx_speed: 15_000.0,
                    rx_total: 7_400_000_000,
                    tx_total: 2_100_000_000,
                },
                NetInterfaceSpeed {
                    name: "enp0s31f6.4094-long-interface-name".to_owned(),
                    rx_speed: 1_200.0,
                    tx_speed: 800.0,
                    rx_total: 30_000_000,
                    tx_total: 12_000_000,
                },
            ],
            mounts: vec![
                MountUsage {
                    mount: "/".to_owned(),
                    fs_type: "ext4".to_owned(),
                    source: "/dev/sda1".to_owned(),
                    used_mib: mib(21.4),
                    total_mib: mib(58.0),
                    percent: 37,
                },
                MountUsage {
                    mount: "/mnt/data/archive/2026/very-long-mount-point-name".to_owned(),
                    fs_type: "xfs".to_owned(),
                    source: "/dev/mapper/vg-data--archive--volume".to_owned(),
                    used_mib: mib(410.0),
                    total_mib: mib(500.0),
                    percent: 82,
                },
            ],
            top_processes: [
                ("812", 12.4, "app-worker"),
                ("950", 9.8, "db-main"),
                ("1290", 6.1, "a-process-with-an-unusually-long-command-name"),
                ("1500", 4.3, "job-runner"),
                ("123", 3.7, "cache-store"),
                ("124", 2.5, "web-server"),
                ("620", 1.2, "resolver"),
                ("800", 0.6, "sshd"),
                ("701", 0.4, "mdns-agent"),
                ("1", 0.3, "init"),
            ]
            .into_iter()
            .map(|(pid, mem_percent, command)| ProcessInfo {
                pid: pid.to_owned(),
                mem_percent,
                command: command.to_owned(),
            })
            .collect(),
            top_cpu_processes: [
                ("812", 62.3, "app-worker"),
                ("123", 18.4, "cache-store"),
                ("950", 11.0, "db-main"),
                ("1500", 7.5, "job-runner"),
                ("124", 3.2, "web-server"),
                ("1290", 1.8, "a-process-with-an-unusually-long-command-name"),
                ("620", 0.9, "resolver"),
                ("701", 0.4, "mdns-agent"),
                ("800", 0.1, "sshd"),
                ("1", 0.0, "init"),
            ]
            .into_iter()
            .map(|(pid, cpu_percent, command)| CpuProcessInfo {
                pid: pid.to_owned(),
                cpu_percent,
                command: command.to_owned(),
            })
            .collect(),
            hostname: "web-1".to_owned(),
            os_name: "Ubuntu 22.04.5 LTS".to_owned(),
            kernel: "5.15.0-202-generic".to_owned(),
            uptime_secs: Some(2 * 86_400 + 3 * 3_600),
            load_1m: Some(0.42),
            load_5m: Some(0.31),
            load_15m: Some(0.27),
            cpu_model: Some("Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz".to_owned()),
        }
    }

    fn fake_status(
        this: &mut MainWindow,
        who: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let host = if let Some(name) = who.strip_prefix('@') {
            this.store
                .hosts()
                .iter()
                .find(|host| host.name == name)
                .map(|host| host.id)
                .ok_or("没有这个名字的主机")?
        } else {
            who.parse::<i64>().map_err(|_| "主机写 @名字 或 id")?
        };
        let status = HostStatus {
            summary: Some(StatusSummary {
                stats: sample_stats(),
                net_rate: Some((48_000.0, 15_000.0)),
            }),
            error: None,
            revision: 1,
        };
        this.dev_seed_net(host, 1, net_series());
        this.dev_fake.status = Some((host, status));
        cx.notify();
        Ok(Outcome::Done)
    }

    /// 一条像样的网速曲线（下行起伏、上行小一些）。
    fn net_series() -> Vec<(f64, f64)> {
        (0..60)
            .map(|i| {
                let t = f64::from(i);
                let rx = 30_000.0
                    + 25_000.0 * (t / 6.0).sin().abs()
                    + if i % 17 == 0 { 40_000.0 } else { 0.0 };
                let tx = 8_000.0 + 6_000.0 * (t / 9.0).cos().abs();
                (rx, tx)
            })
            .collect()
    }

    const SS_FIXTURE: &str = "\
@@ZSSH uid 0
@@ZSSH source ss
udp   UNCONN 0      0       127.0.0.53%lo:53         0.0.0.0:*     users:((\"resolver\",pid=620,fd=13))
udp   UNCONN 0      0                   *:5353             *:*     users:((\"mdns-agent\",pid=701,fd=12))
udp   UNCONN 0      0   [fe80::1]%eth0:546              [::]:*     users:((\"netd\",pid=588,fd=22))
tcp   LISTEN 0      511           0.0.0.0:80         0.0.0.0:*     users:((\"web-server\",pid=123,fd=6),(\"web-server\",pid=124,fd=6))
tcp   LISTEN 0      511           0.0.0.0:443        0.0.0.0:*     users:((\"web-server\",pid=123,fd=7),(\"web-server\",pid=124,fd=7))
tcp   LISTEN 0      4096             [::]:22            [::]:*     users:((\"sshd\",pid=800,fd=4))
tcp   LISTEN 0      4096          0.0.0.0:22          0.0.0.0:*     users:((\"sshd\",pid=800,fd=3))
tcp   LISTEN 0      128         127.0.0.1:5432       0.0.0.0:*     users:((\"db-main\",pid=950,fd=5))
tcp   ESTAB  0      0          192.0.2.10:22      198.51.100.7:51234 users:((\"sshd\",pid=1200,fd=4),(\"sshd\",pid=1180,fd=4))
tcp   ESTAB  0      0          192.0.2.10:443     198.51.100.21:40112 users:((\"web-server\",pid=124,fd=11))
tcp   ESTAB  0      0          192.0.2.10:443     198.51.100.22:40118 users:((\"web-server\",pid=124,fd=12))
tcp   ESTAB  0      36    [2001:db8::10]:443     [2001:db8:1234:5678:9abc:def0:1111:2222]:40000 users:((\"web-server\",pid=124,fd=13))
tcp   ESTAB  0      0        127.0.0.1:41022      127.0.0.1:5432  users:((\"app-worker\",pid=812,fd=9))
tcp   TIME-WAIT 0    0          192.0.2.10:443     203.0.113.9:33333
tcp   TIME-WAIT 0    0          192.0.2.10:443     203.0.113.10:33340
tcp   SYN-SENT  0    1          192.0.2.10:45678   203.0.113.5:443  users:((\"updater\",pid=1500,fd=8))
tcp   CLOSE-WAIT 1   0          192.0.2.10:8080    198.51.100.30:50000 users:((\"app-worker\",pid=812,fd=14))
";

    /// 非 root 登录：只有自己的进程拿得到，别的行没有 users:。
    const SS_NONROOT_FIXTURE: &str = "\
@@ZSSH uid 1000
@@ZSSH source ss
udp   UNCONN 0      0       127.0.0.53%lo:53         0.0.0.0:*
tcp   LISTEN 0      511           0.0.0.0:80         0.0.0.0:*
tcp   LISTEN 0      4096             [::]:22            [::]:*
tcp   LISTEN 0      128         127.0.0.1:5432       0.0.0.0:*
tcp   ESTAB  0      0          192.0.2.10:22      198.51.100.7:51234 users:((\"sshd\",pid=1200,fd=4))
tcp   ESTAB  0      0          192.0.2.10:443     198.51.100.21:40112
tcp   TIME-WAIT 0    0          192.0.2.10:443     203.0.113.9:33333
";

    const NETSTAT_FIXTURE: &str = "\
@@ZSSH uid 0
@@ZSSH source netstat
Active Internet connections (servers and established)
Proto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name
tcp        0      0 0.0.0.0:22              0.0.0.0:*               LISTEN      800/sshd
tcp6       0      0 :::80                   :::*                    LISTEN      123/web-server: mast
tcp        0      0 192.0.2.10:22           198.51.100.7:51234      ESTABLISHED 1200/sshd: user@pts
tcp        0      0 192.0.2.10:8080         198.51.100.9:33333      TIME_WAIT   -
tcp6       0      0 2001:db8::10:443        2001:db8::77:40000      ESTABLISHED -
udp        0      0 127.0.0.53:53           0.0.0.0:*                           620/resolver
udp6       0      0 fe80::1:546             :::*                                -
";

    const PROC_FIXTURE: &str = "\
@@ZSSH uid 1000
@@ZSSH source proc
@@ZSSH file tcp
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   101        0 17583 1 0000000000000000 100 0 0 10 0
   1: 0A0200C0:0016 076433C6:C822 01 00000000:00000000 02:000A6B2B 00000000     0        0 18000 2 0000000000000000 20 4 29 10 -1
@@ZSSH file tcp6
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 11111 1 0000000000000000 100 0 0 10 0
@@ZSSH file udp
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  11: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 44444 2 0000000000000000 0
@@ZSSH file udp6
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
";

    fn many() -> String {
        let mut out = String::from("@@ZSSH uid 0\n@@ZSSH source ss\n");
        for i in 0..(CONNECTIONS_MAX_ROWS + 500) {
            out.push_str(&format!(
                "tcp ESTAB 0 0 192.0.2.10:{} 198.51.100.{}:{} users:((\"app-worker\",pid={},fd=9))\n",
                1024 + (i % 60_000),
                1 + (i % 250),
                2000 + (i % 50_000),
                800 + (i % 400)
            ));
        }
        out
    }

    fn fake_conn(this: &mut MainWindow, what: &str) -> Result<Outcome, String> {
        this.dev_fake.connections = match what {
            "ss" => Some(Ok(SS_FIXTURE.to_owned())),
            "ss_nonroot" => Some(Ok(SS_NONROOT_FIXTURE.to_owned())),
            "netstat" => Some(Ok(NETSTAT_FIXTURE.to_owned())),
            "proc" => Some(Ok(PROC_FIXTURE.to_owned())),
            "many" => Some(Ok(many())),
            "unsupported" => Some(Err(ConnectionsError::Unsupported)),
            "timeout" => Some(Err(ConnectionsError::Timeout)),
            "none" => None,
            _ => {
                return Err(
                    "fake_conn:ss|ss_nonroot|netstat|proc|many|unsupported|timeout|none".to_owned(),
                );
            }
        };
        Ok(Outcome::Done)
    }

    // ---- 概览面板 ----

    fn assert_overview(
        this: &MainWindow,
        what: &str,
        want: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "open" => {
                let have = this.docks.overview_open && this.status_host.is_some();
                check(have == (want == "on"), format!("概览面板开着 = {have}"))
            }
            "width" => {
                let have = this.overview_width_now(cx);
                check(
                    format!("{have}") == want,
                    format!("概览面板宽度是 {have}，不是 {want}"),
                )
            }
            "layout" => {
                let layout = overview::layout_for(this.overview_width_now(cx));
                let have = format!("{},{}", layout.ring_columns, layout.system_columns);
                check(have == want, format!("版式是 {have}，不是 {want}"))
            }
            "stored" => {
                let have = this.store.bars().overview_width;
                check(
                    format!("{have}") == want,
                    format!("设置表里存的宽度是 {have}，不是 {want}"),
                )
            }
            _ => Err("assert_overview:open|width|layout|stored:<值>".to_owned()),
        }
    }

    fn overview_width(
        this: &mut MainWindow,
        what: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let width: f32 = if what == "default" {
            PanelKind::Overview.default_width()
        } else {
            what.parse().map_err(|_| "宽度写像素数或 default")?
        };
        let key = PanelKind::Overview.setting_key();
        this.store
            .write_bar_setting(|db| super::super::bar_settings::write_number(db, key, width))?;
        cx.notify();
        Ok(Outcome::Done)
    }

    // ---- 连接对话框 ----

    fn conn(
        this: &mut MainWindow,
        what: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "open" => this.open_connections_dialog(window, cx),
            "refresh" => this.refresh_connections(window, cx),
            "close" => this.close_modal(window, cx),
            "filter" => {
                let filter = ConnFilter::ALL
                    .iter()
                    .copied()
                    .find(|f| f.label() == value || format!("{f:?}").eq_ignore_ascii_case(value))
                    .ok_or("筛选写 全部 / TCP / UDP / 监听 / 已建立（或 all/tcp/udp/listen/established）")?;
                let Some(Modal::Connections(dialog)) = &mut this.modal else {
                    return Err("连接对话框没开着".to_owned());
                };
                dialog.filter = filter;
                this.connections_refilter(cx);
            }
            "search" => {
                let input = this.conn_query.clone();
                let text = value.to_owned();
                input.update(cx, |state, cx| state.set_value(text, window, cx));
                // 程序里写入文字不一定发 Change 事件：直接重算（真实键入走 key: 步骤，靠的是事件）。
                this.connections_refilter(cx);
            }
            "sort" => {
                let key = match value {
                    "proto" => SortKey::Proto,
                    "local" => SortKey::Local,
                    "remote" => SortKey::Remote,
                    "state" => SortKey::State,
                    "process" => SortKey::Process,
                    _ => return Err("排序写 proto / local / remote / state / process".to_owned()),
                };
                let Some(Modal::Connections(dialog)) = &mut this.modal else {
                    return Err("连接对话框没开着".to_owned());
                };
                dialog.sort = dialog.sort.toggled(key);
                this.connections_refilter(cx);
            }
            "auto" => {
                let on = on_off(value)?;
                let Some(Modal::Connections(dialog)) = &mut this.modal else {
                    return Err("连接对话框没开着".to_owned());
                };
                dialog.auto = on;
                cx.notify();
            }
            _ => return Err("conn:open|refresh|close|filter|search|sort|auto[:值]".to_owned()),
        }
        Ok(Outcome::Done)
    }

    fn assert_conn(
        this: &MainWindow,
        what: &str,
        want: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        if what == "open" {
            let have = matches!(this.modal, Some(Modal::Connections(_)));
            return check(have == (want == "on"), format!("连接对话框开着 = {have}"));
        }
        let Some(Modal::Connections(dialog)) = &this.modal else {
            return Err("连接对话框没开着".to_owned());
        };
        // 还在等采集结果的时候，除了 loading 本身，其余断言都先等一等。
        if dialog.loading && what != "loading" {
            return Ok(Outcome::Pending);
        }
        let _ = cx;
        match what {
            "count" => check(
                dialog.shown.len().to_string() == want,
                format!("显示 {} 条，不是 {want}", dialog.shown.len()),
            ),
            "total" => {
                let have = dialog.report.as_ref().map_or(0, |r| r.rows.len());
                check(
                    have.to_string() == want,
                    format!("共 {have} 条，不是 {want}"),
                )
            }
            "source" => {
                let have = dialog.report.as_ref().map(|r| r.source);
                let want_source = match want {
                    "ss" => Some(ConnSource::Ss),
                    "netstat" => Some(ConnSource::Netstat),
                    "proc" => Some(ConnSource::Proc),
                    "none" => None,
                    _ => return Err("来源写 ss / netstat / proc / none".to_owned()),
                };
                check(have == want_source, format!("来源是 {have:?}，不是 {want}"))
            }
            "error" => {
                let have = dialog.error.clone().unwrap_or_default();
                if want == "none" {
                    check(have.is_empty(), "不该有错误".to_owned())
                } else {
                    check(have.contains(want), "错误文字里没有预期的内容".to_owned())
                }
            }
            "loading" => check(
                dialog.loading == (want == "on"),
                "loading 状态不对".to_owned(),
            ),
            "hint" => {
                let have = dialog.report.as_ref().and_then(|r| r.process_hint());
                let want_hint = match want {
                    "none" => None,
                    "not_root" => Some(ProcessHint::NotRoot),
                    "no_source" => Some(ProcessHint::NoProcessSource),
                    _ => return Err("提示写 none / not_root / no_source".to_owned()),
                };
                check(
                    have == want_hint,
                    format!("进程提示是 {have:?}，不是 {want}"),
                )
            }
            "truncated" => {
                let have = dialog.report.as_ref().is_some_and(|r| r.truncated);
                check(have == (want == "on"), format!("截断标记是 {have}"))
            }
            "auto" => check(dialog.auto == (want == "on"), "自动刷新开关不对".to_owned()),
            "filter" => check(
                dialog.filter.label() == want,
                format!("筛选是 {}，不是 {want}", dialog.filter.label()),
            ),
            "sort" => {
                let have = match dialog.sort {
                    SortSpec { key: None, .. } => "none".to_owned(),
                    SortSpec {
                        key: Some(key),
                        descending,
                    } => {
                        format!("{key:?}:{}", if descending { "desc" } else { "asc" })
                    }
                };
                check(
                    have.eq_ignore_ascii_case(want),
                    format!("排序是 {have}，不是 {want}"),
                )
            }
            "first_local_port" => {
                let have = dialog
                    .shown
                    .first()
                    .and_then(|i| {
                        dialog
                            .report
                            .as_ref()
                            .map(|r| r.rows[*i].local.port.clone())
                    })
                    .unwrap_or_default();
                check(
                    have == want,
                    format!("第一行本地端口是 {have}，不是 {want}"),
                )
            }
            _ => Err(
                "assert_conn:count|total|source|error|loading|hint|truncated|auto|filter|sort|open"
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
