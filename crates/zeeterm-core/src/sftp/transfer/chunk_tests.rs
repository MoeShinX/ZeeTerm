//! 传输块大小按方向取（W32）：下载看源的读上限，上传看目标的写上限，远端复制取较小者。

use std::sync::Arc;

use super::backend::{SftpEndpoints, TransferChannelProvider, TransferEndpoints};
use super::Direction;
use crate::sftp::error::SftpError;
use crate::sftp::session::{SftpChannel, CONSERVATIVE_CHUNK};
use crate::ssh::BoxFuture;

/// 读写上限不同的假数据面：不开真实通道，只回报自己声明的上限。
struct Limits {
    read: u32,
    write: u32,
}

impl TransferChannelProvider for Limits {
    fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
        Box::pin(async { Err(SftpError::Remote("not opened in this test".to_owned())) })
    }
    fn suggested_read_len(&self) -> u32 {
        self.read
    }
    fn suggested_write_len(&self) -> u32 {
        self.write
    }
}

/// 本机连接读上限大、写上限小；peer 读上限小、写上限大。
fn endpoints() -> SftpEndpoints {
    let endpoints = SftpEndpoints::new(Arc::new(Limits {
        read: 256 * 1024,
        write: 32 * 1024,
    }));
    endpoints.register_peer(
        "view-2",
        Arc::new(Limits {
            read: 64 * 1024,
            write: 256 * 1024,
        }),
    );
    endpoints
}

#[test]
fn upload_follows_the_target_write_limit_not_the_read_limit() {
    // 修之前上传用的是读上限 256K，超过目标写上限 32K。
    assert_eq!(
        endpoints().suggested_chunk_len(Direction::Upload, None),
        32 * 1024
    );
}

#[test]
fn download_follows_the_source_read_limit() {
    assert_eq!(
        endpoints().suggested_chunk_len(Direction::Download, None),
        256 * 1024
    );
}

#[test]
fn remote_copy_takes_the_smaller_of_source_read_and_target_write() {
    let endpoints = endpoints();
    // 同一台主机：读 256K、写 32K，取 32K。
    assert_eq!(
        endpoints.suggested_chunk_len(Direction::RemoteCopy, None),
        32 * 1024
    );
    // 源在 peer 上：peer 读 64K、目标写 32K，取 32K。
    assert_eq!(
        endpoints.suggested_chunk_len(Direction::RemoteCopy, Some("view-2")),
        32 * 1024
    );
    // 目标写上限更大时，由源读上限卡住。
    let endpoints = SftpEndpoints::new(Arc::new(Limits {
        read: 256 * 1024,
        write: 128 * 1024,
    }));
    endpoints.register_peer(
        "view-2",
        Arc::new(Limits {
            read: 64 * 1024,
            write: 256 * 1024,
        }),
    );
    assert_eq!(
        endpoints.suggested_chunk_len(Direction::RemoteCopy, Some("view-2")),
        64 * 1024
    );
}

#[test]
fn remote_copy_with_an_unregistered_peer_falls_back_to_the_conservative_size() {
    // 源登记丢了本来就会在 pair 里报错；这里只保证建议值不会因此变大。
    assert_eq!(
        endpoints().suggested_chunk_len(Direction::RemoteCopy, Some("gone")),
        CONSERVATIVE_CHUNK
    );
}

#[test]
fn a_data_plane_without_hints_stays_conservative_for_every_direction() {
    struct Plain;
    impl TransferChannelProvider for Plain {
        fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
            Box::pin(async { Err(SftpError::Remote("unused".to_owned())) })
        }
    }
    let endpoints = SftpEndpoints::new(Arc::new(Plain));
    for direction in [
        Direction::Upload,
        Direction::Download,
        Direction::RemoteCopy,
        Direction::LocalCopy,
    ] {
        assert_eq!(
            endpoints.suggested_chunk_len(direction, None),
            CONSERVATIVE_CHUNK
        );
    }
}
