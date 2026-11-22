//! 会话表：条目状态、代次、ID 分配。
//!
//! 会话表只保存"连接中 / 已连上"的条目和它们的元数据，打开、关闭的具体动作由
//! 入口层通过条目里的值完成。G04.2.2 把 Tauri 的会话表搬到这里，Tauri 只保留
//! 取句柄的薄封装。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::{CancelToken, Generation, SessionId, SpawnError, TaskRegistry};

/// 取消"正在连接"的会话。入口层提供具体实现（Tauri 传的是自己的取消信号）。
pub type CancelHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Open,
    Closing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub generation: Generation,
    pub state: SessionState,
    pub host_id: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionError {
    AlreadyExists(SessionId),
    NotFound(SessionId),
    WrongGeneration {
        expected: Generation,
        actual: Generation,
    },
    InvalidState {
        id: SessionId,
        state: SessionState,
    },
    /// 登记表不接受这次打开的后台任务（key 已被占用）；会话条目已经回滚。
    TaskRejected(SpawnError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists(id) => write!(f, "会话已经存在：{id}"),
            Self::NotFound(id) => write!(f, "会话不存在：{id}"),
            Self::WrongGeneration { expected, actual } => {
                write!(f, "会话代次不匹配：期望 {expected}，实际 {actual}")
            }
            Self::InvalidState { id, state } => {
                write!(f, "会话 {id} 当前状态是 {state:?}，不能执行该操作")
            }
            Self::TaskRejected(err) => write!(f, "无法启动连接任务：{err}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// 入口发起一次打开时要填的部分；会话 ID 和取消信号由服务补上。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSpec {
    pub host_id: Option<i64>,
    pub generation: Generation,
    pub cols: u16,
    pub rows: u16,
}

/// 服务派给 [`SessionOpener`] 的一次打开：会话 ID、取消信号和入口填的参数。
///
/// 取消信号由服务创建，表里的 `CancelHook` 直接取消它；入口在连接过程中等它，
/// 关掉还在连接的会话就能中断握手和认证。
pub struct OpenRequest {
    pub session: SessionId,
    pub host_id: Option<i64>,
    pub generation: Generation,
    pub cols: u16,
    pub rows: u16,
    pub cancel: CancelToken,
}

/// 打开失败的两类结果：用户关掉了还在连接的会话，或者连接本身失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenFailure {
    Cancelled,
    Failed(String),
}

/// 入口实现的打开动作。
///
/// 服务只负责分配 ID、登记、推进状态和收尾；怎么连、发什么事件仍由入口决定，
/// 这样运行层不依赖 Tauri / SSH。
pub trait SessionOpener<E>: Send + Sync + 'static {
    /// 已经登记为「连接中」、后台任务马上开始。入口在这里发 connecting 事件。
    fn connecting(&self, _id: &SessionId) {}

    /// 真正去连；返回的条目会在成功后放进会话表。
    fn open<'a>(&'a self, req: OpenRequest) -> BoxFuture<'a, Result<E, OpenFailure>>;

    /// 条目已经放进表里（`slot`）。入口在这里发 connected、起会话任务。
    fn opened(&self, id: &SessionId, slot: &E);

    /// 后台任务没登记上，`connecting` 之后不会再有任何回调；会话条目已经被撤掉。
    /// 入口在这里告诉界面这条会话没起来（界面可能已经看到了 connecting）。
    fn rejected(&self, _id: &SessionId, _reason: &str) {}

    /// 这次打开没成：`Failed` 带原因，`Cancelled` 只做清理、不再发断线事件。
    fn failed<'a>(&'a self, id: &'a SessionId, failure: &'a OpenFailure) -> BoxFuture<'a, ()>;
}

/// 会话表里的条目：正在连接（后台任务已经起了）或者已经连上。
///
/// 条目本身决定 `Connecting` / `Open` 状态，不另存一份状态字段。
pub enum SessionEntry<E> {
    Connecting { cancel: CancelHook },
    Open(E),
}

/// 表里的内部记录：条目 + 查询用的元数据。
struct Slot<E> {
    generation: Generation,
    host_id: Option<i64>,
    entry: SessionEntry<E>,
    /// 已经开始关闭（`begin_close`）。关闭只是标记，条目仍在表里，直到被取走。
    closing: bool,
}

impl<E> Slot<E> {
    fn state(&self) -> SessionState {
        if self.closing {
            return SessionState::Closing;
        }
        match &self.entry {
            SessionEntry::Connecting { .. } => SessionState::Connecting,
            SessionEntry::Open(_) => SessionState::Open,
        }
    }

    fn info(&self, id: &SessionId) -> SessionInfo {
        SessionInfo {
            id: id.clone(),
            generation: self.generation,
            state: self.state(),
            host_id: self.host_id,
        }
    }
}

/// 会话表；所有方法都是短临界区，不跨 await 持锁，也不在锁里调条目里的回调。
pub struct SessionService<E> {
    sessions: Mutex<HashMap<SessionId, Slot<E>>>,
    next_id: AtomicU64,
}

impl<E> Default for SessionService<E> {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }
}

impl<E> SessionService<E> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 分配会话 ID：`sess-{n}`，从 1 开始递增（方便和日志对照）。
    pub fn allocate(&self) -> SessionId {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        SessionId::new(format!("sess-{n}"))
    }

    pub fn insert_connecting(
        &self,
        id: SessionId,
        generation: Generation,
        host_id: Option<i64>,
        cancel: CancelHook,
    ) -> Result<(), SessionError> {
        let mut sessions = self.lock();
        if sessions.contains_key(&id) {
            return Err(SessionError::AlreadyExists(id));
        }
        sessions.insert(
            id,
            Slot {
                generation,
                host_id,
                entry: SessionEntry::Connecting { cancel },
                closing: false,
            },
        );
        Ok(())
    }

    /// 连接成功：Connecting -> Open，代次必须一致。
    pub fn promote(
        &self,
        id: &SessionId,
        generation: Generation,
        entry: E,
    ) -> Result<(), SessionError> {
        let mut sessions = self.lock();
        let slot = sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.clone()))?;
        if slot.generation != generation {
            return Err(SessionError::WrongGeneration {
                expected: slot.generation,
                actual: generation,
            });
        }
        let state = slot.state();
        if state != SessionState::Connecting {
            return Err(SessionError::InvalidState {
                id: id.clone(),
                state,
            });
        }
        slot.entry = SessionEntry::Open(entry);
        slot.closing = false;
        Ok(())
    }

    /// 开始关闭：标记 Closing，重复调用幂等。代次不一致仍然拒绝。
    pub fn begin_close(&self, id: &SessionId, generation: Generation) -> Result<(), SessionError> {
        let mut sessions = self.lock();
        let slot = sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.clone()))?;
        if slot.generation != generation {
            return Err(SessionError::WrongGeneration {
                expected: slot.generation,
                actual: generation,
            });
        }
        slot.closing = true;
        Ok(())
    }

    /// 关闭的第一步：一次加锁把条目取走。取走之后新的查询立刻看不到它，
    /// 调用方在锁外做关闭动作，重复取会拿到 `NotFound`。
    pub fn take_for_close(&self, id: &SessionId) -> Result<SessionEntry<E>, SessionError> {
        let mut sessions = self.lock();
        sessions
            .remove(id)
            .map(|slot| slot.entry)
            .ok_or_else(|| SessionError::NotFound(id.clone()))
    }

    /// 只摘掉还在连接中的条目；已经连上的不动。返回有没有摘到。
    pub fn remove_connecting(&self, id: &SessionId) -> bool {
        let mut sessions = self.lock();
        let connecting = matches!(
            sessions.get(id).map(|slot| &slot.entry),
            Some(SessionEntry::Connecting { .. })
        );
        if connecting {
            sessions.remove(id);
        }
        connecting
    }

    pub fn contains(&self, id: &SessionId) -> bool {
        self.lock().contains_key(id)
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionInfo> {
        self.lock().get(id).map(|slot| slot.info(id))
    }

    /// 已连上的条目（连接中的不算）。
    pub fn open_entry(&self, id: &SessionId) -> Option<E>
    where
        E: Clone,
    {
        match self.lock().get(id).map(|slot| &slot.entry) {
            Some(SessionEntry::Open(entry)) => Some(entry.clone()),
            _ => None,
        }
    }

    /// 已连上的条目快照，按 id 排序；连接中的不出现。锁在返回前已经释放。
    pub fn open_entries(&self) -> Vec<(SessionId, E)>
    where
        E: Clone,
    {
        let sessions = self.lock();
        let mut entries: Vec<(SessionId, E)> = sessions
            .iter()
            .filter_map(|(id, slot)| match &slot.entry {
                SessionEntry::Open(entry) => Some((id.clone(), entry.clone())),
                SessionEntry::Connecting { .. } => None,
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// 按 id 排序，便于界面和测试稳定比较。
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> =
            self.lock().iter().map(|(id, slot)| slot.info(id)).collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取走调用这一刻表里的所有条目（快照语义）：之后新插入的不受影响。
    /// 锁在返回前已经释放，调用方在锁外做关闭动作。
    pub fn drain_all(&self) -> Vec<(SessionId, SessionEntry<E>)> {
        let mut sessions = self.lock();
        let mut drained: Vec<(SessionId, SessionEntry<E>)> = sessions
            .drain()
            .map(|(id, slot)| (id, slot.entry))
            .collect();
        drained.sort_by(|a, b| a.0.cmp(&b.0));
        drained
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Slot<E>>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<E: Clone + Send + 'static> SessionService<E> {
    /// 登记一个「连接中」的会话并在后台打开；立即返回会话 ID。
    pub fn begin_open(
        self: &Arc<Self>,
        runtime: &Arc<TaskRegistry>,
        spec: OpenSpec,
        opener: Arc<dyn SessionOpener<E>>,
    ) -> Result<SessionId, SessionError> {
        let id = self.allocate();
        self.start_open(
            runtime,
            OpenRequest {
                session: id,
                host_id: spec.host_id,
                generation: spec.generation,
                cols: spec.cols,
                rows: spec.rows,
                cancel: CancelToken::new(),
            },
            opener,
        )
    }

    /// 从一条已经连上的会话复制一个：只有源会话在表里且已连上才允许。
    pub fn begin_duplicate(
        self: &Arc<Self>,
        runtime: &Arc<TaskRegistry>,
        source: &SessionId,
        spec: OpenSpec,
        opener: Arc<dyn SessionOpener<E>>,
    ) -> Result<SessionId, SessionError> {
        match self.get(source) {
            Some(info) if info.state == SessionState::Open => {}
            Some(info) => {
                return Err(SessionError::InvalidState {
                    id: source.clone(),
                    state: info.state,
                });
            }
            None => return Err(SessionError::NotFound(source.clone())),
        }
        let id = self.allocate();
        self.start_open(
            runtime,
            OpenRequest {
                session: id,
                host_id: spec.host_id,
                generation: spec.generation,
                cols: spec.cols,
                rows: spec.rows,
                cancel: CancelToken::new(),
            },
            opener,
        )
    }

    /// 先登记再起任务：界面在任务开始前就能看到「连接中」。
    fn start_open(
        self: &Arc<Self>,
        runtime: &Arc<TaskRegistry>,
        req: OpenRequest,
        opener: Arc<dyn SessionOpener<E>>,
    ) -> Result<SessionId, SessionError> {
        let id = req.session.clone();
        let generation = req.generation;
        let hook: CancelHook = {
            let cancel = req.cancel.clone();
            Arc::new(move || cancel.cancel())
        };
        self.insert_connecting(id.clone(), generation, req.host_id, hook)?;
        opener.connecting(&id);

        let service = Arc::clone(self);
        let task_opener = Arc::clone(&opener);
        let task_id = id.clone();
        let result = runtime.spawn(format!("session-open:{id}"), move |reg_cancel| async move {
            let cancel = req.cancel.clone();
            let open = task_opener.open(req);
            tokio::pin!(open);
            let outcome = tokio::select! {
                outcome = &mut open => outcome,
                _ = reg_cancel.cancelled() => {
                    // 运行层发的取消也要落到入口的连接上；取消后等它自己收尾，
                    // 不能直接丢掉 future（池子占位和挂着的提问要清掉）。
                    cancel.cancel();
                    open.await
                }
            };
            service
                .finish_open(task_id, generation, outcome, task_opener.as_ref())
                .await;
        });
        if let Err(err) = result {
            // 任务没起来就不会有人收尾这个条目：撤掉它，免得界面上永远停在"连接中"。
            tracing::warn!(%err, %id, "连接任务登记失败，撤销会话条目");
            self.remove_connecting(&id);
            opener.rejected(&id, &err.to_string());
            return Err(SessionError::TaskRejected(err));
        }
        Ok(id)
    }

    /// 打开结束：成功就推进成已连上，失败就摘掉条目并交给入口收尾。
    async fn finish_open(
        &self,
        id: SessionId,
        generation: Generation,
        outcome: Result<E, OpenFailure>,
        opener: &dyn SessionOpener<E>,
    ) {
        match outcome {
            Ok(entry) => {
                // 先留一份给 `opened`：连接刚完成时会话可能已经被关掉了，条目已经
                // 取走，但入口仍然要起会话任务去收尾（和旧入口的行为一致）。
                let slot = entry.clone();
                match self.promote(&id, generation, entry) {
                    Ok(()) => opener.opened(&id, &slot),
                    Err(err) => {
                        tracing::debug!(%err, %id, "连接完成时会话已经不在表里");
                        self.remove_connecting(&id);
                        opener.failed(&id, &OpenFailure::Cancelled).await;
                    }
                }
            }
            Err(OpenFailure::Cancelled) => {
                self.remove_connecting(&id);
                opener.failed(&id, &OpenFailure::Cancelled).await;
            }
            Err(failure) => {
                // 条目已经被关掉了：和取消一样只清理，不再发断线事件。
                if self.remove_connecting(&id) {
                    opener.failed(&id, &failure).await;
                } else {
                    opener.failed(&id, &OpenFailure::Cancelled).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Payload(u32);

    fn service() -> Arc<SessionService<Payload>> {
        SessionService::new()
    }

    fn hook() -> CancelHook {
        Arc::new(|| {})
    }

    fn insert(service: &SessionService<Payload>, id: &str) {
        service
            .insert_connecting(SessionId::new(id), Generation(1), Some(7), hook())
            .unwrap();
    }

    #[test]
    fn insert_then_promote() {
        let service = service();
        let id = SessionId::new("s1");
        service
            .insert_connecting(id.clone(), Generation(1), Some(7), hook())
            .unwrap();
        assert_eq!(service.get(&id).unwrap().state, SessionState::Connecting);
        service.promote(&id, Generation(1), Payload(1)).unwrap();
        let info = service.get(&id).unwrap();
        assert_eq!(info.state, SessionState::Open);
        assert_eq!(info.host_id, Some(7));
    }

    #[test]
    fn duplicate_session_is_rejected() {
        let service = service();
        let id = SessionId::new("s1");
        service
            .insert_connecting(id.clone(), Generation(1), Some(7), hook())
            .unwrap();
        assert_eq!(
            service.insert_connecting(id.clone(), Generation(2), Some(8), hook()),
            Err(SessionError::AlreadyExists(id))
        );
    }

    #[test]
    fn stale_generation_cannot_promote_or_close() {
        let service = service();
        let id = SessionId::new("s1");
        insert(&service, "s1");
        assert!(matches!(
            service.promote(&id, Generation(0), Payload(1)),
            Err(SessionError::WrongGeneration { .. })
        ));
        assert!(matches!(
            service.begin_close(&id, Generation(0)),
            Err(SessionError::WrongGeneration { .. })
        ));
        assert_eq!(service.get(&id).unwrap().state, SessionState::Connecting);
    }

    #[test]
    fn close_is_idempotent_and_take_returns_entry() {
        let service = service();
        let id = SessionId::new("s1");
        insert(&service, "s1");
        service.promote(&id, Generation(1), Payload(1)).unwrap();
        service.begin_close(&id, Generation(1)).unwrap();
        service.begin_close(&id, Generation(1)).unwrap();
        assert_eq!(service.get(&id).unwrap().state, SessionState::Closing);
        assert!(matches!(
            service.take_for_close(&id).unwrap(),
            SessionEntry::Open(Payload(1))
        ));
        assert!(service.get(&id).is_none());
        assert!(service.is_empty());
    }

    #[test]
    fn promote_requires_connecting_state() {
        let service = service();
        let id = SessionId::new("s1");
        insert(&service, "s1");
        service.promote(&id, Generation(1), Payload(1)).unwrap();
        assert!(matches!(
            service.promote(&id, Generation(1), Payload(2)),
            Err(SessionError::InvalidState { .. })
        ));
    }

    #[test]
    fn list_is_sorted_by_id() {
        let service = service();
        for id in ["s3", "s1", "s2"] {
            service
                .insert_connecting(SessionId::new(id), Generation(1), None, hook())
                .unwrap();
        }
        let ids: Vec<String> = service
            .list()
            .into_iter()
            .map(|s| s.id.to_string())
            .collect();
        assert_eq!(ids, ["s1", "s2", "s3"]);
    }

    #[test]
    fn allocate_is_monotonic_and_formatted() {
        let service = service();
        assert_eq!(service.allocate().as_str(), "sess-1");
        assert_eq!(service.allocate().as_str(), "sess-2");
        assert_eq!(service.allocate().as_str(), "sess-3");
    }

    #[test]
    fn promote_after_take_for_close_is_not_found() {
        let service = service();
        let id = SessionId::new("s1");
        insert(&service, "s1");
        assert!(service.take_for_close(&id).is_ok());
        assert_eq!(
            service.promote(&id, Generation(1), Payload(1)).unwrap_err(),
            SessionError::NotFound(id.clone())
        );
        assert!(matches!(
            service.take_for_close(&id),
            Err(SessionError::NotFound(_))
        ));
    }

    #[test]
    fn take_for_close_is_atomic() {
        let service = service();
        let id = SessionId::new("s1");
        insert(&service, "s1");
        let taken: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let service = &service;
                    let id = &id;
                    scope.spawn(move || service.take_for_close(id).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| usize::from(handle.join().unwrap()))
                .sum()
        });
        assert_eq!(taken, 1, "同一个条目只能被取走一次");
        assert!(service.is_empty());
    }

    #[test]
    fn drain_all_is_a_snapshot() {
        let service = service();
        insert(&service, "s1");
        insert(&service, "s2");
        let drained = service.drain_all();
        assert_eq!(drained.len(), 2);
        assert!(service.is_empty());

        // 第一次 drain 之后新插入的条目不属于那次快照。
        insert(&service, "s3");
        let drained = service.drain_all();
        let ids: Vec<String> = drained.iter().map(|(id, _)| id.to_string()).collect();
        assert_eq!(ids, ["s3"]);
        assert!(service.is_empty());
    }

    #[test]
    fn open_entries_excludes_connecting() {
        let service = service();
        insert(&service, "s1");
        insert(&service, "s2");
        let id = SessionId::new("s2");
        service.promote(&id, Generation(1), Payload(2)).unwrap();

        let entries = service.open_entries();
        assert_eq!(
            entries
                .iter()
                .map(|(id, payload)| (id.as_str().to_owned(), payload.clone()))
                .collect::<Vec<_>>(),
            vec![("s2".to_owned(), Payload(2))]
        );
        assert_eq!(service.open_entry(&id), Some(Payload(2)));
        assert_eq!(service.open_entry(&SessionId::new("s1")), None);
    }

    #[test]
    fn remove_connecting_leaves_open_entries_alone() {
        let service = service();
        insert(&service, "s1");
        insert(&service, "s2");
        let id = SessionId::new("s2");
        service.promote(&id, Generation(1), Payload(2)).unwrap();

        assert!(service.remove_connecting(&SessionId::new("s1")));
        assert!(!service.remove_connecting(&id));
        assert_eq!(service.len(), 1);
        assert!(service.contains(&id));
    }

    enum Behavior {
        /// 等测试放行，然后按给定结果结束。
        CompleteAfter(
            tokio::sync::oneshot::Receiver<()>,
            Result<Payload, OpenFailure>,
        ),
        /// 一直等到服务取消，再按取消结束。
        WaitForCancel,
    }

    /// 记录服务调了哪些钩子，`open` 的行为由测试指定。
    struct FakeOpener {
        behavior: Mutex<Option<Behavior>>,
        connecting: Mutex<Vec<String>>,
        opened: Mutex<Vec<(String, Payload)>>,
        failed: Mutex<Vec<(String, OpenFailure)>>,
        rejected: Mutex<Vec<String>>,
    }

    impl FakeOpener {
        fn new(behavior: Behavior) -> Arc<Self> {
            Arc::new(Self {
                behavior: Mutex::new(Some(behavior)),
                connecting: Mutex::new(Vec::new()),
                opened: Mutex::new(Vec::new()),
                failed: Mutex::new(Vec::new()),
                rejected: Mutex::new(Vec::new()),
            })
        }

        fn rejected_ids(&self) -> Vec<String> {
            self.rejected.lock().unwrap().clone()
        }

        fn connecting_ids(&self) -> Vec<String> {
            self.connecting.lock().unwrap().clone()
        }

        fn opened_ids(&self) -> Vec<String> {
            self.opened
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        }

        fn failures(&self) -> Vec<(String, OpenFailure)> {
            self.failed.lock().unwrap().clone()
        }
    }

    impl SessionOpener<Payload> for FakeOpener {
        fn connecting(&self, id: &SessionId) {
            self.connecting.lock().unwrap().push(id.to_string());
        }

        fn open<'a>(&'a self, req: OpenRequest) -> BoxFuture<'a, Result<Payload, OpenFailure>> {
            Box::pin(async move {
                let behavior = self
                    .behavior
                    .lock()
                    .unwrap()
                    .take()
                    .expect("open 只会被调一次");
                match behavior {
                    Behavior::CompleteAfter(release, result) => {
                        let _ = release.await;
                        result
                    }
                    Behavior::WaitForCancel => {
                        req.cancel.cancelled().await;
                        Err(OpenFailure::Cancelled)
                    }
                }
            })
        }

        fn rejected(&self, id: &SessionId, _reason: &str) {
            self.rejected.lock().unwrap().push(id.to_string());
        }

        fn opened(&self, id: &SessionId, slot: &Payload) {
            self.opened
                .lock()
                .unwrap()
                .push((id.to_string(), slot.clone()));
        }

        fn failed<'a>(&'a self, id: &'a SessionId, failure: &'a OpenFailure) -> BoxFuture<'a, ()> {
            self.failed
                .lock()
                .unwrap()
                .push((id.to_string(), failure.clone()));
            Box::pin(async {})
        }
    }

    fn spec() -> OpenSpec {
        OpenSpec {
            host_id: Some(7),
            generation: Generation(1),
            cols: 80,
            rows: 24,
        }
    }

    async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !ready() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("等不到：{what}"));
    }

    #[tokio::test]
    async fn begin_open_registers_before_spawning_then_promotes() {
        let registry = TaskRegistry::new();
        let service: Arc<SessionService<Payload>> = SessionService::new();
        let (release, gate) = tokio::sync::oneshot::channel();
        let opener = FakeOpener::new(Behavior::CompleteAfter(gate, Ok(Payload(7))));

        let id = service
            .begin_open(&registry, spec(), opener.clone())
            .expect("登记会话");

        // 任务马上开始，但还没放行：条目应该已经在表里，connecting 也发过了。
        let info = service.get(&id).expect("连接中的条目");
        assert_eq!(info.state, SessionState::Connecting);
        assert_eq!(info.host_id, Some(7));
        assert_eq!(opener.connecting_ids(), [id.to_string()]);
        assert!(opener.opened_ids().is_empty());

        release.send(()).unwrap();
        wait_until("会话推进成已连上", || {
            service
                .get(&id)
                .is_some_and(|info| info.state == SessionState::Open)
        })
        .await;
        wait_until("opened 被调用", || !opener.opened_ids().is_empty()).await;
        assert_eq!(opener.opened_ids(), [id.to_string()]);
        assert_eq!(service.open_entry(&id), Some(Payload(7)));
        assert!(opener.failures().is_empty());
    }

    #[tokio::test]
    async fn a_rejected_task_registration_rolls_the_connecting_entry_back() {
        let registry = TaskRegistry::new();
        let service: Arc<SessionService<Payload>> = SessionService::new();
        // 让下一个会话（sess-1）的打开任务 key 已经被占着，模拟登记失败。
        registry
            .spawn("session-open:sess-1", |token| async move {
                token.cancelled().await
            })
            .expect("占住 key");
        let opener = FakeOpener::new(Behavior::WaitForCancel);

        let err = service
            .begin_open(&registry, spec(), opener.clone())
            .expect_err("任务没登记上就不能返回会话 ID");

        assert!(matches!(err, SessionError::TaskRejected(_)), "{err}");
        // 不能留下永远"连接中"的条目，也不能让入口去连。
        assert_eq!(service.len(), 0);
        assert!(!service.contains(&SessionId::new("sess-1")));
        assert_eq!(opener.rejected_ids(), ["sess-1"]);
        let _ = registry.shutdown(std::time::Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn failed_open_removes_the_connecting_entry_and_reports_the_reason() {
        let registry = TaskRegistry::new();
        let service: Arc<SessionService<Payload>> = SessionService::new();
        let (release, gate) = tokio::sync::oneshot::channel();
        let opener = FakeOpener::new(Behavior::CompleteAfter(
            gate,
            Err(OpenFailure::Failed("认证失败".to_owned())),
        ));

        let id = service
            .begin_open(&registry, spec(), opener.clone())
            .expect("登记会话");
        release.send(()).unwrap();

        wait_until("失败被上报", || !opener.failures().is_empty()).await;
        assert_eq!(
            opener.failures(),
            [(id.to_string(), OpenFailure::Failed("认证失败".to_owned()))]
        );
        assert!(!service.contains(&id), "失败的条目要从表里摘掉");
        assert!(opener.opened_ids().is_empty());
    }

    #[tokio::test]
    async fn closing_while_connecting_cancels_the_open_through_the_entry_hook() {
        let registry = TaskRegistry::new();
        let service: Arc<SessionService<Payload>> = SessionService::new();
        let opener = FakeOpener::new(Behavior::WaitForCancel);

        let id = service
            .begin_open(&registry, spec(), opener.clone())
            .expect("登记会话");
        let entry = service.take_for_close(&id).expect("取走连接中的条目");
        match entry {
            SessionEntry::Connecting { cancel } => cancel(),
            SessionEntry::Open(_) => panic!("刚登记的不该是已连上"),
        }

        wait_until("取消被上报", || !opener.failures().is_empty()).await;
        assert_eq!(
            opener.failures(),
            [(id.to_string(), OpenFailure::Cancelled)]
        );
        assert!(service.is_empty());
        assert!(opener.opened_ids().is_empty());
    }

    #[tokio::test]
    async fn runtime_cancel_reaches_the_open_task() {
        let registry = TaskRegistry::new();
        let service: Arc<SessionService<Payload>> = SessionService::new();
        let opener = FakeOpener::new(Behavior::WaitForCancel);

        let id = service
            .begin_open(&registry, spec(), opener.clone())
            .expect("登记会话");
        assert!(registry.cancel(&format!("session-open:{id}")));

        wait_until("运行层取消被上报", || !opener.failures().is_empty()).await;
        assert_eq!(
            opener.failures(),
            [(id.to_string(), OpenFailure::Cancelled)]
        );
        assert!(!service.contains(&id));
    }

    #[tokio::test]
    async fn begin_duplicate_requires_an_open_source_and_allocates_a_new_id() {
        let registry = TaskRegistry::new();
        let service: Arc<SessionService<Payload>> = SessionService::new();
        let source = service.allocate();
        service
            .insert_connecting(source.clone(), Generation(1), Some(7), hook())
            .unwrap();
        service.promote(&source, Generation(1), Payload(1)).unwrap();

        let (release, gate) = tokio::sync::oneshot::channel();
        let opener = FakeOpener::new(Behavior::CompleteAfter(gate, Ok(Payload(2))));
        let id = service
            .begin_duplicate(&registry, &source, spec(), opener.clone())
            .expect("复制会话");
        assert_ne!(id, source);

        release.send(()).unwrap();
        wait_until("复制的会话连上", || {
            service
                .get(&id)
                .is_some_and(|info| info.state == SessionState::Open)
        })
        .await;
        assert_eq!(service.open_entry(&id), Some(Payload(2)));
        assert_eq!(service.open_entry(&source), Some(Payload(1)));

        // 源会话还在连接中时不能复制；不存在也不行。
        let connecting = service.allocate();
        service
            .insert_connecting(connecting.clone(), Generation(1), None, hook())
            .unwrap();
        let rejected = FakeOpener::new(Behavior::WaitForCancel);
        assert_eq!(
            service
                .begin_duplicate(&registry, &connecting, spec(), rejected)
                .unwrap_err(),
            SessionError::InvalidState {
                id: connecting.clone(),
                state: SessionState::Connecting,
            }
        );
        let missing = SessionId::new("sess-404");
        assert_eq!(
            service
                .begin_duplicate(
                    &registry,
                    &missing,
                    spec(),
                    FakeOpener::new(Behavior::WaitForCancel),
                )
                .unwrap_err(),
            SessionError::NotFound(missing)
        );
    }
}
