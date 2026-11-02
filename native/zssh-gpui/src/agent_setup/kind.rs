//! 三种智能体的身份：id、显示名、发现规则、手动路径存在设置库的哪个键。
//!
//! 和 0.2.14 的 `AgentId`（`ui/src/api/agent.ts`）、Tauri 侧的 `AgentKind`
//! （`src-tauri/src/commands/agent.rs`）一一对应；顺序也一样（opencode、codex、Claude Code），
//! 面板的下拉和设置页都按这个顺序排。

use zssh_core::ai::agent::discovery::{AgentSpec, CLAUDE, CODEX, OPENCODE};
use zssh_core::error::ZsshError;
use zssh_core::store::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    OpenCode,
    Codex,
    Claude,
}

impl AgentKind {
    /// 面板下拉、设置页的排列顺序。
    pub const ALL: [AgentKind; 3] = [AgentKind::OpenCode, AgentKind::Codex, AgentKind::Claude];

    /// 设置库里 `ai_default_agent` 的值，也是历史目录名（`<数据目录>\ai-history\<id>`）。
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::OpenCode => "opencode",
            AgentKind::Codex => "codex",
            AgentKind::Claude => "claude",
        }
    }

    /// 界面上的名字（0.2.14 的 `AGENT_LABELS`）。
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::OpenCode => "opencode",
            AgentKind::Codex => "codex",
            AgentKind::Claude => "Claude Code",
        }
    }

    /// 通用发现模块里的规则。
    pub fn spec(self) -> &'static AgentSpec {
        match self {
            AgentKind::OpenCode => &OPENCODE,
            AgentKind::Codex => &CODEX,
            AgentKind::Claude => &CLAUDE,
        }
    }

    /// 在 [`Self::ALL`] 里的下标（状态数组用）。
    pub fn index(self) -> usize {
        match self {
            AgentKind::OpenCode => 0,
            AgentKind::Codex => 1,
            AgentKind::Claude => 2,
        }
    }

    /// 设置库里的值 → 智能体。不认识的值按 opencode（和 0.2.14 的默认一致）。
    pub fn from_setting(value: &str) -> AgentKind {
        Self::parse(value).unwrap_or(AgentKind::OpenCode)
    }

    pub fn parse(value: &str) -> Option<AgentKind> {
        match value.trim() {
            "opencode" => Some(AgentKind::OpenCode),
            "codex" => Some(AgentKind::Codex),
            "claude" => Some(AgentKind::Claude),
            _ => None,
        }
    }

    /// 设置页里手动指定的路径（空串 / 没设置 = 自动发现）。
    pub fn saved_path(self, db: &Database) -> Result<Option<String>, ZsshError> {
        match self {
            AgentKind::OpenCode => db.ai_opencode_path(),
            AgentKind::Codex => db.ai_codex_path(),
            AgentKind::Claude => db.ai_claude_path(),
        }
    }

    /// 手动路径的设置键。
    pub fn path_key(self) -> &'static str {
        use zssh_core::store::settings::{
            AI_CLAUDE_PATH_KEY, AI_CODEX_PATH_KEY, AI_OPENCODE_PATH_KEY,
        };
        match self {
            AgentKind::OpenCode => AI_OPENCODE_PATH_KEY,
            AgentKind::Codex => AI_CODEX_PATH_KEY,
            AgentKind::Claude => AI_CLAUDE_PATH_KEY,
        }
    }

    /// 手动路径输入框里的占位示例（0.2.14 设置页同款）。
    pub fn path_placeholder(self) -> &'static str {
        match self {
            AgentKind::OpenCode => r"C:\path\to\opencode.exe",
            AgentKind::Codex => r"C:\path\to\codex.exe",
            AgentKind::Claude => r"C:\path\to\claude.exe",
        }
    }
}

/// 设置库里的默认智能体（读不到按 opencode）。
pub fn default_agent(db: Option<&Database>) -> AgentKind {
    db.and_then(|db| db.ai_default_agent().ok())
        .map(|value| AgentKind::from_setting(&value))
        .unwrap_or(AgentKind::OpenCode)
}

#[cfg(test)]
mod tests {
    use super::AgentKind;
    use zssh_core::store::Database;

    #[test]
    fn ids_labels_and_order_match_0_2_14() {
        let ids: Vec<_> = AgentKind::ALL.iter().map(|kind| kind.id()).collect();
        assert_eq!(ids, ["opencode", "codex", "claude"]);
        let labels: Vec<_> = AgentKind::ALL.iter().map(|kind| kind.label()).collect();
        assert_eq!(labels, ["opencode", "codex", "Claude Code"]);
        for (index, kind) in AgentKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), index);
            assert_eq!(AgentKind::parse(kind.id()), Some(*kind));
        }
    }

    #[test]
    fn unknown_settings_fall_back_to_opencode_like_0_2_14() {
        assert_eq!(AgentKind::from_setting("gemini"), AgentKind::OpenCode);
        assert_eq!(AgentKind::from_setting(""), AgentKind::OpenCode);
        assert_eq!(AgentKind::from_setting(" claude "), AgentKind::Claude);
        assert_eq!(AgentKind::parse("gemini"), None);
    }

    #[test]
    fn saved_paths_come_from_the_same_keys_as_the_tauri_build() {
        let db = Database::open_in_memory().expect("内存库");
        for kind in AgentKind::ALL {
            assert_eq!(kind.saved_path(&db).expect("读"), None, "{kind:?}");
            db.setting_set(
                kind.path_key(),
                &serde_json::to_string(r"C:\tools\my agent\x.exe").expect("json"),
            )
            .expect("写");
            assert_eq!(
                kind.saved_path(&db).expect("读").as_deref(),
                Some(r"C:\tools\my agent\x.exe")
            );
        }
        // 默认智能体：库里没写按 opencode；写了按写的；没有库也按 opencode。
        assert_eq!(super::default_agent(Some(&db)), AgentKind::OpenCode);
        db.set_ai_default_agent("codex").expect("写");
        assert_eq!(super::default_agent(Some(&db)), AgentKind::Codex);
        assert_eq!(super::default_agent(None), AgentKind::OpenCode);
    }
}
