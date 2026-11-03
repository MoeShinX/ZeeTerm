//! 测试用的桩传输层和切块工具。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::ssh::BoxFuture;

use super::error::TransportError;
use super::transport::{ByteStream, HttpRequest, HttpResponse, Transport};

/// 确定性的小随机数（xorshift），测试不依赖 `rand`，同一个种子永远切出同样的块。
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// 把字节随机切成 1..=`max` 字节的块（会切到行中间、`\r\n` 中间、多字节字符中间）。
pub fn random_chunks(bytes: &[u8], seed: u64, max: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let size = (1 + rng.below(max)).min(bytes.len() - at);
        out.push(bytes[at..at + size].to_vec());
        at += size;
    }
    out
}

/// 一次回放的脚本。
#[derive(Clone)]
pub struct Script {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<Vec<u8>>,
    /// 回放完所有块之后不是正常结束，而是报这个传输错误。
    pub then_error: Option<TransportError>,
    /// `post` 本身就失败。
    pub post_error: Option<TransportError>,
}

impl Script {
    pub fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            chunks,
            then_error: None,
            post_error: None,
        }
    }

    pub fn body(status: u16, content_type: &str, body: &[u8]) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            chunks: vec![body.to_vec()],
            then_error: None,
            post_error: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

#[derive(Clone)]
pub struct StubTransport {
    script: Script,
    pub requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl StubTransport {
    pub fn new(script: Script) -> Self {
        Self {
            script,
            requests: Arc::default(),
        }
    }

    pub fn last_request(&self) -> Option<HttpRequest> {
        self.requests.lock().ok().and_then(|r| r.last().cloned())
    }
}

struct StubBody {
    chunks: VecDeque<Vec<u8>>,
    then_error: Option<TransportError>,
}

impl ByteStream for StubBody {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move {
            if let Some(chunk) = self.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            match self.then_error.take() {
                Some(err) => Err(err),
                None => Ok(None),
            }
        })
    }
}

impl Transport for StubTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            if let Ok(mut requests) = self.requests.lock() {
                requests.push(request);
            }
            if let Some(err) = self.script.post_error.clone() {
                return Err(err);
            }
            Ok(HttpResponse {
                status: self.script.status,
                headers: self.script.headers.clone(),
                body: Box::new(StubBody {
                    chunks: self.script.chunks.clone().into(),
                    then_error: self.script.then_error.clone(),
                }),
            })
        })
    }
}
