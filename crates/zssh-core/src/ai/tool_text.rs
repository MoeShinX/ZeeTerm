//! 工具桥结果的文本排版：把桥返回的 JSON 结果排成给模型看的文本。
//!
//! 外部智能体经 `zssh-mcp` 拿到的、内置助手进程内拿到的是同一份文本（同一份代码，措辞和
//! 字段不会分叉）。结果末尾的执行编号标记见 [`crate::ai::protocol::with_exec_marker`]。

use serde_json::Value;

use crate::ai::protocol::{with_exec_marker, EXEC_ID_FIELD};

/// 把 `list_targets` 的结果排版成给模型看的文本。
pub fn format_sessions(result: &Value) -> String {
    let targets = result.get("targets").and_then(Value::as_array);
    let Some(targets) = targets else {
        return "No targets are available in this Z-SSH conversation.".into();
    };
    if targets.is_empty() {
        return "No targets are available in this Z-SSH conversation.".into();
    }
    let mut lines = vec![format!("{} target(s) in this conversation:", targets.len())];
    for target in targets {
        let marker = if target.get("current").and_then(Value::as_bool) == Some(true) {
            " [current]"
        } else if target.get("pinned").and_then(Value::as_bool) == Some(true) {
            " [pinned]"
        } else {
            ""
        };
        lines.push(format!(
            "- id: {} | host: {} | user: {} | status: {}{}",
            str_field(target, "id"),
            str_field(target, "host_label"),
            str_field(target, "username"),
            str_field(target, "status"),
            marker,
        ));
        if let Some(memory) = target.get("memory").and_then(Value::as_str) {
            for line in memory.lines() {
                lines.push(format!("    | {line}"));
            }
        }
    }
    if let Some(note) = result.get("memory_note").and_then(Value::as_str) {
        lines.push(note.to_owned());
    }
    lines.join("\n")
}

pub fn format_remember(result: &Value) -> String {
    format!(
        "Notes saved ({} / {} characters).",
        result
            .get("saved_chars")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        result.get("max_chars").and_then(Value::as_u64).unwrap_or(0)
    )
}

/// 把 `run_command` 的结果排版成给模型看的文本。
pub fn format_run_result(result: &Value) -> String {
    let exit_code = match result.get("exit_code").and_then(Value::as_u64) {
        Some(code) => code.to_string(),
        None => "unknown".into(),
    };
    let danger = str_field(result, "danger");
    let duration_ms = u64_field(result, "duration_ms");
    let location = str_field(result, "location");
    let timed_out = result
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");

    let mut text =
        format!("danger: {danger}\nexit_code: {exit_code}\nduration_ms: {duration_ms}\n");
    text.push_str(&format!("location: {location}\n"));
    if let Some(reason) = result.get("fallback_reason").and_then(Value::as_str) {
        text.push_str(&format!("ran in the background because: {reason}\n"));
    }
    if timed_out {
        text.push_str(
            "timed out: the command is still running in the terminal; the output below is \
             what was captured so far.\n",
        );
    }
    if stdout.is_empty() {
        text.push_str("stdout: (empty)\n");
    } else {
        text.push_str("stdout:\n");
        text.push_str(stdout);
        if !stdout.ends_with('\n') {
            text.push('\n');
        }
    }
    if stderr.is_empty() {
        text.push_str("stderr: (empty)\n");
    } else {
        text.push_str("stderr:\n");
        text.push_str(stderr);
        if !stderr.ends_with('\n') {
            text.push('\n');
        }
    }

    let stdout_truncated = u64_field(result, "stdout_truncated_bytes");
    if stdout_truncated > 0 {
        text.push_str(&format!(
            "stdout truncated: {stdout_truncated} bytes dropped\n"
        ));
    }
    let stderr_truncated = u64_field(result, "stderr_truncated_bytes");
    if stderr_truncated > 0 {
        text.push_str(&format!(
            "stderr truncated: {stderr_truncated} bytes dropped\n"
        ));
    }
    text
}

/// 把 `read_terminal` 的结果排版成给模型看的文本。
pub fn format_terminal_lines(result: &Value) -> String {
    match result.get("lines").and_then(Value::as_array) {
        Some(lines) if !lines.is_empty() => lines
            .iter()
            .map(|line| line.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "The terminal buffer is empty.".into(),
    }
}

/// 把 `list_dir` 的结果排版成给模型看的文本。
pub fn format_list_dir(result: &Value) -> String {
    let mut text = format!("path: {}\n", str_field(result, "path"));
    let Some(entries) = result.get("entries").and_then(Value::as_array) else {
        text.push_str("(no entries)\n");
        return text;
    };
    let total = u64_field(result, "total");
    text.push_str(&format!("entries: {} of {}\n", entries.len(), total));
    if result.get("truncated").and_then(Value::as_bool) == Some(true) {
        text.push_str(&format!(
            "truncated: only the first {} entries are listed ({} more not shown)\n",
            entries.len(),
            total.saturating_sub(entries.len() as u64)
        ));
    }
    for entry in entries {
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into());
        let link = entry
            .get("link_target")
            .and_then(Value::as_str)
            .map(|t| format!(" -> {t}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "{} {} {} {} {}{}\n",
            str_field(entry, "type"),
            str_field(entry, "mode"),
            size,
            entry.get("modified").and_then(Value::as_str).unwrap_or("-"),
            str_field(entry, "name"),
            link,
        ));
    }
    text
}

/// 把 `stat` 的结果排版成给模型看的文本。
pub fn format_stat(result: &Value) -> String {
    let path = str_field(result, "path");
    if result.get("exists").and_then(Value::as_bool) != Some(true) {
        return format!("path: {path}\nexists: false\n");
    }
    format!(
        "path: {path}\nexists: true\ntype: {}\nsize: {}\nmodified: {}\nmode: {}\n",
        str_field(result, "type"),
        u64_field(result, "size"),
        result
            .get("modified")
            .and_then(Value::as_str)
            .unwrap_or("-"),
        str_field(result, "mode"),
    )
}

/// 把 `read_file` 的结果排版成给模型看的文本。
pub fn format_read_file(result: &Value) -> String {
    let path = str_field(result, "path");
    if result.get("binary").and_then(Value::as_bool) == Some(true) {
        return format!(
            "path: {path}\nThis is a binary file (not valid UTF-8 text); its content is not returned.\nsize: {}\n",
            u64_field(result, "size")
        );
    }
    let mut text = format!(
        "path: {path}\nsize: {}\noffset: {}\nreturned_bytes: {}\n",
        u64_field(result, "size"),
        u64_field(result, "offset"),
        u64_field(result, "returned_bytes"),
    );
    match result.get("next_offset").and_then(Value::as_u64) {
        Some(next) => text.push_str(&format!(
            "more: yes, call read_file again with offset={next} to continue\n"
        )),
        None => text.push_str("more: no (end of file)\n"),
    }
    text.push_str("content:\n");
    text.push_str(result.get("content").and_then(Value::as_str).unwrap_or(""));
    text
}

/// 把 `write_file` 的结果排版成给模型看的文本。
pub fn format_write_file(result: &Value) -> String {
    let mut text = format!(
        "path: {}\nwritten_bytes: {}\n{}\n",
        str_field(result, "path"),
        u64_field(result, "bytes_written"),
        if result.get("created").and_then(Value::as_bool) == Some(true) {
            "created: a new file"
        } else {
            "replaced: the existing file was overwritten"
        }
    );
    match result.get("backup_path").and_then(Value::as_str) {
        Some(backup) => text.push_str(&format!(
            "backup: the previous version was saved on the server at {backup}\n"
        )),
        None => text.push_str("backup: none (new file)\n"),
    }
    if let Some(change) = result.get("owner_change") {
        text.push_str(&format!(
            "owner: could not restore the file's owner: it changed from {} to {}{}\n",
            str_field(change, "from"),
            str_field(change, "to"),
            change
                .get("error")
                .and_then(Value::as_str)
                .map(|err| format!(" ({err})"))
                .unwrap_or_default(),
        ));
    }
    if let Some(note) = result.get("retention_note").and_then(Value::as_str) {
        text.push_str(&format!("backups: {note}\n"));
    }
    if result.get("dangerous").and_then(Value::as_bool) == Some(true) {
        let reasons = result
            .get("danger_reasons")
            .and_then(Value::as_array)
            .map(|r| {
                r.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        text.push_str(&format!("danger: flagged as a system path ({reasons})\n"));
    }
    text
}

/// 工具结果文本末尾加上桥给这次调用的执行编号（一行固定格式的标记）。界面靠它把结果对回
/// 执行层回报的那次执行（AI-SCOPE-3）；桥没给编号（`list_targets` 之类）就原样返回。
pub fn with_call_marker(text: String, result: &Value) -> String {
    match result.get(EXEC_ID_FIELD).and_then(Value::as_str) {
        Some(exec_id) if !exec_id.is_empty() => with_exec_marker(&text, exec_id),
        _ => text,
    }
}

pub fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("?")
}

pub fn u64_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// 按工具名把桥返回的结果排成文本（带执行编号标记）。内置助手进程内调用工具桥后用它；
/// 外部智能体那边 `zssh-mcp` 对每个工具调用对应的 `format_*`，两边文字一致。
pub fn format_tool_result(method: &str, result: &Value) -> String {
    use crate::ai::protocol::{
        METHOD_LIST_DIR, METHOD_LIST_TARGETS, METHOD_READ_FILE, METHOD_READ_TERMINAL,
        METHOD_REMEMBER_HOST, METHOD_RUN_COMMAND, METHOD_STAT, METHOD_WRITE_FILE,
    };
    let text = match method {
        METHOD_LIST_TARGETS => format_sessions(result),
        METHOD_REMEMBER_HOST => format_remember(result),
        METHOD_RUN_COMMAND => format_run_result(result),
        METHOD_READ_TERMINAL => format_terminal_lines(result),
        METHOD_LIST_DIR => format_list_dir(result),
        METHOD_READ_FILE => format_read_file(result),
        METHOD_WRITE_FILE => format_write_file(result),
        METHOD_STAT => format_stat(result),
        // 不认识的方法不会走到这里（桥先拒绝了）；兜底给 JSON，免得结果丢掉。
        _ => result.to_string(),
    };
    // list_targets / remember_host 桥不给执行编号，with_call_marker 原样返回。
    with_call_marker(text, result)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ai::protocol::{METHOD_RUN_COMMAND, METHOD_STAT};

    #[test]
    fn dispatcher_matches_the_individual_formatters_and_adds_the_marker() {
        let result = json!({
            "stdout": "hi\n", "stderr": "", "stdout_truncated_bytes": 0,
            "stderr_truncated_bytes": 0, "exit_code": 0, "duration_ms": 3,
            "danger": "safe", "location": "background", "timed_out": false,
            "exec_id": "ab12-3",
        });
        let text = format_tool_result(METHOD_RUN_COMMAND, &result);
        assert_eq!(text, with_call_marker(format_run_result(&result), &result));
        assert!(text.ends_with("[zssh exec ab12-3]"), "{text}");
    }

    #[test]
    fn results_without_an_exec_id_have_no_marker() {
        let result = json!({"path": "/tmp", "exists": false});
        let text = format_tool_result(METHOD_STAT, &result);
        assert_eq!(text, "path: /tmp\nexists: false\n");
        assert_eq!(
            format_tool_result("list_targets", &json!({"targets": []})),
            "No targets are available in this Z-SSH conversation."
        );
    }
}
