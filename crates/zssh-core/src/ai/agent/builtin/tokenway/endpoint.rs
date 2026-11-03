//! 授权成功后把令牌存成一条接口配置，以及"断开 Tokenway"。
//!
//! 令牌（API Key）走接口存储现成的路径：加密进凭据保险库（和主机密码同一套，随同步包加密同步），
//! 接口表里只有 `preset_id`。断开时先通知 Tokenway 停用（失败也继续），再删接口和它的 Key；
//! 删除按现有同步语义传到其他电脑。

use std::collections::BTreeMap;

use super::api::{revoke, ApiError, Authorized};
use super::presets::{Preset, UrlPolicy};
use crate::ai::agent::builtin::config::{
    AiEndpoint, AiEndpointDraft, EndpointKind, OpenAiOptionsConfig,
};
use crate::ai::agent::builtin::provider::{ApiKey, Transport};
use crate::error::ZsshError;
use crate::store::Database;

/// 浏览器授权拿到的 ZeeTerm 专用令牌所属的预设。
pub const PRESET_AUTHORIZED: &str = "tokenway";
/// 用户自己粘贴了 Tokenway 的 Key（普通令牌也能用；余额、停用只有 ZeeTerm 令牌才有）。
pub const PRESET_PASTED: &str = "tokenway_key";

/// 属于 Tokenway 的两种接口。
pub fn is_tokenway(preset_id: Option<&str>) -> bool {
    matches!(preset_id, Some(PRESET_AUTHORIZED | PRESET_PASTED))
}

/// 浏览器授权出来的那种（有余额接口、能"断开"时通知服务端停用）。
pub fn is_authorized_token(preset_id: Option<&str>) -> bool {
    preset_id == Some(PRESET_AUTHORIZED)
}

const NOT_CHAT_MARKERS: &[&str] = &[
    "embed",
    "whisper",
    "tts",
    "dall-e",
    "image",
    "moderation",
    "rerank",
    "speech",
    "audio",
];

/// 默认模型：模型清单里第一个看起来是对话模型的（跳过嵌入、绘图、语音这些）。
pub fn pick_default_model(models: &[String]) -> Option<String> {
    models
        .iter()
        .find(|id| {
            let lower = id.to_ascii_lowercase();
            !NOT_CHAT_MARKERS.iter().any(|marker| lower.contains(marker))
        })
        .or_else(|| models.first())
        .cloned()
}

/// 把授权结果存成接口。已经有一条浏览器授权的 Tokenway 接口（重新连接）就原地换 Key、更新地址，
/// 名字、默认模型、手动覆盖都保留；没有就新建。没有"默认接口"时把它设为默认。
///
/// `models`：用新令牌取到的模型清单（取不到就是 `None`），只在新建时用来挑默认模型。
pub fn save_authorized(
    db: &Database,
    preset: &Preset,
    authorized: &Authorized,
    models: Option<&[String]>,
    policy: &UrlPolicy,
) -> Result<AiEndpoint, ZsshError> {
    let base_url = authorized
        .openai_base
        .clone()
        .or_else(|| preset.openai_base.clone())
        .ok_or_else(|| ZsshError::InvalidInput("预设里没有接入地址".to_owned()))?;
    policy
        .check(&base_url)
        .map_err(|why| ZsshError::InvalidInput(format!("接入地址不合格：{why}")))?;
    let existing = db
        .ai_endpoint_list()?
        .into_iter()
        .find(|endpoint| is_authorized_token(endpoint.preset_id.as_deref()));
    let draft = match &existing {
        Some(endpoint) => AiEndpointDraft {
            id: Some(endpoint.id),
            name: endpoint.name.clone(),
            kind: EndpointKind::OpenAiChat,
            base_url,
            api_key: Some(authorized.api_key.expose().to_owned()),
            default_model: endpoint
                .default_model
                .clone()
                .or_else(|| models.and_then(pick_default_model)),
            models: endpoint.models.clone(),
            overrides: endpoint.overrides.clone(),
            openai_options: endpoint.openai_options.clone(),
            preset_id: Some(PRESET_AUTHORIZED.to_owned()),
        },
        None => AiEndpointDraft {
            id: None,
            name: preset.name.clone(),
            kind: EndpointKind::OpenAiChat,
            base_url,
            api_key: Some(authorized.api_key.expose().to_owned()),
            default_model: models.and_then(pick_default_model),
            models: Vec::new(),
            overrides: BTreeMap::new(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: Some(PRESET_AUTHORIZED.to_owned()),
        },
    };
    let saved = db.ai_endpoint_save(&draft)?;
    if db.ai_builtin_default()?.is_none() {
        db.set_ai_builtin_default(Some((saved.id, saved.default_model.as_deref())))?;
    }
    Ok(saved)
}

/// 断开的结果。
#[derive(Debug)]
pub struct DisconnectReport {
    /// 通知 Tokenway 停用令牌：`Ok` = 服务端已停用；`Err` = 没通知成功（本地照样删了）。
    pub revoked: Result<(), ApiError>,
}

/// "断开 Tokenway"：调 `revoke_url`（失败也继续）→ 删保险库里的 Key → 删接口配置。
/// 浏览器授权的令牌才会通知服务端；自己粘贴的 Key 只删本地。
pub async fn disconnect(
    db: &Database,
    transport: &dyn Transport,
    revoke_url: Option<&str>,
    policy: &UrlPolicy,
    endpoint_id: i64,
) -> Result<DisconnectReport, ZsshError> {
    let endpoint = db.ai_endpoint_get(endpoint_id)?;
    let key = db.ai_endpoint_api_key(endpoint_id)?;
    let revoked = match (key, revoke_url) {
        (Some(key), Some(url)) if is_authorized_token(endpoint.preset_id.as_deref()) => {
            revoke(transport, policy, url, &ApiKey::new(key)).await
        }
        _ => Ok(()),
    };
    db.ai_endpoint_delete(endpoint_id)?;
    Ok(DisconnectReport { revoked })
}

#[cfg(test)]
mod tests;
