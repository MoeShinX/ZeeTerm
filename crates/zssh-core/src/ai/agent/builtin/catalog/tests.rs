//! 能力识别的集成式单测：名字匹配表（真实快照）、固定夹具上的规则、接口自报、合成优先级、兜底。
//! 加载 / 缓存 / 更新的测试在 `cache.rs`。

use super::*;

// ---- 夹具 ----

fn entry(provider: &str, id: &str, ctx: u32, out: u32) -> CatalogEntry {
    CatalogEntry {
        provider: provider.to_string(),
        id: id.to_string(),
        name: format!("Name of {id}"),
        canonical: None,
        family: None,
        context: ctx,
        input: None,
        output: out,
        reasoning: ReasoningSupport::None,
        tool_call: true,
        image: false,
        cost: None,
        deprecated: false,
    }
}

fn catalog(entries: Vec<CatalogEntry>) -> Catalog {
    Catalog::from_entries(entries, CatalogOrigin::Snapshot, Some(1_000))
}

fn key_of(c: &Catalog, name: &str) -> Option<String> {
    c.find(name).map(|m| m.entry.key())
}

// ---- 名字匹配：真实快照上的表驱动 ----

/// (用户填的写法, 期望命中的条目键；None 表示库里确实没有，该走规则兜底)。
/// 期望值是对着当前内置快照核对过的；重新生成快照后若有出入，先看新数据是否合理再改这里。
const SNAPSHOT_TABLE: &[(&str, Option<&str>)] = &[
    // OpenAI：大小写、点与连字符、厂商前缀、日期后缀、变体后缀
    ("gpt-5.5", Some("openai/gpt-5.5")),
    ("GPT-5.5", Some("openai/gpt-5.5")),
    ("gpt-5-5", Some("openai/gpt-5.5")),
    ("gpt_5.5", Some("openai/gpt-5.5")),
    ("  gpt-5.5  ", Some("openai/gpt-5.5")),
    ("openai/gpt-5.5", Some("openai/gpt-5.5")),
    ("openrouter/openai/gpt-5.5", Some("openai/gpt-5.5")),
    ("gpt-5.5-2026-08-01", Some("openai/gpt-5.5")),
    ("gpt-5.5-pro", Some("openai/gpt-5.5-pro")),
    ("gpt-5.5-codex-max", Some("openai/gpt-5.5")),
    ("gpt-5", Some("openai/gpt-5")),
    ("gpt-5-mini", Some("openai/gpt-5-mini")),
    ("gpt-4o", Some("openai/gpt-4o")),
    ("gpt-4.1", Some("openai/gpt-4.1")),
    ("o3", Some("openai/o3")),
    ("o4-mini-high", Some("openai/o4-mini")),
    // DeepSeek
    ("deepseek-v4-flash", Some("deepseek/deepseek-v4-flash")),
    ("DeepSeek-V4-Flash", Some("deepseek/deepseek-v4-flash")),
    (
        "deepseek/deepseek-v4-flash",
        Some("deepseek/deepseek-v4-flash"),
    ),
    (
        "deepseek-ai/DeepSeek-V4-Flash",
        Some("deepseek/deepseek-v4-flash"),
    ),
    ("deepseek-v4-pro", Some("deepseek/deepseek-v4-pro")),
    // Anthropic
    ("anthropic/claude-opus-5", Some("anthropic/claude-opus-5")),
    (
        "openrouter/anthropic/claude-opus-5",
        Some("anthropic/claude-opus-5"),
    ),
    ("claude-opus-5-20261001", Some("anthropic/claude-opus-5")),
    ("claude-opus-5:thinking", Some("anthropic/claude-opus-5")),
    ("claude-opus-5-thinking", Some("anthropic/claude-opus-5")),
    ("claude-sonnet-5-5", Some("anthropic/claude-sonnet-5-5")),
    ("Claude-Sonnet-5-5", Some("anthropic/claude-sonnet-5-5")),
    ("claude-sonnet-5.5", Some("anthropic/claude-sonnet-5-5")),
    (
        "claude-sonnet-5-5@20261001",
        Some("anthropic/claude-sonnet-5-5"),
    ),
    (
        "claude-haiku-4-5-20251001",
        Some("anthropic/claude-haiku-4-5-20251001"),
    ),
    // 用户名缺了后缀：取同样接近的几个里最保守的（上下文最小）
    ("claude-opus-4", Some("anthropic/claude-opus-4-5")),
    // 通义 / Qwen（厂商自家接口里有 alibaba 与 alibaba-cn 两份，并列时按保守值再按键排序）
    ("Qwen/Qwen3-235B-A22B", Some("alibaba-cn/qwen3-235b-a22b")),
    ("qwen3-max", Some("alibaba-cn/qwen3-max")),
    (
        "Qwen/Qwen3-Coder-480B-A35B-Instruct",
        Some("alibaba-cn/qwen3-coder-480b-a35b-instruct"),
    ),
    ("qwen-plus-latest", Some("alibaba-cn/qwen-plus")),
    ("qwen3.7-plus", Some("alibaba-cn/qwen3.7-plus")),
    // 智谱 GLM
    ("glm-4.6", Some("zai/glm-4.6")),
    ("GLM-4.6", Some("zai/glm-4.6")),
    ("zai-org/GLM-4.6", Some("zai/glm-4.6")),
    ("glm-4.5-air", Some("zai/glm-4.5-air")),
    // Kimi
    ("kimi-k2", Some("openrouter/moonshotai/kimi-k2")),
    (
        "moonshotai/Kimi-K2-Instruct",
        Some("openrouter/moonshotai/kimi-k2"),
    ),
    ("kimi-k2.6", Some("moonshotai-cn/kimi-k2.6")),
    ("kimi-k3", Some("moonshotai-cn/kimi-k3")),
    // Google
    ("gemini-2.5-pro", Some("google/gemini-2.5-pro")),
    ("models/gemini-2.5-flash", Some("google/gemini-2.5-flash")),
    (
        "google/gemini-2.5-flash:free",
        Some("google/gemini-2.5-flash"),
    ),
    (
        "gemini-3.1-pro-preview",
        Some("google/gemini-3.1-pro-preview"),
    ),
    // 其他厂商
    ("MiniMax-M3.1-Flash", Some("minimax-cn/MiniMax-M3.1-Flash")),
    ("grok-4.3", Some("xai/grok-4.3")),
    ("mistral-large-latest", Some("mistral/mistral-large-latest")),
    ("devstral-2512", Some("mistral/devstral-2512")),
    (
        "meta-llama/Llama-3.3-70B-Instruct",
        Some("openrouter/meta-llama/llama-3.3-70b-instruct"),
    ),
    ("muse-spark-1.2", Some("meta/muse-spark-1.2")),
    // 库里没有的：自建网关改名、太泛的词、非聊天模型、库里确实没收的新旧版本
    ("my-company-chat-v7", None),
    ("tw-premium-model", None),
    ("gpt", None),
    ("claude", None),
    ("o", None),
    ("", None),
    ("openrouter/auto", None),
    ("claude-opus-6", None),
    ("gemini-3-pro", None),
    ("deepseek-reasoner", None),
    ("THUDM/glm-4-9b-chat", None),
];

#[test]
fn name_matching_table_on_real_snapshot() {
    assert!(SNAPSHOT_TABLE.len() >= 40);
    let db = Catalog::builtin();
    let mut wrong = Vec::new();
    for (input, expected) in SNAPSHOT_TABLE {
        let got = key_of(&db, input);
        if got.as_deref() != *expected {
            wrong.push(format!("{input:?}: 期望 {expected:?}，实际 {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "匹配不符：\n{}", wrong.join("\n"));
}

#[test]
fn match_kinds_on_real_snapshot() {
    let db = Catalog::builtin();
    let kind = |n: &str| db.find(n).map(|m| m.kind);
    assert_eq!(kind("gpt-5.5"), Some(MatchKind::ExactId));
    assert_eq!(kind("gpt-5-5"), Some(MatchKind::ExactName));
    assert_eq!(kind("claude-opus-5-20261001"), Some(MatchKind::ExactName));
    assert_eq!(kind("gpt-5.5-codex-max"), Some(MatchKind::Prefix));
    assert_eq!(kind("claude-opus-4"), Some(MatchKind::Extends));
}

#[test]
fn same_vendor_beats_other_hosts_of_the_same_id() {
    // 这些别家厂商 / 聚合平台里也托管了同名模型；厂商自家接口优先。
    let db = Catalog::builtin();
    for (name, vendor_providers) in [
        ("glm-5.2", &["zai", "zhipuai"][..]),
        ("z-ai/glm-5.2", &["zai", "zhipuai"][..]),
        ("kimi-k3", &["moonshotai", "moonshotai-cn"][..]),
        ("deepseek-v4-flash", &["deepseek"][..]),
    ] {
        let m = db.find(name).expect(name);
        assert!(
            vendor_providers.contains(&m.entry.provider.as_str()),
            "{name} 落到了 {}",
            m.entry.provider
        );
    }
}

// ---- 名字匹配：固定夹具上的规则 ----

#[test]
fn separators_case_and_prefix_are_normalized() {
    let db = catalog(vec![entry("anthropic", "claude-sonnet-5-5", 1000, 100)]);
    for name in [
        "claude-sonnet-5-5",
        "claude-sonnet-5.5",
        "claude_sonnet_5_5",
        "CLAUDE-SONNET-5.5",
        "anthropic/claude-sonnet-5-5",
        "some-gateway/anthropic/claude-sonnet-5.5",
        "claude-sonnet-5-5-20261001",
        "claude-sonnet-5-5-latest",
        "claude-sonnet-5-5@20261001",
        "claude-sonnet-5-5:free",
    ] {
        assert_eq!(
            key_of(&db, name).as_deref(),
            Some("anthropic/claude-sonnet-5-5"),
            "{name}"
        );
    }
    // 5.5 与 55 不是同一个版本：不猜。
    assert_eq!(key_of(&db, "claude-sonnet-55"), None);
}

#[test]
fn date_suffix_forms_are_stripped_only_when_needed() {
    let db = catalog(vec![
        entry("mistral", "mistral-medium-2505", 1000, 100),
        entry("mistral", "mistral-medium-latest", 2000, 100),
        entry("openai", "gpt-x", 3000, 100),
    ]);
    // 日期本身就是 id 的一部分：原样精确匹配，不能被剥掉。
    assert_eq!(
        key_of(&db, "mistral-medium-2505").as_deref(),
        Some("mistral/mistral-medium-2505")
    );
    // 各种日期写法剥掉后命中 gpt-x。
    for name in [
        "gpt-x-20260101",
        "gpt-x-2026-01-01",
        "gpt-x-260101",
        "gpt-x-2601",
        "gpt-x-2601-latest",
    ] {
        assert_eq!(key_of(&db, name).as_deref(), Some("openai/gpt-x"), "{name}");
    }
    // 看起来像日期但月份不对的数字不剥（前缀匹配仍会命中 gpt-x，但那是 Prefix 而不是日期处理）。
    assert_eq!(
        db.find("gpt-x-9999").map(|m| m.kind),
        Some(MatchKind::Prefix)
    );
}

#[test]
fn canonical_id_is_matched() {
    let mut e = entry("deepseek", "deepseek-v4-flash", 1000, 100);
    e.canonical = Some("deepseek/deepseek-v4.1-flash-1017".to_string());
    let db = catalog(vec![e]);
    for name in [
        "deepseek/deepseek-v4.1-flash-1017",
        "deepseek-v4-1-flash-1017",
        "deepseek-v4.1-flash",
    ] {
        assert_eq!(
            key_of(&db, name).as_deref(),
            Some("deepseek/deepseek-v4-flash"),
            "{name}"
        );
    }
}

#[test]
fn longest_prefix_wins_and_short_prefixes_are_guarded() {
    let db = catalog(vec![
        entry("openai", "gpt-5", 100, 10),
        entry("openai", "gpt-5.5", 200, 10),
        entry("openai", "o3", 300, 10),
        entry("anthropic", "claude-haiku-4-5", 400, 10),
    ]);
    assert_eq!(
        key_of(&db, "gpt-5.5-codex-max").as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        key_of(&db, "gpt-5.1-codex").as_deref(),
        Some("openai/gpt-5")
    );
    assert_eq!(key_of(&db, "o3-high").as_deref(), Some("openai/o3"));
    // 带数字的单 token 前缀才放行；`claude` / `gpt` 这种泛词不行。
    assert_eq!(key_of(&db, "claude-whatever"), None);
    assert_eq!(key_of(&db, "gpt-whatever"), None);
    // 版本更高但库里没有：宁可不认也不把 6 当成 5。
    assert_eq!(key_of(&db, "o30"), None);
    // 反向（用户名缺后缀）至少要两个 token。
    assert_eq!(key_of(&db, "gpt"), None);
    assert_eq!(
        key_of(&db, "claude-haiku-4").as_deref(),
        Some("anthropic/claude-haiku-4-5")
    );
}

#[test]
fn ranking_vendor_then_native_then_not_deprecated_then_conservative() {
    // 同 id 的几份：聚合平台、别家托管、厂商自家（已弃用）、厂商自家的另一区域。
    let mut native_old = entry("zai", "glm-9", 300, 30);
    native_old.deprecated = true;
    let db = catalog(vec![
        entry("openrouter", "z-ai/glm-9", 100, 10),
        entry("mistral", "glm-9", 50, 5),
        native_old,
    ]);
    // 厂商由模型名推出（glm → 智谱）：自家接口即使 deprecated 也优先于聚合平台与别家托管。
    assert_eq!(key_of(&db, "glm-9").as_deref(), Some("zai/glm-9"));

    // 厂商自家两份并列且都没弃用 → 保守值（上下文小的）。
    let db = catalog(vec![
        entry("alibaba", "qwen9", 1000, 100),
        entry("alibaba-cn", "qwen9", 500, 100),
    ]);
    assert_eq!(key_of(&db, "qwen9").as_deref(), Some("alibaba-cn/qwen9"));

    // 同样并列，一份弃用 → 没弃用的优先（即使上下文更大）。
    let mut old = entry("alibaba", "qwen8", 100, 10);
    old.deprecated = true;
    let db = catalog(vec![old, entry("alibaba-cn", "qwen8", 900, 10)]);
    assert_eq!(key_of(&db, "qwen8").as_deref(), Some("alibaba-cn/qwen8"));

    // 前缀写法 `Qwen/` 明确指定厂商，覆盖按名字的推断。
    let db = catalog(vec![
        entry("openrouter", "qwen/foo-1", 100, 10),
        entry("mistral", "foo-1", 50, 5),
    ]);
    assert_eq!(
        key_of(&db, "Qwen/foo-1").as_deref(),
        Some("openrouter/qwen/foo-1")
    );
}

#[test]
fn empty_catalog_matches_nothing_and_get_by_key_works() {
    let empty = Catalog::empty();
    assert!(empty.find("gpt-5.5").is_none());
    assert!(empty.is_empty());
    let db = Catalog::builtin();
    let m = db.find("gpt-5.5").expect("gpt-5.5");
    assert_eq!(
        db.get(&m.entry.key()).map(|e| e.id.as_str()),
        Some("gpt-5.5")
    );
    assert!(db.get("nope/nope").is_none());
}

// ---- trim_api_json ----

const MINI_API: &str = r#"{
  "openai": {"id":"openai","name":"OpenAI","models":{
    "gpt-a":{"id":"gpt-a","name":"GPT A","reasoning":true,
      "reasoning_options":[{"type":"toggle"},{"type":"effort","values":["low","high"]},{"type":"budget_tokens","min":2048}],
      "tool_call":true,"modalities":{"input":["text","image","pdf"],"output":["text"]},
      "limit":{"context":400000,"input":272000,"output":128000},
      "cost":{"input":1.25,"output":10,"cache_read":0.125,"tiers":[{"input":2}]},
      "status":"deprecated","canonical_model_id":"openai/gpt-a","family":"gpt"},
    "gpt-budget":{"id":"gpt-budget","reasoning":true,"reasoning_options":[{"type":"budget_tokens","min":0,"max":24576}],
      "tool_call":false,"modalities":{"input":["text"],"output":["text"]},"limit":{"context":1000,"output":500}},
    "gpt-budget-open":{"id":"gpt-budget-open","reasoning":true,"reasoning_options":[{"type":"budget_tokens"}],
      "modalities":{"input":["text"],"output":["text"]},"limit":{"context":1000,"output":500}},
    "gpt-toggle":{"id":"gpt-toggle","reasoning":true,"reasoning_options":[{"type":"toggle"}],
      "modalities":{"input":["text"],"output":["text"]},"limit":{"context":1000,"output":500}},
    "gpt-always-on":{"id":"gpt-always-on","reasoning":true,"reasoning_options":[],
      "modalities":{"input":["text"],"output":["text"]},"limit":{"context":1000,"output":500}},
    "gpt-no-reasoning":{"id":"gpt-no-reasoning","reasoning":false,
      "modalities":{"input":["text"],"output":["text"]},"limit":{"context":1000,"output":500}},
    "embed":{"id":"embed","type":"embedding","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8000,"output":0}},
    "image-gen":{"id":"image-gen","modalities":{"input":["text"],"output":["image"]},"limit":{"context":8000,"output":0}},
    "no-limit":{"id":"no-limit","modalities":{"input":["text"],"output":["text"]}},
    "zero-ctx":{"id":"zero-ctx","modalities":{"input":["text"],"output":["text"]},"limit":{"context":0,"output":0}},
    "broken":{"id":"broken","limit":"oops"}
  }},
  "openrouter": {"models":{
    "anthropic/claude-x":{"id":"anthropic/claude-x","modalities":{"input":["text"],"output":["text"]},"limit":{"context":200000,"output":64000}},
    "~anthropic/claude-latest":{"id":"~anthropic/claude-latest","modalities":{"input":["text"],"output":["text"]},"limit":{"context":200000,"output":64000}},
    "thedrummer/skyfall":{"id":"thedrummer/skyfall","modalities":{"input":["text"],"output":["text"]},"limit":{"context":32000,"output":4000}}
  }},
  "some-random-host": {"models":{"x":{"id":"x","limit":{"context":1,"output":1}}}}
}"#;

#[test]
fn trim_keeps_wanted_models_and_maps_reasoning() {
    let entries = trim_api_json(MINI_API.as_bytes()).expect("trim");
    let ids: Vec<String> = entries.iter().map(|e| e.key()).collect();
    // 按 provider、id 字典序排；嵌入向量、生图、没有 limit、上下文为 0、字段损坏的都不收，
    // 不认识的厂商和 OpenRouter 里的冷门厂商也不收。
    assert_eq!(
        ids,
        [
            "openai/gpt-a",
            "openai/gpt-always-on",
            "openai/gpt-budget",
            "openai/gpt-budget-open",
            "openai/gpt-no-reasoning",
            "openai/gpt-toggle",
            "openrouter/anthropic/claude-x",
            "openrouter/~anthropic/claude-latest",
        ]
    );
    let get = |id: &str| entries.iter().find(|e| e.id == id).expect(id);

    let a = get("gpt-a");
    // effort 优先于 budget 与 toggle。
    assert_eq!(
        a.reasoning,
        ReasoningSupport::Levels(vec!["low".into(), "high".into()])
    );
    assert_eq!(
        (a.context, a.input, a.output),
        (400_000, Some(272_000), 128_000)
    );
    assert!(a.image && a.tool_call && a.deprecated);
    assert_eq!(a.canonical.as_deref(), Some("openai/gpt-a"));
    assert_eq!(a.family.as_deref(), Some("gpt"));
    let cost = a.cost.expect("cost");
    assert_eq!(
        (cost.input, cost.output, cost.cache_read, cost.cache_write),
        (1.25, 10.0, Some(0.125), None)
    );

    assert_eq!(
        get("gpt-budget").reasoning,
        ReasoningSupport::Budget {
            min: 0,
            max: 24_576
        }
    );
    assert!(!get("gpt-budget").tool_call);
    // 缺 min / max：min 取 1024，max 取最大输出。
    assert_eq!(
        get("gpt-budget-open").reasoning,
        ReasoningSupport::Budget {
            min: 1024,
            max: 500
        }
    );
    assert_eq!(get("gpt-toggle").reasoning, ReasoningSupport::Toggle);
    assert_eq!(get("gpt-always-on").reasoning, ReasoningSupport::None);
    assert_eq!(get("gpt-no-reasoning").reasoning, ReasoningSupport::None);
}

#[test]
fn trim_rejects_non_json_and_trailing_garbage() {
    assert!(trim_api_json(b"<html>nope</html>").is_err());
    assert!(trim_api_json(b"{\"openai\":").is_err());
    assert!(trim_api_json(b"[]").is_err());
    assert!(trim_api_json(b"{} trailing").is_err());
    assert_eq!(trim_api_json(b"{}").expect("empty ok").len(), 0);
}

#[test]
fn snapshot_json_builder_checks_date_and_roundtrips() {
    assert!(build_snapshot_json(MINI_API.as_bytes(), "2026-13-45").is_err());
    assert!(build_snapshot_json(MINI_API.as_bytes(), "yesterday").is_err());
    let text = build_snapshot_json(MINI_API.as_bytes(), "2026-11-03").expect("build");
    let file: CatalogFile = serde_json::from_str(&text).expect("valid json");
    assert_eq!(file.version, FORMAT_VERSION);
    assert_eq!(file.generated.as_deref(), Some("2026-11-03"));
    assert_eq!(file.entries.len(), 8);
    assert_eq!(file.timestamp(), date_to_unix("2026-11-03"));
}

#[test]
fn date_conversion_roundtrips() {
    assert_eq!(date_to_unix("1970-01-01"), Some(0));
    assert_eq!(date_to_unix("2000-03-01"), Some(951_868_800));
    assert_eq!(date_to_unix("2024-02-29"), Some(1_709_164_800));
    assert_eq!(date_to_unix("2026-11-03"), Some(1_793_664_000));
    for d in [
        "1970-01-01",
        "2000-02-29",
        "2024-12-31",
        "2026-11-03",
        "2099-06-15",
    ] {
        assert_eq!(unix_to_date(date_to_unix(d).expect(d)), d);
    }
    assert_eq!(date_to_unix("2026-1"), None);
    assert_eq!(date_to_unix("2026-00-10"), None);
    assert_eq!(date_to_unix("x-y-z"), None);
    assert_eq!(date_to_unix("1969-12-31"), None);
}

// ---- 接口自报 ----

#[test]
fn endpoint_openrouter_style() {
    let json = r#"{"data":[
      {"id":"anthropic/claude-opus-5","name":"Anthropic: Claude Opus 5","context_length":1000000,
       "architecture":{"input_modalities":["text","image"],"output_modalities":["text"],"modality":"text+image->text"},
       "top_provider":{"context_length":1000000,"max_completion_tokens":128000},
       "supported_parameters":["max_tokens","reasoning","include_reasoning","tools","tool_choice"]},
      {"id":"vendor/plain-llm","name":"vendor/plain-llm","context_length":"32768",
       "top_provider":{"max_completion_tokens":null},
       "architecture":{"modality":"text->text"},
       "supported_parameters":["max_tokens","temperature"]}
    ]}"#;
    let got = parse_models_endpoint(json);
    assert_eq!(got.len(), 2);
    let (id, c) = &got[0];
    assert_eq!(id, "anthropic/claude-opus-5");
    assert_eq!(c.context_window, Some(1_000_000));
    assert_eq!(c.max_output, Some(128_000));
    assert_eq!(c.reasoning, Some(ReasoningSupport::Toggle));
    assert_eq!(c.tool_call, Some(true));
    assert_eq!(c.image_input, Some(true));
    assert_eq!(c.display_name.as_deref(), Some("Anthropic: Claude Opus 5"));

    let (_, p) = &got[1];
    assert_eq!(p.context_window, Some(32_768)); // 字符串形式的数字
    assert_eq!(p.max_output, None);
    assert_eq!(p.reasoning, Some(ReasoningSupport::None)); // 给了参数清单但没有 reasoning
    assert_eq!(p.tool_call, Some(false));
    assert_eq!(p.image_input, Some(false)); // modality 左边没有 image
    assert_eq!(p.display_name, None); // name 和 id 一样不算显示名
}

#[test]
fn endpoint_plain_gateway_only_has_ids() {
    let json = r#"{"object":"list","data":[{"id":"gpt-5.5","object":"model","created":1,"owned_by":"openai"},{"id":"  deepseek-v4-flash "}]}"#;
    let got = parse_models_endpoint(json);
    assert_eq!(
        got.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(),
        ["gpt-5.5", "deepseek-v4-flash"]
    );
    for (_, c) in &got {
        assert_eq!(c, &PartialCaps::default(), "没有自报就是全空，不能猜");
    }
}

#[test]
fn endpoint_alternate_shapes_and_junk() {
    // 裸数组、`models` 字段、别名字段。
    let got = parse_models_endpoint(
        r#"[{"id":"a","context_window":1000,"max_output_tokens":200,"max_input_tokens":800}]"#,
    );
    assert_eq!(got[0].1.context_window, Some(1000));
    assert_eq!(got[0].1.max_output, Some(200));
    assert_eq!(got[0].1.max_input, Some(800));
    let got = parse_models_endpoint(
        r#"{"models":[{"name":"llama3:8b"},{"model":"qwen2.5:7b"},{"id":""},{"nope":1},42]}"#,
    );
    assert_eq!(
        got.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(),
        ["llama3:8b", "qwen2.5:7b"]
    );
    // 0 / 负数 / 小数不当真。
    let got =
        parse_models_endpoint(r#"{"data":[{"id":"z","context_length":0,"max_output_tokens":-5}]}"#);
    assert_eq!(got[0].1.context_window, None);
    assert_eq!(got[0].1.max_output, None);
    // 垃圾输入。
    for bad in [
        "",
        "not json",
        "null",
        "42",
        r#"{"data":"x"}"#,
        r#"{"error":{"message":"unauthorized"}}"#,
        "[1,2,3]",
    ] {
        assert!(parse_models_endpoint(bad).is_empty(), "{bad}");
    }
}

// ---- 合成 ----

fn rich_db() -> Catalog {
    let mut e = entry("anthropic", "claude-opus-5", 1_000_000, 128_000);
    e.name = "Claude Opus 5".to_string();
    e.input = Some(900_000);
    e.reasoning = ReasoningSupport::Levels(vec!["low".into(), "high".into()]);
    e.image = true;
    e.cost = Some(Cost {
        input: 5.0,
        output: 25.0,
        cache_read: Some(0.5),
        cache_write: None,
    });
    catalog(vec![e])
}

#[test]
fn resolve_uses_models_dev_when_nothing_else_is_known() {
    let caps = resolve("claude-opus-5-20261001", None, None, &rich_db());
    assert_eq!(caps.context_window, 1_000_000);
    assert_eq!(caps.max_input, Some(900_000));
    assert_eq!(caps.max_output, 128_000);
    assert_eq!(
        caps.reasoning,
        ReasoningSupport::Levels(vec!["low".into(), "high".into()])
    );
    assert!(caps.tool_call && caps.image_input);
    assert_eq!(caps.cost.map(|c| c.output), Some(25.0));
    assert_eq!(caps.source, CapsSource::Builtin);
    assert_eq!(caps.display_name, "Claude Opus 5");
    assert_eq!(caps.matched_id.as_deref(), Some("anthropic/claude-opus-5"));
}

#[test]
fn resolve_source_is_models_dev_for_cache_data() {
    let db = Catalog::from_entries(
        vec![entry("openai", "gpt-z", 1000, 100)],
        CatalogOrigin::Cache,
        Some(5),
    );
    assert_eq!(
        resolve("gpt-z", None, None, &db).source,
        CapsSource::ModelsDev
    );
}

#[test]
fn resolve_priority_manual_over_endpoint_over_models_dev_per_field() {
    let db = rich_db();
    let endpoint = PartialCaps {
        context_window: Some(500_000),
        max_output: Some(64_000),
        tool_call: Some(false),
        image_input: Some(false),
        ..Default::default()
    };
    let manual = CapsOverride {
        max_output: Some(8_000),
        image_input: Some(true),
        reasoning: Some(ReasoningSupport::Toggle),
        cost: Some(Cost {
            input: 1.0,
            output: 2.0,
            cache_read: None,
            cache_write: None,
        }),
        ..Default::default()
    };
    let caps = resolve("claude-opus-5", Some(manual), Some(endpoint), &db);
    // 手动有的用手动。
    assert_eq!(caps.max_output, 8_000);
    assert!(caps.image_input);
    assert_eq!(caps.reasoning, ReasoningSupport::Toggle);
    assert_eq!(caps.cost.map(|c| c.input), Some(1.0));
    // 手动没有、接口有的用接口。
    assert_eq!(caps.context_window, 500_000);
    assert!(!caps.tool_call);
    assert_eq!(caps.source, CapsSource::Endpoint);
    // 都没有的落到 models.dev。
    assert_eq!(caps.max_input, Some(900_000));
    assert_eq!(caps.display_name, "Claude Opus 5");
}

#[test]
fn resolve_manual_context_marks_source_manual_and_clamps_output() {
    let manual = CapsOverride {
        context_window: Some(50_000),
        ..Default::default()
    };
    let caps = resolve("claude-opus-5", Some(manual), None, &rich_db());
    assert_eq!(caps.context_window, 50_000);
    assert_eq!(caps.source, CapsSource::Manual);
    // 数据库的最大输出 128K 不能超过手动填的 50K 上下文。
    assert_eq!(caps.max_output, 50_000);
    // 其余仍来自数据库。
    assert_eq!(
        caps.reasoning,
        ReasoningSupport::Levels(vec!["low".into(), "high".into()])
    );
}

#[test]
fn resolve_zero_values_count_as_missing() {
    let manual = CapsOverride {
        context_window: Some(0),
        max_output: Some(0),
        ..Default::default()
    };
    let endpoint = PartialCaps {
        context_window: Some(0),
        ..Default::default()
    };
    let caps = resolve("claude-opus-5", Some(manual), Some(endpoint), &rich_db());
    assert_eq!(
        (caps.context_window, caps.max_output, caps.source),
        (1_000_000, 128_000, CapsSource::Builtin)
    );
}

#[test]
fn resolve_endpoint_toggle_does_not_erase_known_levels_but_can_say_no() {
    let db = rich_db();
    // 接口只说"能思考" → 保留 models.dev 的等级。
    let ep = PartialCaps {
        reasoning: Some(ReasoningSupport::Toggle),
        ..Default::default()
    };
    let caps = resolve("claude-opus-5", None, Some(ep), &db);
    assert_eq!(
        caps.reasoning,
        ReasoningSupport::Levels(vec!["low".into(), "high".into()])
    );
    // 接口明确没有 reasoning 参数 → 听接口的。
    let ep = PartialCaps {
        reasoning: Some(ReasoningSupport::None),
        ..Default::default()
    };
    assert_eq!(
        resolve("claude-opus-5", None, Some(ep), &db).reasoning,
        ReasoningSupport::None
    );
    // 库里也没有时，接口的 Toggle 照用。
    let ep = PartialCaps {
        reasoning: Some(ReasoningSupport::Toggle),
        ..Default::default()
    };
    assert_eq!(
        resolve("mystery-model", None, Some(ep), &db).reasoning,
        ReasoningSupport::Toggle
    );
}

#[test]
fn resolve_pick_overrides_name_matching() {
    let db = catalog(vec![
        entry("openai", "gpt-a", 111_000, 1000),
        entry("openai", "gpt-b", 222_000, 2000),
    ]);
    let manual = CapsOverride {
        pick: Some("openai/gpt-b".to_string()),
        ..Default::default()
    };
    let caps = resolve("gpt-a", Some(manual), None, &db);
    assert_eq!(caps.context_window, 222_000);
    assert_eq!(caps.matched_id.as_deref(), Some("openai/gpt-b"));
    // 过期的键退回按名字匹配。
    let manual = CapsOverride {
        pick: Some("openai/gone".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve("gpt-a", Some(manual), None, &db).context_window,
        111_000
    );
}

#[test]
fn resolve_unknown_model_uses_conservative_defaults() {
    let caps = resolve("  my-company-chat-v7 ", None, None, &Catalog::builtin());
    assert_eq!(caps.context_window, 32_000);
    assert_eq!(caps.max_output, 4_096);
    assert_eq!(caps.reasoning, ReasoningSupport::None);
    assert!(caps.tool_call, "未知 = 允许尝试");
    assert!(!caps.image_input);
    assert_eq!(caps.source, CapsSource::Heuristic);
    assert_eq!(caps.display_name, "my-company-chat-v7");
    assert_eq!(caps.matched_id, None);
    assert_eq!(caps.cost, None);
    // 空模型名也不 panic。
    assert_eq!(
        resolve("", None, None, &Catalog::empty()).context_window,
        32_000
    );
}

#[test]
fn resolve_heuristics_by_family_when_not_in_db() {
    let db = Catalog::empty();
    let r = |n: &str| resolve(n, None, None, &db);
    // 名字里带厂商前缀、版本号等都不影响家族判断。
    let c = r("anthropic/claude-opus-9-preview");
    assert_eq!(
        (c.context_window, c.source),
        (200_000, CapsSource::Heuristic)
    );
    assert!(matches!(c.reasoning, ReasoningSupport::Budget { .. }));
    assert!(c.image_input && c.tool_call);
    assert_eq!(r("claude-3-5-sonnet-20241022").max_output, 8_192);
    assert_eq!(
        r("claude-3-5-sonnet-20241022").reasoning,
        ReasoningSupport::None
    );
    assert_eq!(r("claude-3-opus-20240229").max_output, 4_096);

    assert_eq!(r("gpt-4o-2024-08-06").context_window, 128_000);
    assert!(r("gpt-4o-2024-08-06").image_input);
    assert_eq!(r("gpt-3.5-turbo").context_window, 16_000);
    assert!(matches!(
        r("gpt-5.9").reasoning,
        ReasoningSupport::Levels(_)
    ));
    assert!(matches!(
        r("o9-mini").reasoning,
        ReasoningSupport::Levels(_)
    ));
    assert_eq!(r("gemini-9-pro").context_window, 1_000_000);
    assert_eq!(r("deepseek-reasoner").context_window, 128_000);
    assert_eq!(r("deepseek-reasoner").reasoning, ReasoningSupport::Toggle);
    assert_eq!(r("deepseek-chat").reasoning, ReasoningSupport::None);
    assert_eq!(r("Qwen/Qwen2.5-72B-Instruct").context_window, 128_000);
    assert!(r("qwen2.5-vl-7b").image_input);
    assert_eq!(r("qwq-32b").reasoning, ReasoningSupport::Toggle);
    assert_eq!(r("kimi-k9").context_window, 128_000);
    assert_eq!(r("glm-9").reasoning, ReasoningSupport::Toggle);
    assert!(r("glm-4.6v").image_input);
    assert_eq!(r("MiniMax-M9").context_window, 200_000);
    assert_eq!(r("grok-9").context_window, 128_000);
    assert_eq!(r("llama3.1:8b").context_window, 128_000);
    assert_eq!(r("mistral-9").context_window, 128_000);
    // 兜底给的最大输出永远不超过上下文。
    for n in ["gpt-3.5-turbo", "claude-3-opus", "unknown-thing"] {
        let c = r(n);
        assert!(c.max_output <= c.context_window, "{n}");
    }
}

#[test]
fn resolve_on_real_snapshot_end_to_end() {
    let db = Catalog::builtin();
    let c = resolve("anthropic/claude-opus-5", None, None, &db);
    assert_eq!(c.matched_id.as_deref(), Some("anthropic/claude-opus-5"));
    assert_eq!(c.display_name, "Claude Opus 5");
    assert_eq!(c.context_window, 1_000_000);
    assert!(
        matches!(c.reasoning, ReasoningSupport::Levels(ref l) if l.iter().any(|x| x == "xhigh"))
    );
    assert!(c.cost.is_some());
    assert_eq!(c.source, CapsSource::Builtin);

    // 用户在自建网关里把 deepseek-v4-flash 改名成不认识的名字，只能靠手动覆盖。
    let manual = CapsOverride {
        context_window: Some(128_000),
        max_output: Some(8_000),
        tool_call: Some(true),
        ..Default::default()
    };
    let c = resolve("acme-fast-1", Some(manual), None, &db);
    assert_eq!(
        (c.context_window, c.max_output, c.source),
        (128_000, 8_000, CapsSource::Manual)
    );
    assert_eq!(c.matched_id, None);
}

#[test]
fn caps_types_serialize_stably() {
    let o = CapsOverride {
        reasoning: Some(ReasoningSupport::Budget {
            min: 1024,
            max: 8000,
        }),
        pick: Some("openai/gpt-5".into()),
        ..Default::default()
    };
    let json = serde_json::to_string(&o).expect("ser");
    let back: CapsOverride = serde_json::from_str(&json).expect("de");
    assert_eq!(o, back);
    assert_eq!(
        serde_json::to_string(&CapsSource::ModelsDev).expect("ser"),
        "\"models_dev\""
    );
    assert_eq!(
        serde_json::to_string(&ReasoningSupport::Toggle).expect("ser"),
        "\"toggle\""
    );
}
