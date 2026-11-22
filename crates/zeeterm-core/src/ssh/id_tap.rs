//! 记下服务器发来的 SSH 版本行（`SSH-2.0-OpenSSH_9.6` 这种）。
//!
//! russh 不对外给「服务器的 SSH 标识串」，而握手之前它是明文发出的第一行。这里在字节流外面包一层，
//! 读到第一行以 `SSH-` 开头的文字就存进共享的槽里，之后原样透传；字节本身一个不动、不缓冲、不改写，
//! 所以不影响 russh 的读写和流控。只用来给「测试连接」显示服务器版本。

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// 版本行最多看这么多字节：超过还没见到换行就不找了（服务器横幅可以在版本行前面带若干行文字）。
const MAX_SCAN: usize = 8 * 1024;

pub(crate) struct IdTap<S> {
    inner: S,
    slot: Arc<OnceLock<String>>,
    scanned: Vec<u8>,
    done: bool,
}

impl<S> IdTap<S> {
    pub(crate) fn new(inner: S, slot: Arc<OnceLock<String>>) -> Self {
        Self {
            inner,
            slot,
            scanned: Vec::new(),
            done: false,
        }
    }

    /// 新读到的字节：凑满一行就看是不是版本行。
    fn absorb(&mut self, fresh: &[u8]) {
        self.scanned.extend_from_slice(fresh);
        while let Some(end) = self.scanned.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.scanned.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\r', '\n']);
            if text.starts_with("SSH-") {
                let _ = self.slot.set(text.to_owned());
                self.finish();
                return;
            }
        }
        if self.scanned.len() > MAX_SCAN {
            self.finish();
        }
    }

    fn finish(&mut self) {
        self.done = true;
        self.scanned = Vec::new();
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for IdTap<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && !self.done {
            let fresh = buf.filled()[before..].to_vec();
            self.absorb(&fresh);
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for IdTap<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn read_all(bytes: &[u8], chunk: usize) -> (Vec<u8>, Option<String>) {
        let (mut near, far) = tokio::io::duplex(64);
        let slot = Arc::new(OnceLock::new());
        let mut tap = IdTap::new(far, Arc::clone(&slot));
        let data = bytes.to_vec();
        tokio::spawn(async move {
            for piece in data.chunks(chunk) {
                if near.write_all(piece).await.is_err() {
                    return;
                }
            }
            let _ = near.shutdown().await;
        });
        let mut out = Vec::new();
        let _ = tap.read_to_end(&mut out).await;
        (out, slot.get().cloned())
    }

    #[tokio::test]
    async fn the_version_line_is_captured_and_every_byte_still_passes_through() {
        let wire = b"SSH-2.0-OpenSSH_9.6p1 Ubuntu-3\r\n\x00\x00\x01binary-after";
        let (out, version) = read_all(wire, 7).await;
        assert_eq!(out, wire, "字节原样透传");
        assert_eq!(version.as_deref(), Some("SSH-2.0-OpenSSH_9.6p1 Ubuntu-3"));
    }

    #[tokio::test]
    async fn banner_lines_before_the_version_line_are_skipped() {
        let wire = b"Welcome\r\nSSH-2.0-dropbear_2022.83\r\nrest";
        let (out, version) = read_all(wire, 3).await;
        assert_eq!(out, wire);
        assert_eq!(version.as_deref(), Some("SSH-2.0-dropbear_2022.83"));
    }

    #[tokio::test]
    async fn no_version_line_means_no_version() {
        let (out, version) = read_all(b"just some bytes without a newline", 5).await;
        assert_eq!(out, b"just some bytes without a newline");
        assert_eq!(version, None);
    }
}
