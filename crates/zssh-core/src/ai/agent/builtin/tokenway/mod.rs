//! Tokenway 一键连接（V2-A5）：预设清单（内置 + 签名更新）、浏览器授权（PKCE + 本机回环）、
//! 余额状态、停用。ZeeTerm 没有账号、不登录；授权拿到的是这台设备专用的 API 令牌，存进加密
//! 保险库（和主机密码同一套），接口配置里只有 `preset_id`。
//!
//! 只请求这几样：预设清单、换令牌、余额状态、停用，以及接口本身的模型列表和对话。不上报任何
//! 使用数据。
//!
//! - [`presets`] / [`canonical`]：预设清单、验签、域名白名单、缓存与 24 小时更新；
//! - [`connect`] / [`loopback`]：浏览器授权；
//! - [`api`]：换令牌 / 余额 / 停用 / 对话失败分类 / 刷新节流；
//! - [`endpoint`]：把令牌存成接口、断开；
//! - [`fake`]：本机假 Tokenway（测试夹具）。

pub mod api;
pub mod canonical;
pub mod connect;
pub mod endpoint;
pub mod fake;
pub mod loopback;
pub mod presets;
#[cfg(test)]
pub(crate) mod testsupport;

pub use api::{
    classify_chat_failure, format_yuan, ApiError, ApiErrorKind, Authorized, Balance,
    ChatFailureAction, RefreshGate, BALANCE_MIN_INTERVAL, REQUIRES_TOPUP_CODE,
};
pub use connect::{authorize, ConnectError, Stage, UrlOpener, AUTHORIZE_TIMEOUT};
pub use presets::{
    builtin_manifest, production_key, ActivePresets, Preset, PresetError, PresetManifest,
    PresetSource, PresetStore, PublicKey, RefreshOutcome, UrlPolicy, PRESETS_URL, TOKENWAY_ID,
};
#[cfg(test)]
mod e2e_tests;
