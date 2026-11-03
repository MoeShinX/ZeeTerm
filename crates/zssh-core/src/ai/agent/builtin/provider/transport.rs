//! HTTP 层：一个只会「发 POST、拿回字节流」的小接口。
//!
//! 真实实现 [`ReqwestTransport`] 用核心库已有的 `reqwest`（schannel，不引新依赖）；代理、
//! 超时由调用方构造 `reqwest::Client` 时配置，或用 [`ReqwestTransport::build`] 套用
//! Z-SSH 的默认代理设置。测试用桩实现（见 `testing.rs`）回放录好的字节。
//!
//! 取消：调用方直接丢掉正在 await 的 future 就行，`reqwest` 的响应被 drop 时会关连接。

use std::fmt;
use std::time::Duration;

use crate::ssh::BoxFuture;
use crate::sync::net::SyncProxy;

use super::error::TransportError;

/// 建立连接的超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 发出请求后等响应头的最长时间。有些网关到第一个 token 才发响应头，思考模型更慢，
/// 所以给得宽。
pub const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(180);
/// 响应体两个数据块之间最长的静默时间；超过就当流断了。
pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// 一次 POST 请求。
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    /// 头（名字大小写不限）。里面有 API Key，不能打印。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// `Debug` 手写：头里有 Key、请求体是对话正文，都不进日志。
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// 响应体字节流。
pub trait ByteStream: Send {
    /// 下一块；`Ok(None)` = 正常结束。
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>>;
}

/// 响应头 + 响应体流。
pub struct HttpResponse {
    pub status: u16,
    /// 头名字已转成小写。
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn ByteStream>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// 发 POST 并返回流式响应。
pub trait Transport: Send + Sync {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>>;
}

/// 基于 `reqwest` 的真实实现。
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// 用调用方配好的客户端（代理、证书等都在客户端上）。客户端**不能**设整体超时
    /// （`timeout`）：那会把一次长回复从中间切断；连接超时可以设。
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// 套用 Z-SSH 的默认代理设置（`None` = 直连，且不读系统 / 环境变量里的代理）建客户端。
    ///
    /// 不自动跟随重定向：POST 被 301/302 带到别处多半会丢掉请求体，不如直接告诉用户
    /// 把 Base URL 改成最终地址。
    pub fn build(proxy: Option<&SyncProxy>) -> Result<Self, TransportError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("ZeeTerm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        builder = match proxy {
            Some(proxy) => {
                let proxy = reqwest::Proxy::all(proxy.url_with_credentials()).map_err(|err| {
                    TransportError::Other(format!("代理设置不合法：{}", err.without_url()))
                })?;
                builder.proxy(proxy)
            }
            None => builder.no_proxy(),
        };
        let client = builder.build().map_err(|err| {
            TransportError::Other(format!("HTTP 客户端创建失败：{}", err.without_url()))
        })?;
        Ok(Self { client })
    }
}

fn map_send_error(err: reqwest::Error) -> TransportError {
    let err = err.without_url();
    if err.is_timeout() {
        TransportError::Timeout(err.to_string())
    } else if err.is_connect() {
        TransportError::Connect(err.to_string())
    } else {
        TransportError::Other(err.to_string())
    }
}

struct ReqwestBody {
    response: reqwest::Response,
}

impl ByteStream for ReqwestBody {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move {
            match tokio::time::timeout(BODY_IDLE_TIMEOUT, self.response.chunk()).await {
                Err(_) => Err(TransportError::Interrupted(format!(
                    "{} 秒没有收到数据",
                    BODY_IDLE_TIMEOUT.as_secs()
                ))),
                Ok(Ok(chunk)) => Ok(chunk.map(|bytes| bytes.to_vec())),
                Ok(Err(err)) => Err(TransportError::Interrupted(err.without_url().to_string())),
            }
        })
    }
}

impl Transport for ReqwestTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let mut builder = self.client.post(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let sent =
                tokio::time::timeout(RESPONSE_HEADER_TIMEOUT, builder.body(request.body).send())
                    .await
                    .map_err(|_| {
                        TransportError::Timeout(format!(
                            "{} 秒没有收到响应头",
                            RESPONSE_HEADER_TIMEOUT.as_secs()
                        ))
                    })?
                    .map_err(map_send_error)?;
            let status = sent.status().as_u16();
            let headers = sent
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
                })
                .collect();
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(ReqwestBody { response: sent }),
            })
        })
    }
}
