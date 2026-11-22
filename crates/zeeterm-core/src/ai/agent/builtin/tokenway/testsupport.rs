//! 测试共用：按顺序回放预设答复、记下每个请求的桩传输层。

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::ai::agent::builtin::provider::{
    ByteStream, HttpRequest, HttpResponse, Transport, TransportError,
};
use crate::ssh::BoxFuture;

struct Chunk(Option<Vec<u8>>);

impl ByteStream for Chunk {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move { Ok(self.0.take()) })
    }
}

type Reply = Result<(u16, Vec<u8>), TransportError>;

/// 每次请求（GET / POST 都算）依次取出下一个答复；用完了就一直报"没有预设答复"。
pub struct Stub {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<(bool, HttpRequest)>>,
}

impl Stub {
    pub fn new() -> Self {
        Self {
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn reply(self, status: u16, body: &str) -> Self {
        self.replies
            .lock()
            .unwrap()
            .push_back(Ok((status, body.as_bytes().to_vec())));
        self
    }

    pub fn fail(self, error: TransportError) -> Self {
        self.replies.lock().unwrap().push_back(Err(error));
        self
    }

    /// （是不是 GET，请求）。
    pub fn requests(&self) -> Vec<(bool, HttpRequest)> {
        self.requests.lock().unwrap().clone()
    }

    fn take(&self, get: bool, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.requests.lock().unwrap().push((get, request));
        let next = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(TransportError::Other("没有预设答复".into())))?;
        Ok(HttpResponse {
            status: next.0,
            headers: Vec::new(),
            body: Box::new(Chunk(Some(next.1))),
        })
    }
}

impl Transport for Stub {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move { self.take(false, request) })
    }

    fn get<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move { self.take(true, request) })
    }
}
