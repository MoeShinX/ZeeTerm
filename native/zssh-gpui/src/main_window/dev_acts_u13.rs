//! 调试驱动里 V2-U13（主机属性单栏表单、测试连接）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里只剩两个空函数。按钮都用真点击，这里给键盘 / 鼠标够不着的：
//!
//! - `host_row:<行>`：展开该行所在的折叠区并滚到它（行名见 `host_props::Row::id`：`name` `endpoint` `credentials`
//!   `routing` `routing_detail` `x11` `legacy` `fingerprint` `telnet_term` `serial_framing` `memory` ……）；
//! - `host_fold:<advanced|memory>:<on|off>`：展开 / 折叠高级设置 / AI 记忆；
//! - `host_conn:<direct|jump|proxy>`：点「连接方式」分段；
//! - `assert_u13:<rows|conn_mode|advanced|memory_open|keeps_proxy|saved_routing|test>:<值>`：
//!   表单现在从上到下的行（逗号分隔）/ 连接方式 / 折叠状态 / 旧数据里跳板机和代理并存 /
//!   保存时会写进库的 `代理方式,代理,跳板机` / 测试连接（`running` `ok` `fail` `none`，或 `contains:<文字>`
//!   结果行里应有的文字）；
//! - `host_db_routing:<@主机>:<default|none|custom>:<代理 id|none>:<@跳板机|none>`：直接改库里这台主机的代理 /
//!   跳板机设置（造「旧数据里跳板机和代理都设了」这种界面已经设不出来的组合）；
//! - `assert_stored_routing:<@主机>:<代理方式,代理,跳板机>`：从库里**重新读**这台主机的这三个字段；
//! - `host_test:<run|cancel>`：点「测试连接」/「取消」；
//! - `host_fake_test:<种类>`：摆一个测试结果（只改界面状态，不联网）。SSH 没有真服务器时截图用，文案走核心库
//!   同一批函数：`ssh_ok` `ssh_untrusted` `ssh_auth_failed` `ssh_refused` `ssh_dns` `ssh_proxy` `ssh_jump`
//!   `ssh_hostkey` `ssh_timeout` `telnet_ok` `serial_ok` `serial_missing`。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::error::{HostKeyMismatch, ZsshError};
    use zssh_core::ssh::connection_test::{
        FailurePhase, FirstByte, HopRole, Interaction, TestOutcome, TestStage, classify_failure,
        serial_failure, serial_success, ssh_success_headline, tcp_success_headline,
        untrusted_stop_headline,
    };

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::host_form::{ConnMode, effective_routing};
    use super::super::host_props::{self, Row};

    const KNOWN: &[&str] = &[
        "host_row",
        "host_fold",
        "host_conn",
        "assert_u13",
        "host_test",
        "host_fake_test",
        "host_db_routing",
        "assert_stored_routing",
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
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(match name {
            "host_db_routing" => host_db_routing(this, a, b, cx),
            "assert_stored_routing" => assert_stored_routing(this, a, b),
            "host_row" => host_row(this, a, window, cx),
            "host_fold" => host_fold(this, a, b, cx),
            "host_conn" => host_conn(this, a, cx),
            "assert_u13" => assert_u13(this, a, b, cx),
            "host_test" => host_test(this, a, window, cx),
            _ => host_fake_test(this, a, cx),
        })
    }

    fn host_named(this: &MainWindow, raw: &str) -> Result<i64, String> {
        let name = raw.strip_prefix('@').ok_or("主机要写成 @名字")?;
        this.store
            .hosts()
            .iter()
            .find(|host| host.name == name)
            .map(|host| host.id)
            .ok_or_else(|| "没有这个名字的主机".to_owned())
    }

    fn host_db_routing(
        this: &mut MainWindow,
        who: &str,
        rest: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        use zssh_core::model::{HostDraft, ProxyMode};
        let id = host_named(this, who)?;
        let mut parts = rest.splitn(3, ':');
        let mode = match parts.next().unwrap_or("") {
            "default" => ProxyMode::Default,
            "none" => ProxyMode::None,
            "custom" => ProxyMode::Custom,
            _ => return Err("代理方式写 default / none / custom".to_owned()),
        };
        let proxy = match parts.next().unwrap_or("") {
            "none" => None,
            raw => Some(raw.parse::<i64>().map_err(|_| "代理 id 不是数字")?),
        };
        let jump = match parts.next().unwrap_or("") {
            "none" => None,
            raw => Some(host_named(this, raw)?),
        };
        let host = this.store.host_fresh(id)?;
        let draft = HostDraft {
            id: Some(host.id),
            group_id: host.group_id,
            name: host.name.clone(),
            host: host.host.clone(),
            port: host.port,
            username: host.username.clone(),
            auth_method: host.auth_method,
            identity_id: host.identity_id,
            proxy_id: proxy,
            jump_host_id: jump,
            proxy_mode: mode,
            tags: host.tags.clone(),
            sort: host.sort,
            legacy_algorithms: host.legacy_algorithms,
            protocol: None,
        };
        this.store
            .db()?
            .host_save(&draft)
            .map_err(|err| err.to_string())?;
        this.store.reload();
        cx.notify();
        Ok(Outcome::Done)
    }

    fn assert_stored_routing(this: &MainWindow, who: &str, want: &str) -> Result<Outcome, String> {
        let id = host_named(this, who)?;
        let host = this.store.host_fresh(id)?;
        let jump_name = host.jump_host_id.map(|id| {
            this.store
                .host(id)
                .map_or_else(|| id.to_string(), |h| h.name.clone())
        });
        let have = format!(
            "{},{},{}",
            host.proxy_mode.as_str(),
            host.proxy_id.map_or("none".to_owned(), |id| id.to_string()),
            jump_name.map_or("none".to_owned(), |name| format!("@{name}")),
        );
        check(have == want, format!("库里是 {have}，不是 {want}"))
    }

    fn all_rows() -> Vec<Row> {
        use host_props::AdvRow::*;
        let mut rows = vec![
            Row::Name,
            Row::Protocol,
            Row::Endpoint,
            Row::AuthMethod,
            Row::Credentials,
            Row::AutoLogin,
            Row::AutoLoginPrompts,
            Row::Group,
            Row::Routing,
            Row::RoutingDetail,
            Row::AdvancedHeader,
            Row::MemoryHeader,
            Row::MemoryBody,
        ];
        rows.extend(
            [
                X11,
                Legacy,
                Fingerprint,
                TelnetKeys,
                TelnetTerm,
                TelnetNaws,
                TelnetRaw,
                SerialFraming,
                SerialFlow,
                SerialEncoding,
                SerialEcho,
            ]
            .map(Row::Advanced),
        );
        rows
    }

    fn host_row(
        this: &mut MainWindow,
        id: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let row = all_rows()
            .into_iter()
            .find(|row| row.id() == id)
            .ok_or("不认识的行名")?;
        let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
        if host_props::is_advanced(row) {
            form.advanced_open = true;
        }
        if matches!(row, Row::MemoryBody) {
            form.memory_open = true;
        }
        this.host_scroll_to(row, window, cx);
        Ok(Outcome::Done)
    }

    fn host_fold(
        this: &mut MainWindow,
        which: &str,
        value: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let on = on_off(value)?;
        let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
        match which {
            "advanced" => form.advanced_open = on,
            "memory" => form.memory_open = on,
            _ => return Err("折叠区写 advanced 或 memory".to_owned()),
        }
        cx.notify();
        Ok(Outcome::Done)
    }

    fn host_conn(
        this: &mut MainWindow,
        mode: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let mode = ConnMode::ALL
            .into_iter()
            .find(|candidate| candidate.id() == mode)
            .ok_or("连接方式写 direct / jump / proxy")?;
        this.host_set_conn_mode(mode, cx);
        Ok(Outcome::Done)
    }

    fn assert_u13(this: &MainWindow, what: &str, want: &str, cx: &App) -> Result<Outcome, String> {
        let form = this.host_form.as_ref().ok_or("主机表单没开着")?;
        match what {
            "rows" => {
                let have: Vec<&str> = host_props::rows(form.layout_state())
                    .into_iter()
                    .map(Row::id)
                    .collect();
                check(
                    have.join(",") == want,
                    format!("表单的行是 {}，不是 {want}", have.join(",")),
                )
            }
            "conn_mode" => check(
                form.conn_mode.id() == want,
                format!("连接方式是 {}，不是 {want}", form.conn_mode.id()),
            ),
            "advanced" => check(
                form.advanced_open == on_off(want)?,
                format!("高级设置展开 = {}", form.advanced_open),
            ),
            "memory_open" => check(
                form.memory_open == on_off(want)?,
                format!("AI 记忆展开 = {}", form.memory_open),
            ),
            "keeps_proxy" => check(
                form.jump_keeps_proxy == (want == "true"),
                format!("keeps_proxy = {}", form.jump_keeps_proxy),
            ),
            "saved_routing" => {
                let routing = effective_routing(
                    form.conn_mode,
                    form.proxy_mode,
                    form.proxy,
                    form.jump,
                    form.jump_keeps_proxy,
                );
                let have = format!(
                    "{},{},{}",
                    routing.proxy_mode.as_str(),
                    routing.proxy.map_or("none".to_owned(), |id| id.to_string()),
                    routing.jump.map_or("none".to_owned(), |id| id.to_string()),
                );
                check(have == want, format!("保存时写 {have}，不是 {want}"))
            }
            "test" => {
                if let Some(text) = want.strip_prefix("contains:") {
                    // 还在跑就等一等。
                    if form.test.is_running() {
                        return Ok(Outcome::Pending);
                    }
                    let have = form
                        .test
                        .result
                        .as_ref()
                        .is_some_and(|outcome| outcome.headline.contains(text));
                    return check(have, "测试结果行里没有预期的文字".to_owned());
                }
                // 还在跑就等一等（除非就是在断言 running）。
                if form.test.is_running() && want != "running" {
                    return Ok(Outcome::Pending);
                }
                let state = match (&form.test.running, &form.test.result) {
                    (Some(_), _) => "running",
                    (None, None) => "none",
                    (None, Some(outcome)) if outcome.ok => "ok",
                    (None, Some(_)) => "fail",
                };
                let _ = cx;
                check(state == want, format!("测试状态是 {state}，不是 {want}"))
            }
            _ => Err("assert_u13:rows|conn_mode|advanced|memory_open|keeps_proxy|saved_routing|test:<值>".to_owned()),
        }
    }

    fn host_test(
        this: &mut MainWindow,
        what: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match what {
            "run" => this.host_test_connection(window, cx),
            "cancel" => this.host_test_cancel(cx),
            _ => return Err("host_test:run|cancel".to_owned()),
        }
        Ok(Outcome::Done)
    }

    fn failure(error: &ZsshError, phase: FailurePhase, role: HopRole, label: &str) -> TestOutcome {
        let f = classify_failure(error, phase, role, label, Interaction::default());
        TestOutcome {
            ok: false,
            headline: f.message.clone(),
            detail: format!(
                "测试连接（SSH）\n结果：失败（阶段：{}）\n原因：{}",
                f.stage.label(),
                f.message
            ),
            elapsed_ms: 20,
            stage: Some(f.stage),
            untrusted_fingerprint: None,
        }
    }

    fn io_error(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::from(kind)
    }

    fn host_fake_test(
        this: &mut MainWindow,
        kind: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let version = "SSH-2.0-OpenSSH_9.6";
        let direct = FailurePhase::Connect { via_proxy: false };
        let ok = |headline: zssh_core::error::Reason| TestOutcome {
            ok: true,
            headline,
            detail: "测试连接\n结果：成功".to_owned(),
            elapsed_ms: 86,
            stage: None,
            untrusted_fingerprint: None,
        };
        let outcome = match kind {
            "ssh_ok" => ok(ssh_success_headline(86, Some(version))),
            // 指纹没信任：测试停在认证之前，既不是成功也不是红色失败（和真实流程一样 stage 留空）。
            "ssh_untrusted" => TestOutcome {
                ok: false,
                headline: untrusted_stop_headline(
                    "SHA256:3xAmPlEfIngErPrInTdOcUmEnTaTiOnOnLy0123456",
                    false,
                    None,
                ),
                detail: "测试连接\n结果：没有测试登录".to_owned(),
                elapsed_ms: 92,
                stage: None,
                untrusted_fingerprint: Some(
                    "SHA256:3xAmPlEfIngErPrInTdOcUmEnTaTiOnOnLy0123456".to_owned(),
                ),
            },
            "ssh_auth_failed" => failure(
                &ZsshError::AuthFailed,
                FailurePhase::Handshake,
                HopRole::Target,
                "目标 web-1",
            ),
            "ssh_refused" => failure(
                &ZsshError::Connect {
                    host: "192.0.2.10".into(),
                    port: 22,
                    tried: "192.0.2.10:22".into(),
                    source: io_error(std::io::ErrorKind::ConnectionRefused),
                },
                direct,
                HopRole::Target,
                "目标 web-1",
            ),
            "ssh_dns" => failure(
                &ZsshError::Dns {
                    host: "no-such-host.example".into(),
                    source: std::io::Error::other("lookup failed"),
                },
                direct,
                HopRole::Target,
                "目标 web-1",
            ),
            "ssh_proxy" => failure(
                &ZsshError::Proxy {
                    proxy: "socks5 198.51.100.7:1080".into(),
                    detail: "代理拒绝了连接".into(),
                },
                FailurePhase::Connect { via_proxy: true },
                HopRole::Target,
                "目标 web-1",
            ),
            "ssh_jump" => failure(
                &ZsshError::AuthFailed,
                FailurePhase::Handshake,
                HopRole::Jump,
                // 真实流程里这个前缀来自界面文案（`host_test`），夹具照着语言给一份。
                if crate::i18n::current() == crate::i18n::Lang::En {
                    "Jump host edge-1"
                } else {
                    "跳板机 edge-1"
                },
            ),
            "ssh_hostkey" => failure(
                &ZsshError::HostKeyChanged(Box::new(HostKeyMismatch {
                    host: "192.0.2.10".into(),
                    port: 22,
                    key_type: "ssh-ed25519".into(),
                    stored: "SHA256:oLdFiNgErPrInT".into(),
                    presented: "SHA256:nEwFiNgErPrInT".into(),
                    stored_in: "ZeeTerm 自己的 known_hosts".into(),
                    advice: String::new(),
                    source: zssh_core::error::HostKeySource::Own,
                })),
                FailurePhase::Handshake,
                HopRole::Target,
                "目标 web-1",
            ),
            "ssh_timeout" => {
                let f = zssh_core::ssh::connection_test::classify_failure(
                    &ZsshError::ConnectTimeout {
                        host: "192.0.2.10".into(),
                        port: 22,
                        seconds: 10,
                    },
                    direct,
                    HopRole::Target,
                    "目标 web-1",
                    Interaction::default(),
                );
                TestOutcome {
                    ok: false,
                    headline: f.message,
                    detail: String::new(),
                    elapsed_ms: 10_000,
                    stage: Some(TestStage::Connect),
                    untrusted_fingerprint: None,
                }
            }
            "telnet_ok" => ok(tcp_success_headline(31, FirstByte::Data)),
            "serial_ok" => serial_success("COM3", 9),
            "serial_missing" => serial_failure(
                "COM3",
                "COM3 不存在：设备没有插好，或者设备名写错了（可以在设备管理器的“端口”里查看当前的 COM 口）",
                4,
            ),
            _ => return Err("host_fake_test:ssh_ok|ssh_untrusted|ssh_auth_failed|ssh_refused|ssh_dns|ssh_proxy|ssh_jump|ssh_hostkey|ssh_timeout|telnet_ok|serial_ok|serial_missing".to_owned()),
        };
        let form = this.host_form.as_mut().ok_or("主机表单没开着")?;
        form.test.result = Some(outcome);
        cx.notify();
        Ok(Outcome::Done)
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
