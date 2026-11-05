//! 语言判断：文件名 / 路径 → 高亮语言，认不出再看第一行的 shebang。
//!
//! 照 0.2.14 的 `ui/src/state/editorLanguage.ts`（映射表）和 `components/editor/loadLanguage.ts`
//! （注释符号：语言包自带的，加上 nginx / properties 补上的 `#`，纯文本也给 `#`）。
//! 高亮本身是 gpui-kit 的 syntect 后端，这里只负责把 0.2.14 的语言 id 翻成它认的名字。

/// 0.2.14 的语言 id（`LanguageId`）加纯文本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Text,
    Shell,
    Properties,
    Yaml,
    Json,
    Xml,
    Html,
    Nginx,
    Python,
    JavaScript,
    TypeScript,
    Sql,
    Dockerfile,
    Markdown,
    Toml,
    Css,
    // 下面几个不在 0.2.14 的映射表里：F3 的原生编辑器本来就给这些扩展名上色，保留，不缩减。
    Rust,
    C,
    Cpp,
    Go,
    Java,
}

/// 注释符号：行注释优先；没有行注释的语言用块注释。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comment {
    Line(&'static str),
    Block(&'static str, &'static str),
    None,
}

impl Lang {
    /// 状态栏上的名字（同 0.2.14 `LANGUAGE_NAMES`）。
    pub fn display(self) -> &'static str {
        match self {
            Self::Text => "纯文本",
            Self::Shell => "Shell",
            Self::Properties => "配置文件",
            Self::Yaml => "YAML",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Html => "HTML",
            Self::Nginx => "Nginx",
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Sql => "SQL",
            Self::Dockerfile => "Dockerfile",
            Self::Markdown => "Markdown",
            Self::Toml => "TOML",
            Self::Css => "CSS",
            Self::Rust => "Rust",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Go => "Go",
            Self::Java => "Java",
        }
    }

    /// gpui-kit（syntect + bat 的语法集）认的名字；纯文本用 `text`（不高亮）。
    ///
    /// 这些名字都拿同一份语法集（two-face 0.4.5 的 extra 集合）逐个 `find_syntax_by_token`
    /// 实测过，全部命中。
    pub fn syntax_token(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Shell => "bash",
            // 「配置文件」一组（.conf / .ini / .service / .env 等）：INI 语法最接近，`#` 和 `;` 注释都认。
            Self::Properties => "ini",
            Self::Yaml => "yaml",
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Html => "html",
            Self::Nginx => "nginx",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Sql => "sql",
            Self::Dockerfile => "dockerfile",
            Self::Markdown => "markdown",
            Self::Toml => "toml",
            Self::Css => "css",
            Self::Rust => "rust",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Go => "go",
            Self::Java => "java",
        }
    }

    /// Ctrl+/ 用的注释符号（对照 0.2.14 各语言包自带的 `commentTokens`，纯文本 / nginx / properties 是 `#`）。
    pub fn comment(self) -> Comment {
        match self {
            Self::Text
            | Self::Shell
            | Self::Properties
            | Self::Yaml
            | Self::Nginx
            | Self::Python
            | Self::Dockerfile
            | Self::Toml => Comment::Line("#"),
            Self::JavaScript
            | Self::TypeScript
            | Self::Rust
            | Self::C
            | Self::Cpp
            | Self::Go
            | Self::Java => Comment::Line("//"),
            Self::Sql => Comment::Line("--"),
            Self::Css => Comment::Block("/*", "*/"),
            Self::Xml | Self::Html | Self::Markdown => Comment::Block("<!--", "-->"),
            // JSON 没有注释；CodeMirror 的 json 包也没声明注释符号，Ctrl+/ 什么都不做。
            Self::Json => Comment::None,
        }
    }
}

/// 按扩展名认语言（同 `BY_EXTENSION`）。
fn by_extension(ext: &str) -> Option<Lang> {
    Some(match ext {
        "sh" | "bash" | "zsh" | "ksh" | "fish" => Lang::Shell,
        "conf" | "ini" | "cfg" | "cnf" | "properties" | "env" | "service" | "socket" | "timer"
        | "desktop" | "repo" => Lang::Properties,
        "yaml" | "yml" => Lang::Yaml,
        "json" | "jsonc" => Lang::Json,
        "xml" | "svg" | "plist" | "xsl" => Lang::Xml,
        "html" | "htm" => Lang::Html,
        "py" | "pyw" | "pyi" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" | "tsx" | "mts" | "cts" => Lang::TypeScript,
        "sql" => Lang::Sql,
        "md" | "mdx" | "markdown" => Lang::Markdown,
        "toml" => Lang::Toml,
        "css" | "scss" => Lang::Css,
        "rs" => Lang::Rust,
        "c" | "h" => Lang::C,
        "cc" | "cpp" | "cxx" | "hpp" => Lang::Cpp,
        "go" => Lang::Go,
        "java" => Lang::Java,
        _ => return None,
    })
}

/// 整个文件名就能认出来的（没有扩展名，或者扩展名不说明问题；同 `BY_NAME`）。
fn by_name(name: &str) -> Option<Lang> {
    Some(match name {
        "dockerfile" | "containerfile" => Lang::Dockerfile,
        "nginx.conf" => Lang::Nginx,
        ".bashrc" | ".bash_profile" | ".bash_aliases" | ".bash_logout" | ".profile" | ".zshrc"
        | ".zprofile" | ".zshenv" | ".kshrc" | "profile" | "bashrc" | "crontab" => Lang::Shell,
        "authorized_keys" | "sshd_config" | "ssh_config" | "fstab" | "hosts" | "resolv.conf"
        | ".gitconfig" | ".editorconfig" | ".env" => Lang::Properties,
        "pipfile" | "cargo.lock" => Lang::Toml,
        _ => return None,
    })
}

/// 路径最后一段（`/` 和 `\` 都认；尾部的分隔符不算）。
fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(cut) => &trimmed[cut + 1..],
        None => trimmed,
    }
}

/// 按文件名（可以带路径）判断语言；认不出是纯文本（同 `languageFor`）。
pub fn language_for(path: &str) -> Lang {
    let name = base_name(path);
    let lower = name.to_lowercase();
    if let Some(lang) = by_name(&lower) {
        return lang;
    }
    // Dockerfile.dev、dev.Dockerfile
    if lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile") {
        return Lang::Dockerfile;
    }
    let extension = match lower.rfind('.') {
        Some(dot) if dot > 0 => &lower[dot + 1..],
        _ => "",
    };
    // nginx 的配置：`.nginx`，或者 nginx 目录里没有扩展名 / 扩展名是 .conf 的文件
    //（/etc/nginx/sites-available/default、/etc/nginx/conf.d/site.conf）。
    if extension == "nginx" {
        return Lang::Nginx;
    }
    let normalized = path.replace('\\', "/").to_lowercase();
    let in_nginx_dir = normalized.starts_with("nginx/") || normalized.contains("/nginx/");
    if in_nginx_dir && (extension == "conf" || extension.is_empty()) {
        return Lang::Nginx;
    }
    by_extension(extension).unwrap_or(Lang::Text)
}

/// 看第一行的 shebang 补一刀（`#!/bin/bash`、`#!/usr/bin/env python3`；同 `languageFromShebang`）。
pub fn language_from_shebang(first_line: &str) -> Option<Lang> {
    if !first_line.starts_with("#!") {
        return None;
    }
    let line = first_line.to_lowercase();
    if has_word(&line, |word| {
        matches!(word, "sh" | "bash" | "zsh" | "ksh" | "dash" | "ash")
    }) {
        return Some(Lang::Shell);
    }
    if has_word(&line, |word| {
        word.strip_prefix("python")
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
    }) {
        return Some(Lang::Python);
    }
    if has_word(&line, |word| {
        matches!(word, "node" | "nodejs" | "deno" | "bun")
    }) {
        return Some(Lang::JavaScript);
    }
    None
}

/// 把 shebang 行按非单词字符切开，看有没有哪个词满足条件（对应正则里的 `\b…\b`）。
fn has_word(line: &str, pred: impl Fn(&str) -> bool) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .any(pred)
}

/// 文件名认不出（纯文本）时再看第一行的 shebang。
pub fn detect(path: &str, first_line: &str) -> Lang {
    match language_for(path) {
        Lang::Text => language_from_shebang(first_line).unwrap_or(Lang::Text),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_follow_the_0_2_14_table() {
        for (path, want) in [
            ("/srv/app/run.sh", Lang::Shell),
            ("/srv/app/a.ZSH", Lang::Shell),
            ("/etc/app.conf", Lang::Properties),
            ("/etc/app.service", Lang::Properties),
            ("/srv/a.yml", Lang::Yaml),
            ("/srv/a.jsonc", Lang::Json),
            ("/srv/a.svg", Lang::Xml),
            ("/srv/a.htm", Lang::Html),
            ("/srv/a.pyw", Lang::Python),
            ("/srv/a.jsx", Lang::JavaScript),
            ("/srv/a.tsx", Lang::TypeScript),
            ("/srv/a.sql", Lang::Sql),
            ("/srv/README.markdown", Lang::Markdown),
            ("/srv/a.toml", Lang::Toml),
            ("/srv/a.css", Lang::Css),
            ("/srv/a.rs", Lang::Rust),
            ("/srv/a.go", Lang::Go),
            ("/srv/notes.txt", Lang::Text),
            ("/srv/Makefile", Lang::Text),
        ] {
            assert_eq!(language_for(path), want, "{path}");
        }
    }

    #[test]
    fn well_known_names_win_over_the_extension() {
        for (path, want) in [
            ("/home/u/.bashrc", Lang::Shell),
            ("/home/u/.profile", Lang::Shell),
            ("/etc/crontab", Lang::Shell),
            ("/etc/ssh/sshd_config", Lang::Properties),
            ("/etc/fstab", Lang::Properties),
            ("/etc/hosts", Lang::Properties),
            ("/etc/resolv.conf", Lang::Properties),
            ("/srv/.env", Lang::Properties),
            ("/srv/Dockerfile", Lang::Dockerfile),
            ("/srv/Containerfile", Lang::Dockerfile),
            ("/srv/Dockerfile.dev", Lang::Dockerfile),
            ("/srv/app.dockerfile", Lang::Dockerfile),
            ("/srv/Pipfile", Lang::Toml),
            ("/srv/Cargo.lock", Lang::Toml),
            ("/etc/nginx/nginx.conf", Lang::Nginx),
        ] {
            assert_eq!(language_for(path), want, "{path}");
        }
    }

    #[test]
    fn nginx_directories_make_conf_and_extensionless_files_nginx() {
        assert_eq!(language_for("/etc/nginx/conf.d/site.conf"), Lang::Nginx);
        assert_eq!(
            language_for("/etc/nginx/sites-available/default"),
            Lang::Nginx
        );
        assert_eq!(language_for("/srv/site.nginx"), Lang::Nginx);
        // 不在 nginx 目录里的 .conf 还是普通配置文件。
        assert_eq!(language_for("/etc/other/site.conf"), Lang::Properties);
        assert_eq!(language_for("nginx/site.conf"), Lang::Nginx);
        // 目录名只是长得像 nginx 的不算。
        assert_eq!(language_for("/etc/nginxish/site.conf"), Lang::Properties);
    }

    #[test]
    fn windows_style_paths_are_understood() {
        assert_eq!(language_for(r"C:\work\run.sh"), Lang::Shell);
        assert_eq!(language_for(r"C:\work\nginx\site.conf"), Lang::Nginx);
    }

    #[test]
    fn a_leading_dot_alone_is_not_an_extension() {
        // `.gitignore` 这样的点文件没有扩展名，不能被当成 `gitignore` 扩展名之类。
        assert_eq!(language_for("/srv/.gitignore"), Lang::Text);
        assert_eq!(language_for("/srv/.json"), Lang::Text);
    }

    #[test]
    fn shebang_fills_in_for_unknown_names() {
        assert_eq!(language_from_shebang("#!/bin/bash"), Some(Lang::Shell));
        assert_eq!(language_from_shebang("#!/bin/sh"), Some(Lang::Shell));
        assert_eq!(
            language_from_shebang("#!/usr/bin/env zsh -e"),
            Some(Lang::Shell)
        );
        assert_eq!(
            language_from_shebang("#!/usr/bin/env python3"),
            Some(Lang::Python)
        );
        assert_eq!(
            language_from_shebang("#!/usr/bin/python3.11"),
            Some(Lang::Python)
        );
        assert_eq!(
            language_from_shebang("#!/usr/bin/env node"),
            Some(Lang::JavaScript)
        );
        assert_eq!(language_from_shebang("#!/usr/bin/perl"), None);
        assert_eq!(language_from_shebang("echo #!/bin/sh"), None);
        assert_eq!(language_from_shebang(""), None);
    }

    #[test]
    fn shebang_only_applies_when_the_name_says_nothing() {
        assert_eq!(detect("/usr/local/bin/deploy", "#!/bin/bash"), Lang::Shell);
        // `.txt` 不在 0.2.14 的表里，和没有扩展名一样会看 shebang。
        assert_eq!(detect("/srv/notes.txt", "#!/bin/bash"), Lang::Shell);
        assert_eq!(detect("/srv/a.json", "#!/bin/bash"), Lang::Json);
        assert_eq!(detect("/usr/local/bin/deploy", "plain"), Lang::Text);
    }

    #[test]
    fn comment_tokens_follow_the_language_packs() {
        assert_eq!(Lang::Text.comment(), Comment::Line("#"));
        assert_eq!(Lang::Nginx.comment(), Comment::Line("#"));
        assert_eq!(Lang::Properties.comment(), Comment::Line("#"));
        assert_eq!(Lang::Yaml.comment(), Comment::Line("#"));
        assert_eq!(Lang::JavaScript.comment(), Comment::Line("//"));
        assert_eq!(Lang::Sql.comment(), Comment::Line("--"));
        assert_eq!(Lang::Css.comment(), Comment::Block("/*", "*/"));
        assert_eq!(Lang::Html.comment(), Comment::Block("<!--", "-->"));
        assert_eq!(Lang::Json.comment(), Comment::None);
    }

    #[test]
    fn display_names_match_the_status_bar_of_0_2_14() {
        assert_eq!(Lang::Text.display(), "纯文本");
        assert_eq!(Lang::Properties.display(), "配置文件");
        assert_eq!(Lang::Dockerfile.display(), "Dockerfile");
    }

    #[test]
    fn every_language_has_a_syntax_token() {
        for lang in [
            Lang::Shell,
            Lang::Properties,
            Lang::Yaml,
            Lang::Json,
            Lang::Xml,
            Lang::Html,
            Lang::Nginx,
            Lang::Python,
            Lang::JavaScript,
            Lang::TypeScript,
            Lang::Sql,
            Lang::Dockerfile,
            Lang::Markdown,
            Lang::Toml,
            Lang::Css,
        ] {
            assert!(!lang.syntax_token().is_empty());
            assert_ne!(lang.syntax_token(), "text", "{lang:?}");
        }
        assert_eq!(Lang::Text.syntax_token(), "text");
    }
}
