//! 智能体的身份：id、显示名、发现规则、手动路径存在设置库的哪个键。
//!
//! 三个外部智能体和 0.2.14 的 `AgentId`（`ui/src/api/agent.ts`）、Tauri 侧的 `AgentKind`
//! （`src-tauri/src/commands/agent.rs`）一一对应；顺序也一样（opencode、codex、Claude Code）。
//! 第四个 [`AgentKind::Builtin`] 是 ZeeTerm 内置助手（V2-A4）：没有安装路径、没有发现过程，
//! 只看设置里有没有配好 Key 的接口，所以 [`AgentKind::ALL`] 仍然只含三个外部智能体（发现、路径、
//! 下载那些按外部智能体循环的代码不用动），面板下拉和默认智能体选择用 [`AgentKind::SELECTABLE`]。

use zssh_core::ai::agent::discovery::{AgentSpec, CLAUDE, CODEX, OPENCODE};
use zssh_core::error::ZsshError;
use zssh_core::store::Database;
use zssh_core::store::settings::AI_DEFAULT_AGENT_KEY;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    OpenCode,
    Codex,
    Claude,
    /// ZeeTerm 内置助手：直接对接用户配置的第三方模型接口。
    Builtin,
}

impl AgentKind {
    /// 三个外部智能体，顺序同 0.2.14：发现、手动路径、下载都按它循环。
    pub const ALL: [AgentKind; 3] = [AgentKind::OpenCode, AgentKind::Codex, AgentKind::Claude];

    /// 面板下拉、默认智能体选择里的排列顺序：内置助手排第一，其后是三个外部智能体。
    pub const SELECTABLE: [AgentKind; 4] = [
        AgentKind::Builtin,
        AgentKind::OpenCode,
        AgentKind::Codex,
        AgentKind::Claude,
    ];

    /// 设置库里 `ai_default_agent` 的值，也是历史目录名（`<数据目录>\ai-history\<id>`）。
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::OpenCode => "opencode",
            AgentKind::Codex => "codex",
            AgentKind::Claude => "claude",
            AgentKind::Builtin => "builtin",
        }
    }

    /// 界面上的名字（0.2.14 的 `AGENT_LABELS`）。
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::OpenCode => "opencode",
            AgentKind::Codex => "codex",
            AgentKind::Claude => "Claude Code",
            AgentKind::Builtin => "ZeePilot",
        }
    }

    /// 通用发现模块里的规则；内置助手没有安装可找，是 `None`。
    pub fn spec(self) -> Option<&'static AgentSpec> {
        match self {
            AgentKind::OpenCode => Some(&OPENCODE),
            AgentKind::Codex => Some(&CODEX),
            AgentKind::Claude => Some(&CLAUDE),
            AgentKind::Builtin => None,
        }
    }

    /// 在 [`Self::ALL`] 里的下标（外部智能体的状态数组用）；内置助手不在数组里，给 3 只是
    /// "不是外部"的占位（`Snapshot::get` 单独处理它），别拿它去索引数组。
    pub fn index(self) -> usize {
        match self {
            AgentKind::OpenCode => 0,
            AgentKind::Codex => 1,
            AgentKind::Claude => 2,
            AgentKind::Builtin => 3,
        }
    }

    /// 设置库里的值 → 智能体。不认识的值按 opencode（和 0.2.14 的默认一致）。
    #[cfg(test)]
    pub fn from_setting(value: &str) -> AgentKind {
        Self::parse(value).unwrap_or(AgentKind::OpenCode)
    }

    pub fn parse(value: &str) -> Option<AgentKind> {
        match value.trim() {
            "opencode" => Some(AgentKind::OpenCode),
            "codex" => Some(AgentKind::Codex),
            "claude" => Some(AgentKind::Claude),
            "builtin" => Some(AgentKind::Builtin),
            _ => None,
        }
    }

    /// 设置页里手动指定的路径（空串 / 没设置 = 自动发现）。
    pub fn saved_path(self, db: &Database) -> Result<Option<String>, ZsshError> {
        match self {
            AgentKind::OpenCode => db.ai_opencode_path(),
            AgentKind::Codex => db.ai_codex_path(),
            AgentKind::Claude => db.ai_claude_path(),
            AgentKind::Builtin => Ok(None),
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
            // 内置助手没有手动路径；空串不是任何设置键，写进去会被拒绝。
            AgentKind::Builtin => "",
        }
    }

    /// 手动路径输入框里的占位示例（0.2.14 设置页同款）。
    #[cfg(windows)]
    pub fn path_placeholder(self) -> &'static str {
        match self {
            AgentKind::OpenCode => r"C:\path\to\opencode.exe",
            AgentKind::Codex => r"C:\path\to\codex.exe",
            AgentKind::Claude => r"C:\path\to\claude.exe",
            AgentKind::Builtin => "",
        }
    }

    /// 手动路径输入框里的占位示例（Linux 写法）。
    #[cfg(unix)]
    pub fn path_placeholder(self) -> &'static str {
        match self {
            AgentKind::OpenCode => "/path/to/opencode",
            AgentKind::Codex => "/path/to/codex",
            AgentKind::Claude => "/path/to/claude",
            AgentKind::Builtin => "",
        }
    }
}

/// 默认用哪个智能体（V2-A4，V2-U32 改）：
/// - 用户在 AI 设置里明确选过（设置库里有认得的 `ai_default_agent`）→ 以用户为准，选内置助手
///   但还没配好接口时也不退回别的智能体，面板显示 ZeePilot 自己的配置引导；
/// - 没选过 → ZeePilot（不管配没配好）。存的值读不懂（手改、别的版本）也当没选过。
///
/// `saved` 是设置库里存的原始值（JSON 字符串，没存过是 `None`）。
pub fn choose_default_agent(saved: Option<&str>) -> AgentKind {
    saved
        .and_then(|raw| serde_json::from_str::<String>(raw).ok())
        .and_then(|value| AgentKind::parse(&value))
        .unwrap_or(AgentKind::Builtin)
}

/// 设置库里的默认智能体，套用 [`choose_default_agent`] 的规则；设置库打不开按没选过算。
pub fn default_agent(db: Option<&Database>) -> AgentKind {
    let saved = db.and_then(|db| db.setting_get(AI_DEFAULT_AGENT_KEY).ok().flatten());
    choose_default_agent(saved.as_deref())
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

    /// 读一个目录下所有 .rs（不含测试文件和 `#[cfg(test)]` 之后的部分、不含注释行）。
    fn product_code_lines(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                product_code_lines(&path, out);
                continue;
            }
            let name = path.to_string_lossy().replace('\\', "/");
            if path.extension().is_none_or(|e| e != "rs")
                || name.ends_with("tests.rs")
                || name.contains("/tests/")
                // 假的 Tokenway 服务端：它回放的是服务端自己的原文，不是客户端文案。
                || name.ends_with("tokenway/fake.rs")
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let code = text.split("#[cfg(test)]").next().unwrap_or(&text);
            for line in code.lines() {
                if !line.trim_start().starts_with("//") {
                    out.push((name.clone(), line.to_owned()));
                }
            }
        }
    }

    #[test]
    fn the_builtin_agent_is_called_zeepilot_everywhere_in_the_product_text() {
        // 界面（含同步 / 存储里给用户看的提示）里不再出现「内置助手」：只改显示名，类型名和存储键不动。
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut lines = Vec::new();
        product_code_lines(&manifest.join("src"), &mut lines);
        product_code_lines(&manifest.join("../../crates/zssh-core/src"), &mut lines);
        assert!(lines.len() > 1000, "扫描没读到源码");
        for (name, line) in lines {
            assert!(
                !line.contains("内置助手"),
                "{name} 里还有「内置助手」：{}",
                line.trim()
            );
        }
        assert_eq!(AgentKind::Builtin.label(), "ZeePilot");
        assert_eq!(AgentKind::Builtin.id(), "builtin", "存储键不能变");
    }

    #[test]
    fn the_builtin_agent_sits_outside_the_external_ones() {
        // 外部智能体的数组、发现、手动路径都按 ALL 循环，不能混进内置助手。
        assert!(!AgentKind::ALL.contains(&AgentKind::Builtin));
        assert_eq!(
            AgentKind::SELECTABLE.map(|kind| kind.id()),
            ["builtin", "opencode", "codex", "claude"]
        );
        assert_eq!(AgentKind::Builtin.label(), "ZeePilot");
        assert_eq!(AgentKind::parse("builtin"), Some(AgentKind::Builtin));
        assert_eq!(AgentKind::from_setting("builtin"), AgentKind::Builtin);
        assert!(AgentKind::Builtin.spec().is_none());
    }

    #[test]
    fn default_agent_is_zeepilot_until_the_user_chooses() {
        use super::{AgentKind::*, choose_default_agent as choose};
        // 没选过：ZeePilot，不管配没配好。
        assert_eq!(choose(None), Builtin);
        // 用户选过的以用户为准。
        assert_eq!(choose(Some("\"claude\"")), Claude);
        assert_eq!(choose(Some("\"codex\"")), Codex);
        assert_eq!(choose(Some("\"opencode\"")), OpenCode);
        assert_eq!(choose(Some("\"builtin\"")), Builtin);
        // 存的值读不懂：当没选过。
        assert_eq!(choose(Some("\"gemini\"")), Builtin);
        assert_eq!(choose(Some("not json")), Builtin);
    }

    #[test]
    fn unknown_settings_fall_back_to_opencode_like_0_2_14() {
        assert_eq!(AgentKind::from_setting("gemini"), AgentKind::OpenCode);
        assert_eq!(AgentKind::from_setting(""), AgentKind::OpenCode);
        assert_eq!(AgentKind::from_setting(" claude "), AgentKind::Claude);
        assert_eq!(AgentKind::parse("gemini"), None);
    }

    fn endpoint_draft(
        name: &str,
        key: Option<&str>,
    ) -> zssh_core::ai::agent::builtin::config::AiEndpointDraft {
        use zssh_core::ai::agent::builtin::config::{
            AiEndpointDraft, EndpointKind, OpenAiOptionsConfig,
        };
        AiEndpointDraft {
            id: None,
            name: name.to_owned(),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.example.com/v1".to_owned(),
            api_key: key.map(str::to_owned),
            default_model: Some("m".to_owned()),
            models: Vec::new(),
            overrides: Default::default(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: None,
        }
    }

    #[test]
    fn the_default_agent_follows_the_database_and_never_falls_back_to_another_agent() {
        let db = Database::open_in_memory().expect("内存库");
        // 没选过、没配接口：仍是 ZeePilot（面板里显示配置引导）。
        assert_eq!(super::default_agent(Some(&db)), AgentKind::Builtin);
        assert_eq!(super::default_agent(None), AgentKind::Builtin);
        assert_eq!(db.ai_default_agent().expect("读"), "builtin");
        // 配好接口不影响。
        let keyed = db
            .ai_endpoint_save(&endpoint_draft("主力", Some("sk-test-0001")))
            .expect("存");
        assert_eq!(super::default_agent(Some(&db)), AgentKind::Builtin);
        // 用户明确选了别的：以用户为准，哪怕内置配好了；明确选 opencode 也是选过。
        for (id, kind) in [
            ("claude", AgentKind::Claude),
            ("opencode", AgentKind::OpenCode),
            ("codex", AgentKind::Codex),
        ] {
            db.set_ai_default_agent(id).expect("写");
            assert_eq!(super::default_agent(Some(&db)), kind, "{id}");
            assert_eq!(db.ai_default_agent().expect("读"), id);
        }
        // 明确选内置，之后把接口删光：还是 ZeePilot，不悄悄退回 opencode。
        db.set_ai_default_agent("builtin").expect("写");
        db.ai_endpoint_delete(keyed.id).expect("删");
        assert_eq!(super::default_agent(Some(&db)), AgentKind::Builtin);
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
        // 默认智能体：库里没写是 ZeePilot；写了按写的；没有库也是 ZeePilot。
        assert_eq!(super::default_agent(Some(&db)), AgentKind::Builtin);
        db.set_ai_default_agent("codex").expect("写");
        assert_eq!(super::default_agent(Some(&db)), AgentKind::Codex);
        assert_eq!(super::default_agent(None), AgentKind::Builtin);
    }
}
