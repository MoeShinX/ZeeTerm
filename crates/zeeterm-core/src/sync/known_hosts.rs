//! 自己维护的 known_hosts 的同步：按“主机 + 端口 + 密钥类型”作为记录键。
//!
//! 只同步 Z-SSH 自己那份文件（调用方传路径）；系统的 `~/.ssh/known_hosts`
//! 只读，不碰。解析规则和 [`crate::ssh::known_hosts`] 一致：跳过注释、空行、
//! `@cert-authority`，保留 `@revoked`。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use base64::Engine as _;

use super::error::SyncError;
use super::package::{known_host_key, KnownHostRecord};
use crate::ssh::known_hosts::normalize_host;

/// 解析文件内容成同步记录。同一记录键出现多次只保留第一条。
///
/// 记录键里的主机名已按 OpenSSH 规则转小写（见 [`known_host_key`]），所以文件里同时有
/// `Web-1` 和 `web-1` 两行（同一密钥类型）也算同一条，留文件里排在前面的那条；记录里存的
/// 主机名也是小写的规范写法，导出的包里不会再出现大小写不同的两份。
pub fn collect(content: &str, device_id: &str, updated_at: i64) -> Vec<KnownHostRecord> {
    let mut records: BTreeMap<String, KnownHostRecord> = BTreeMap::new();
    for line in content.lines() {
        let Some((marker, patterns, key_type, key)) = parse_line(line) else {
            continue;
        };
        for (host, port) in patterns.iter().filter_map(|pattern| split_pattern(pattern)) {
            let record = KnownHostRecord {
                host: normalize_host(&host),
                port,
                key_type: key_type.to_owned(),
                key: key.to_owned(),
                marker: marker.map(str::to_owned),
                updated_at,
                device_id: device_id.to_owned(),
            };
            records.entry(record.key()).or_insert(record);
        }
    }
    records.into_values().collect()
}

fn parse_line(line: &str) -> Option<(Option<&str>, Vec<&str>, &str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let mut rest = trimmed;
    let mut marker = None;
    if let Some(after) = rest.strip_prefix('@') {
        let (name, tail) = after.split_once(char::is_whitespace)?;
        marker = match name {
            "revoked" => Some("revoked"),
            // M1 不支持证书，整行跳过（和 ssh::known_hosts 一致）。
            _ => return None,
        };
        rest = tail.trim_start();
    }
    let mut fields = rest.split_whitespace();
    let patterns = fields.next()?;
    let key_type = fields.next()?;
    let key = fields.next()?;
    if patterns.is_empty() || key_type.is_empty() || key.is_empty() {
        return None;
    }
    Some((marker, patterns.split(',').collect(), key_type, key))
}

/// `[host]:port` / `host` / 哈希主机名 → (host, port)。
fn split_pattern(pattern: &str) -> Option<(String, u16)> {
    if let Some(rest) = pattern.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(raw) => raw.parse().ok()?,
            None => 22,
        };
        return Some((host.to_owned(), port));
    }
    // 哈希主机名解不出真实主机，原样保留，端口按默认 22。
    Some((pattern.to_owned(), 22))
}

/// 把已经被删除（别的设备"忘记了"）的指纹从文件里去掉，返回去掉的记录条数。
///
/// `keys` 是 [`KnownHostRecord::key`] 的值。只动自己的文件里的普通记录：
/// - `@revoked` 行永远不删（删掉等于放行一把被吊销的密钥）；
/// - 一行写了好几个主机（`host1,host2 ssh-ed25519 …`）只去掉要删的那个，其余主机的指纹留着；
///   行末注释、密钥内容和行尾（LF / CRLF）原样保留；
/// - 读不了（权限、不是 UTF-8……）就报错、什么都不写：读不了 = 不确定，不能当成"没有可删的"；
/// - 整份换掉用同目录临时文件 + 原子替换，并拿着连接确认指纹时的那把写锁。
pub fn remove_records(path: &Path, keys: &BTreeSet<String>) -> Result<usize, SyncError> {
    if keys.is_empty() {
        return Ok(0);
    }
    let _guard = crate::ssh::known_hosts::lock_for_rewrite();
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(SyncError::Io(err)),
    };
    let mut removed = 0usize;
    let mut output = String::with_capacity(content.len());
    for raw in content.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        let terminator = &raw[body.len()..];
        match strip_records(body, keys) {
            LineEdit::Keep => output.push_str(raw),
            LineEdit::Drop(count) => removed += count,
            LineEdit::Replace(line, count) => {
                removed += count;
                output.push_str(&line);
                output.push_str(terminator);
            }
        }
    }
    if removed > 0 {
        crate::sftp::local::write_atomic(path, output.as_bytes())?;
    }
    Ok(removed)
}

enum LineEdit {
    Keep,
    /// 整行删掉（含删掉的记录条数）。
    Drop(usize),
    Replace(String, usize),
}

fn strip_records(line: &str, keys: &BTreeSet<String>) -> LineEdit {
    let Some((marker, patterns, key_type, _key)) = parse_line(line) else {
        return LineEdit::Keep;
    };
    if marker.is_some() {
        return LineEdit::Keep;
    }
    let remaining: Vec<&str> = patterns
        .iter()
        .copied()
        .filter(|pattern| {
            !split_pattern(pattern)
                .is_some_and(|(host, port)| keys.contains(&known_host_key(&host, port, key_type)))
        })
        .collect();
    let removed = patterns.len() - remaining.len();
    if removed == 0 {
        return LineEdit::Keep;
    }
    // 只剩否定模式（`!host`）的行不匹配任何主机，留着只会误导人。
    if remaining.iter().all(|pattern| pattern.starts_with('!')) {
        return LineEdit::Drop(removed);
    }
    // 主机名一栏是第一个非空白字段（上面已排除带 `@` 标记的行），只替换这一栏。
    let start = line.len() - line.trim_start().len();
    let field_len = line[start..]
        .find(char::is_whitespace)
        .unwrap_or(line.len() - start);
    LineEdit::Replace(
        format!(
            "{}{}{}",
            &line[..start],
            remaining.join(","),
            &line[start + field_len..]
        ),
        removed,
    )
}

/// 同步来的一条记录能不能写成 known_hosts 的一行。不能就返回原因（只描述哪个字段
/// 不对，不带公钥内容）。
///
/// 同步包来自别的设备，记录里的字段会被原样拼成一行：任何一个字段带空白或换行，
/// 就能多出字段、多出一整行（例如再塞一条 `@cert-authority *`），把信任扩到别的主机。
/// 所以每个字段都必须是单个合法 token：
/// - 主机：逗号分隔的一个或多个模式，每个只能是主机名 / IP 字符，允许 `*` `?` 通配
///   （本机查表支持，`*.example.com` 这类导入很常见），但每个模式至少要有一段（按 `.`
///   分）完全不含通配符，见 [`pattern_is_anchored`]；不收 `!` 否定——采集时一行
///   被拆成了单个模式，单独一条否定模式已经没有原来的意思。哈希主机名
///   `|1|salt|hash` 两段都必须是 base64。
/// - 密钥类型：算法名字符（字母数字和 `-._@`）。
/// - 公钥：能解开的标准 base64。
/// - 标记：只认 `revoked`；不认识的标记写出去会变成普通信任行，不能放过。
fn validate(record: &KnownHostRecord) -> Result<(), &'static str> {
    if record.port == 0 {
        return Err("端口不能是 0");
    }
    validate_host(&record.host, record.port)?;
    let key_type = &record.key_type;
    if key_type.is_empty()
        || key_type.len() > 128
        || !key_type
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@'))
    {
        return Err("密钥类型不是合法的算法名");
    }
    if !is_base64(&record.key) {
        return Err("公钥不是合法的 base64");
    }
    match record.marker.as_deref() {
        None | Some("revoked") => Ok(()),
        Some(_) => Err("不认识的标记"),
    }
}

fn validate_host(host: &str, port: u16) -> Result<(), &'static str> {
    if let Some(hashed) = host.strip_prefix("|1|") {
        let (salt, hash) = hashed.split_once('|').ok_or("哈希主机名格式不对")?;
        if !is_base64(salt) || !is_base64(hash) {
            return Err("哈希主机名格式不对");
        }
        return Ok(());
    }
    let patterns: Vec<&str> = host.split(',').collect();
    // 非 22 端口要写成 `[host]:port`，括号里只能放一个主机。
    if port != 22 && patterns.len() > 1 {
        return Err("非默认端口的记录只能有一个主机");
    }
    for pattern in patterns {
        let ok = !pattern.is_empty()
            && pattern.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%' | '*' | '?')
            });
        if !ok {
            return Err("主机名里有不允许的字符");
        }
        // 导入的记录会被写进用户自己的 known_hosts，`*` 这样的万能模式一旦进去，
        // 任意主机都会被判成“指纹一致”，等于关掉全局校验。
        if !pattern_is_anchored(pattern) {
            return Err("通配符模式没有固定的主机名段，会匹配任意主机");
        }
    }
    Ok(())
}

/// 模式里至少有一段（按 `.` 分）非空且完全不含 `*` / `?`。
///
/// `*`、`*.*`、`?*`、`**` 不合格；`*.example.com`、`10.0.0.*` 合格。不含通配符的
/// 普通主机名只有“一整段”，自然合格。这是故意写得保守的下限：只挡住“匹配一切”，
/// 不去猜 `*.com` 这类过宽但仍有固定段的模式。
fn pattern_is_anchored(pattern: &str) -> bool {
    pattern
        .split('.')
        .any(|segment| !segment.is_empty() && !segment.contains(['*', '?']))
}

fn is_base64(text: &str) -> bool {
    !text.is_empty()
        && base64::engine::general_purpose::STANDARD
            .decode(text)
            .is_ok_and(|bytes| !bytes.is_empty())
}

fn line_for(record: &KnownHostRecord) -> String {
    // 写出去的也是小写的规范写法（旧版本的同步包里可能还是原样大小写）；哈希主机名原样。
    let name = normalize_host(&record.host);
    let host = if name.starts_with("|1|") || record.port == 22 {
        // 哈希主机名和默认端口都按原样写；非 22 端口要 `[host]:port`。
        name
    } else {
        format!("[{name}]:{}", record.port)
    };
    let marker = match record.marker.as_deref() {
        Some("revoked") => "@revoked ",
        _ => "",
    };
    format!("{marker}{host} {} {}\n", record.key_type, record.key)
}

/// [`append_missing`] 的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppendOutcome {
    /// 追加了几条。
    pub added: usize,
    /// 因为字段不合格（含万能通配）被跳过的条数，界面上要告诉用户。
    pub skipped: usize,
}

/// 导入时，这条记录是否会被 [`append_missing`] 接受。预览用它预估“新增几条”。
pub fn is_importable(record: &KnownHostRecord) -> bool {
    validate(record).is_ok()
}

/// 把还没有的记录追加到文件里，返回追加和跳过的条数。
///
/// 只追加，不改已有行：吊销记录、注释和其他程序写的内容都原样保留。字段不合格的
/// 记录跳过并记一条警告（见 [`validate`]）。
///
/// 现有文件读不了（权限、不是 UTF-8……）就报错、什么都不写：当成空文件的话会把
/// 已有的记录再追加一遍，还可能写进一个本来就不该动的文件。只有“文件不存在”才
/// 等于空。
pub fn append_missing(
    path: &Path,
    records: &[KnownHostRecord],
) -> Result<AppendOutcome, SyncError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(SyncError::Io(err)),
    };
    let mut present: BTreeSet<String> = collect(&existing, "local", 0)
        .into_iter()
        .map(|record| record.key())
        .collect();

    let mut lines = String::new();
    let mut appended = 0usize;
    let mut skipped = 0usize;
    for record in records {
        if let Err(reason) = validate(record) {
            skipped += 1;
            // 不打印公钥；主机名截短，防止异常长的字段刷爆日志。
            let host: String = record.host.chars().take(80).collect();
            tracing::warn!(host = %host.escape_debug(), port = record.port, reason, "同步来的主机指纹字段不合格，没有写入 known_hosts");
            continue;
        }
        // `insert` 同时挡住这一批里自己重复的（`Web-1` 和 `web-1` 现在是同一个键），
        // 不然同一台主机会被写两行。
        if !present.insert(record.key()) {
            continue;
        }
        lines.push_str(&line_for(record));
        appended += 1;
    }
    if appended == 0 {
        return Ok(AppendOutcome { added: 0, skipped });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 文件末尾没有换行（最后一行是别的程序写的注释或记录，没带行尾）时先补一个，否则新记录会
    // 被拼进上一行：返回 added=1，重新解析却一条也没有。补 `\n`：本函数写的行尾一直是 `\n`
    // （见 `line_for`）；CRLF 文件本来就以 `\n` 结尾，不会走到这里，末尾裸 `\r` 补 `\n` 也正好成 CRLF。
    let mut chunk = String::with_capacity(lines.len() + 1);
    if !existing.is_empty() && !existing.ends_with('\n') {
        chunk.push('\n');
    }
    chunk.push_str(&lines);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(chunk.as_bytes())?;
    file.flush()?;
    Ok(AppendOutcome {
        added: appended,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::package::known_host_key;

    const SAMPLE: &str = "\
# 注释
web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa
[web-2.example.internal]:2222 ssh-rsa AAAAB3NzaC1yc2Eaaa
@revoked old.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIrevoked
@cert-authority *.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIcert
|1|abc|def ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIhash
坏行
";

    #[test]
    fn parses_hosts_ports_key_types_and_markers() {
        let records = collect(SAMPLE, "dev-a", 100);
        assert_eq!(records.len(), 4, "{records:?}");
        let by_key: BTreeMap<String, KnownHostRecord> = records
            .into_iter()
            .map(|record| (record.key(), record))
            .collect();
        let web1 = &by_key[&known_host_key("web-1.example.internal", 22, "ssh-ed25519")];
        assert_eq!(web1.marker, None);
        let web2 = &by_key[&known_host_key("web-2.example.internal", 2222, "ssh-rsa")];
        assert_eq!(web2.port, 2222);
        let revoked = &by_key[&known_host_key("old.example.internal", 22, "ssh-ed25519")];
        assert_eq!(revoked.marker.as_deref(), Some("revoked"));
        assert!(by_key.keys().any(|key| key.starts_with("|1|abc|def")));
    }

    #[test]
    fn appends_only_missing_records_and_keeps_everything_else() {
        let dir = crate::test_util::TestDir::new("kh-sync");
        let path = dir.join("known_hosts");
        std::fs::write(&path, SAMPLE).expect("写样本");

        let records = vec![
            KnownHostRecord {
                host: "web-1.example.internal".to_owned(),
                port: 22,
                key_type: "ssh-ed25519".to_owned(),
                key: "AAAAC3NzaC1lZDI1NTE5AAAAIaaa".to_owned(),
                marker: None,
                updated_at: 1,
                device_id: "dev-a".to_owned(),
            },
            KnownHostRecord {
                host: "new.example.internal".to_owned(),
                port: 2200,
                key_type: "ssh-ed25519".to_owned(),
                key: "AAAAC3NzaC1lZDI1NTE5AAAAInew".to_owned(),
                marker: None,
                updated_at: 1,
                device_id: "dev-a".to_owned(),
            },
            KnownHostRecord {
                host: "bad.example.internal".to_owned(),
                port: 22,
                key_type: "ssh-ed25519".to_owned(),
                key: "AAAAC3NzaC1lZDI1NTE5AAAAIbad".to_owned(),
                marker: Some("revoked".to_owned()),
                updated_at: 1,
                device_id: "dev-a".to_owned(),
            },
        ];
        assert_eq!(append_missing(&path, &records).expect("追加").added, 2);
        let content = std::fs::read_to_string(&path).expect("读回");
        assert!(content.contains("# 注释"), "注释要保留");
        assert!(
            content.contains("@cert-authority"),
            "不认识的标记行原样保留"
        );
        assert!(content.contains("@revoked old.example.internal"));
        assert!(content.contains("[new.example.internal]:2200 ssh-ed25519"));
        assert!(content.contains("@revoked bad.example.internal"));
        assert_eq!(
            content
                .matches("web-1.example.internal ssh-ed25519")
                .count(),
            1,
            "已有的记录不能重复追加"
        );
    }

    #[test]
    fn missing_file_is_created() {
        let tmp = crate::test_util::TestDir::new("kh-sync-dir");
        let dir = tmp.join("nested");
        let path = dir.join("known_hosts");
        let records = vec![KnownHostRecord {
            host: "a.example.internal".to_owned(),
            port: 22,
            key_type: "ssh-ed25519".to_owned(),
            key: "AAAAC3NzaC1lZDI1NTE5AAAAIaaa".to_owned(),
            marker: None,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        }];
        assert_eq!(append_missing(&path, &records).expect("创建").added, 1);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("a.example.internal"));
    }

    fn record(host: &str, port: u16, key_type: &str, key: &str) -> KnownHostRecord {
        KnownHostRecord {
            host: host.to_owned(),
            port,
            key_type: key_type.to_owned(),
            key: key.to_owned(),
            marker: None,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        }
    }

    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAInew";

    /// 往 `initial` 内容的文件里追加一条新记录，返回（追加条数，重新解析出的全部 key，文件原文）。
    fn append_one_to(tag: &str, initial: &[u8]) -> (usize, BTreeSet<String>, String) {
        let dir = crate::test_util::TestDir::new(tag);
        let path = dir.join("known_hosts");
        std::fs::write(&path, initial).expect("写样本");
        let added = append_missing(
            &path,
            &[record("new.example.internal", 22, "ssh-ed25519", KEY)],
        )
        .expect("追加")
        .added;
        let content = String::from_utf8(std::fs::read(&path).expect("读回")).expect("UTF-8");
        let keys = collect(&content, "local", 0)
            .into_iter()
            .map(|record| record.key())
            .collect();
        (added, keys, content)
    }

    fn key_of(host: &str) -> String {
        known_host_key(host, 22, "ssh-ed25519")
    }

    #[test]
    fn a_last_line_without_a_newline_does_not_swallow_the_new_record() {
        let (added, keys, content) = append_one_to(
            "kh-nonl-key",
            b"web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa",
        );
        assert_eq!(added, 1);
        assert_eq!(
            keys,
            BTreeSet::from([
                key_of("web-1.example.internal"),
                key_of("new.example.internal")
            ]),
            "{content:?}"
        );
        // 补的是一个换行，不是空行。
        assert!(!content.contains("\n\n"), "{content:?}");
    }

    #[test]
    fn a_trailing_comment_without_a_newline_is_kept_and_the_record_starts_a_new_line() {
        let (added, keys, content) = append_one_to(
            "kh-nonl-comment",
            b"web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa\n# managed elsewhere",
        );
        assert_eq!(added, 1);
        assert!(content.contains("\n# managed elsewhere\n"), "{content:?}");
        assert_eq!(
            keys,
            BTreeSet::from([
                key_of("web-1.example.internal"),
                key_of("new.example.internal")
            ])
        );
    }

    #[test]
    fn crlf_files_get_no_extra_blank_line_and_unterminated_crlf_files_get_a_newline() {
        // 本来就以 CRLF 结尾：不补任何东西，新行紧跟在后面。
        let (added, keys, content) = append_one_to(
            "kh-crlf",
            b"# c\r\nweb-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa\r\n",
        );
        assert_eq!(added, 1);
        assert!(
            !content.contains("\n\n") && !content.contains("\r\n\r\n"),
            "{content:?}"
        );
        assert!(
            content.ends_with(&format!("ssh-ed25519 {KEY}\n")),
            "{content:?}"
        );
        assert_eq!(
            keys,
            BTreeSet::from([
                key_of("web-1.example.internal"),
                key_of("new.example.internal")
            ])
        );

        // CRLF 文件最后一行没有行尾：补一个换行再写。
        let (added, keys, _) = append_one_to(
            "kh-crlf-unterminated",
            b"# c\r\nweb-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa",
        );
        assert_eq!(added, 1);
        assert_eq!(
            keys,
            BTreeSet::from([
                key_of("web-1.example.internal"),
                key_of("new.example.internal")
            ])
        );

        // 末尾只有一个 `\r`：补 `\n` 正好是 CRLF，不多出空行。
        let (added, keys, content) = append_one_to("kh-lone-cr", b"# c\r");
        assert_eq!(added, 1);
        assert!(
            content.starts_with("# c\r\nnew.example.internal"),
            "{content:?}"
        );
        assert_eq!(keys, BTreeSet::from([key_of("new.example.internal")]));
    }

    #[test]
    fn an_empty_existing_file_gets_no_leading_blank_line() {
        let (added, keys, content) = append_one_to("kh-empty", b"");
        assert_eq!(added, 1);
        assert!(content.starts_with("new.example.internal "), "{content:?}");
        assert_eq!(keys, BTreeSet::from([key_of("new.example.internal")]));
    }

    #[test]
    fn records_with_injected_fields_are_skipped_and_never_written() {
        let dir = crate::test_util::TestDir::new("kh-sync-inject");
        let path = dir.join("known_hosts");
        std::fs::write(
            &path,
            "web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa
",
        )
        .expect("写样本");
        let mut bad_marker = record("m.example.internal", 22, "ssh-ed25519", KEY);
        bad_marker.marker = Some("cert-authority".to_owned());
        let records = vec![
            // 主机字段里藏换行，想再塞一条 CA 记录
            record(
                "a.example.internal ssh-ed25519 AAAA
@cert-authority *",
                22,
                "ssh-ed25519",
                KEY,
            ),
            record("b.example.internal\r", 22, "ssh-ed25519", KEY),
            record("c.example.internal", 22, "ssh-ed25519 extra", KEY),
            record(
                "d.example.internal",
                22,
                "ssh-ed25519
",
                KEY,
            ),
            record("e.example.internal", 22, "ssh-ed25519", "AAAA AAAA"),
            record(
                "f.example.internal",
                22,
                "ssh-ed25519",
                "AAAA
@cert-authority * x y",
            ),
            record("g.example.internal", 22, "ssh-ed25519", "not*base64"),
            record("h.example.internal", 22, "", KEY),
            record("", 22, "ssh-ed25519", KEY),
            record("[i.example.internal]:22", 2222, "ssh-ed25519", KEY),
            record("!j.example.internal", 22, "ssh-ed25519", KEY),
            record(
                "k1.example.internal,k2.example.internal",
                2222,
                "ssh-ed25519",
                KEY,
            ),
            record(
                "|1|abc|d
ef",
                22,
                "ssh-ed25519",
                KEY,
            ),
            record("l.example.internal", 0, "ssh-ed25519", KEY),
            bad_marker,
        ];
        let outcome = append_missing(&path, &records).expect("追加");
        assert_eq!((outcome.added, outcome.skipped), (0, 15));
        let content = std::fs::read_to_string(&path).expect("读回");
        assert_eq!(
            content,
            "web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa
",
            "不合格的记录一条都不能写进去"
        );
    }

    #[test]
    fn legal_forms_are_still_written() {
        let dir = crate::test_util::TestDir::new("kh-sync-legal");
        let path = dir.join("known_hosts");
        let hashed = "|1|c2FsdHNhbHRzYWx0c2FsdHNhbHQ=|aGFzaGhhc2hoYXNoaGFzaGhhc2g=";
        let mut revoked = record("r.example.internal", 22, "ssh-ed25519", KEY);
        revoked.marker = Some("revoked".to_owned());
        let records = vec![
            record("a.example.internal,10.0.0.1", 22, "ssh-ed25519", KEY),
            record("*.example.internal", 22, "ecdsa-sha2-nistp256", KEY),
            record("fe80::1%eth0", 2222, "sk-ssh-ed25519@openssh.com", KEY),
            record(hashed, 22, "ssh-ed25519", KEY),
            revoked,
        ];
        assert_eq!(append_missing(&path, &records).expect("追加").added, 5);
        let content = std::fs::read_to_string(&path).expect("读回");
        assert!(content.contains("a.example.internal,10.0.0.1 ssh-ed25519 "));
        assert!(content.contains("*.example.internal ecdsa-sha2-nistp256 "));
        assert!(content.contains("[fe80::1%eth0]:2222 sk-ssh-ed25519@openssh.com "));
        assert!(content.contains(&format!("{hashed} ssh-ed25519 ")));
        assert!(content.contains("@revoked r.example.internal ssh-ed25519 "));
        assert_eq!(content.lines().count(), 5, "{content}");
    }

    #[test]
    fn an_unreadable_existing_file_is_an_error_not_an_empty_file() {
        let dir = crate::test_util::TestDir::new("kh-sync-unreadable");
        let path = dir.join("known_hosts");
        // 不是 UTF-8：读不了，不能当成空文件再追加一遍
        let original: &[u8] =
            b"web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa # \xff\xfe\n";
        std::fs::write(&path, original).expect("写样本");
        let records = vec![
            record(
                "web-1.example.internal",
                22,
                "ssh-ed25519",
                "AAAAC3NzaC1lZDI1NTE5AAAAIaaa",
            ),
            record("new.example.internal", 22, "ssh-ed25519", KEY),
        ];
        let err = append_missing(&path, &records).expect_err("读不了要报错");
        assert!(matches!(err, SyncError::Io(_)), "{err:?}");
        assert_eq!(
            std::fs::read(&path).expect("读回"),
            original,
            "文件不能被改动"
        );
    }

    #[test]
    fn match_everything_patterns_are_skipped_and_counted() {
        let dir = crate::test_util::TestDir::new("kh-sync-wildcard");
        let path = dir.join("known_hosts");
        let records = vec![
            record("*", 22, "ssh-ed25519", KEY),
            record("*.*", 22, "ssh-ed25519", KEY),
            record("?*", 22, "ssh-ed25519", KEY),
            record("**", 22, "ssh-ed25519", KEY),
            record("*.?", 22, "ssh-ed25519", KEY),
            record("web-*", 22, "ssh-ed25519", KEY),
            record("*,ok.example.internal", 22, "ssh-ed25519", KEY),
            record("ok.example.internal,*.*", 22, "ssh-ed25519", KEY),
            record("*", 2222, "ssh-ed25519", KEY),
            record("*.example.internal", 22, "ssh-ed25519", KEY),
            record("10.0.0.*", 22, "ssh-ed25519", KEY),
        ];
        let outcome = append_missing(&path, &records).expect("追加");
        assert_eq!((outcome.added, outcome.skipped), (2, 9));
        let content = std::fs::read_to_string(&path).expect("读回");
        assert_eq!(content.lines().count(), 2, "{content}");
        assert!(content.contains("*.example.internal ssh-ed25519 "));
        assert!(content.contains("10.0.0.* ssh-ed25519 "));
        for line in content.lines() {
            let host = line.split_whitespace().next().unwrap_or_default();
            assert!(host != "*" && host != "*.*", "万能模式不能落盘：{line}");
        }
    }

    #[test]
    fn anchored_pattern_rule() {
        for ok in [
            "web-1.example.internal",
            "*.example.internal",
            "10.0.0.*",
            "web-*.example.internal",
            "fe80::1%eth0",
            "localhost",
        ] {
            assert!(pattern_is_anchored(ok), "{ok}");
        }
        for bad in ["*", "?", "*.*", "?*", "**", "*.?", "web-*", "*.*.*", "*."] {
            assert!(!pattern_is_anchored(bad), "{bad}");
        }
    }

    fn keys_of(entries: &[(&str, u16, &str)]) -> BTreeSet<String> {
        entries
            .iter()
            .map(|(host, port, key_type)| known_host_key(host, *port, key_type))
            .collect()
    }

    #[test]
    fn remove_records_drops_only_the_forgotten_keys_and_keeps_the_rest_of_the_file() {
        let dir = crate::test_util::TestDir::new("kh-remove");
        let path = dir.join("known_hosts");
        // CRLF 文件：行尾要原样保留。
        let original = [
            "# 注释",
            "web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa",
            "web-1.example.internal ssh-rsa AAAAB3NzaC1yc2Eaaa",
            "[web-2.example.internal]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIbbb",
            "web-3.example.internal,web-4.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIccc # 备注",
            "@revoked web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIrevoked",
            "@cert-authority *.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIcert",
            "|1|abc|def ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIhash",
        ]
        .join("\r\n")
            + "\r\n";
        std::fs::write(&path, original).expect("写样本");

        let removed = remove_records(
            &path,
            &keys_of(&[
                ("web-1.example.internal", 22, "ssh-ed25519"),
                ("not-there.example.internal", 22, "ssh-ed25519"),
                ("web-2.example.internal", 2222, "ssh-ed25519"),
                ("web-3.example.internal", 22, "ssh-ed25519"),
                ("|1|abc|def", 22, "ssh-ed25519"),
            ]),
        )
        .expect("去掉");
        assert_eq!(removed, 4);
        let content = std::fs::read_to_string(&path).expect("读回");
        let expected = [
            "# 注释",
            "web-1.example.internal ssh-rsa AAAAB3NzaC1yc2Eaaa",
            "web-4.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIccc # 备注",
            "@revoked web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIrevoked",
            "@cert-authority *.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIcert",
        ]
        .join("\r\n")
            + "\r\n";
        assert_eq!(
            content, expected,
            "只去掉要删的；@revoked、证书行、注释、行尾原样"
        );

        // 没有要删的：文件一个字节都不动（连同没有行尾的最后一行）。
        let untouched = "web-9.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIzzz";
        std::fs::write(&path, untouched).expect("写样本");
        assert_eq!(
            remove_records(
                &path,
                &keys_of(&[("web-1.example.internal", 22, "ssh-ed25519")])
            )
            .expect("去掉"),
            0
        );
        assert_eq!(std::fs::read_to_string(&path).expect("读回"), untouched);
    }

    #[test]
    fn remove_records_does_nothing_when_the_file_is_missing_or_unreadable() {
        let dir = crate::test_util::TestDir::new("kh-remove-unreadable");
        let keys = keys_of(&[("web-1.example.internal", 22, "ssh-ed25519")]);
        assert_eq!(
            remove_records(&dir.join("not-there"), &keys).expect("不存在 = 没有可删的"),
            0
        );
        // 不是 UTF-8：读不了 = 不确定，报错并且不改文件。
        let path = dir.join("known_hosts");
        let bytes: &[u8] =
            b"web-1.example.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIaaa # \xff\xfe\n";
        std::fs::write(&path, bytes).expect("写样本");
        assert!(remove_records(&path, &keys).is_err());
        assert_eq!(std::fs::read(&path).expect("读回"), bytes);
    }

    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIaaa";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIbbb";

    fn rec(host: &str, key: &str) -> KnownHostRecord {
        KnownHostRecord {
            host: host.to_owned(),
            port: 22,
            key_type: "ssh-ed25519".to_owned(),
            key: key.to_owned(),
            marker: None,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        }
    }

    #[test]
    fn collect_treats_host_names_that_differ_only_in_case_as_one_record() {
        // 文件里先后写了两种大小写：和 OpenSSH 一样是同一台主机，留排在前面的那条，
        // 记录里存小写的规范写法。
        let content = format!(
            "Web-1.Example.Internal ssh-ed25519 {KEY_A}\n\
             web-1.example.internal ssh-ed25519 {KEY_B}\n\
             [Web-1.Example.Internal]:2222 ssh-ed25519 {KEY_A}\n"
        );
        let records = collect(&content, "dev-a", 1);
        assert_eq!(records.len(), 2, "{records:?}");
        let default_port = records.iter().find(|r| r.port == 22).expect("22 端口");
        assert_eq!(default_port.host, "web-1.example.internal");
        assert_eq!(default_port.key, KEY_A, "留文件里排在前面的那条");
        assert!(records.iter().any(|r| r.port == 2222));
    }

    #[test]
    fn collect_keeps_hashed_host_names_untouched() {
        // 哈希主机名的 base64 区分大小写，转了就再也认不出来。
        let records = collect(&format!("|1|AbC=|DeF= ssh-ed25519 {KEY_A}\n"), "dev-a", 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].host, "|1|AbC=|DeF=");
    }

    #[test]
    fn record_keys_ignore_host_name_case() {
        assert_eq!(
            rec("Web-1.Example.Internal", KEY_A).key(),
            rec("web-1.example.internal", KEY_B).key()
        );
        assert_ne!(
            rec("|1|AbC=|DeF=", KEY_A).key(),
            rec("|1|abc=|def=", KEY_A).key()
        );
    }

    #[test]
    fn append_missing_does_not_duplicate_a_host_that_differs_only_in_case() {
        let dir = crate::test_util::TestDir::new("kh-sync-case");
        let path = dir.join("known_hosts");
        std::fs::write(
            &path,
            format!("web-1.example.internal ssh-ed25519 {KEY_A}\n"),
        )
        .expect("写样本");
        // 文件里已有的主机，同步包里换了个大小写写法：已经有了，不追加。
        let outcome = append_missing(&path, &[rec("Web-1.Example.Internal", KEY_A)]).expect("追加");
        assert_eq!(outcome.added, 0);
        // 同一批里自己重复（只差大小写）的只写一行，而且写成小写。
        let outcome = append_missing(
            &path,
            &[
                rec("New-1.Example.Internal", KEY_A),
                rec("new-1.example.internal", KEY_A),
            ],
        )
        .expect("追加");
        assert_eq!(outcome.added, 1);
        let content = std::fs::read_to_string(&path).expect("读回");
        assert_eq!(
            content.matches("new-1.example.internal").count(),
            1,
            "{content}"
        );
        assert!(!content.contains("New-1"), "{content}");
    }

    #[test]
    fn remove_records_drops_a_line_written_with_another_case() {
        let dir = crate::test_util::TestDir::new("kh-remove-case");
        let path = dir.join("known_hosts");
        std::fs::write(
            &path,
            format!("Web-1.Example.Internal ssh-ed25519 {KEY_A}\n"),
        )
        .expect("写样本");
        let removed = remove_records(
            &path,
            &keys_of(&[("web-1.example.internal", 22, "ssh-ed25519")]),
        )
        .expect("去掉");
        assert_eq!(removed, 1);
        assert_eq!(std::fs::read_to_string(&path).expect("读回"), "");
    }
}
