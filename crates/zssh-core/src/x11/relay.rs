//! 一条 X11 通道的转发任务。
//!
//! 流程：通道被 [`crate::ssh::connection`] 的回调立即接受并交到这里 →
//! 读握手包（有大小和时间上限）→ 按握手包里的 cookie 查分发表 → 连本机 X 服务器 →
//! 把握手包里的假 cookie 换成真 cookie 写过去 → 双向拷贝，直到任何一端关闭或者所属终端关闭。
//!
//! 只有握手包会被改写；之后的 X 协议字节原样转发。

use std::time::Duration;

use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{watch, OwnedSemaphorePermit};
use tokio::time::timeout;

use super::cookie::{replace_cookie, setup_auth, setup_packet_len, SetupLen, MIT_MAGIC_COOKIE};
use super::display::XEndpoint;
use super::failure::X11Failure;
use super::registry::{RouteEntry, X11Hub};

/// 握手包最大字节数。正常的 MIT-MAGIC-COOKIE 握手包只有 48 字节，留足余量即可。
pub const SETUP_MAX_BYTES: usize = 4096;
/// 等握手包发完的时限。
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// 连本机 X 服务器的时限。
pub const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 一个方向结束后，等另一个方向收尾的宽限。
const GRACE: Duration = Duration::from_secs(5);

/// 处理一条已被接受的 X11 通道。`permit` 在整个转发期间占着连接级的通道名额。
pub(crate) async fn serve(hub: X11Hub, mut channel: Channel<Msg>, permit: OwnedSemaphorePermit) {
    let _permit = permit;

    let (buffered, packet_len) = match timeout(SETUP_TIMEOUT, read_setup(&mut channel)).await {
        Ok(Ok(setup)) => setup,
        Ok(Err(None)) => return, // 对端在发握手包前就关了，没什么好报的
        Ok(Err(Some(failure))) => {
            hub.report_to_all(&failure);
            let _ = channel.close().await;
            return;
        }
        Err(_) => {
            hub.report_to_all(&X11Failure::BadSetup);
            let _ = channel.close().await;
            return;
        }
    };

    let Some(entry) = lookup_entry(&hub, &buffered[..packet_len]) else {
        hub.report_to_all(&X11Failure::UnknownCookie);
        let _ = channel.close().await;
        return;
    };
    let Ok(_channel_permit) = entry.permits.clone().try_acquire_owned() else {
        entry.report(&X11Failure::TooManyChannels);
        let _ = channel.close().await;
        return;
    };
    let mut cancelled = entry.cancelled();
    if *cancelled.borrow_and_update() {
        let _ = channel.close().await;
        return;
    }

    let endpoint = &entry.route.endpoint;
    let local = match until_cancelled(&mut cancelled, connect_local(endpoint)).await {
        None => {
            let _ = channel.close().await;
            return;
        }
        Some(Ok(local)) => local,
        Some(Err(failure)) => {
            entry.report(&failure);
            let _ = channel.close().await;
            return;
        }
    };

    let mut packet = buffered[..packet_len].to_vec();
    if let Some(real) = &entry.route.real_cookie {
        // 查表命中说明协议名和长度都对，这里理论上不会失败；失败就别把假 cookie 送出去。
        if !replace_cookie(&mut packet, real) {
            entry.report(&X11Failure::BadSetup);
            let _ = channel.close().await;
            return;
        }
    }

    let (mut local_read, mut local_write) = local.into_split();
    let setup = async {
        local_write.write_all(&packet).await?;
        local_write.write_all(&buffered[packet_len..]).await
    };
    if !matches!(until_cancelled(&mut cancelled, setup).await, Some(Ok(()))) {
        let _ = channel.close().await;
        return;
    }

    let (mut remote_read, mut remote_write) = tokio::io::split(channel.into_stream());
    let up = async {
        let _ = tokio::io::copy(&mut remote_read, &mut local_write).await;
        let _ = local_write.shutdown().await;
    };
    let down = async {
        let mut chunk = vec![0u8; 16 * 1024];
        let mut first = true;
        loop {
            let n = match local_read.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if first {
                first = false;
                if let Some(reason) = rejected_reason(&chunk[..n]) {
                    entry.report(&X11Failure::LocalXServerRejected {
                        display: endpoint.describe(),
                        reason,
                        passthrough: entry.route.real_cookie.is_none(),
                    });
                }
            }
            if remote_write.write_all(&chunk[..n]).await.is_err() {
                break;
            }
        }
        let _ = remote_write.shutdown().await;
    };
    tokio::pin!(up, down);
    let pump = async {
        tokio::select! {
            _ = &mut up => { let _ = timeout(GRACE, &mut down).await; }
            _ = &mut down => { let _ = timeout(GRACE, &mut up).await; }
        }
    };
    until_cancelled(&mut cancelled, pump).await;
}

// 每个阶段先检查持久状态；同时就绪时取消优先，避免已取消后仍连接或发送握手。
async fn until_cancelled<T>(
    cancelled: &mut watch::Receiver<bool>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = wait_cancelled(cancelled) => None,
        result = work => Some(result),
    }
}

async fn wait_cancelled(cancelled: &mut watch::Receiver<bool>) {
    while !*cancelled.borrow_and_update() {
        // 发送端（登记项）没了也算取消。
        if cancelled.changed().await.is_err() {
            return;
        }
    }
}

/// 读到一个完整的握手包。返回（已读到的全部字节，握手包长度）。
/// `Err(None)` = 对端没发完就关了；`Err(Some(_))` = 握手包不合法或太大。
async fn read_setup(channel: &mut Channel<Msg>) -> Result<(Vec<u8>, usize), Option<X11Failure>> {
    let mut buffered: Vec<u8> = Vec::new();
    loop {
        match setup_packet_len(&buffered) {
            SetupLen::Invalid => return Err(Some(X11Failure::BadSetup)),
            SetupLen::Total(len) if len > SETUP_MAX_BYTES => {
                return Err(Some(X11Failure::BadSetup))
            }
            SetupLen::Total(len) if buffered.len() >= len => return Ok((buffered, len)),
            _ => {}
        }
        if buffered.len() >= SETUP_MAX_BYTES {
            return Err(Some(X11Failure::BadSetup));
        }
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => buffered.extend_from_slice(&data),
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => return Err(None),
            Some(_) => {}
        }
    }
}

/// 握手包里是 MIT-MAGIC-COOKIE-1 且 cookie 是某个终端登记的假值，才返回它的登记项。
fn lookup_entry(hub: &X11Hub, packet: &[u8]) -> Option<std::sync::Arc<RouteEntry>> {
    let (protocol, data) = setup_auth(packet)?;
    if protocol != MIT_MAGIC_COOKIE.as_bytes() {
        return None;
    }
    hub.lookup(data)
}

async fn connect_local(endpoint: &XEndpoint) -> Result<TcpStream, X11Failure> {
    let shown = endpoint.describe();
    match timeout(
        LOCAL_CONNECT_TIMEOUT,
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    {
        Ok(Ok(stream)) => {
            // X 协议是小包来回，别让 Nagle 攒包拖慢交互。
            let _ = stream.set_nodelay(true);
            Ok(stream)
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, x_server = %shown, "连接本机 X 服务器失败");
            Err(X11Failure::NoLocalXServer {
                display: shown,
                installable: false,
            })
        }
        Err(_) => Err(X11Failure::LocalXServerTimeout { display: shown }),
    }
}

/// X 服务器对握手包的第一个回复：首字节 0 = 失败（后面带原因），2 = 要求进一步认证，1 = 成功。
/// 失败/要求认证时返回给用户看的原因。
pub(crate) fn rejected_reason(first_reply: &[u8]) -> Option<String> {
    match *first_reply.first()? {
        0 => {
            let len = *first_reply.get(1)? as usize;
            let reason = first_reply
                .get(8..8 + len)
                .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
                .filter(|text| !text.is_empty() && text.chars().all(|c| !c.is_control()));
            Some(reason.unwrap_or_else(|| "未说明原因".to_owned()))
        }
        2 => Some("服务器要求进一步认证".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x11::cookie::test_setup_packet;
    use crate::x11::registry::X11Route;

    #[tokio::test]
    async fn cancellation_prevents_polling_a_late_connect() {
        let (cancel, mut cancelled) = watch::channel(false);
        cancel.send_replace(true);
        let result = until_cancelled(&mut cancelled, async {
            panic!("取消后不能再轮询连接");
        })
        .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cancellation_interrupts_pending_connect() {
        let (cancel, mut cancelled) = watch::channel(false);
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            until_cancelled(&mut cancelled, async move {
                started.send(()).unwrap();
                std::future::pending::<()>().await;
            })
            .await
        });
        ready.await.unwrap();
        cancel.send_replace(true);
        assert!(timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_setup_write() {
        let (cancel, mut cancelled) = watch::channel(false);
        // 容量为 1 且对端不读：首字节之后的握手写入必然挂起。
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            until_cancelled(&mut cancelled, async {
                started.send(()).unwrap();
                writer.write_all(&[1; 48]).await
            })
            .await
        });
        ready.await.unwrap();
        cancel.send_replace(true);
        assert!(timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.len() < 48);
    }

    #[test]
    fn rejection_reasons_are_read_from_the_first_reply() {
        let mut failed = vec![0u8, 9, 11, 0, 0, 0, 3, 0];
        failed.extend_from_slice(b"No auth!!");
        assert_eq!(rejected_reason(&failed).as_deref(), Some("No auth!!"));
        // 原因被截断或为空：仍然判定为失败，只是原因说不清。
        assert_eq!(
            rejected_reason(&[0, 20, 11, 0, 0, 0, 5, 0, b'x']).as_deref(),
            Some("未说明原因")
        );
        assert_eq!(rejected_reason(&[0, 0]).as_deref(), Some("未说明原因"));
        assert!(rejected_reason(&[2, 0, 0, 0, 0, 0, 0, 0])
            .unwrap()
            .contains("认证"));
        assert_eq!(rejected_reason(&[1, 0, 11, 0, 0, 0, 0, 0]), None);
        assert_eq!(rejected_reason(&[]), None);
        // 原因里混进控制字符不原样显示。
        assert_eq!(
            rejected_reason(&[0, 3, 11, 0, 0, 0, 1, 0, 0x1b, b'[', b'x']).as_deref(),
            Some("未说明原因")
        );
    }

    #[test]
    fn lookup_requires_the_mit_protocol_and_a_registered_cookie() {
        let hub = X11Hub::default();
        let reg = hub
            .register(X11Route {
                endpoint: XEndpoint::loopback(1),
                real_cookie: None,
            })
            .expect("登记");
        let cookie = crate::x11::cookie::decode_hex(&reg.cookie_hex()).expect("hex");

        let good = test_setup_packet(b'l', MIT_MAGIC_COOKIE.as_bytes(), &cookie);
        assert!(lookup_entry(&hub, &good).is_some());
        let big = test_setup_packet(b'B', MIT_MAGIC_COOKIE.as_bytes(), &cookie);
        assert!(lookup_entry(&hub, &big).is_some());

        let other_protocol = test_setup_packet(b'l', b"XDM-AUTHORIZATION-1", &cookie);
        assert!(lookup_entry(&hub, &other_protocol).is_none());
        let wrong_cookie = test_setup_packet(b'l', MIT_MAGIC_COOKIE.as_bytes(), &[0u8; 16]);
        assert!(lookup_entry(&hub, &wrong_cookie).is_none());
        let no_auth = test_setup_packet(b'l', b"", b"");
        assert!(lookup_entry(&hub, &no_auth).is_none());
        assert!(lookup_entry(&hub, &good[..10]).is_none());
    }

    #[tokio::test]
    async fn connecting_to_a_closed_port_is_classified_as_no_local_x_server() {
        // 先占一个端口再放掉，保证这个端口此刻没人监听。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let endpoint = XEndpoint {
            host: "127.0.0.1".into(),
            port,
            screen: 0,
        };
        match connect_local(&endpoint).await {
            Err(X11Failure::NoLocalXServer { display, .. }) => {
                assert_eq!(display, format!("127.0.0.1:{port}"));
            }
            other => panic!("预期 NoLocalXServer：{:?}", other.map(|_| ())),
        }
    }
}
