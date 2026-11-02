//! 假智能体的工具调用（V2-G10b，窗口级自动化 / 联网验收用）：连上工具桥，按 core 的行分隔 JSON 协议
//! 发请求，和 `zssh-mcp.exe` 走的是同一条路——审批、执行位置、终端里执行、Ctrl+C、文件工具、长期记忆
//! 全是真的，只是「发起调用的智能体」是脚本。**不拉任何真实智能体进程，不读任何智能体配置。**
//!
//! 假对话收到 `call [{"tool":"run_command","input":{...}}, ...]` 开头的消息就按顺序发这些调用，每一个
//! 发 `ToolStart` / `ToolEnd` 事件（工具名带 `mcp__zssh__` 前缀，和 Claude Code 一样；结果文本末尾带
//! 后端给的执行编号标记，和 `zssh-mcp` 一样）。结果文本只排版自动化要核对的几项（退出码、执行位置、
//! 备份路径、读到的内容），不求和 `zssh-mcp` 逐字一致。

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use zssh_core::ai::protocol::{EXEC_ID_FIELD, with_exec_marker};

/// 连工具桥需要的三样东西：端口、连接层令牌、这个对话的能力密钥。
#[derive(Clone)]
pub struct FakeBridge {
    pub port: u16,
    pub token: String,
    pub capability: String,
}

impl std::fmt::Debug for FakeBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 令牌和密钥不进日志。
        f.debug_struct("FakeBridge")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// 工具桥的一条连接。
pub struct BridgeClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    capability: String,
    next_id: u64,
}

/// 一次工具调用的结果：成功是 `result` 对象，失败是错误文字；两边都可能带执行编号。
pub struct CallOutcome {
    pub ok: bool,
    pub text: String,
}

impl BridgeClient {
    pub async fn connect(bridge: &FakeBridge) -> Result<Self, String> {
        let stream = TcpStream::connect(("127.0.0.1", bridge.port))
            .await
            .map_err(|err| format!("连不上工具桥：{err}"))?;
        let (reader, mut writer) = stream.into_split();
        let hello = format!("{}\n", json!({ "token": bridge.token }));
        writer
            .write_all(hello.as_bytes())
            .await
            .map_err(|err| format!("工具桥握手失败：{err}"))?;
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            capability: bridge.capability.clone(),
            next_id: 1,
        })
    }

    /// 发一条请求，等对应 `id` 的响应（可能要等人审批，没有超时）。
    pub async fn call(&mut self, method: &str, params: Value) -> Result<CallOutcome, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = format!(
            "{}\n",
            json!({ "id": id, "method": method, "params": params, "capability": self.capability })
        );
        self.writer
            .write_all(request.as_bytes())
            .await
            .map_err(|err| format!("发请求失败：{err}"))?;
        self.writer
            .flush()
            .await
            .map_err(|err| format!("发请求失败：{err}"))?;
        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|err| format!("读响应失败：{err}"))?;
            if read == 0 {
                return Err("工具桥关闭了连接".to_owned());
            }
            let Ok(response) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return Ok(outcome_of(method, &response));
        }
    }
}

fn outcome_of(method: &str, response: &Value) -> CallOutcome {
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        let text = match error.get(EXEC_ID_FIELD).and_then(Value::as_str) {
            Some(exec_id) => with_exec_marker(message, exec_id),
            None => message.to_owned(),
        };
        return CallOutcome { ok: false, text };
    }
    let result = response.get("result").cloned().unwrap_or(Value::Null);
    let text = format_result(method, &result);
    let text = match result.get(EXEC_ID_FIELD).and_then(Value::as_str) {
        Some(exec_id) => with_exec_marker(&text, exec_id),
        None => text,
    };
    CallOutcome { ok: true, text }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// 把工具结果排成文本。只覆盖验收要看的几项，别的方法直接给 JSON。
fn format_result(method: &str, result: &Value) -> String {
    match method {
        "run_command" => {
            let exit = result
                .get("exit_code")
                .and_then(Value::as_u64)
                .map_or("unknown".to_owned(), |code| code.to_string());
            let mut text = format!(
                "danger: {}\nexit_code: {exit}\nlocation: {}\n",
                str_of(result, "danger"),
                str_of(result, "location")
            );
            if let Some(reason) = result.get("fallback_reason").and_then(Value::as_str) {
                text.push_str(&format!("ran in the background because: {reason}\n"));
            }
            text.push_str(&format!("stdout:\n{}\n", str_of(result, "stdout")));
            text
        }
        "read_terminal" => result
            .get("lines")
            .and_then(Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "read_file" => format!(
            "path: {}\nsize: {}\ncontent:\n{}",
            str_of(result, "path"),
            result.get("size").and_then(Value::as_u64).unwrap_or(0),
            str_of(result, "content")
        ),
        "write_file" => {
            let mut text = format!(
                "path: {}\nwritten_bytes: {}\n",
                str_of(result, "path"),
                result
                    .get("bytes_written")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            );
            match result.get("backup_path").and_then(Value::as_str) {
                Some(backup) => text.push_str(&format!(
                    "backup: the previous version was saved on the server at {backup}\n"
                )),
                None => text.push_str("backup: none (new file)\n"),
            }
            text
        }
        "list_dir" => {
            let mut text = format!("path: {}\n", str_of(result, "path"));
            for entry in result
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                text.push_str(&format!(
                    "{} {}\n",
                    str_of(entry, "type"),
                    str_of(entry, "name")
                ));
            }
            text
        }
        "stat" => format!(
            "path: {}\nexists: {}\nsize: {}\n",
            str_of(result, "path"),
            result
                .get("exists")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            result.get("size").and_then(Value::as_u64).unwrap_or(0)
        ),
        _ => result.to_string(),
    }
}

/// 假对话里一次要发的调用。
#[derive(Debug, Clone, PartialEq)]
pub struct FakeCall {
    /// 不带前缀的工具名（`run_command`、`read_file`……）；也是桥上的方法名。
    pub tool: String,
    pub input: Value,
}

/// 解析 `call ...` 消息；不是这种消息返回 `None`，写法不对返回 `Some(Err)`。
///
/// 两种写法：JSON 数组 `call [{"tool":"run_command","input":{...}}, ...]`，或者窗口自动化脚本里
/// 好写的紧凑写法（脚本步骤用 `:` `;` 分隔，写 JSON 不方便）：调用之间用 ` ## ` 隔开，每个调用
/// 是一个动词加参数——
///
/// | 写法 | 工具 |
/// |---|---|
/// | `run <命令>` | `run_command` |
/// | `runt <秒数> <命令>` | `run_command`，带超时 |
/// | `term` | `read_terminal` |
/// | `ls <路径>` / `stat <路径>` / `cat <路径>` | `list_dir` / `stat` / `read_file` |
/// | `write <路径> <内容>` | `write_file`（内容里的 `~` 是换行） |
/// | `remember <笔记>` | `remember_host`（`~` 是换行） |
/// | `targets` | `list_targets` |
///
/// 命令 / 路径里不能有 ` ## `。
pub fn parse_calls(text: &str) -> Option<Result<Vec<FakeCall>, String>> {
    let rest = text.strip_prefix("call ")?.trim();
    if rest.starts_with('[') {
        return Some(parse_json_calls(rest));
    }
    Some(rest.split(" ## ").map(parse_compact_call).collect())
}

fn parse_json_calls(rest: &str) -> Result<Vec<FakeCall>, String> {
    let items: Vec<Value> =
        serde_json::from_str(rest).map_err(|err| format!("call 后面要跟 JSON 数组：{err}"))?;
    items
        .into_iter()
        .map(|item| {
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .ok_or("每个调用要有 tool")?
                .to_owned();
            Ok(FakeCall {
                tool,
                input: item.get("input").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

fn parse_compact_call(item: &str) -> Result<FakeCall, String> {
    let item = item.trim();
    let (verb, arg) = item
        .split_once(' ')
        .map_or((item, ""), |(verb, arg)| (verb, arg.trim()));
    let call = |tool: &str, input: Value| FakeCall {
        tool: tool.to_owned(),
        input,
    };
    let need = |what: &str| {
        if arg.is_empty() {
            Err(format!("{verb} 后面要写{what}"))
        } else {
            Ok(arg)
        }
    };
    match verb {
        "run" => Ok(call("run_command", json!({ "command": need("命令")? }))),
        "runt" => {
            let (secs, command) = need("秒数和命令")?
                .split_once(' ')
                .ok_or("runt 后面要写 <秒数> <命令>")?;
            let secs: u64 = secs.parse().map_err(|_| "runt 的秒数不是数字".to_owned())?;
            Ok(call(
                "run_command",
                json!({ "command": command.trim(), "timeout_sec": secs }),
            ))
        }
        "term" => Ok(call("read_terminal", json!({ "max_lines": 30 }))),
        "ls" => Ok(call("list_dir", json!({ "path": need("路径")? }))),
        "stat" => Ok(call("stat", json!({ "path": need("路径")? }))),
        "cat" => Ok(call("read_file", json!({ "path": need("路径")? }))),
        "write" => {
            let (path, content) = need("路径和内容")?
                .split_once(' ')
                .ok_or("write 后面要写 <路径> <内容>")?;
            Ok(call(
                "write_file",
                json!({ "path": path, "content": content.replace('~', "\n") }),
            ))
        }
        "remember" => Ok(call(
            "remember_host",
            json!({ "notes": need("笔记")?.replace('~', "\n") }),
        )),
        "targets" => Ok(call("list_targets", json!({}))),
        other => Err(format!("不认识的调用：{other}")),
    }
}

/// 智能体给工具名加的前缀（Claude Code 的写法）。
pub const TOOL_PREFIX: &str = "mcp__zssh__";

/// 假调用之间稍微停一下，窗口里能看见「正在执行」。
pub const CALL_GAP: Duration = Duration::from_millis(120);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_messages_parse_into_tool_calls() {
        let calls = parse_calls(
            r#"call [{"tool":"run_command","input":{"command":"uptime"}},{"tool":"stat","input":{"path":"/tmp"}}]"#,
        )
        .expect("是 call 消息")
        .expect("JSON 对");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool, "run_command");
        assert_eq!(calls[0].input["command"], "uptime");
        let compact = parse_calls(
            "call run echo hi ## runt 5 sleep 3 ## term ## write /tmp/a.txt x~y ## ls /tmp ## targets",
        )
        .expect("是 call 消息")
        .expect("写法对");
        assert_eq!(compact.len(), 6);
        assert_eq!(compact[0].input["command"], "echo hi");
        assert_eq!(compact[1].input["timeout_sec"], 5);
        assert_eq!(compact[1].input["command"], "sleep 3");
        assert_eq!(compact[3].input["content"], "x\ny", "~ 是换行");
        assert_eq!(compact[5].tool, "list_targets");
        assert!(parse_calls("call fly away").expect("是 call 消息").is_err());
        assert!(parse_calls("call run").expect("是 call 消息").is_err());
        assert!(parse_calls("你好").is_none(), "普通消息不是 call");
        assert!(
            parse_calls("call {not json")
                .expect("是 call 消息")
                .is_err()
        );
        assert!(
            parse_calls(r#"call [{"input":{}}]"#)
                .expect("是 call 消息")
                .is_err(),
            "没有 tool 的调用要报错"
        );
    }

    #[test]
    fn results_carry_the_exec_marker_the_card_pairs_on() {
        let ok = outcome_of(
            "run_command",
            &json!({"result": {"exit_code": 0, "location": "terminal", "stdout": "hi",
                               "danger": "safe", "exec_id": "ab-1"}, "id": 1}),
        );
        assert!(ok.ok);
        assert!(ok.text.contains("exit_code: 0") && ok.text.contains("location: terminal"));
        assert!(ok.text.ends_with("[zssh exec ab-1]"), "{}", ok.text);
        let failed = outcome_of(
            "run_command",
            &json!({"error": {"message": "用户拒绝", "exec_id": "ab-2"}, "id": 1}),
        );
        assert!(!failed.ok);
        assert!(failed.text.starts_with("用户拒绝") && failed.text.ends_with("[zssh exec ab-2]"));
    }
}
