//! Z-SSH 的 Windows 平台能力，刻意不依赖 Tauri 或 GPUI。
//!
//! 目前只包含敏感剪贴板：迁移前这段逻辑在 `src-tauri/src/commands/clipboard.rs`，
//! 和 Tauri 状态混在一起。抽出来后 Tauri 入口与原生入口可以共用同一份实现。

pub mod clipboard;
pub mod ime;
pub mod single_instance;
pub mod theme;
