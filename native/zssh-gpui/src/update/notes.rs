//! 更新说明（`latest.json` 里的 `notes`，Markdown）显示前的处理。
//!
//! 说明来自网络（虽然只来自发布页），所以渲染前先做三件事，再交给和 AI 面板同一个 `TextView`：
//! - **不执行 HTML**：HTML 块 / 行内 HTML 一律改成字面文字（注释直接丢掉）；
//! - **不加载图片**：Markdown 图片换成它的替代文字（`TextView` 那边还有一道 `image_source`，
//!   把任何图片地址都指向不存在的内嵌资源）；
//! - **链接只放行 `https://`**：`http://`、`mailto:`、`javascript:`、`file:` 等链接换成纯文字，
//!   文字里的标点全部转义，免得又被识别成"裸网址 / 邮箱"自动链接。
//!
//! 点击时还有第二道关：[`link_target`] 只认 https，并且要过 core 的 `validate_external_url`
//! （限长、拒空白和控制字符、要有主机名），再用系统浏览器打开规范化后的网址。

use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};

use super::UpdateMsg;
use crate::agent_text::markdown_fence;
use crate::i18n::t;

/// 清单里的说明去掉首尾空白；全是空白算没有。
pub(super) fn notes_text(notes: Option<&str>) -> Option<String> {
    notes
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// 窗口里真正交给 `TextView` 的 Markdown：说明去掉首尾空白，再做安全改写；没有说明返回 `None`
/// （界面显示"这个版本没有附带更新说明"）。
pub(super) fn display_markdown(notes: Option<&str>) -> Option<String> {
    notes_text(notes).map(|text| sanitize_markdown(&text))
}

/// 点链接时真正要打开的地址：只认 `https://`，且要过 `validate_external_url`；其余一律 `None`。
pub(super) fn link_target(raw: &str) -> Option<String> {
    let url = zssh_core::external_url::validate_external_url(raw.trim()).ok()?;
    (url.scheme() == "https").then(|| url.as_str().to_owned())
}

/// 和 `TextView` 解析时用的同一套构造，只关掉数学公式（`$5 和 $10` 不该被当成公式吞掉标记）。
fn parse_options() -> ParseOptions {
    let mut options = ParseOptions::gfm();
    options.constructs.math_text = false;
    options.constructs.math_flow = false;
    options
}

/// 一处改写：把源码里 `start..end` 这段换成 `text`。
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// 把更新说明改写成"安全的 Markdown"：没有 HTML、没有图片、没有非 https 链接。
///
/// 代码块、行内代码里的内容原样保留（它们本来就只是文字）。解析失败（不应该发生）时整段放进代码围栏，
/// 当纯文字显示。
pub(super) fn sanitize_markdown(source: &str) -> String {
    let Ok(root) = to_mdast(source, &parse_options()) else {
        return markdown_fence(source);
    };
    let mut edits = Vec::new();
    collect_edits(&root, &mut edits);
    edits.sort_by_key(|edit| edit.start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for edit in edits {
        // 改写区间不会重叠（命中的节点不再往下找）；防御一下，真重叠就跳过后一个。
        if edit.start < cursor || edit.end > source.len() || edit.start > edit.end {
            continue;
        }
        out.push_str(&source[cursor..edit.start]);
        out.push_str(&edit.text);
        cursor = edit.end;
    }
    out.push_str(&source[cursor..]);
    out
}

fn collect_edits(node: &Node, edits: &mut Vec<Edit>) {
    let replacement = match node {
        Node::Html(html) => {
            let value = html.value.trim();
            // 注释不显示；其他标签原样当文字显示（不渲染）。
            let text = if value.starts_with("<!--") && value.ends_with("-->") {
                String::new()
            } else {
                escape_punctuation(&html.value)
            };
            Some((node.position(), text))
        }
        Node::Image(image) => Some((node.position(), image_text(&image.alt))),
        Node::ImageReference(image) => Some((node.position(), image_text(&image.alt))),
        Node::Link(link) if link_target(&link.url).is_none() => {
            let mut text = String::new();
            plain_text(node, &mut text);
            if text.trim().is_empty() {
                text = link.url.clone();
            }
            Some((node.position(), escape_punctuation(&text)))
        }
        _ => None,
    };
    if let Some((Some(position), text)) = replacement {
        edits.push(Edit {
            start: position.start.offset,
            end: position.end.offset,
            text,
        });
        return;
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_edits(child, edits);
        }
    }
}

/// 图片换成替代文字；没有替代文字就写"（图片）"。
fn image_text(alt: &str) -> String {
    let alt = alt.trim();
    if alt.is_empty() {
        t(UpdateMsg::ImagePlaceholder).to_owned()
    } else {
        escape_punctuation(alt)
    }
}

/// 一个节点底下所有看得见的文字（图片取替代文字）。
fn plain_text(node: &Node, out: &mut String) {
    match node {
        Node::Text(text) => out.push_str(&text.value),
        Node::InlineCode(code) => out.push_str(&code.value),
        Node::Image(image) => out.push_str(&image.alt),
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    plain_text(child, out);
                }
            }
        }
    }
}

/// 给每个 ASCII 标点前加反斜杠：CommonMark 里"反斜杠 + ASCII 标点"永远是字面字符，
/// 所以结果既不会被当成标记，也不会再被识别成自动链接（`http://`、`www.`、邮箱都会被打断）。
fn escape_punctuation(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for ch in text.chars() {
        if ch.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::UpdateMsg;
    use super::{link_target, notes_text, parse_options, sanitize_markdown};
    use crate::i18n::t;
    use markdown::mdast::Node;

    /// 解析后还剩多少"危险节点"：HTML、图片、非 https 链接。
    fn dangerous(markdown: &str) -> Vec<String> {
        fn walk(node: &Node, found: &mut Vec<String>) {
            match node {
                Node::Html(html) => found.push(format!("html:{}", html.value)),
                Node::Image(image) => found.push(format!("image:{}", image.url)),
                Node::ImageReference(image) => found.push(format!("image-ref:{}", image.alt)),
                Node::Link(link) if link_target(&link.url).is_none() => {
                    found.push(format!("link:{}", link.url));
                }
                _ => {}
            }
            if let Some(children) = node.children() {
                for child in children {
                    walk(child, found);
                }
            }
        }
        let root = markdown::to_mdast(markdown, &parse_options()).expect("解析");
        let mut found = Vec::new();
        walk(&root, &mut found);
        found
    }

    /// 清洗前确实有危险节点、清洗后一个都没有。
    fn assert_neutralised(markdown: &str) {
        assert!(
            !dangerous(markdown).is_empty(),
            "测试样本本身就该是危险的：{markdown}"
        );
        let clean = sanitize_markdown(markdown);
        assert_eq!(dangerous(&clean), Vec::<String>::new(), "清洗后：{clean}");
        // 再清洗一遍结果不变：已经安全的文字不会被反复改写。
        assert_eq!(sanitize_markdown(&clean), clean);
    }

    #[test]
    fn html_is_never_executed() {
        for sample in [
            "<script>alert(1)</script>",
            "前面\n\n<div onclick=\"x()\">块</div>\n\n后面",
            "行内 <b>粗</b> 和 <img src=x onerror=alert(1)> 混在一起",
            "<iframe src=\"https://example.com\"></iframe>",
            "<a href=\"javascript:alert(1)\">点我</a>",
            "- 列表里 <span style=\"color:red\">红</span>\n  - 嵌套 <br> 换行",
            "| 表头 |\n| --- |\n| <u>单元格</u> |",
        ] {
            assert_neutralised(sample);
        }
    }

    #[test]
    fn html_comments_are_dropped_and_other_tags_are_shown_as_text() {
        assert_eq!(sanitize_markdown("甲<!-- 秘密 -->乙"), "甲乙");
        let clean = sanitize_markdown("<b>粗</b>");
        assert!(clean.contains("粗"));
        assert!(clean.contains("b"), "标签名还在，只是不再是标签：{clean}");
    }

    #[test]
    fn images_never_load_and_show_their_alt_text() {
        for sample in [
            "![截图](https://example.com/a.png)",
            "![](https://example.com/a.png)",
            "![远程][ref]\n\n[ref]: https://example.com/a.png",
            "[![徽章](https://example.com/b.svg)](https://example.com)",
            "![本地](file:///C:/secret.png)",
            "![内嵌](data:image/png;base64,AAAA)",
        ] {
            assert_neutralised(sample);
        }
        assert_eq!(
            sanitize_markdown("看 ![界面截图](https://example.com/a.png) 这个"),
            "看 界面截图 这个"
        );
        assert!(sanitize_markdown("![](https://example.com/a.png)").contains("图片"));
    }

    #[test]
    fn only_https_links_stay_links() {
        for sample in [
            "[官网](http://example.com)",
            "[脚本](javascript:alert(1))",
            "[本地](file:///C:/Windows/system32/calc.exe)",
            "[邮件](mailto:someone@example.com)",
            "[FTP](ftp://example.com/a)",
            "<http://example.com>",
            "直接写网址 http://example.com/a 也不行",
            "www.example.com 也不行",
            "someone@example.com 也不行",
            "[http://a.example](http://b.example)",
            "[相对路径](./readme.md)",
        ] {
            assert_neutralised(sample);
        }
        // https 链接原样保留（含裸网址）。
        let keep = "[官网](https://example.com/a?b=1) 和 https://example.com/c";
        assert_eq!(sanitize_markdown(keep), keep);
    }

    #[test]
    fn link_text_survives_when_the_link_is_removed() {
        let clean = sanitize_markdown("请看 [官方 说明](http://example.com/a)。");
        assert!(clean.contains("官方 说明"), "{clean}");
        assert!(!clean.contains("](http"), "{clean}");
    }

    #[test]
    fn code_is_left_alone() {
        let code = "```html\n<script>alert(1)</script>\n![x](http://e.com/a.png)\n```\n\n行内 `<b>` 和 `[a](http://x)`。";
        assert_eq!(sanitize_markdown(code), code);
        assert_eq!(dangerous(code), Vec::<String>::new());
    }

    #[test]
    fn ordinary_release_notes_pass_through_unchanged() {
        let notes = "一句话概述。\n\n### 新功能\n\n- 在线更新：**自动检查**，`下载后校验`。\n  - 嵌套一条\n- 第二条\n\n---\n\n| 项 | 说明 |\n| --- | --- |\n| 甲 | 乙 |\n\n```text\n代码\n```\n";
        assert_eq!(sanitize_markdown(notes), notes);
    }

    #[test]
    fn link_click_only_opens_https() {
        assert_eq!(
            link_target("https://example.com/a?b=1#c").as_deref(),
            Some("https://example.com/a?b=1#c")
        );
        assert_eq!(
            link_target(" HTTPS://Example.COM ").as_deref(),
            Some("https://example.com/")
        );
        for bad in [
            "http://example.com",
            "file:///C:/Windows/system32/calc.exe",
            "javascript:alert(1)",
            "mailto:someone@example.com",
            "ms-settings:network",
            "ftp://example.com",
            "C:\\Windows\\system32\\calc.exe",
            "//example.com/a",
            "https://",
            "https://example.com/a b",
            "",
        ] {
            assert_eq!(link_target(bad), None, "{bad}");
        }
        let long = format!("https://example.com/{}", "a".repeat(3000));
        assert_eq!(link_target(&long), None);
    }

    #[test]
    fn empty_notes_are_detected() {
        assert_eq!(notes_text(None), None);
        assert_eq!(notes_text(Some("")), None);
        assert_eq!(notes_text(Some(" \r\n\t ")), None);
        assert_eq!(notes_text(Some("  好  ")).as_deref(), Some("好"));
        assert!(t(UpdateMsg::EmptyNotes).contains("没有附带更新说明"));
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for sample in [
            "",
            "<",
            "![",
            "[a](",
            "[a](<http://x>)",
            "\u{feff}<b>",
            "🙂 <b>表情</b> ![😀](http://x)",
            "<<<<>>>>",
            "[[[]]]((()))",
        ] {
            let _ = sanitize_markdown(sample);
        }
    }
}
