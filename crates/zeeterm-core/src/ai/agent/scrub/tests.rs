use super::*;

// 假秘密一律拼出来：源码里不出现完整的令牌 / 私钥头样式，免得隐私扫描把测试数据当成泄露。
fn fake_sk() -> String {
    format!("{}{}", "sk-", "Zq81xLmN0pQ4rStUvWx9YzAb")
}

fn fake_pem_begin() -> String {
    format!("{}{}", "-----BEGIN OPENSSH ", "PRIVATE KEY-----")
}

fn fake_pem_end() -> String {
    format!("{}{}", "-----END OPENSSH ", "PRIVATE KEY-----")
}

const BODY: &str = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW";

#[test]
fn a_token_is_masked_but_surrounding_text_stays() {
    let sk = fake_sk();
    let out = scrub_line(&format!("使用 key {sk} 调用失败"));
    assert!(!out.contains(&sk), "{out}");
    assert!(out.contains("调用失败"), "{out}");
    assert!(out.contains(MASK), "{out}");
}

#[test]
fn common_token_shapes_are_masked() {
    let ghp = format!("{}{}", "ghp_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4");
    let pat = format!("{}{}", "github_pat_", "11ABCDEFG0abcdefghijklmnopqrstuv");
    let slack = format!("{}{}", "xoxb-", "123456789012-abcdefghijkl");
    let aws = format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP");
    let jwt = format!(
        "{}.{}.{}",
        "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJ1c2VyIn0", "c2lnbmF0dXJlLXNpZ25hdHVyZQ"
    );
    for secret in [&ghp, &pat, &slack, &aws, &jwt] {
        let out = scrub_line(&format!("env: X={secret} done"));
        assert!(!out.contains(secret.as_str()), "{secret} -> {out}");
    }
}

#[test]
fn ordinary_words_are_left_alone() {
    for text in [
        "task-run finished",
        "sk-learn is a library",
        "listening on 127.0.0.1:8080",
        "tokenizer loaded",
        "error: file not found: /srv/app/main.rs",
    ] {
        assert_eq!(scrub_line(text), text);
    }
}

#[test]
fn bearer_and_key_value_secrets_are_masked() {
    let out = scrub_line("Authorization: Bearer abcdefghijklmnop1234 ok");
    assert!(!out.contains("abcdefghijklmnop1234"), "{out}");
    assert!(out.ends_with(" ok"), "{out}");

    for (text, secret) in [
        ("password=hunter2hunter2 rest", "hunter2hunter2"),
        ("DB_PASSWORD: p@ssw0rd!", "p@ssw0rd!"),
        (r#"{"api_key":"abc123def456","x":1}"#, "abc123def456"),
        (r#"{\"token\": \"abc123def456\"}"#, "abc123def456"),
        ("curl --password hunter2hunter2 host", "hunter2hunter2"),
        ("client_secret = 's3cr3t-value'", "s3cr3t-value"),
    ] {
        let out = scrub_line(text);
        assert!(!out.contains(secret), "{text} -> {out}");
    }
    // 键名本身还在，方便看出是什么被遮了。
    assert!(scrub_line("password=hunter2hunter2").starts_with("password="));
}

#[test]
fn a_private_key_block_on_one_line_is_masked_even_with_escaped_newlines() {
    let line = format!(
        r#"{{"text":"{}\n{BODY}\n{}\n"}}"#,
        fake_pem_begin(),
        fake_pem_end()
    );
    let out = scrub_line(&line);
    assert!(!out.contains(BODY), "{out}");
    assert!(!out.contains("PRIVATE KEY"), "{out}");
    assert!(out.contains("\"text\""), "{out}");
}

#[test]
fn a_private_key_block_spanning_lines_is_masked_with_state() {
    let lines = [
        "before".to_owned(),
        format!("cat id: {}", fake_pem_begin()),
        BODY.to_owned(),
        BODY.to_owned(),
        format!("{} after", fake_pem_end()),
        "plain".to_owned(),
    ];
    let mut in_block = false;
    let mut shown = Vec::new();
    for line in &lines {
        let scrubbed = scrub_stream_line(line, in_block);
        in_block = scrubbed.in_key_block;
        if let Some(text) = scrubbed.text {
            shown.push(text);
        }
    }
    let joined = shown.join("\n");
    assert!(!joined.contains(BODY), "{joined}");
    assert!(!joined.contains("PRIVATE KEY"), "{joined}");
    assert!(
        joined.contains("before") && joined.contains("plain"),
        "{joined}"
    );
    assert!(!in_block);
}

#[test]
fn clip_cuts_by_characters_not_bytes() {
    assert_eq!(clip("你好世界", 2), "你好…");
    assert_eq!(clip("abc", 5), "abc");
    let sk = fake_sk();
    let long = format!("{} {sk}", "x".repeat(MAX_LINE_CHARS - 5));
    let out = scrub_and_clip(&long);
    assert!(!out.contains("sk-Zq"), "令牌先遮后截，不会留下半截：{out}");
}
