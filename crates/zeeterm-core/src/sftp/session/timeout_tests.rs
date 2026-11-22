//! C7b：SFTP 请求超时。用进程内的假 SFTP 服务器，按需把应答拖慢；时钟是暂停的，
//! 不用真等几十秒。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh_sftp::protocol::{Attrs, FileAttributes, Packet, Status, StatusCode, Version};
use russh_sftp::server::StatusReply;

use super::*;

#[derive(Default)]
struct Delays {
    lstat: Duration,
    copy_data: Duration,
}

struct Slow(Arc<Mutex<Delays>>);

impl russh_sftp::server::Handler for Slow {
    type Error = StatusReply;

    fn unimplemented(&self) -> StatusReply {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, StatusReply> {
        Ok(Version::new())
    }

    async fn lstat(&mut self, id: u32, _path: String) -> Result<Attrs, StatusReply> {
        let delay = self.0.lock().expect("锁").lstat;
        tokio::time::sleep(delay).await;
        Ok(Attrs {
            id,
            attrs: FileAttributes::default(),
        })
    }

    async fn extended(
        &mut self,
        id: u32,
        _request: String,
        _data: Vec<u8>,
    ) -> Result<Packet, StatusReply> {
        let delay = self.0.lock().expect("锁").copy_data;
        tokio::time::sleep(delay).await;
        Ok(Packet::Status(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_owned(),
            language_tag: "en-US".to_owned(),
        }))
    }
}

async fn session(configure: bool) -> (RawSftpSession, Arc<Mutex<Delays>>) {
    let delays = Arc::new(Mutex::new(Delays::default()));
    let (client, server) = tokio::io::duplex(64 * 1024);
    russh_sftp::server::run(server, Slow(Arc::clone(&delays))).await;
    let raw = RawSftpSession::new(client);
    raw.init().await.expect("握手");
    if configure {
        apply_request_timeout(&raw);
    }
    (raw, delays)
}

fn set_lstat_delay(delays: &Arc<Mutex<Delays>>, delay: Duration) {
    delays.lock().expect("锁").lstat = delay;
}

#[tokio::test(start_paused = true)]
async fn a_server_that_takes_15_seconds_to_answer_no_longer_times_out() {
    // 前提：不设置的话，上游默认的 10 秒会让这个请求失败。
    let (raw, delays) = session(false).await;
    set_lstat_delay(&delays, Duration::from_secs(15));
    let err = raw.lstat("/a").await.expect_err("默认 10 秒应当超时");
    assert!(matches!(err, russh_sftp::client::error::Error::Timeout));

    // 设置之后：同样的服务器 15 秒回复，请求成功。
    let (raw, delays) = session(true).await;
    set_lstat_delay(&delays, Duration::from_secs(15));
    raw.lstat("/a").await.expect("60 秒的上限里 15 秒不算超时");
    assert_eq!(REQUEST_TIMEOUT_SECS, 60);
}

#[tokio::test(start_paused = true)]
async fn an_ordinary_request_that_never_comes_back_reports_the_seconds() {
    let (raw, delays) = session(true).await;
    set_lstat_delay(&delays, Duration::from_secs(90));
    let err = SftpError::from(raw.lstat("/a").await.expect_err("90 秒超过 60 秒的上限"));
    assert!(
        err.to_string().contains("服务器 60 秒没有响应"),
        "错误里要写清楚等了多久：{err}"
    );
}

#[tokio::test(start_paused = true)]
async fn copy_data_gets_its_own_longer_limit_and_the_ordinary_one_comes_back() {
    let (raw, delays) = session(true).await;
    let active = AtomicUsize::new(0);

    // 服务器拷 5 分钟才回：对 copy-data 这类请求不算超时。
    delays.lock().expect("锁").copy_data = Duration::from_secs(5 * 60);
    extended_with_long_timeout(&raw, &active, COPY_DATA, Vec::new())
        .await
        .expect("5 分钟在 30 分钟的上限之内");
    assert_eq!(active.load(Ordering::SeqCst), 0, "守卫要放掉");

    // 之后的普通请求回到 60 秒。
    set_lstat_delay(&delays, Duration::from_secs(90));
    assert!(raw.lstat("/a").await.is_err(), "常规上限要恢复成 60 秒");

    // 超过 30 分钟也会报，并且写的是这个请求自己的上限。
    delays.lock().expect("锁").copy_data = Duration::from_secs(31 * 60);
    let err = extended_with_long_timeout(&raw, &active, COPY_DATA, Vec::new())
        .await
        .expect_err("31 分钟超过上限");
    assert!(err.to_string().contains("服务器 1800 秒没有响应"), "{err}");
    assert_eq!(active.load(Ordering::SeqCst), 0);
    set_lstat_delay(&delays, Duration::from_secs(90));
    assert!(raw.lstat("/a").await.is_err(), "出错之后也要恢复");
}
