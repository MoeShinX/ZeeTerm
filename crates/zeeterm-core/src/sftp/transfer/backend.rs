//! 传输引擎的数据面接口（M3-1 第 3 节）。
//!
//! 引擎不直接碰 SFTP，而是通过 [`TransferEndpoints`]：
//! - 生产实现 [`SftpEndpoints`]：向 [`TransferChannelProvider`] 要通道，建"本地 ↔ 远端"
//!   的端点对；
//! - 单测实现（`memory.rs` 里的 `MemoryEndpoints`）：两端都在内存里，能造断线、磁盘满
//!   这类边界，不用服务器。
//!
//! 展开远端目录要读目录，所以接口里也带上 [`RemoteLister`]（下载时要走它）。

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use super::endpoint::{FileEndpoint, LocalEndpoint, RemoteEndpoint};
use super::Direction;
use crate::error::{reason_keys, Reason};
use crate::sftp::error::SftpError;
use crate::sftp::fs::DirEntry;
use crate::sftp::session::SftpChannel;
use crate::ssh::{BoxFuture, Connection};

/// 一个文件的源和目标。
pub struct FilePair {
    pub source: Arc<dyn FileEndpoint>,
    pub target: Arc<dyn FileEndpoint>,
}

/// 读远端（展开下载任务用）：属性 + 目录。
pub trait RemoteLister: Send + Sync {
    fn list_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<DirEntry>, SftpError>>;
    /// 读属性（`lstat`；判断是不是目录）。
    fn stat_remote<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>>;
    /// 读链接指向的目标的属性（`stat`）。用户直接选中一个符号链接下载时，下载的是它指向的
    /// 文件 / 目录，大小和类型都要按目标算。没有符号链接的实现（内存夹具）用默认的 `lstat` 即可。
    fn stat_follow<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        self.stat_remote(path)
    }
}

/// 传输用得到的数据面。
pub trait TransferEndpoints: RemoteLister {
    /// 建一个文件的源 / 目标端点（按任务方向决定哪边是本地）。`owner` 是任务标识，
    /// 本地临时文件的名字里带着它，重试同一个任务时才找得回自己的临时文件。
    ///
    /// `peer`：只有 `RemoteCopy` 用，源在另一台主机时是那台主机的登记名（见
    /// [`SftpEndpoints::register_peer`]）；`None` = 源和目标在同一台主机。
    fn pair<'a>(
        &'a self,
        direction: Direction,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<FilePair, SftpError>>;

    /// 展开"远端 → 远端"复制任务：源树从 `peer`（或本机的连接）读，目标全是本机连接上的路径。
    fn plan_remote_copy<'a>(
        &'a self,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        recursive: bool,
    ) -> BoxFuture<'a, Result<super::plan::TransferPlan, SftpError>>;

    /// 目标端的目录（上传时是远端目录）。
    fn create_target_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), SftpError>>;

    /// 断线之后让下一次拿到的通道是新的（真实现会重开通道 / 重连）。
    fn reset<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// 建议的读写块大小（其它实现给保守值）。
    ///
    /// 要按方向取：同一块数据在源端是一次读、在目标端是一次写，两端各有自己的上限
    /// （SFTP 的 `limits` 读写上限可以不同）。下载受源的读上限约束，上传受目标的写上限约束，
    /// 远端到远端两者都要守。`peer` 的含义同 [`Self::pair`]。
    fn suggested_chunk_len(&self, _direction: Direction, _peer: Option<&str>) -> u32 {
        crate::sftp::session::CONSERVATIVE_CHUNK
    }

    /// 这个数据面连的服务器身份（用户名@主机:端口，见 `ssh::connection::server_identity`）。
    /// 远端目标锁用它当键，所以**所有**引擎、编辑、AI 写文件只要实际连到的服务器相同就互斥。
    fn server_identity<'a>(&'a self) -> BoxFuture<'a, Result<String, SftpError>>;
}

/// 传输通道从哪来（浏览/小操作用的那条是另一条；传输按需另开）。
pub trait TransferChannelProvider: Send + Sync {
    /// 拿一条传输用的通道（通常复用一条；断线之后重开会新开）。
    fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>>;
    /// 开通道被服务器拒绝时改走专用连接（默认实现就是直接报错）。
    fn dedicated_channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
        self.channel()
    }
    /// 断线之后让下次 `channel()` 重新开一条。
    fn reset<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    /// 通道已经开着时，用它协商出来的读限制当块大小；还没开就用保守值。
    fn suggested_read_len(&self) -> u32 {
        crate::sftp::session::CONSERVATIVE_CHUNK
    }
    /// 通道已经开着时，用它协商出来的写限制当块大小；还没开就用保守值。
    fn suggested_write_len(&self) -> u32 {
        crate::sftp::session::CONSERVATIVE_CHUNK
    }
}

/// 按传输方向选块大小：块在源端是一次读、在目标端是一次写，要同时守住两端各自的上限。
///
/// - 下载：只受源（远端）的读上限约束，本地写没有上限；
/// - 上传：只受目标（远端）的写上限约束；
/// - 远端到远端：两端都是 SFTP，取源读上限与目标写上限里小的那个；
/// - 本地到本地：不经过 SFTP，沿用读侧的值。
fn chunk_len_for(direction: Direction, source_read: u32, target_write: u32) -> u32 {
    match direction {
        Direction::Download | Direction::LocalCopy => source_read,
        Direction::Upload => target_write,
        Direction::RemoteCopy => source_read.min(target_write),
    }
}

/// 需要一条专用连接时怎么连（生产上是"用保存的凭据 + 已确认的指纹再连一次"）。
pub trait SftpReconnect: Send + Sync {
    fn reconnect<'a>(&'a self) -> BoxFuture<'a, Result<Arc<Connection>, SftpError>>;
}

/// 通道提供者缓存的两个槽：主连接上的传输通道，和主连接开不出通道时改用的专用连接上的通道。
///
/// 单独成类型是为了让「`reset` 两个槽一起清」这件事不依赖真实的 SSH 连接就能测。
struct ChannelSlots<T> {
    cached: tokio::sync::Mutex<Option<Arc<T>>>,
    dedicated: tokio::sync::Mutex<Option<Arc<T>>>,
}

impl<T> ChannelSlots<T> {
    fn new() -> Self {
        Self {
            cached: tokio::sync::Mutex::new(None),
            dedicated: tokio::sync::Mutex::new(None),
        }
    }

    /// 专用槽里有就用，没有就用 `open` 新开一条存进去。
    async fn dedicated_or_open<F, Fut>(&self, open: F) -> Result<Arc<T>, SftpError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, SftpError>>,
    {
        if let Some(channel) = self.dedicated.lock().await.clone() {
            return Ok(channel);
        }
        let channel = Arc::new(open().await?);
        *self.dedicated.lock().await = Some(Arc::clone(&channel));
        Ok(channel)
    }

    /// 断线之后两个槽都要清：只清 `cached` 的话，之后一直走专用连接的传输会反复拿到那条
    /// 已经断掉的专用通道，重试永远失败。
    async fn reset(&self) {
        self.cached.lock().await.take();
        self.dedicated.lock().await.take();
    }
}

/// 生产用的通道提供者（M3-1 第 1 节）。
///
/// - 正常情况：在**已有连接**上开一条传输通道（浏览/小操作那条是另一条，互不干扰）；
/// - 服务器拒绝开通道（`MaxSessions` 到顶）：给了 [`SftpReconnect`] 就**新连一条专用
///   连接**再开——选这个而不是"排队等通道空出来"，是因为终端通道由用户自己开关，
///   可能一整天都占着，排队会把传输无限期拖住；
/// - 连接断了：`reset()` 之后下次重开。
pub struct SftpChannelProvider {
    /// 当前用的连接（断线之后会被换成新连的那条）。
    connection: tokio::sync::Mutex<Arc<Connection>>,
    reconnect: Option<Arc<dyn SftpReconnect>>,
    /// 缓存的传输通道和专用连接上的通道（两个槽一起失效，见 [`ChannelSlots::reset`]）。
    slots: ChannelSlots<SftpChannel>,
    /// 重连次数（诊断 / 测试）。
    reconnects: std::sync::atomic::AtomicU64,
}

impl SftpChannelProvider {
    pub fn new(connection: Arc<Connection>, reconnect: Option<Arc<dyn SftpReconnect>>) -> Self {
        Self {
            connection: tokio::sync::Mutex::new(connection),
            reconnect,
            slots: ChannelSlots::new(),
            reconnects: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 重连过几次（诊断 / 测试）。
    pub fn reconnect_count(&self) -> u64 {
        self.reconnects.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 断开之后换一条新连接（配置了 [`SftpReconnect`] 才有）。
    ///
    /// 为什么在这里做：连接死了之后，光把缓存的通道清掉没用——底层那条 SSH 会话
    /// 已经发不出包了（实测报 "Channel send error"），必须重新握手认证。
    async fn reconnect_if_needed(&self) -> Result<(), SftpError> {
        let stale = {
            let guard = self.connection.lock().await;
            guard.is_closed()
        };
        if !stale {
            return Ok(());
        }
        let Some(reconnect) = self.reconnect.clone() else {
            return Ok(());
        };
        let fresh = reconnect.reconnect().await?;
        *self.connection.lock().await = fresh;
        self.reconnects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("传输连接断了：已经重连一条");
        Ok(())
    }

    /// 已经协商好的读块大小（通道还没开就用保守值）。
    pub fn read_len_hint(&self) -> u32 {
        self.negotiated(|caps| caps.max_read_len())
    }

    /// 已经协商好的写块大小（通道还没开就用保守值）。
    pub fn write_len_hint(&self) -> u32 {
        self.negotiated(|caps| caps.max_write_len())
    }

    /// 取已开通道协商出的某个上限：先看主连接上的通道，没有再看专用连接上的；
    /// 都还没开（或锁正被占着）就用保守值——宁可小，不能超过服务器的上限。
    fn negotiated(&self, pick: impl Fn(&crate::sftp::session::SftpCaps) -> u32) -> u32 {
        for slot in [&self.slots.cached, &self.slots.dedicated] {
            if let Ok(guard) = slot.try_lock() {
                if let Some(channel) = guard.as_ref() {
                    return pick(channel.caps());
                }
            }
        }
        crate::sftp::session::CONSERVATIVE_CHUNK
    }
}

impl TransferChannelProvider for SftpChannelProvider {
    fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
        Box::pin(async move {
            if let Some(channel) = self.slots.cached.lock().await.clone() {
                return Ok(channel);
            }
            // 连接死了就先重连（不然开通道只会报 "Channel send error"）。
            self.reconnect_if_needed().await?;
            let connection = Arc::clone(&*self.connection.lock().await);
            match SftpChannel::open(&connection).await {
                Ok(channel) => {
                    let channel = Arc::new(channel);
                    *self.slots.cached.lock().await = Some(Arc::clone(&channel));
                    Ok(channel)
                }
                // 通道被拒（多半到上限）：改成另开一条专用连接。
                Err(SftpError::ChannelLimit(message)) => {
                    tracing::warn!(%message, "开传输通道被拒，改用专用连接");
                    if self.reconnect.is_some() {
                        self.dedicated_channel().await
                    } else {
                        Err(SftpError::ChannelLimit(message))
                    }
                }
                Err(err) => Err(err),
            }
        })
    }

    fn dedicated_channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
        Box::pin(async move {
            let Some(reconnect) = self.reconnect.clone() else {
                return Err(SftpError::ChannelLimit("没有配置专用连接的兜底".to_owned()));
            };
            self.slots
                .dedicated_or_open(|| async move {
                    let connection = reconnect.reconnect().await?;
                    SftpChannel::open(&connection).await
                })
                .await
        })
    }

    fn reset<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // 丢掉缓存通道和专用连接上的通道；连接死了的话，下一次 `channel()` 会重连。
            self.slots.reset().await;
        })
    }

    fn suggested_read_len(&self) -> u32 {
        self.read_len_hint()
    }

    fn suggested_write_len(&self) -> u32 {
        self.write_len_hint()
    }
}

/// 生产实现：本地 ↔ 远端。
pub struct SftpEndpoints {
    provider: Arc<dyn TransferChannelProvider>,
    /// "远端 → 远端"复制时，源所在的另一台主机的数据面（按登记名找）。
    peers: std::sync::Mutex<std::collections::HashMap<String, Arc<dyn TransferChannelProvider>>>,
}

impl SftpEndpoints {
    pub fn new(provider: Arc<dyn TransferChannelProvider>) -> Self {
        Self {
            provider,
            peers: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 登记另一台主机的数据面（复制粘贴的源在别的主机上时，提交任务前先登记）。
    pub fn register_peer(&self, name: &str, provider: Arc<dyn TransferChannelProvider>) {
        self.peers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_owned(), provider);
    }

    fn provider_for(
        &self,
        peer: Option<&str>,
    ) -> Result<Arc<dyn TransferChannelProvider>, SftpError> {
        match peer {
            None => Ok(Arc::clone(&self.provider)),
            Some(name) => self
                .peers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    SftpError::RemoteReason(Reason::new(
                        reason_keys::SFTP_COPY_PEER_CLOSED,
                        "复制的源所在的连接已经关闭，请重新打开那个标签再试",
                    ))
                }),
        }
    }
}

/// 在某个通道提供者上读目录 / 属性（展开复制任务的源树用）。
struct ProviderLister(Arc<dyn TransferChannelProvider>);

impl RemoteLister for ProviderLister {
    fn list_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<DirEntry>, SftpError>> {
        Box::pin(async move { self.0.channel().await?.read_dir(path).await })
    }

    fn stat_remote<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        Box::pin(async move { self.0.channel().await?.lstat(path).await })
    }

    fn stat_follow<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        Box::pin(async move { self.0.channel().await?.stat(path).await })
    }
}

impl RemoteLister for SftpEndpoints {
    fn list_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<DirEntry>, SftpError>> {
        Box::pin(async move {
            let channel = self.provider.channel().await?;
            channel.read_dir(path).await
        })
    }

    fn stat_remote<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        Box::pin(async move {
            let channel = self.provider.channel().await?;
            channel.lstat(path).await
        })
    }

    fn stat_follow<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxFuture<'a, Result<russh_sftp::protocol::FileAttributes, SftpError>> {
        Box::pin(async move {
            let channel = self.provider.channel().await?;
            channel.stat(path).await
        })
    }
}

impl TransferEndpoints for SftpEndpoints {
    fn server_identity<'a>(&'a self) -> BoxFuture<'a, Result<String, SftpError>> {
        Box::pin(async move {
            let channel = self.provider.channel().await?;
            Ok(channel.server_key().to_owned())
        })
    }

    fn plan_remote_copy<'a>(
        &'a self,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        recursive: bool,
    ) -> BoxFuture<'a, Result<super::plan::TransferPlan, SftpError>> {
        Box::pin(async move {
            let lister = ProviderLister(self.provider_for(peer)?);
            super::plan::plan_remote_copy(&lister, source, target, recursive).await
        })
    }

    fn pair<'a>(
        &'a self,
        direction: Direction,
        peer: Option<&'a str>,
        source: &'a str,
        target: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<FilePair, SftpError>> {
        Box::pin(async move {
            match direction {
                Direction::RemoteCopy => {
                    let from = self.provider_for(peer)?.channel().await?;
                    let to = self.provider.channel().await?;
                    Ok(FilePair {
                        source: Arc::new(RemoteEndpoint::with_owner(from, source, owner)),
                        target: Arc::new(RemoteEndpoint::with_owner(to, target, owner)),
                    })
                }
                Direction::LocalCopy => Ok(FilePair {
                    source: Arc::new(LocalEndpoint::new(Path::new(source))),
                    target: Arc::new(LocalEndpoint::with_owner(Path::new(target), owner)),
                }),
                Direction::Upload => {
                    let channel = self.provider.channel().await?;
                    Ok(FilePair {
                        source: Arc::new(LocalEndpoint::new(Path::new(source))),
                        target: Arc::new(RemoteEndpoint::with_owner(channel, target, owner)),
                    })
                }
                Direction::Download => {
                    let channel = self.provider.channel().await?;
                    Ok(FilePair {
                        source: Arc::new(RemoteEndpoint::with_owner(channel, source, owner)),
                        target: Arc::new(LocalEndpoint::with_owner(Path::new(target), owner)),
                    })
                }
            }
        })
    }

    fn create_target_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), SftpError>> {
        Box::pin(async move {
            let channel = self.provider.channel().await?;
            match channel.create_dir(path).await {
                Ok(()) => Ok(()),
                // "已经存在"的说法各家服务器不一样（有的只报一个光秃秃的 Failure）：
                // stat 一次，目录真在了就算成功。
                Err(_) => match channel.lstat(path).await {
                    Ok(attrs) if attrs.permissions.is_some_and(|mode| mode & 0o40000 != 0) => {
                        Ok(())
                    }
                    Ok(_) => Err(SftpError::RemoteReason(
                        Reason::new(
                            reason_keys::SFTP_EXISTS_NOT_DIR,
                            format!("{path} 已存在，但不是目录"),
                        )
                        .with("path", path),
                    )),
                    Err(err) => Err(err),
                },
            }
        })
    }

    fn reset<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.provider.reset().await;
            // 跨主机复制的源端（peer）也是数据面：断线可能出在它那一侧，只重置目标端的话，
            // 重试会继续拿 peer 上缓存的旧通道 / 专用连接。先取出再逐个等，不跨 await 持有锁。
            let peers: Vec<Arc<dyn TransferChannelProvider>> = self
                .peers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .cloned()
                .collect();
            for peer in peers {
                peer.reset().await;
            }
        })
    }

    fn suggested_chunk_len(&self, direction: Direction, peer: Option<&str>) -> u32 {
        // 源端：远端到远端时是 peer（缺失就当没开通道，用保守值）；下载时就是本机的连接。
        let source_read = match direction {
            Direction::RemoteCopy => self
                .provider_for(peer)
                .map(|provider| provider.suggested_read_len())
                .unwrap_or(crate::sftp::session::CONSERVATIVE_CHUNK),
            _ => self.provider.suggested_read_len(),
        };
        // 目标端永远是本机连接上的那条（上传 / 远端复制的目标）。
        let target_write = self.provider.suggested_write_len();
        chunk_len_for(direction, source_read, target_write)
    }
}

#[cfg(test)]
mod peer_tests {
    use super::*;

    /// 开通道一律失败，错误文字带着自己的标签：看错误就知道请求落在了哪台主机上。
    struct Labelled(&'static str);

    impl TransferChannelProvider for Labelled {
        fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
            let label = self.0;
            Box::pin(async move { Err(SftpError::Remote(format!("labelled:{label}"))) })
        }
    }

    fn endpoints() -> SftpEndpoints {
        SftpEndpoints::new(Arc::new(Labelled("target")))
    }

    async fn source_error(endpoints: &SftpEndpoints, peer: Option<&str>) -> SftpError {
        match endpoints
            .pair(Direction::RemoteCopy, peer, "/src/a", "/dst/a", "owner")
            .await
        {
            Ok(_) => panic!("测试里的数据面都开不了通道"),
            Err(err) => err,
        }
    }

    /// W04：没登记过的 peer 要明确报"源已关闭"，绝不能退回目标主机当源
    /// （目标机碰巧有同路径时会复制错内容还报成功）。
    #[tokio::test]
    async fn an_unregistered_peer_is_refused_instead_of_falling_back_to_the_target() {
        let endpoints = endpoints();
        match source_error(&endpoints, Some("view-9")).await {
            SftpError::RemoteReason(reason) => {
                assert_eq!(reason.key(), reason_keys::SFTP_COPY_PEER_CLOSED);
            }
            other => panic!("应该报源连接已关闭：{other}"),
        }
        // 计划阶段（展开源树）同样不退回。
        let plan = endpoints
            .plan_remote_copy(Some("view-9"), "/src", "/dst", true)
            .await;
        assert!(
            matches!(&plan, Err(SftpError::RemoteReason(reason)) if reason.key() == reason_keys::SFTP_COPY_PEER_CLOSED),
            "{plan:?}"
        );
        // 对照：peer 为 None 才是同一台主机，落在目标的数据面上。
        let same_host = source_error(&endpoints, None).await.to_string();
        assert!(same_host.contains("labelled:target"), "{same_host}");
    }

    /// 登记之后，源端落在登记的那台主机上；重复登记同名会换成新的数据面。
    #[tokio::test]
    async fn a_registered_peer_is_used_as_the_source() {
        let endpoints = endpoints();
        endpoints.register_peer("view-9", Arc::new(Labelled("first")));
        let err = source_error(&endpoints, Some("view-9")).await.to_string();
        assert!(err.contains("labelled:first"), "{err}");
        endpoints.register_peer("view-9", Arc::new(Labelled("second")));
        let err = source_error(&endpoints, Some("view-9")).await.to_string();
        assert!(err.contains("labelled:second"), "{err}");
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 假通道：只记它是第几次开出来的。
    struct FakeChannel(usize);

    /// W14：主连接开不出通道、改走专用连接；专用连接断了之后 `reset` 必须把它也清掉，
    /// 重试才会拿到新开的，而不是反复复用已经断掉的那条。
    #[tokio::test]
    async fn reset_drops_the_dedicated_channel_too() {
        let slots = ChannelSlots::<FakeChannel>::new();
        let opened = AtomicUsize::new(0);
        let open = || {
            let id = opened.fetch_add(1, Ordering::SeqCst) + 1;
            async move { Ok(FakeChannel(id)) }
        };

        // 到达上限，改走专用：第一次新开，之后复用。
        let first = slots.dedicated_or_open(open).await.unwrap();
        assert_eq!(first.0, 1);
        let again = slots.dedicated_or_open(open).await.unwrap();
        assert_eq!(again.0, 1, "没有断线时复用同一条专用通道");
        assert_eq!(opened.load(Ordering::SeqCst), 1);

        // 专用连接断了，引擎 reset 后重试。
        *slots.cached.lock().await = Some(Arc::new(FakeChannel(100)));
        slots.reset().await;
        assert!(slots.cached.lock().await.is_none());
        assert!(slots.dedicated.lock().await.is_none(), "专用通道也要清掉");
        let fresh = slots.dedicated_or_open(open).await.unwrap();
        assert_eq!(fresh.0, 2, "重试要拿到新开的专用通道");
        assert_eq!(opened.load(Ordering::SeqCst), 2);
    }

    /// 新开失败时槽里不留东西，下次还会再试。
    #[tokio::test]
    async fn a_failed_open_leaves_the_dedicated_slot_empty() {
        let slots = ChannelSlots::<FakeChannel>::new();
        let err = slots
            .dedicated_or_open(|| async { Err(SftpError::Remote("连不上".to_owned())) })
            .await;
        assert!(err.is_err());
        assert!(slots.dedicated.lock().await.is_none());
        let ok = slots
            .dedicated_or_open(|| async { Ok(FakeChannel(7)) })
            .await
            .unwrap();
        assert_eq!(ok.0, 7);
    }

    /// 数一数被 reset 了几次的提供者。
    struct Counting(AtomicUsize);

    impl TransferChannelProvider for Counting {
        fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
            Box::pin(async { Err(SftpError::Remote("测试里不开通道".to_owned())) })
        }

        fn reset<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    /// W14：跨主机复制时，目标端和源端（peer）的数据面都要 reset。
    #[tokio::test]
    async fn endpoints_reset_reaches_the_target_and_every_peer() {
        let target = Arc::new(Counting(AtomicUsize::new(0)));
        let peer_a = Arc::new(Counting(AtomicUsize::new(0)));
        let peer_b = Arc::new(Counting(AtomicUsize::new(0)));
        let endpoints = SftpEndpoints::new(target.clone());
        endpoints.register_peer("view-1", peer_a.clone());
        endpoints.register_peer("view-2", peer_b.clone());

        endpoints.reset().await;

        assert_eq!(target.0.load(Ordering::SeqCst), 1);
        assert_eq!(peer_a.0.load(Ordering::SeqCst), 1, "源端的数据面没有被重置");
        assert_eq!(peer_b.0.load(Ordering::SeqCst), 1);
    }
}
