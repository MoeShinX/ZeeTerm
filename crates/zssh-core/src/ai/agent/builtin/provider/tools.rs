//! 从工具桥现有的工具清单生成 [`ToolSpec`]。
//!
//! 工具桥的 8 个工具（`list_targets`、`run_command`、`read_terminal`、`list_dir`、
//! `read_file`、`write_file`、`stat`、`remember_host`）对外是 `zssh-mcp` 那个 MCP 服务端
//! 暴露的，名称、描述和参数结构定义在 `crates/zssh-mcp/src/main.rs`；核心库不能反向依赖
//! 那个二进制 crate，所以这里按它**照抄一份**，并用测试盯住不漂移：
//! - 名称与 `protocol::METHOD_*` 一致，且都在 `zssh-mcp` 源码里有同名的工具函数；
//! - 每个工具的参数字段都能反序列化进 `protocol` 里对应的 `*Params` 结构体。
//!
//! 与 MCP 版的区别：没有 `capability` 参数——内置助手直接在进程内调用工具桥，
//! 对话归属由调用方（A3）绑定，不需要模型回填密钥。

use serde_json::{json, Value};

use crate::ai::protocol::{
    METHOD_LIST_DIR, METHOD_LIST_TARGETS, METHOD_READ_FILE, METHOD_READ_TERMINAL,
    METHOD_REMEMBER_HOST, METHOD_RUN_COMMAND, METHOD_STAT, METHOD_WRITE_FILE,
};

use super::types::ToolSpec;

const TARGET_ID_DOC: &str =
    "Target id from list_targets. Omit to use the current focus terminal's server.";

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

/// 工具桥的全部工具。
pub fn bridge_tool_specs() -> Vec<ToolSpec> {
    vec![
        spec(
            METHOD_LIST_TARGETS,
            "List the targets (servers) available in this conversation. The current focus terminal is marked [current]; servers the user added with @ are marked [pinned]. Returns target ids (use them with run_command and read_terminal), host display names, usernames and connection status. Addresses and credentials are never exposed.",
            json!({}),
            &[],
        ),
        spec(
            METHOD_RUN_COMMAND,
            "Run a shell command on an open session and wait for it to finish. Output is captured; each stream is capped at 64KB and truncation is reported. In manual mode the command goes through the user's approval gate first: it can be denied by the user, and approval can time out; in both cases the tool reports the denial instead of output. In auto mode it runs without asking. The reported danger level (safe/dangerous) is a marker only. Returns the exit code, duration and captured stdout/stderr.",
            json!({
                "target_id": {"type": "string", "description": "Target id from list_targets. Omit to use the current focus terminal; to work on another server you MUST use a target id from list_targets."},
                "command": {"type": "string", "description": "Shell command to run on that target."},
                "timeout_sec": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Timeout in seconds (default 60, max 600). On timeout the command keeps running in the terminal."},
            }),
            &["command"],
        ),
        spec(
            METHOD_READ_TERMINAL,
            "Read the most recent terminal output of a target (ANSI escape sequences are removed). Use max_lines to control how many lines to read (default 200, max 2000). Omit target_id to use the current focus terminal.",
            json!({
                "target_id": {"type": "string", "description": "Target id from list_targets. Omit to use the current focus terminal."},
                "max_lines": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "How many most recent lines to read (default 200, max 2000)."},
            }),
            &[],
        ),
        spec(
            METHOD_LIST_DIR,
            "List a directory ON THE SERVER of a target: name, type, size, modification time and permissions for each entry (at most 1000 entries; truncation is reported). The path is a path on the server, not on the user's computer. Omit target_id to use the current focus terminal's server. Read-only: never asks the user for confirmation.",
            json!({
                "target_id": {"type": "string", "description": TARGET_ID_DOC},
                "path": {"type": "string", "description": "Directory path ON THE SERVER (absolute, or relative to the login directory; ~ is the login directory)."},
            }),
            &["path"],
        ),
        spec(
            METHOD_READ_FILE,
            "Read a text file ON THE SERVER of a target. Returns at most 64KB per call; when more remains the result says which offset to pass next so you can read the file in segments. Binary files are reported as binary and their content is not returned. The path is a path on the server, not on the user's computer. Omit target_id to use the current focus terminal's server. Read-only: never asks the user for confirmation.",
            json!({
                "target_id": {"type": "string", "description": TARGET_ID_DOC},
                "path": {"type": "string", "description": "File path ON THE SERVER."},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to start reading from (default 0). Use the next_offset a previous call returned to continue."},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": 65536, "description": "Maximum bytes to return (default and maximum 65536)."},
            }),
            &["path"],
        ),
        spec(
            METHOD_WRITE_FILE,
            "Write a whole text file ON THE SERVER of a target (the file is replaced; max 5 MiB). The path is a path on the server, not on the user's computer. Before overwriting, the original is copied to <name>.zssh-bak-<time> next to it (if that backup fails nothing is written), and the new content is written to a temporary file and renamed into place, so a half-written file is never left behind. In manual mode the user sees a diff and must approve first; writing system paths (/etc, /boot, /usr, authorized_keys, sudoers, crontab ...) is flagged in red. Set create_only to refuse overwriting an existing file. Read the file with read_file first when editing. Omit target_id to use the current focus terminal's server.",
            json!({
                "target_id": {"type": "string", "description": TARGET_ID_DOC},
                "path": {"type": "string", "description": "File path ON THE SERVER."},
                "content": {"type": "string", "description": "The complete new text content of the file (the whole file is replaced, max 5 MiB)."},
                "create_only": {"type": "boolean", "description": "If true, fail when the file already exists (only create new files)."},
            }),
            &["path", "content"],
        ),
        spec(
            METHOD_STAT,
            "Check a path ON THE SERVER of a target: whether it exists, its type (file/dir/symlink/other), size, modification time and permissions. The path is a path on the server, not on the user's computer. Omit target_id to use the current focus terminal's server. Read-only: never asks the user for confirmation.",
            json!({
                "target_id": {"type": "string", "description": TARGET_ID_DOC},
                "path": {"type": "string", "description": "File or directory path ON THE SERVER."},
            }),
            &["path"],
        ),
        spec(
            METHOD_REMEMBER_HOST,
            "Save long-term notes about a host so the next conversation starts with them (they come back in list_targets). Send the COMPLETE updated notes; they replace the old ones. Keep them short and factual: what the host runs, where configs live, problems found and how they were fixed, recent changes, open to-dos. Never store passwords, tokens or keys (values that look like secrets are hidden). Notes stay on the user's computer and the user can view or edit them. Does not touch the server and never asks for confirmation.",
            json!({
                "target_id": {"type": "string", "description": TARGET_ID_DOC},
                "notes": {"type": "string", "description": "The COMPLETE notes for this host (they replace the previous notes; an empty string clears them). Max 4000 characters. Facts only: overview, services, config locations, known issues, recent changes, to-dos. Never include passwords, tokens or keys."},
            }),
            &["notes"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::protocol::{
        PathParams, ReadFileParams, ReadTerminalParams, RememberHostParams, RunCommandParams,
        WriteFileParams,
    };
    use serde::de::DeserializeOwned;

    const MCP_SOURCE: &str = include_str!("../../../../../../zssh-mcp/src/main.rs");

    fn find<'a>(specs: &'a [ToolSpec], name: &str) -> &'a ToolSpec {
        specs
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("缺工具 {name}"))
    }

    /// 按 schema 的 required + 每个属性造一个示例对象（所有属性都填上）。
    fn sample_args(tool: &ToolSpec) -> Value {
        let mut object = serde_json::Map::new();
        for (key, schema) in tool.input_schema["properties"]
            .as_object()
            .into_iter()
            .flatten()
        {
            let value = match schema["type"].as_str() {
                Some("string") => json!("x"),
                Some("integer") => json!(1),
                Some("boolean") => json!(true),
                other => panic!("没处理的类型 {other:?}"),
            };
            object.insert(key.clone(), value);
        }
        Value::Object(object)
    }

    fn assert_fits<P: DeserializeOwned>(tool: &ToolSpec) {
        // 全字段都能被协议结构体认；协议结构体拒绝未知字段的话这里会炸，没拒绝的话下面的
        // 「必填字段缺了就失败」仍能抓到必填漂移。
        let full = sample_args(tool);
        serde_json::from_value::<P>(full.clone())
            .unwrap_or_else(|err| panic!("{} 的参数对不上协议结构体：{err}", tool.name));
        for required in tool.input_schema["required"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let key = required.as_str().unwrap_or_default();
            let mut without = full.clone();
            if let Some(object) = without.as_object_mut() {
                object.remove(key);
            }
            assert!(
                serde_json::from_value::<P>(without).is_err(),
                "{}：schema 标了必填 {key}，但协议结构体不要求",
                tool.name
            );
        }
        // 协议结构体里必填的，schema 里也得标。
        let required: Vec<&str> = tool.input_schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for key in full.as_object().into_iter().flat_map(|o| o.keys()) {
            if required.contains(&key.as_str()) {
                continue;
            }
            let mut without = full.clone();
            if let Some(object) = without.as_object_mut() {
                object.remove(key);
            }
            assert!(
                serde_json::from_value::<P>(without).is_ok(),
                "{}：{key} 在 schema 里是可选的，协议结构体却要求它",
                tool.name
            );
        }
    }

    #[test]
    fn names_match_the_protocol_methods() {
        let specs = bridge_tool_specs();
        let mut names: Vec<&str> = specs.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        let mut expect = vec![
            METHOD_LIST_TARGETS,
            METHOD_RUN_COMMAND,
            METHOD_READ_TERMINAL,
            METHOD_LIST_DIR,
            METHOD_READ_FILE,
            METHOD_WRITE_FILE,
            METHOD_STAT,
            METHOD_REMEMBER_HOST,
        ];
        expect.sort_unstable();
        assert_eq!(names, expect);
    }

    #[test]
    fn every_tool_exists_in_the_mcp_server_too() {
        for tool in bridge_tool_specs() {
            assert!(
                MCP_SOURCE.contains(&format!("async fn {}(", tool.name)),
                "zssh-mcp 里没有工具 {}：清单漂移了",
                tool.name
            );
        }
        // 反过来：MCP 里新增了工具，这里也要跟上。
        let mcp_tools = MCP_SOURCE.matches("#[tool(").count();
        assert_eq!(
            mcp_tools,
            bridge_tool_specs().len(),
            "zssh-mcp 的工具数量变了"
        );
    }

    #[test]
    fn schemas_are_well_formed() {
        for tool in bridge_tool_specs() {
            assert!(!tool.description.is_empty(), "{}", tool.name);
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(tool.input_schema["properties"].is_object(), "{}", tool.name);
            assert!(tool.input_schema["required"].is_array(), "{}", tool.name);
            for required in tool.input_schema["required"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let key = required.as_str().unwrap_or_default();
                assert!(
                    tool.input_schema["properties"].get(key).is_some(),
                    "{}：必填 {key} 不在 properties 里",
                    tool.name
                );
            }
            // 内置助手进程内调用，不让模型填 capability。
            assert!(tool.input_schema["properties"].get("capability").is_none());
            assert!(!tool.description.contains("capability key"));
        }
    }

    #[test]
    fn params_line_up_with_protocol_structs() {
        let specs = bridge_tool_specs();
        assert_fits::<RunCommandParams>(find(&specs, METHOD_RUN_COMMAND));
        assert_fits::<ReadTerminalParams>(find(&specs, METHOD_READ_TERMINAL));
        assert_fits::<PathParams>(find(&specs, METHOD_LIST_DIR));
        assert_fits::<PathParams>(find(&specs, METHOD_STAT));
        assert_fits::<ReadFileParams>(find(&specs, METHOD_READ_FILE));
        assert_fits::<WriteFileParams>(find(&specs, METHOD_WRITE_FILE));
        assert_fits::<RememberHostParams>(find(&specs, METHOD_REMEMBER_HOST));
    }

    #[test]
    fn both_wire_formats_accept_the_specs() {
        use super::super::{anthropic, openai_chat};
        use crate::ai::agent::builtin::provider::types::{ChatRequest, ReasoningParam};
        let request = ChatRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![],
            tools: bridge_tool_specs(),
            max_output_tokens: 1000,
            reasoning: ReasoningParam::Off,
            temperature: None,
            cache_hint: false,
        };
        let openai = openai_chat::build_body(&request, &Default::default());
        assert_eq!(openai["tools"].as_array().map(Vec::len), Some(8));
        assert_eq!(openai["tools"][1]["function"]["name"], METHOD_RUN_COMMAND);
        let claude = anthropic::build_body(&request, &Default::default());
        assert_eq!(claude["tools"].as_array().map(Vec::len), Some(8));
        assert_eq!(
            claude["tools"][1]["input_schema"]["required"],
            json!(["command"])
        );
    }
}
