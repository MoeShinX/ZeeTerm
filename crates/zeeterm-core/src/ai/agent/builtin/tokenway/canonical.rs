//! 预设清单签名用的规范化 JSON（V2-A5）。必须和 Tokenway 侧 `scripts/zeeterm-presets.mjs` 的
//! `canonicalJson` 逐字节一致：
//!
//! - 对象的键按 JS 默认 `sort()` 的顺序（UTF-16 码元顺序，不是 UTF-8 / 码点顺序）递归排好；
//! - 数组保持原顺序，不留任何空白；
//! - 字符串按 `JSON.stringify` 输出：只转义 `"`、`\`、U+0000–U+001F（`\b \t \n \f \r` 用短写，
//!   其余写成小写 `\u00xx`），其它字符（含 U+007F、U+2028/2029、emoji）原样输出；
//! - 数字按 `JSON.stringify` 输出。JS 的数字都是 f64，写法和 Rust 的 ryu 不一样（`1.0` → `1`、
//!   `1e21` → `1e+21`），所以这里**只放行安全整数范围内的整数**（含 `1.0` 这种整数值的浮点），
//!   别的数字直接拒绝——宁可验签失败退回内置清单，也不能两边算出不同的字节。

use serde_json::Value;

/// 超出这个绝对值的整数，JS 的 f64 已经装不下，不同实现写法会不一致。
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalError {
    #[error("清单里有无法稳定规范化的数字：{0}")]
    Number(String),
}

/// 规范化。
pub fn canonical_json(value: &Value) -> Result<String, CanonicalError> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number_text(number)?),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                if let Some(item) = map.get(key) {
                    write_value(item, out)?;
                }
            }
            out.push('}');
        }
    }
    Ok(())
}

fn number_text(number: &serde_json::Number) -> Result<String, CanonicalError> {
    if let Some(n) = number.as_i64() {
        if n.abs() <= MAX_SAFE_INTEGER {
            return Ok(n.to_string());
        }
        return Err(CanonicalError::Number(number.to_string()));
    }
    if let Some(n) = number.as_u64() {
        if n <= MAX_SAFE_INTEGER as u64 {
            return Ok(n.to_string());
        }
        return Err(CanonicalError::Number(number.to_string()));
    }
    // 浮点：只认值恰好是安全整数的（`1.0` → `1`，`-0.0` → `0`，和 JS 一致）。
    match number.as_f64() {
        Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64 => {
            Ok(format!("{}", f as i64))
        }
        _ => Err(CanonicalError::Number(number.to_string())),
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{9}' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_by_utf16_code_unit_not_by_code_point() {
        // U+FF71（半角片假名）> U+D83D（emoji 的高代理项）：JS 排序里 emoji 在前；
        // 按码点或 UTF-8 排序则相反。
        let value: Value =
            serde_json::from_str(r#"{"ｱ":1,"😀":2,"中":3,"Z":4,"a":5,"é":6}"#).unwrap();
        assert_eq!(
            canonical_json(&value).unwrap(),
            r#"{"Z":4,"a":5,"é":6,"中":3,"😀":2,"ｱ":1}"#
        );
    }

    #[test]
    fn strings_are_escaped_like_json_stringify() {
        let value =
            json!("q\" b\\ n\n t\t r\r b\u{8} f\u{c} c\u{1} d\u{1f} del\u{7f} ls\u{2028} 😀 é <>&");
        assert_eq!(
            canonical_json(&value).unwrap(),
            "\"q\\\" b\\\\ n\\n t\\t r\\r b\\b f\\f c\\u0001 d\\u001f del\u{7f} ls\u{2028} 😀 é <>&\""
        );
    }

    #[test]
    fn integers_print_plainly_and_whole_floats_collapse_to_integers() {
        let value: Value = serde_json::from_str("[0,-3,1.0,-0.0,9007199254740991,2.0e3]").unwrap();
        assert_eq!(
            canonical_json(&value).unwrap(),
            "[0,-3,1,0,9007199254740991,2000]"
        );
    }

    #[test]
    fn numbers_that_could_print_differently_in_js_are_refused() {
        for text in [
            "1.5",
            "1e21",
            "9007199254740993",
            "-9007199254740993",
            "0.1",
            "1e-7",
        ] {
            let value: Value = serde_json::from_str(text).unwrap();
            assert!(canonical_json(&value).is_err(), "{text} 应当被拒绝");
        }
    }

    #[test]
    fn arrays_keep_order_and_nesting_has_no_whitespace() {
        let value = json!({"b": [3, {"y": null, "x": true}, []], "a": {}});
        assert_eq!(
            canonical_json(&value).unwrap(),
            r#"{"a":{},"b":[3,{"x":true,"y":null},[]]}"#
        );
    }
}
