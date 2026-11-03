//! Tokenway 一键连接（V2-A5）：预设清单（内置 + 签名更新）、浏览器授权（PKCE + 本机回环）、
//! 余额状态、停用。ZeeTerm 没有账号、不登录；授权拿到的是这台设备专用的 API 令牌，存进加密
//! 保险库（和主机密码同一套），接口配置里只有 `preset_id`。
//!
//! 只请求这几样：预设清单、换令牌、余额状态、停用，以及接口本身的模型列表和对话。不上报任何
//! 使用数据。

pub mod canonical;
pub mod presets;

pub use presets::{
    builtin_manifest, production_key, ActivePresets, Preset, PresetError, PresetManifest,
    PresetSource, PresetStore, PublicKey, RefreshOutcome, UrlPolicy, PRESETS_URL, TOKENWAY_ID,
};
