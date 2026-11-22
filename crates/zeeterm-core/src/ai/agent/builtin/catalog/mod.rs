//! 模型能力识别（V2-A1）：models.dev 数据（内置快照 + 运行时缓存）+ 名字匹配 + 接口自报 +
//! 规则兜底 + 手动覆盖，合成出 [`ModelCaps`]。
//!
//! 入口：
//! - [`Catalog::load`]：按"缓存 > 内置快照 > 空表"加载数据库；
//! - [`refresh_if_stale`]：后台按需更新缓存（HTTP 由调用方注入）；
//! - [`parse_models_endpoint`]：解析 `GET /models` 的返回；
//! - [`resolve`]：手动 > 接口自报 > models.dev > 兜底，逐字段合成。
//!
//! 数据来自 models.dev（sst，MIT），见仓库根 `NOTICE`。

mod cache;
mod data;
mod endpoint;
mod heuristic;
mod matcher;
mod resolve;
mod types;

pub use cache::{
    refresh_if_stale, refresh_now, unix_now, HttpFetch, RefreshOutcome, ReqwestFetch,
    CACHE_FILE_NAME, MODELS_DEV_API_URL, STALE_AFTER_SECS,
};
pub use data::{
    build_snapshot_json, date_to_unix, trim_api_json, unix_to_date, CatalogEntry, CatalogError,
    CatalogFile, FORMAT_VERSION,
};
pub use endpoint::parse_models_endpoint;
pub use matcher::{Catalog, CatalogMatch, CatalogOrigin, MatchKind};
pub use resolve::resolve;
pub use types::{CapsOverride, CapsSource, Cost, ModelCaps, PartialCaps, ReasoningSupport};

#[cfg(test)]
mod tests;
