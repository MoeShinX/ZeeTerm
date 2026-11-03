// Design borrowed from Netcatty components/AIChatSidePanel.tsx (per-conversation scope, dangling, rebind); code is original.
//! AI 对话的锚定（V2-U15）：对话在发出第一条消息时绑定到当时聚焦窗格的主机，之后不跟着标签 / 分屏
//! 焦点走。纯函数，不依赖 GPUI。
//!
//! 一个对话的目标 = 绑定的那台（没发第一条消息之前是「预览」：当前聚焦窗格的主机，随焦点变）+ 用户 `@`
//! 进来的主机。命令落在哪个终端不在这里算：执行时由终端登记表按「聚焦窗格 → 最近聚焦 → 最早打开」现取
//! （见 `scope_targets` 的说明和 `agent_host::NativeBridgeHost::execution_session`），和锚定无关。
//!
//! 绑定的对象有两种：主机库里的主机（`Key::Host`），或者快速连接的那个终端（`Key::Terminal`，快速连接
//! 没有主机记录，只能认终端本身）。绑定的对象没有任何打开的终端时对话「失效」（`is_alive`）：不自动改
//! 绑，由用户选「重新连接」「改用当前主机」或「新对话」。

use std::collections::HashSet;

/// 用户 `@` 进来的一台主机，留在对话范围里直到用户 × 掉。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub host_id: i64,
    pub name: String,
}

/// 存进历史时快速连接终端的占位名（不存 `用户@地址`）。
pub const QUICK_NAME: &str = "快速连接";

/// 目标的身份：主机库里的一台主机，或快速连接的一个终端（终端编号，`term-<n>`）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Host(i64),
    Terminal(String),
}

/// 对话绑定的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub key: Key,
    pub name: String,
}

/// 当前标签聚焦窗格（主窗口每个节拍告诉面板）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    Host {
        id: i64,
        name: String,
        /// 不能当 AI 执行目标的原因（Telnet / 串口主机）。
        refusal: Option<String>,
    },
    /// 快速连接窗格；`terminal` 是它的终端编号，还没连上时为 `None`。
    Quick {
        terminal: Option<String>,
        label: String,
    },
}

/// 不能锚定当前窗格的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unanchorable {
    Refused { name: String, reason: String },
    NotConnected { name: String },
}

impl Unanchorable {
    /// 给用户看的一句话（对话没有绑定、保持未锁定）。
    pub fn message(&self) -> String {
        match self {
            Self::Refused { name, reason } => {
                format!("「{name}」不能当目标：{reason}；这次对话先不绑定主机")
            }
            Self::NotConnected { name } => {
                format!("「{name}」还没连上，这次对话先不绑定主机")
            }
        }
    }
}

impl Focus {
    pub fn name(&self) -> &str {
        match self {
            Self::Host { name, .. } => name,
            Self::Quick { label, .. } => label,
        }
    }

    /// 窗格的身份；快速连接还没连上时没有。
    pub fn key(&self) -> Option<Key> {
        match self {
            Self::Host { id, .. } => Some(Key::Host(*id)),
            Self::Quick { terminal, .. } => terminal.clone().map(Key::Terminal),
        }
    }

    /// 能不能当锚定（Telnet / 串口不行；快速连接要先连上）。
    pub fn anchor(&self) -> Result<Anchor, Unanchorable> {
        match self {
            Self::Host {
                refusal: Some(reason),
                name,
                ..
            } => Err(Unanchorable::Refused {
                name: name.clone(),
                reason: reason.clone(),
            }),
            Self::Host { id, name, .. } => Ok(Anchor {
                key: Key::Host(*id),
                name: name.clone(),
            }),
            Self::Quick {
                terminal: Some(id),
                label,
            } => Ok(Anchor {
                key: Key::Terminal(id.clone()),
                name: label.clone(),
            }),
            Self::Quick {
                terminal: None,
                label,
            } => Err(Unanchorable::NotConnected {
                name: label.clone(),
            }),
        }
    }
}

/// 对话有没有锁定。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Binding {
    /// 还没发第一条消息：目标栏是当前聚焦窗格的预览，随焦点变。
    #[default]
    Unlocked,
    Locked(Anchor),
}

impl Binding {
    pub fn anchor(&self) -> Option<&Anchor> {
        match self {
            Self::Locked(anchor) => Some(anchor),
            Self::Unlocked => None,
        }
    }
}

/// 现在还开着终端的主机 / 终端。主机按「这个窗口里还有标签」或「全进程的终端登记表里有」算。
#[derive(Clone, Copy, Debug)]
pub struct Open<'a> {
    pub hosts: &'a HashSet<i64>,
    pub terminals: &'a HashSet<String>,
}

pub fn is_alive(anchor: &Anchor, open: &Open) -> bool {
    match &anchor.key {
        Key::Host(id) => open.hosts.contains(id),
        Key::Terminal(id) => open.terminals.contains(id),
    }
}

/// 发送时的锁定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockOutcome {
    /// 早就锁定了，什么也不改。
    Already,
    Locked(Anchor),
    /// 当前窗格不能锚定（Telnet / 串口 / 快速连接没连上）：保持未锁定。
    Skipped(Unanchorable),
    /// 没有聚焦窗格（没有标签 / 空白标签）：保持未锁定，没有目标。
    NoFocus,
}

/// 第一条消息发出时：没锁定就按当前聚焦窗格锁定。
pub fn lock_on_send(binding: &Binding, focus: Option<&Focus>) -> LockOutcome {
    if matches!(binding, Binding::Locked(_)) {
        return LockOutcome::Already;
    }
    match focus.map(Focus::anchor) {
        None => LockOutcome::NoFocus,
        Some(Ok(anchor)) => LockOutcome::Locked(anchor),
        Some(Err(why)) => LockOutcome::Skipped(why),
    }
}

/// 改绑（「改用 <当前>」）：按当前聚焦窗格重新锚定。没有聚焦窗格返回 `None`。
pub fn rebind(focus: Option<&Focus>) -> Option<Result<Anchor, Unanchorable>> {
    focus.map(Focus::anchor)
}

/// 目标栏下面的一行提示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Banner {
    /// 对话绑定在 `anchor_name`，当前标签是 `current_name`（淡色提示 + 「改用」「新对话」）。
    Mismatch {
        anchor_name: String,
        current_name: String,
        /// 当前窗格能锚定（不是 Telnet / 串口、快速连接已连上）才给「改用」。
        can_rebind: bool,
    },
    /// 绑定的目标已经没有打开的终端。
    Dangling {
        anchor_name: String,
        /// 主机库里的主机可以重新打开它的标签；快速连接的终端回不来。
        can_reconnect: bool,
        /// 当前聚焦窗格能锚定才给「改用当前主机」。
        can_use_current: bool,
    },
}

pub fn banner(binding: &Binding, focus: Option<&Focus>, open: &Open) -> Option<Banner> {
    let anchor = binding.anchor()?;
    if !is_alive(anchor, open) {
        return Some(Banner::Dangling {
            anchor_name: anchor.name.clone(),
            can_reconnect: matches!(anchor.key, Key::Host(_)),
            can_use_current: focus.is_some_and(|focus| focus.anchor().is_ok()),
        });
    }
    let focus = focus?;
    if focus.key().as_ref() == Some(&anchor.key) {
        return None;
    }
    Some(Banner::Mismatch {
        anchor_name: anchor.name.clone(),
        current_name: focus.name().to_owned(),
        can_rebind: focus.anchor().is_ok(),
    })
}

/// 目标栏里的一个标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub key: Key,
    pub name: String,
    /// 对话绑定的那台（没锁定时是当前聚焦窗格的预览）：没写 `target_id` 的工具调用落在它身上，不能 ×。
    pub current: bool,
    /// 也是用户 `@` 进来的。
    pub pinned: bool,
    /// 对话已经锁定在它身上（`false` = 还没发第一条消息，只是当前聚焦窗格的预览）。
    pub locked: bool,
    /// 绑定的目标没有打开的终端：标签灰掉写「未连接」。
    pub dangling: bool,
}

impl Target {
    pub fn host_id(&self) -> Option<i64> {
        match self.key {
            Key::Host(id) => Some(id),
            Key::Terminal(_) => None,
        }
    }
}

/// 拼目标列表：绑定的（或预览的）在最前，后面是用户 `@` 的。**一个目标只出现一次**。
pub fn build(binding: &Binding, focus: Option<&Focus>, pins: &[Pin], open: &Open) -> Vec<Target> {
    let pinned_host =
        |key: &Key| matches!(key, Key::Host(id) if pins.iter().any(|pin| pin.host_id == *id));
    let mut out: Vec<Target> = Vec::new();
    match binding {
        Binding::Locked(anchor) => out.push(Target {
            key: anchor.key.clone(),
            name: anchor.name.clone(),
            current: true,
            pinned: pinned_host(&anchor.key),
            locked: true,
            dangling: !is_alive(anchor, open),
        }),
        Binding::Unlocked => {
            if let Some(key) = focus.and_then(Focus::key) {
                out.push(Target {
                    pinned: pinned_host(&key),
                    key,
                    name: focus.map(Focus::name).unwrap_or_default().to_owned(),
                    current: true,
                    locked: false,
                    dangling: false,
                });
            }
        }
    }
    for pin in pins {
        let key = Key::Host(pin.host_id);
        if out.iter().any(|target| target.key == key) {
            continue;
        }
        out.push(Target {
            key,
            name: pin.name.clone(),
            current: false,
            pinned: true,
            locked: false,
            dangling: false,
        });
    }
    out
}

/// `@` 一台主机后的固定目标列表，以及这次要不要提示「已经在对话里了」。
pub fn pin_host(pins: &[Pin], host_id: i64, name: &str, bound: Option<&Key>) -> (Vec<Pin>, bool) {
    if pins.iter().any(|pin| pin.host_id == host_id) {
        return (pins.to_vec(), true);
    }
    // 绑定的就是这台主机：它本来就是目标，不再加一份固定的（改绑走之后它也不会凭空留下）。
    if bound == Some(&Key::Host(host_id)) {
        return (pins.to_vec(), true);
    }
    let mut next = pins.to_vec();
    next.push(Pin {
        host_id,
        name: name.to_owned(),
    });
    (next, false)
}

/// 去掉一个固定目标（标签上的 ×）。
pub fn unpin(pins: &[Pin], host_id: i64) -> Vec<Pin> {
    pins.iter()
        .filter(|pin| pin.host_id != host_id)
        .cloned()
        .collect()
}

/// 历史里存的锚定 → 界面用的锚定。主机库里还有这台主机就用它现在的名字（可能改过名）；已经删了就用
/// 记下来的名字，它自然没有打开的终端（失效）。没有主机 id 的是快速连接，终端早没了：编号留空，
/// 永远「失效」，只能改绑或新对话。
pub fn from_history(
    host_id: Option<i64>,
    name: &str,
    current_name: impl Fn(i64) -> Option<String>,
) -> Anchor {
    match host_id {
        Some(id) => Anchor {
            key: Key::Host(id),
            name: current_name(id).unwrap_or_else(|| name.to_owned()),
        },
        None => Anchor {
            key: Key::Terminal(String::new()),
            name: QUICK_NAME.to_owned(),
        },
    }
}

/// 界面用的锚定 → 历史里存的（主机 id，显示名）。快速连接不存地址，只存占位名。
pub fn to_history(anchor: &Anchor) -> (Option<i64>, String) {
    match &anchor.key {
        Key::Host(id) => (Some(*id), anchor.name.clone()),
        Key::Terminal(_) => (None, QUICK_NAME.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: i64, name: &str) -> Focus {
        Focus::Host {
            id,
            name: name.into(),
            refusal: None,
        }
    }

    fn telnet(id: i64, name: &str) -> Focus {
        Focus::Host {
            id,
            name: name.into(),
            refusal: Some("Telnet 主机不能让 AI 执行命令".into()),
        }
    }

    fn quick(terminal: Option<&str>, label: &str) -> Focus {
        Focus::Quick {
            terminal: terminal.map(str::to_owned),
            label: label.into(),
        }
    }

    fn hosts(ids: &[i64]) -> HashSet<i64> {
        ids.iter().copied().collect()
    }

    fn terms(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn names(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|t| t.name.as_str()).collect()
    }

    fn locked(focus: &Focus) -> Binding {
        match lock_on_send(&Binding::Unlocked, Some(focus)) {
            LockOutcome::Locked(anchor) => Binding::Locked(anchor),
            other => panic!("应该锁定：{other:?}"),
        }
    }

    #[test]
    fn unlocked_preview_follows_the_focus() {
        let h = hosts(&[1, 2]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let web = host(1, "web-1");
        let db = host(2, "db-1");
        let preview = build(&Binding::Unlocked, Some(&web), &[], &open);
        assert_eq!(names(&preview), vec!["web-1"]);
        assert!(preview[0].current && !preview[0].dangling);
        let preview = build(&Binding::Unlocked, Some(&db), &[], &open);
        assert_eq!(names(&preview), vec!["db-1"], "没发消息：预览跟着焦点走");
        assert!(build(&Binding::Unlocked, None, &[], &open).is_empty());
        // 快速连接还没连上：预览里没有它。
        assert!(build(&Binding::Unlocked, Some(&quick(None, "a@x")), &[], &open).is_empty());
        let live = quick(Some("term-3"), "a@x");
        let preview = build(&Binding::Unlocked, Some(&live), &[], &open);
        assert_eq!(preview[0].key, Key::Terminal("term-3".into()));
    }

    #[test]
    fn the_first_message_locks_to_the_focused_host() {
        let web = host(1, "web-1");
        assert_eq!(
            lock_on_send(&Binding::Unlocked, Some(&web)),
            LockOutcome::Locked(Anchor {
                key: Key::Host(1),
                name: "web-1".into()
            })
        );
        assert_eq!(lock_on_send(&Binding::Unlocked, None), LockOutcome::NoFocus);
        // 已经锁定：焦点在别处也不改。
        let binding = locked(&web);
        assert_eq!(
            lock_on_send(&binding, Some(&host(2, "db-1"))),
            LockOutcome::Already
        );
    }

    #[test]
    fn switching_tabs_or_split_focus_after_locking_keeps_the_target() {
        let h = hosts(&[1, 2]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let binding = locked(&host(1, "web-1"));
        // 切到标签 B / 分屏里别的窗格：目标栏不变，不出现别的主机，也没有自动保留的。
        for focus in [
            host(2, "db-1"),
            host(1, "web-1"),
            quick(Some("term-9"), "q"),
        ] {
            let targets = build(&binding, Some(&focus), &[], &open);
            assert_eq!(names(&targets), vec!["web-1"]);
            assert!(targets[0].current && !targets[0].dangling);
        }
        assert!(build(&binding, None, &[], &open)[0].current);
    }

    #[test]
    fn mention_adds_and_removes_hosts_without_touching_the_anchor() {
        let h = hosts(&[1, 2, 3]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let binding = locked(&host(1, "web-1"));
        let bound = binding.anchor().map(|a| a.key.clone());
        let (pins, already) = pin_host(&[], 2, "db-1", bound.as_ref());
        assert!(!already);
        let (pins2, already) = pin_host(&pins, 2, "db-1", bound.as_ref());
        assert!(already && pins2 == pins, "第二次 @ 同一台");
        let (same, already) = pin_host(&pins, 1, "web-1", bound.as_ref());
        assert!(already && same == pins, "@ 绑定的那台：已经在对话里");
        let targets = build(&binding, Some(&host(3, "cache")), &pins, &open);
        assert_eq!(names(&targets), vec!["web-1", "db-1"]);
        assert!(!targets[1].current && targets[1].pinned);
        assert!(unpin(&pins, 2).is_empty());
        // 绑定的主机之前就被 @ 过：只算一个目标。
        let before = vec![Pin {
            host_id: 1,
            name: "web-1".into(),
        }];
        let targets = build(&binding, None, &before, &open);
        assert_eq!(names(&targets), vec!["web-1"]);
        assert!(targets[0].current && targets[0].pinned);
    }

    #[test]
    fn rebinding_moves_the_conversation_to_the_current_focus() {
        let h = hosts(&[1, 2]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let binding = locked(&host(1, "web-1"));
        let db = host(2, "db-1");
        let Some(Banner::Mismatch {
            anchor_name,
            current_name,
            can_rebind,
        }) = banner(&binding, Some(&db), &open)
        else {
            panic!("应该提示不一致");
        };
        assert_eq!(
            (anchor_name.as_str(), current_name.as_str()),
            ("web-1", "db-1")
        );
        assert!(can_rebind);
        let Some(Ok(anchor)) = rebind(Some(&db)) else {
            panic!("应该能改绑");
        };
        let binding = Binding::Locked(anchor);
        assert_eq!(binding.anchor().map(|a| a.name.as_str()), Some("db-1"));
        assert_eq!(banner(&binding, Some(&db), &open), None, "一致时不提示");
        // 改绑到 Telnet 窗格不行；没有聚焦窗格没有结果。
        assert!(matches!(
            rebind(Some(&telnet(5, "sw-1"))),
            Some(Err(Unanchorable::Refused { .. }))
        ));
        assert!(rebind(None).is_none());
    }

    #[test]
    fn mismatch_banner_is_hidden_when_it_matches_and_blocks_rebind_for_telnet() {
        let h = hosts(&[1, 5]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let binding = locked(&host(1, "web-1"));
        assert_eq!(banner(&binding, Some(&host(1, "web-1")), &open), None);
        assert_eq!(banner(&binding, None, &open), None);
        assert_eq!(
            banner(&Binding::Unlocked, Some(&host(1, "web-1")), &open),
            None
        );
        let Some(Banner::Mismatch { can_rebind, .. }) =
            banner(&binding, Some(&telnet(5, "sw-1")), &open)
        else {
            panic!("应该提示不一致");
        };
        assert!(!can_rebind, "Telnet 窗格不能当改绑目标");
    }

    #[test]
    fn closing_every_tab_of_the_anchor_makes_it_dangling() {
        let t = terms(&[]);
        let binding = locked(&host(1, "web-1"));
        let both = hosts(&[1, 2]);
        let only_db = hosts(&[2]);
        let alive = Open {
            hosts: &both,
            terminals: &t,
        };
        let gone = Open {
            hosts: &only_db,
            terminals: &t,
        };
        assert!(!build(&binding, Some(&host(2, "db-1")), &[], &alive)[0].dangling);
        let targets = build(&binding, Some(&host(2, "db-1")), &[], &gone);
        assert!(
            targets[0].dangling && targets[0].current,
            "灰掉但仍是绑定的那台，不自动改绑"
        );
        assert_eq!(
            banner(&binding, Some(&host(2, "db-1")), &gone),
            Some(Banner::Dangling {
                anchor_name: "web-1".into(),
                can_reconnect: true,
                can_use_current: true,
            })
        );
        // 当前窗格是 Telnet / 没有标签：不给「改用当前主机」。
        assert!(matches!(
            banner(&binding, Some(&telnet(2, "sw-1")), &gone),
            Some(Banner::Dangling {
                can_use_current: false,
                ..
            })
        ));
        assert!(matches!(
            banner(&binding, None, &gone),
            Some(Banner::Dangling {
                can_use_current: false,
                can_reconnect: true,
                ..
            })
        ));
    }

    #[test]
    fn quick_connect_anchors_the_terminal_and_cannot_reconnect() {
        let h = hosts(&[]);
        let live = terms(&["term-4"]);
        let none = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &live,
        };
        let closed = Open {
            hosts: &h,
            terminals: &none,
        };
        let focus = quick(Some("term-4"), "ops@192.0.2.9");
        let binding = locked(&focus);
        assert_eq!(
            binding.anchor().map(|a| a.key.clone()),
            Some(Key::Terminal("term-4".into()))
        );
        assert_eq!(banner(&binding, Some(&focus), &open), None);
        let Some(Banner::Dangling {
            can_reconnect,
            can_use_current,
            ..
        }) = banner(&binding, Some(&host(2, "db-1")), &closed)
        else {
            panic!("终端关了应该失效");
        };
        assert!(!can_reconnect, "快速连接没有主机记录，回不来");
        assert!(can_use_current);
        // 还没连上的快速连接窗格不能锚定。
        assert_eq!(
            lock_on_send(&Binding::Unlocked, Some(&quick(None, "a@x"))),
            LockOutcome::Skipped(Unanchorable::NotConnected { name: "a@x".into() })
        );
    }

    #[test]
    fn telnet_and_serial_panes_are_not_anchored() {
        let outcome = lock_on_send(&Binding::Unlocked, Some(&telnet(5, "sw-1")));
        let LockOutcome::Skipped(why) = outcome else {
            panic!("不应该锚定：{outcome:?}");
        };
        assert!(why.message().contains("sw-1") && why.message().contains("Telnet"));
    }

    #[test]
    fn a_new_conversation_goes_back_to_unlocked() {
        // 新对话 = `Binding::default()`：预览重新跟着焦点走。
        let h = hosts(&[1, 2]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let _ = locked(&host(1, "web-1"));
        let fresh = Binding::default();
        let targets = build(&fresh, Some(&host(2, "db-1")), &[], &open);
        assert_eq!(names(&targets), vec!["db-1"]);
        assert_eq!(
            lock_on_send(&fresh, Some(&host(2, "db-1"))),
            LockOutcome::Locked(Anchor {
                key: Key::Host(2),
                name: "db-1".into()
            })
        );
    }

    #[test]
    fn history_round_trip_keeps_the_host_and_hides_quick_addresses() {
        let anchor = Anchor {
            key: Key::Host(7),
            name: "web-1".into(),
        };
        assert_eq!(to_history(&anchor), (Some(7), "web-1".to_owned()));
        // 主机改过名：用现在的名字；删掉了：用记下来的。
        let renamed = from_history(Some(7), "web-1", |id| (id == 7).then(|| "web-9".to_owned()));
        assert_eq!(renamed.name, "web-9");
        let deleted = from_history(Some(7), "web-1", |_| None);
        assert_eq!(
            (deleted.key.clone(), deleted.name.as_str()),
            (Key::Host(7), "web-1")
        );
        let quick_anchor = Anchor {
            key: Key::Terminal("term-4".into()),
            name: "ops@192.0.2.9".into(),
        };
        assert_eq!(
            to_history(&quick_anchor),
            (None, QUICK_NAME.to_owned()),
            "不存地址"
        );
        let restored = from_history(None, QUICK_NAME, |_| None);
        let h = hosts(&[]);
        let t = terms(&["term-4"]);
        assert!(
            !is_alive(
                &restored,
                &Open {
                    hosts: &h,
                    terminals: &t
                }
            ),
            "恢复出来的快速连接终端永远失效"
        );
    }

    #[test]
    fn a_host_with_no_open_tab_is_dangling_after_restoring_from_history() {
        let h = hosts(&[2]);
        let t = terms(&[]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        let binding = Binding::Locked(from_history(Some(1), "web-1", |_| Some("web-1".into())));
        assert!(matches!(
            banner(&binding, Some(&host(2, "db-1")), &open),
            Some(Banner::Dangling {
                can_reconnect: true,
                ..
            })
        ));
        let h = hosts(&[1, 2]);
        let open = Open {
            hosts: &h,
            terminals: &t,
        };
        assert!(matches!(
            banner(&binding, Some(&host(2, "db-1")), &open),
            Some(Banner::Mismatch { .. })
        ));
    }
}
