//! SSH 会话意外断开后的自动重连（V2-U44 B）的纯逻辑：什么算「意外断开」、等多久重连、哪些失败要停下来等人、
//! 一个窗格的这一轮自动重连走到哪一步了。没有界面、没有定时器、不碰连接：时间由调用方传进来（`Instant`），
//! 单元测试不用真的等。真正的重连走和手动重连同一条路（`TerminalWorkspace::reconnect_pane`），这里只回答
//! 「现在该不该试、试第几次、试完了接下来怎么办」。
//!
//! 一轮的节奏：断开后 [`delay_before`]`(1)` = 3 秒第一次重连；还失败就按 5、10、20、30 秒，之后每 30 秒一次，
//! 最多 [`MAX_ATTEMPTS`] 次（连上之前的等待加起来约 3 分钟，加上每次连接的耗时大约 4 分钟）。用尽后停下，
//! 遮罩回到手动重连（按 R / 点遮罩）并写明「已停止自动重连」。

use std::time::{Duration, Instant};

use zssh_core::error::{ErrorWithCode, ZsshError};

/// 一轮最多自动试几次。
pub const MAX_ATTEMPTS: u32 = 10;

/// 第 `attempt` 次（从 1 开始）重连之前等多久：3、5、10、20、30 秒，之后一直是 30 秒。
pub fn delay_before(attempt: u32) -> Duration {
    let secs = match attempt {
        0 | 1 => 3,
        2 => 5,
        3 => 10,
        4 => 20,
        _ => 30,
    };
    Duration::from_secs(secs)
}

/// 这次会话结束算不算「意外断开」（值得自动重连）。
///
/// - 只管 SSH 会话（`is_ssh`）；Telnet、串口、离线演示从来不自动重连。
/// - 远端 shell 给了退出码（不论是不是 0，包括用户敲了 `exit`）= shell 自己结束了，不重连。
/// - 带原因的结束（连接中途断开、写失败、保活超时……）= 意外。
/// - 退出码和原因都没有（远端关了通道但没给退出码：服务器重启、sshd 被杀……）= 意外。
///
/// 「会话已经连上过」「用户主动断开」「第一次连接就失败」由调用的位置保证：只有 `Live` 的窗格收到会话结束事件
/// 才会走到这里，用户主动断开和连接失败走别的路。
pub fn is_unexpected_drop(is_ssh: bool, code: Option<i32>, reason: Option<&str>) -> bool {
    is_ssh && (reason.is_some() || code.is_none())
}

/// 连接失败的原因是不是「要人来处理」：自动重试多少次都不会好，反而会让服务器 / 账号被反复尝试。
///
/// 依据是 `zssh_core` 的稳定错误码（`ErrorWithCode::code`），不解析中文原文：
/// - 认证：`ssh.auth_failed`、`ssh.all_auth_failed`、`ssh.auth_timeout`、`ssh.key_load_failed`、
///   `ssh.agent_unavailable`、`ssh.rsa_sha1_refused`；
/// - 主机密钥：`ssh.host_key_rejected` / `changed` / `revoked` / `rekey_mismatch`、`ssh.known_hosts_failed`、
///   `ssh.missing_host_key`（要用户确认指纹或处理 known_hosts）；
/// - 用户在提问框里点了取消（`core.cancelled`）；
/// - 配置问题：`ssh.no_common_algorithm`、`ssh.quick_target_*`、`core.invalid_input`、`store.not_found`
///   （主机被删了）、`store.secret_error`。
///
/// 拨号链某一跳失败（`ssh.hop_failed`）看它里面包着的那个错误。DNS、超时、拒绝连接、代理、协议错误、
/// 开 shell 失败、通道数上限这类「网络 / 服务器暂时不行」的都继续试。
pub fn needs_human(error: &ZsshError) -> bool {
    if let ZsshError::Hop { source, .. } = error {
        return needs_human(source);
    }
    matches!(
        error.code(),
        "ssh.auth_failed"
            | "ssh.all_auth_failed"
            | "ssh.auth_timeout"
            | "ssh.key_load_failed"
            | "ssh.agent_unavailable"
            | "ssh.rsa_sha1_refused"
            | "ssh.host_key_rejected"
            | "ssh.host_key_changed"
            | "ssh.host_key_revoked"
            | "ssh.host_key_rekey_mismatch"
            | "ssh.known_hosts_failed"
            | "ssh.missing_host_key"
            | "core.cancelled"
            | "ssh.no_common_algorithm"
            | "ssh.quick_target_missing_user"
            | "ssh.quick_target_empty_host"
            | "ssh.quick_target_bad_port"
            | "ssh.quick_target_format"
            | "core.invalid_input"
            | "store.not_found"
            | "store.secret_error"
    )
}

/// 一个窗格这一轮自动重连的阶段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    /// 没有在自动重连（连着的、用户主动断开的、取消了的、手动重连过的）。
    #[default]
    Idle,
    /// 等第 `attempt` 次，`due` 到点就连。
    Waiting { attempt: u32, due: Instant },
    /// 第 `attempt` 次正在连。
    Connecting { attempt: u32 },
    /// 试满了次数，停下，等用户手动重连。
    Exhausted,
    /// 遇到要人处理的情况（认证失败、指纹不符、弹过提问仍失败、用户取消提问……），这一轮到此为止。
    NeedsHuman,
}

/// 遮罩上要画的自动重连状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoView {
    /// 「N 秒后自动重连（第 k 次）」。`secs` 向上取整，所以最后一秒显示 1、不会显示 0。
    Waiting { secs: u64, attempt: u32 },
    /// 「正在重连…」。
    Connecting { attempt: u32 },
    /// 「已停止自动重连」：次数用尽或遇到要人处理的情况。
    Stopped,
}

/// `AutoReconnect::tick` 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    /// 什么也不用做。
    Idle,
    /// 到点了：现在发起第 `attempt` 次重连（状态已经切到「正在连」）。
    Fire { attempt: u32 },
}

/// 一次尝试失败之后的去向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// 过 `delay` 再试第 `attempt` 次。
    Retry { attempt: u32, delay: Duration },
    /// 次数用尽。
    Exhausted,
    /// 要人处理，不再试。
    NeedsHuman,
}

/// 一个窗格的自动重连状态机。
#[derive(Clone, Debug, Default)]
pub struct AutoReconnect {
    phase: Phase,
    /// 上一次断开 / 失败的原因：「正在重连…」的遮罩上还要写着它。
    last_reason: String,
}

impl AutoReconnect {
    /// 会话刚意外断开：开始新的一轮，3 秒后试第 1 次。`reason` 是遮罩上的断开原因。
    pub fn start(&mut self, now: Instant, reason: &str) {
        self.last_reason = reason.to_owned();
        self.phase = Phase::Waiting {
            attempt: 1,
            due: now + delay_before(1),
        };
    }

    /// 这一轮还在进行（等着或正在连）。
    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Waiting { .. } | Phase::Connecting { .. })
    }

    pub fn is_waiting(&self) -> bool {
        matches!(self.phase, Phase::Waiting { .. })
    }

    pub fn is_connecting(&self) -> bool {
        matches!(self.phase, Phase::Connecting { .. })
    }

    pub fn last_reason(&self) -> &str {
        &self.last_reason
    }

    /// 到点就发起这一次：状态切到「正在连」。`paused`（应用锁着）时不发起。
    pub fn tick(&mut self, now: Instant, paused: bool) -> Tick {
        match self.phase {
            Phase::Waiting { attempt, due } if !paused && now >= due => {
                self.phase = Phase::Connecting { attempt };
                Tick::Fire { attempt }
            }
            _ => Tick::Idle,
        }
    }

    /// 应用锁着的这段时间不计时：把等待的到期时间往后推 `by`（解锁后接着数）。
    pub fn pause_for(&mut self, by: Duration) {
        if let Phase::Waiting { attempt, due } = self.phase {
            self.phase = Phase::Waiting {
                attempt,
                due: due + by,
            };
        }
    }

    /// 正在连的这一次失败了。`blocked`：这一轮不能再试了（错误是「要人处理」，或者这次尝试弹过提问）。
    /// `reason` 是这次失败的原因（遮罩上换成它）。不在「正在连」时什么也不改，返回 `Exhausted`。
    pub fn attempt_failed(&mut self, now: Instant, blocked: bool, reason: &str) -> Next {
        let Phase::Connecting { attempt } = self.phase else {
            return Next::Exhausted;
        };
        self.last_reason = reason.to_owned();
        if blocked {
            self.phase = Phase::NeedsHuman;
            return Next::NeedsHuman;
        }
        if attempt >= MAX_ATTEMPTS {
            self.phase = Phase::Exhausted;
            return Next::Exhausted;
        }
        let next = attempt + 1;
        let delay = delay_before(next);
        self.phase = Phase::Waiting {
            attempt: next,
            due: now + delay,
        };
        Next::Retry {
            attempt: next,
            delay,
        }
    }

    /// 调试 / 截图：直接摆成某个阶段（`waiting`：第 `attempt` 次、`secs` 秒后到点；`connecting`；`stopped`）。
    #[cfg(debug_assertions)]
    pub fn dev_force(&mut self, kind: &str, attempt: u32, secs: u64, now: Instant, reason: &str) {
        self.last_reason = reason.to_owned();
        self.phase = match kind {
            "waiting" => Phase::Waiting {
                attempt,
                due: now + Duration::from_secs(secs),
            },
            "connecting" => Phase::Connecting { attempt },
            "stopped" => Phase::Exhausted,
            _ => Phase::Idle,
        };
    }

    /// 连上了（任何方式）：这一轮结束，计数清零。
    pub fn connected(&mut self) {
        *self = Self::default();
    }

    /// 用户点了「取消」、关了设置、手动重连、主动断开……：停止这一轮，回到手动状态。
    pub fn cancel(&mut self) {
        self.phase = Phase::Idle;
    }

    /// 同一台主机上别的窗格遇到了要人处理的情况（或弹过提问仍失败）：还在等着的这个窗格也停下来，
    /// 免得它再去弹同样的提问。正在连的不动（它自己的结果会带着同样的失败回来）。
    pub fn stop_for_human(&mut self) {
        if matches!(self.phase, Phase::Waiting { .. }) {
            self.phase = Phase::NeedsHuman;
        }
    }

    /// 遮罩上要画的状态（没有自动重连、也没有「已停止」要写时是 `None`）。
    pub fn view(&self, now: Instant) -> Option<AutoView> {
        match self.phase {
            Phase::Idle => None,
            Phase::Waiting { attempt, due } => {
                let left = due.saturating_duration_since(now);
                // 向上取整到秒：剩 2.3 秒显示 3。
                let secs = left.as_millis().div_ceil(1000) as u64;
                Some(AutoView::Waiting {
                    secs: secs.max(1),
                    attempt,
                })
            }
            Phase::Connecting { attempt } => Some(AutoView::Connecting { attempt }),
            Phase::Exhausted | Phase::NeedsHuman => Some(AutoView::Stopped),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoReconnect, AutoView, MAX_ATTEMPTS, Next, Tick, delay_before, is_unexpected_drop,
        needs_human,
    };
    use std::time::{Duration, Instant};
    use zssh_core::error::HostKeyMismatch;
    use zssh_core::error::ZsshError;

    #[test]
    fn the_schedule_is_3_5_10_20_30_and_then_30_forever() {
        let secs: Vec<u64> = (1..=MAX_ATTEMPTS)
            .map(|attempt| delay_before(attempt).as_secs())
            .collect();
        assert_eq!(secs, [3, 5, 10, 20, 30, 30, 30, 30, 30, 30]);
        // 序号 0 不会出现，但给个不爆炸的值。
        assert_eq!(delay_before(0), Duration::from_secs(3));
        assert_eq!(MAX_ATTEMPTS, 10);
    }

    #[test]
    fn only_ssh_drops_without_a_shell_exit_code_are_unexpected() {
        // 带原因：连接中途断开、写失败、保活超时。
        assert!(is_unexpected_drop(true, None, Some("读远端失败")));
        // 退出码和原因都没有：远端关了通道（服务器重启、sshd 被杀）。
        assert!(is_unexpected_drop(true, None, None));
        // 远端 shell 给了退出码：不论 0 还是非 0 都是 shell 自己结束的。
        assert!(!is_unexpected_drop(true, Some(0), None));
        assert!(!is_unexpected_drop(true, Some(1), None));
        assert!(!is_unexpected_drop(true, Some(130), None));
        // 原因优先：就算带了退出码，有原因就是异常结束。
        assert!(is_unexpected_drop(true, Some(0), Some("连接被重置")));
        // Telnet / 串口 / 离线演示：不是 SSH，从来不自动重连。
        assert!(!is_unexpected_drop(false, None, None));
        assert!(!is_unexpected_drop(false, None, Some("断开")));
    }

    fn at(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn a_round_waits_fires_retries_with_growing_gaps_and_gives_up_after_ten() {
        let base = Instant::now();
        let mut auto = AutoReconnect::default();
        assert!(!auto.is_active());
        assert_eq!(auto.view(base), None);

        auto.start(base, "远端关闭了连接");
        assert!(auto.is_waiting());
        assert_eq!(
            auto.view(base),
            Some(AutoView::Waiting {
                secs: 3,
                attempt: 1
            })
        );
        // 没到点不发起。
        assert_eq!(auto.tick(at(base, 2.9), false), Tick::Idle);
        assert_eq!(
            auto.view(at(base, 2.9)),
            Some(AutoView::Waiting {
                secs: 1,
                attempt: 1
            }),
            "向上取整，最后一刻也显示 1"
        );
        // 到点发起第 1 次；之后到了点也不会重复发起。
        assert_eq!(auto.tick(at(base, 3.0), false), Tick::Fire { attempt: 1 });
        assert!(auto.is_connecting());
        assert_eq!(auto.tick(at(base, 99.0), false), Tick::Idle);
        assert_eq!(
            auto.view(at(base, 3.0)),
            Some(AutoView::Connecting { attempt: 1 })
        );

        // 每次失败后的等待：5、10、20、30、30……，第 10 次失败后用尽。
        let mut now = at(base, 3.0);
        let expected = [5, 10, 20, 30, 30, 30, 30, 30, 30];
        for (index, want) in expected.iter().enumerate() {
            let next = auto.attempt_failed(now, false, "连接失败：timed out");
            assert_eq!(
                next,
                Next::Retry {
                    attempt: index as u32 + 2,
                    delay: Duration::from_secs(*want)
                }
            );
            now += Duration::from_secs(*want);
            assert_eq!(
                auto.tick(now, false),
                Tick::Fire {
                    attempt: index as u32 + 2
                }
            );
        }
        assert_eq!(auto.attempt_failed(now, false, "x"), Next::Exhausted);
        assert!(!auto.is_active());
        assert_eq!(auto.view(now), Some(AutoView::Stopped), "已停止自动重连");
        assert_eq!(auto.tick(at(now, 999.0), false), Tick::Idle, "用尽后不再试");
    }

    #[test]
    fn a_failure_that_needs_a_person_stops_the_round_at_once() {
        let base = Instant::now();
        let mut auto = AutoReconnect::default();
        auto.start(base, "断开");
        assert_eq!(auto.tick(at(base, 3.0), false), Tick::Fire { attempt: 1 });
        assert_eq!(
            auto.attempt_failed(at(base, 3.5), true, "认证失败"),
            Next::NeedsHuman
        );
        assert_eq!(auto.view(at(base, 4.0)), Some(AutoView::Stopped));
        assert_eq!(auto.tick(at(base, 999.0), false), Tick::Idle);
        assert_eq!(auto.last_reason(), "认证失败", "遮罩上换成这次失败的原因");
    }

    #[test]
    fn success_clears_the_count_so_the_next_drop_starts_a_new_round() {
        let base = Instant::now();
        let mut auto = AutoReconnect::default();
        auto.start(base, "断开");
        auto.tick(at(base, 3.0), false);
        auto.attempt_failed(at(base, 3.0), false, "超时");
        auto.tick(at(base, 8.0), false);
        auto.connected();
        assert!(!auto.is_active());
        assert_eq!(auto.view(base), None);
        auto.start(at(base, 100.0), "又断了");
        assert_eq!(
            auto.view(at(base, 100.0)),
            Some(AutoView::Waiting {
                secs: 3,
                attempt: 1
            }),
            "新的一轮从第 1 次、3 秒开始"
        );
    }

    #[test]
    fn cancel_stops_waiting_and_connecting_and_leaves_no_note() {
        let base = Instant::now();
        let mut auto = AutoReconnect::default();
        auto.start(base, "断开");
        auto.cancel();
        assert!(!auto.is_active());
        assert_eq!(auto.view(base), None, "取消 = 回到普通的手动遮罩");
        assert_eq!(auto.tick(at(base, 99.0), false), Tick::Idle);

        auto.start(base, "断开");
        auto.tick(at(base, 3.0), false);
        assert!(auto.is_connecting());
        auto.cancel();
        assert_eq!(auto.view(at(base, 4.0)), None);
        // 取消之后迟到的失败结果不改状态。
        assert_eq!(
            auto.attempt_failed(at(base, 4.0), false, "x"),
            Next::Exhausted
        );
        assert!(!auto.is_active());
    }

    #[test]
    fn the_lock_screen_pauses_the_countdown_and_blocks_firing() {
        let base = Instant::now();
        let mut auto = AutoReconnect::default();
        auto.start(base, "断开");
        // 锁着：到了点也不发起。
        assert_eq!(auto.tick(at(base, 5.0), true), Tick::Idle);
        // 解锁时把锁着的 5 秒补回去：还剩 3 秒才到点，不是已经过期。
        auto.pause_for(Duration::from_secs(5));
        assert_eq!(auto.tick(at(base, 5.0), false), Tick::Idle);
        assert_eq!(
            auto.view(at(base, 5.0)),
            Some(AutoView::Waiting {
                secs: 3,
                attempt: 1
            })
        );
        assert_eq!(auto.tick(at(base, 8.0), false), Tick::Fire { attempt: 1 });
        // 正在连的不受影响。
        auto.pause_for(Duration::from_secs(5));
        assert!(auto.is_connecting());
    }

    #[test]
    fn stopping_for_a_person_only_touches_panes_still_waiting() {
        let base = Instant::now();
        let mut waiting = AutoReconnect::default();
        waiting.start(base, "断开");
        waiting.stop_for_human();
        assert_eq!(waiting.view(base), Some(AutoView::Stopped));

        let mut connecting = AutoReconnect::default();
        connecting.start(base, "断开");
        connecting.tick(at(base, 3.0), false);
        connecting.stop_for_human();
        assert!(connecting.is_connecting(), "正在连的等它自己的结果");

        let mut idle = AutoReconnect::default();
        idle.stop_for_human();
        assert_eq!(idle.view(base), None);
    }

    fn mismatch() -> ZsshError {
        ZsshError::HostKeyChanged(Box::new(HostKeyMismatch {
            host: "web-1".to_owned(),
            port: 22,
            key_type: "ssh-ed25519".to_owned(),
            stored: "SHA256:aaa".to_owned(),
            presented: "SHA256:bbb".to_owned(),
            stored_in: "known_hosts".to_owned(),
            advice: String::new(),
            source: zssh_core::error::HostKeySource::Own,
        }))
    }

    #[test]
    fn errors_that_need_a_person_are_told_apart_from_network_trouble() {
        // 要人处理：认证、指纹、用户取消了提问、配置问题。
        assert!(needs_human(&ZsshError::AuthFailed));
        assert!(needs_human(&ZsshError::AuthTimeout { seconds: 30 }));
        assert!(needs_human(&ZsshError::Cancelled));
        assert!(needs_human(&mismatch()));
        assert!(needs_human(&ZsshError::HostKeyRejected {
            host: "web-1".to_owned(),
            port: 22,
            fingerprint: "SHA256:x".to_owned(),
        }));
        assert!(needs_human(&ZsshError::QuickTargetEmptyHost));
        assert!(needs_human(&ZsshError::NotFound {
            entity: zssh_core::error::Entity::Host,
            id: 7
        }));
        // 网络 / 服务器暂时不行：继续试。
        assert!(!needs_human(&ZsshError::ConnectTimeout {
            host: "web-1".to_owned(),
            port: 22,
            seconds: 15
        }));
        assert!(!needs_human(&ZsshError::Dns {
            host: "web-1".to_owned(),
            source: std::io::Error::other("no such host")
        }));
        assert!(!needs_human(&ZsshError::Connect {
            host: "web-1".to_owned(),
            port: 22,
            tried: "192.0.2.1".to_owned(),
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused)
        }));
        assert!(!needs_human(&ZsshError::ChannelLimit(
            "MaxSessions".to_owned()
        )));
    }

    #[test]
    fn a_failed_hop_is_judged_by_what_it_wraps() {
        use zssh_core::error::HopStage;
        let wrap = |source: ZsshError| ZsshError::Hop {
            index: 1,
            label: "jump".to_owned(),
            stage: HopStage::Handshake,
            source: Box::new(source),
        };
        assert!(
            needs_human(&wrap(ZsshError::AuthFailed)),
            "跳板机认证失败要人处理"
        );
        assert!(!needs_human(&wrap(ZsshError::ConnectTimeout {
            host: "jump".to_owned(),
            port: 22,
            seconds: 15
        })));
        // 套了两层也一样。
        assert!(needs_human(&wrap(wrap(mismatch()))));
    }
}
