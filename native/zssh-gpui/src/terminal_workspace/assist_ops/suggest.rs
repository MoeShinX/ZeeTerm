//! 终端内命令建议的候选状态：什么时候检索、检索结果怎么核对、选择模式、按键怎么分。
//!
//! 这一层只管"本地状态"，不碰任何会写远端的东西，写远端只有一个出口：[`Pane::accept_suggestion`]
//! （只追加后缀、不发回车、先登记再发送）。
//!
//! 生成候选的条件（任何一条不满足就不生成，并取消在途检索，而不只是禁止接受）：
//! 开关开着、见过可靠的输入起点标记、窗格是聚焦窗格且已连接、状态机在 `Input`（不是 Suspended / Desynced /
//! Running / Unknown，所以广播、密码交互、REPL / 备用屏幕、鼠标上报、锁屏、粘贴确认、滚回历史、断线都在这里被挡住）、
//! 没有输入法预编辑、没有未确认的写入、光标在行尾、确认的文字至少两个字符。
//!
//! 检索在后台线程里做，结果回来先核对 [`InputTracker::is_current`]（输入修订变了就作废）和确认文字，
//! 不匹配就丢；展示层再过一遍严格前缀和敏感规则。**不预选**：选中是用户的显式动作。

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use zssh_core::terminal_assist::history;
use zssh_core::terminal_assist::sensitive::is_sensitive;
use zssh_core::terminal_assist::suggest::{self, Candidate, QuickSource, SettleTimer};
use zssh_core::terminal_assist::{AcceptRefusal, Phase, RequestIdentity, WriteOutcome};

use super::PaneAssist;

/// 接受一个候选被拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SuggestRefusal {
    /// 现在没有展示的候选。
    NothingShown,
    /// 没有这一条。
    NoSuchCandidate,
    /// 输入或状态已经变了，这批候选作废。
    Stale,
    /// 展示层再次判定为敏感（不会走到这里，除非规则变了）。
    Sensitive,
    /// 状态机不让接受。
    Accept(AcceptRefusal),
    /// 窗格里放不下列表（一行候选都画不出来）：看不见的候选不能被选、被填入。
    Hidden,
}

/// 已经核对过、正在展示的一批候选。
pub(crate) struct Shown {
    id: RequestIdentity,
    typed: String,
    items: Vec<Candidate>,
    /// 检索时的历史数据源（清除修订、历史开关），见 [`PaneAssist::history_source`]。
    source: HistorySource,
}

struct Inflight {
    id: RequestIdentity,
    typed: String,
    rx: Receiver<Vec<Candidate>>,
    source: HistorySource,
}

/// 一批候选是从哪份历史检索出来的：（清除修订, 历史开关）。两者任一变了，这批候选里的历史条目就可能
/// 已经被清掉或不该再出现，整批作废、重新检索（快捷命令还能照常出现）。
type HistorySource = (u64, bool);

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub(crate) struct SuggestState {
    /// 是不是聚焦窗格（工作区每帧告诉）。不是就不生成、不展示。
    active: bool,
    settle: SettleTimer,
    inflight: Option<Inflight>,
    shown: Option<Shown>,
    selected: Option<usize>,
    select_mode: bool,
    /// 光标这里放不下浮层（连标题加一行候选都画不出来），由工作区每帧按窗格几何告诉。
    /// 放不下时候选照样保留（窗格变大就重新出现），但不能进入选择模式、不能被键盘选中或填入。
    no_room: bool,
    quick: Arc<Vec<QuickSource>>,
    /// 测试里让检索任务停在这里，由测试决定什么时候跑（模拟在途请求）。
    #[cfg(test)]
    deferred: Option<Vec<Job>>,
    /// 测试里数一数发起过几次检索（"零生成"的证据）。
    #[cfg(test)]
    started: u32,
}

impl SuggestState {
    /// 用户关掉列表（Esc、接受之后、点击终端、滚动）：列表、选择和在途检索都收掉，但"这个输入已经检索过一次"
    /// 的记录保留——同一个输入不会自己再弹出来，再打字才会重新检索。
    fn dismiss(&mut self) {
        self.inflight = None;
        self.shown = None;
        self.selected = None;
        self.select_mode = false;
    }

    /// 条件不满足了（受限、断线、关开关……）：连计时一起忘掉。
    fn cancel_all(&mut self) {
        self.settle.reset();
        self.inflight = None;
        self.shown = None;
        self.selected = None;
        self.select_mode = false;
    }

    fn spawn(&mut self, job: Job) {
        #[cfg(test)]
        {
            self.started += 1;
            if let Some(jobs) = &mut self.deferred {
                jobs.push(job);
                return;
            }
            job();
        }
        #[cfg(not(test))]
        {
            // 起不来线程就当没有候选：任务被丢掉，结果通道断开，在途请求随即清掉。
            let _ = std::thread::Builder::new()
                .name("zssh-suggest".to_owned())
                .spawn(job);
        }
    }

    /// 界面要不要重画的指纹：展示 / 选择 / 模式任何一项变了就变。
    fn signature(&self) -> (Option<u64>, Option<usize>, bool) {
        (
            self.shown.as_ref().map(|shown| shown.id.input_revision),
            self.selected,
            self.select_mode,
        )
    }
}

/// 浮层要画的东西。
pub(crate) struct SuggestView<'a> {
    pub(crate) items: &'a [Candidate],
    pub(crate) selected: Option<usize>,
    pub(crate) select_mode: bool,
}

/// 一次按键在建议这一层的处理结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyDecision {
    /// 不是建议这一层的事：照常处理（编码后发给 shell）。
    Pass,
    /// 建议这一层消费了它，不发给 shell。
    Consume,
    /// 选择模式里用 Tab / Enter 接受第几条。
    Accept(usize),
}

impl PaneAssist {
    /// 工作区每帧告诉：这个窗格是不是聚焦窗格。不是的话一切候选工作立刻停掉。
    pub(crate) fn set_suggest_active(&mut self, active: bool) {
        if self.suggest.active != active {
            self.suggest.active = active;
            if !active {
                self.suggest.cancel_all();
            }
        }
    }

    /// 快捷命令快照换了：旧候选作废，下次输入重新检索。
    pub(crate) fn set_quick_sources(&mut self, quick: &Arc<Vec<QuickSource>>) {
        if !Arc::ptr_eq(&self.suggest.quick, quick) {
            self.suggest.quick = Arc::clone(quick);
            self.suggest.cancel_all();
        }
    }

    /// 条件变了（开关关掉、连接重置、输入法开始预编辑）：关掉列表和选择模式，取消在途检索，计时重来。
    pub(crate) fn close_suggestions(&mut self) {
        self.suggest.cancel_all();
    }

    /// 用户关掉列表（Esc、接受之后、点击终端、滚动）：同一个输入不会自己再弹出来。
    pub(crate) fn dismiss_suggestions(&mut self) {
        self.suggest.dismiss();
    }

    /// 工作区每帧按窗格几何告诉：光标这里画不画得下浮层（至少标题加一行候选）。
    /// 画不下就退出选择模式、清掉选择，之后按键照常交给终端；候选留着，空间够了再出现。
    pub(crate) fn set_suggest_room(&mut self, room: bool) {
        self.suggest.no_room = !room;
        if !room {
            self.exit_select_mode();
        }
    }

    pub(crate) fn exit_select_mode(&mut self) {
        self.suggest.select_mode = false;
        self.suggest.selected = None;
    }

    pub(crate) fn in_select_mode(&self) -> bool {
        self.suggest.select_mode
    }

    /// 现在的历史数据源：共享历史的清除修订 + 本窗格的历史开关。
    fn history_source(&self) -> HistorySource {
        (
            history::lock(&self.history).clear_revision(),
            self.history_enabled,
        )
    }

    /// 已展示或还在路上的候选是按旧的历史数据源检索的（历史被清除——可能是别的窗口清的——或者
    /// 历史开关关了）：整批作废，并让同一个输入重新检索一次（快捷命令照常出现，旧历史不再出现）。
    pub(super) fn drop_results_from_a_stale_history(&mut self) {
        if self.suggest.shown.is_none() && self.suggest.inflight.is_none() {
            return;
        }
        let current = self.history_source();
        let stale = self
            .suggest
            .shown
            .as_ref()
            .is_some_and(|shown| shown.source != current)
            || self
                .suggest
                .inflight
                .as_ref()
                .is_some_and(|inflight| inflight.source != current);
        if stale {
            self.suggest.cancel_all();
        }
    }

    /// 现在有没有资格生成候选（见模块说明）。
    fn suggest_eligible(&self, live: bool) -> bool {
        if !(live && self.suggest.active && self.enabled && self.suggestions_available()) {
            return false;
        }
        if self.tracker.phase() != Phase::Input || !self.tracker.can_accept() {
            return false;
        }
        let state = self.tracker.state();
        state.predicted_text == state.confirmed_text
            && suggest::eligible_input(&state.confirmed_text)
    }

    /// 每次处理完输出（以及定时）调用：收结果、核对、必要时发起检索。返回浮层上要画的东西有没有变。
    pub(crate) fn suggest_tick(&mut self, live: bool, now: Instant) -> bool {
        let before = self.suggest.signature();
        self.collect_results();
        self.drop_results_from_a_stale_history();
        if !self.suggest_eligible(live) {
            self.suggest.cancel_all();
        } else {
            if self
                .suggest
                .inflight
                .as_ref()
                .is_some_and(|inflight| !self.tracker.is_current(&inflight.id))
            {
                // 输入修订变了：在途请求作废。
                self.suggest.inflight = None;
            }
            if self
                .suggest
                .shown
                .as_ref()
                .is_some_and(|shown| !self.tracker.is_current(&shown.id))
            {
                self.suggest.shown = None;
                self.suggest.selected = None;
                self.suggest.select_mode = false;
            }
            let revision = self.tracker.state().input_revision;
            if self.suggest.settle.poll(revision, now)
                && self.suggest.shown.is_none()
                && self.suggest.inflight.is_none()
            {
                self.start_search();
                self.collect_results();
            }
        }
        before != self.suggest.signature()
    }

    /// 后台检索：把请求身份和输入快照交出去，结果经通道回来。
    fn start_search(&mut self) {
        let typed = self.tracker.state().confirmed_text.clone();
        let target = self.tracker.context().target.clone();
        let id = self.request_identity();
        let (tx, rx) = mpsc::channel();
        let store = Arc::clone(&self.history);
        let quick = Arc::clone(&self.suggest.quick);
        let source = self.history_source();
        let history_on = self.history_enabled;
        let query = typed.clone();
        let job: Job = Box::new(move || {
            let hits = if history_on {
                history::lock(&store).matches(&target, &query)
            } else {
                Vec::new()
            };
            let _ = tx.send(suggest::search(&query, hits, &quick));
        });
        self.suggest.inflight = Some(Inflight {
            id,
            typed,
            rx,
            source,
        });
        self.suggest.spawn(job);
    }

    fn collect_results(&mut self) {
        let Some(inflight) = &self.suggest.inflight else {
            return;
        };
        match inflight.rx.try_recv() {
            Ok(items) => {
                if let Some(inflight) = self.suggest.inflight.take() {
                    self.deliver(inflight.id, inflight.typed, inflight.source, items);
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.suggest.inflight = None,
        }
    }

    /// 结果回来：先核对请求身份和确认文字，再按展示层规则过滤，最后才展示。不预选。
    fn deliver(
        &mut self,
        id: RequestIdentity,
        typed: String,
        source: HistorySource,
        items: Vec<Candidate>,
    ) {
        if !self.tracker.is_current(&id) || self.tracker.state().confirmed_text != typed {
            return;
        }
        if source != self.history_source() {
            // 结果是按清除之前（或开关变化之前）的历史算的：丢掉，并让同一个输入重新检索一次。
            // 输入没变，不复位计时就永远不会再检索，连仍然有效的快捷命令也不出现。
            self.suggest.cancel_all();
            return;
        }
        let items: Vec<Candidate> = items
            .into_iter()
            .filter(|item| item.suffix_after(&typed).is_some() && !is_sensitive(&item.text))
            .collect();
        if items.is_empty() {
            return;
        }
        self.suggest.shown = Some(Shown {
            id,
            typed,
            items,
            source,
        });
        self.suggest.selected = None;
    }

    /// 现在能画的候选：只在请求身份仍然有效时才有（输入一变就立刻没有，不等下一次 tick）。
    pub(crate) fn suggest_view(&self) -> Option<SuggestView<'_>> {
        let shown = self.suggest.shown.as_ref()?;
        if !self.tracker.is_current(&shown.id) {
            return None;
        }
        Some(SuggestView {
            items: &shown.items,
            selected: self.suggest.selected,
            select_mode: self.suggest.select_mode,
        })
    }

    /// 用户看得见、能操作的候选：有效，并且窗格里画得下。键盘选择、进入选择模式、Esc 都看这个。
    fn interactive_view(&self) -> Option<SuggestView<'_>> {
        if self.suggest.no_room {
            return None;
        }
        self.suggest_view()
    }

    /// 显式进入选择模式：需要有看得见的有效候选，并且现在能接受。初始仍不选中。
    pub(crate) fn enter_select_mode(&mut self) -> bool {
        if self.interactive_view().is_none() || !self.tracker.can_accept() {
            return false;
        }
        self.suggest.select_mode = true;
        self.suggest.selected = None;
        true
    }

    /// 选择模式里上下移动。第一次按下：↓ 选第一条、↑ 选最后一条；到头回绕。
    pub(crate) fn move_selection(&mut self, down: bool) {
        let Some(count) = self.interactive_view().map(|view| view.items.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        self.suggest.selected = Some(match (self.suggest.selected, down) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
        });
    }

    /// 接受前的计划：重新核对一切，返回要追加的后缀和请求身份。
    pub(crate) fn plan_accept(
        &self,
        index: usize,
    ) -> Result<(String, RequestIdentity), SuggestRefusal> {
        let shown = self
            .suggest
            .shown
            .as_ref()
            .ok_or(SuggestRefusal::NothingShown)?;
        if self.suggest.no_room {
            return Err(SuggestRefusal::Hidden);
        }
        if !self.tracker.is_current(&shown.id)
            || self.tracker.state().confirmed_text != shown.typed
            || shown.source != self.history_source()
        {
            return Err(SuggestRefusal::Stale);
        }
        let item = shown
            .items
            .get(index)
            .ok_or(SuggestRefusal::NoSuchCandidate)?;
        if is_sensitive(&item.text) {
            return Err(SuggestRefusal::Sensitive);
        }
        let suffix = item
            .suffix_after(&shown.typed)
            .ok_or(SuggestRefusal::Stale)?;
        Ok((suffix.to_owned(), shown.id.clone()))
    }

    /// 一次按键在建议这一层怎么分（方案 6.2 的按键规则表）。
    ///
    /// - `plain`：没有任何修饰键；`is_held`：这是按住产生的重复事件；`is_bound`：这个组合是用户自己绑定的"选择建议"键。
    /// - 普通模式下 Tab / Enter / 上下键（含 Alt、Ctrl+Alt 组合）一律 [`KeyDecision::Pass`]，不接受第一条、不预选；
    /// - 接受键按住的重复事件由 latch 吃掉，直到松开；一次新的按下（非重复）先释放旧 latch；
    /// - 选择模式：上下只选候选；Tab / Enter 有选择就接受（只追加后缀并退出），没选择就关闭模式、按原样交给终端；
    ///   Esc 关闭并消费；其他键（普通输入、退格、带修饰键的组合……）退出模式并交回原输入链一次；
    /// - Esc 只在浮层拥有它（列表正在显示）时消费，否则交给前台程序。
    pub(crate) fn key_decision(
        &mut self,
        key: &str,
        plain: bool,
        is_held: bool,
        is_bound: bool,
    ) -> KeyDecision {
        if self.tracker.latch_consumes(key) {
            if is_held {
                return KeyDecision::Consume;
            }
            // 新的一次按下：旧的 latch 是松开事件丢了留下的。
            self.tracker.release_accept_latch(key);
        }
        if self.suggest.select_mode {
            if self.interactive_view().is_none() {
                self.exit_select_mode();
            } else if is_bound && !is_held {
                // 再按一次绑定键 = 退出选择模式；这个组合在本上下文里归建议所有，不交给 shell。
                self.exit_select_mode();
                self.tracker.arm_accept_latch(key);
                return KeyDecision::Consume;
            } else {
                return self.select_mode_key(key, plain);
            }
        }
        if is_bound && !is_held && self.enter_select_mode() {
            self.tracker.arm_accept_latch(key);
            return KeyDecision::Consume;
        }
        // 只是显示着的列表没有拿到键盘：Esc 收起它，但照样交给前台程序（vi 模式的 shell、
        // 把 Esc 当 Meta 前缀的用法都靠这一下）。只有显式进入选择模式后 Esc 才归列表、被消费。
        if plain && key == "escape" && self.interactive_view().is_some() {
            self.suggest.dismiss();
        }
        KeyDecision::Pass
    }

    fn select_mode_key(&mut self, key: &str, plain: bool) -> KeyDecision {
        match (key, plain) {
            ("up", true) => {
                self.move_selection(false);
                KeyDecision::Consume
            }
            ("down", true) => {
                self.move_selection(true);
                KeyDecision::Consume
            }
            ("tab" | "enter", true) => match self.suggest.selected {
                Some(index) => KeyDecision::Accept(index),
                None => {
                    self.exit_select_mode();
                    KeyDecision::Pass
                }
            },
            ("escape", true) => {
                self.suggest.dismiss();
                KeyDecision::Consume
            }
            _ => {
                self.exit_select_mode();
                KeyDecision::Pass
            }
        }
    }

    /// 接受键松开 / 窗口失焦：清掉 latch。
    pub(crate) fn release_latch(&mut self, key: &str) {
        self.tracker.release_accept_latch(key);
    }

    pub(crate) fn focus_lost(&mut self) {
        self.tracker.on_focus_lost();
    }

    pub(crate) fn arm_latch(&mut self, key: &str) {
        self.tracker.arm_accept_latch(key);
    }

    /// 测试：让检索任务停住，由测试决定什么时候跑。
    #[cfg(test)]
    pub(crate) fn defer_search_jobs(&mut self) {
        self.suggest.deferred = Some(Vec::new());
    }

    /// 测试：跑掉停住的检索任务。
    #[cfg(test)]
    pub(crate) fn run_deferred_jobs(&mut self) -> usize {
        let jobs = self.suggest.deferred.as_mut().map(std::mem::take);
        let jobs = jobs.unwrap_or_default();
        let count = jobs.len();
        for job in jobs {
            job();
        }
        count
    }

    #[cfg(test)]
    pub(crate) fn searches_started(&self) -> u32 {
        self.suggest.started
    }

    #[cfg(test)]
    pub(crate) fn search_in_flight(&self) -> bool {
        self.suggest.inflight.is_some()
    }

    #[cfg(test)]
    pub(crate) fn suggest_typed(&self) -> Option<&str> {
        self.suggest
            .shown
            .as_ref()
            .map(|shown| shown.typed.as_str())
    }

    #[cfg(test)]
    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.suggest.selected
    }
}

impl super::super::Pane {
    /// 接受第 `index` 条候选：只把后缀交给 [`Self::accept_suggestion`]（不发回车、先登记再发送）。
    /// 不管成功与否，列表都关掉；被拒绝时一个字节也不发。
    pub(crate) fn accept_candidate(
        &mut self,
        index: usize,
    ) -> Result<WriteOutcome, SuggestRefusal> {
        let plan = self.assist.plan_accept(index);
        self.assist.dismiss_suggestions();
        let (suffix, id) = plan?;
        self.accept_suggestion(&suffix, &id)
            .map_err(SuggestRefusal::Accept)
    }

    /// 一次按键在建议这一层的处理；返回这次按键是否已经被建议这一层消费。
    pub(crate) fn suggest_key(
        &mut self,
        key: &str,
        plain: bool,
        is_held: bool,
        is_bound: bool,
    ) -> bool {
        match self.assist.key_decision(key, plain, is_held, is_bound) {
            KeyDecision::Pass => false,
            KeyDecision::Consume => true,
            KeyDecision::Accept(index) => {
                // 结果不重要：成功会进入 AwaitingEcho，失败状态机自己失同步。键本身一定被吃掉，
                // 否则 Enter 会变成"接受 + 执行"。
                let _ = self.accept_candidate(index);
                self.assist.arm_latch(key);
                true
            }
        }
    }
}
