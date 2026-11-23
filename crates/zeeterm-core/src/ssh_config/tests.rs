//! SSH 配置导入的单元测试：解析（用内存里的假文件系统，不碰真实的 `~/.ssh`）、计划、落库。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::model::{AuthMethod, HostDraft, IdentityDraft, IdentityKind, ProxyMode};
use crate::store::Database;

// ---------- 假文件系统 ----------

/// 内存里的文件系统：路径 → 内容。`denied` 里的路径读的时候报权限错误。
#[derive(Default)]
struct MemFs {
    files: BTreeMap<PathBuf, String>,
    denied: Vec<PathBuf>,
    /// 读过的路径，用来断言「没读不该读的」。
    reads: std::cell::RefCell<Vec<PathBuf>>,
}

impl ConfigFs for MemFs {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.reads.borrow_mut().push(path.to_path_buf());
        if self.denied.iter().any(|denied| denied == path) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        match self.files.get(path) {
            Some(text) => Ok(text.clone().into_bytes()),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    }

    fn list_dir(&self, dir: &Path) -> std::io::Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for path in self.files.keys() {
            if let Ok(rest) = path.strip_prefix(dir) {
                if let Some(first) = rest.components().next() {
                    names.insert(first.as_os_str().to_string_lossy().into_owned());
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    fn is_file(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
}

fn home() -> PathBuf {
    PathBuf::from("/h/tester")
}

fn ssh_dir() -> PathBuf {
    home().join(".ssh")
}

fn options() -> ParseOptions {
    ParseOptions {
        ssh_dir: ssh_dir(),
        home: home(),
    }
}

/// 在 `ssh_dir` 下的路径（`/` 分隔）。
fn at(relative: &str) -> PathBuf {
    let mut path = ssh_dir();
    for part in relative.split('/') {
        path.push(part);
    }
    path
}

fn fs_with(files: &[(&str, &str)]) -> MemFs {
    let mut fs = MemFs::default();
    for (relative, text) in files {
        fs.files.insert(at(relative), (*text).to_owned());
    }
    fs
}

fn parse(text: &str) -> ParsedConfig {
    parse_files(&[("config", text)])
}

fn parse_files(files: &[(&str, &str)]) -> ParsedConfig {
    let fs = fs_with(files);
    parse_config(&fs, &at("config"), &options()).expect("配置应该能读")
}

fn host<'a>(parsed: &'a ParsedConfig, alias: &str) -> &'a ConfigHost {
    parsed
        .hosts
        .iter()
        .find(|host| host.alias == alias)
        .unwrap_or_else(|| panic!("没有解析出主机 {alias}：{:?}", aliases(parsed)))
}

fn aliases(parsed: &ParsedConfig) -> Vec<&str> {
    parsed
        .hosts
        .iter()
        .map(|host| host.alias.as_str())
        .collect()
}

fn native(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------- 解析：基本字段 ----------

#[test]
fn reads_the_basic_fields() {
    let parsed = parse(
        "Host web\n  HostName web.example.com\n  User deploy\n  Port 2222\n  IdentityFile ~/.ssh/id_ed25519\n",
    );
    let web = host(&parsed, "web");
    assert_eq!(web.address, "web.example.com");
    assert_eq!(web.user.as_deref(), Some("deploy"));
    assert_eq!(web.port, 2222);
    assert_eq!(
        web.identity_file.as_deref(),
        Some(native(&home().join(".ssh").join("id_ed25519")).as_str())
    );
    assert_eq!(web.proxy_jump, ProxyJump::Unset);
    assert!(web.unsupported.is_empty() && web.notes.is_empty());
}

#[test]
fn address_defaults_to_the_alias_and_port_to_22() {
    let parsed = parse("Host web\n  User deploy\n");
    let web = host(&parsed, "web");
    assert_eq!((web.address.as_str(), web.port), ("web", 22));
    assert_eq!(web.identity_file, None);
}

#[test]
fn accepts_equals_quotes_comments_and_any_letter_case() {
    let parsed = parse(
        "# 注释\nHOST=web\n  hostname = \"web.example.net\"\n  PORT=2200\n  uSeR bob # 行尾注释\n  \
         IdentityFile \"~/.ssh/my key\"\n\n",
    );
    let web = host(&parsed, "web");
    assert_eq!(web.address, "web.example.net");
    assert_eq!(web.port, 2200);
    assert_eq!(web.user.as_deref(), Some("bob"), "行尾注释不算值");
    assert!(
        web.identity_file
            .as_deref()
            .is_some_and(|path| path.ends_with("my key")),
        "引号里的空格保留"
    );
}

#[test]
fn tolerates_a_bom_and_windows_line_endings() {
    let parsed = parse("\u{feff}Host web\r\n  User deploy\r\n  Port 2200\r\n");
    let web = host(&parsed, "web");
    assert_eq!((web.user.as_deref(), web.port), (Some("deploy"), 2200));
}

#[test]
fn one_host_line_can_name_several_aliases() {
    let parsed = parse("Host web web-prod\n  HostName shared.example.com\n  User deploy\n");
    for alias in ["web", "web-prod"] {
        let found = host(&parsed, alias);
        assert_eq!(found.address, "shared.example.com");
        assert_eq!(found.user.as_deref(), Some("deploy"));
    }
}

#[test]
fn wildcard_and_negated_patterns_make_no_host_of_their_own() {
    let parsed = parse("Host *.example.com db? !skip real\n  User u\nHost *\n  Port 2200\n");
    assert_eq!(aliases(&parsed), ["real"]);
    // 通配符那一段的设置照样参与：real 取到了 User，也取到了 Host * 的端口。
    let real = host(&parsed, "real");
    assert_eq!(real.user.as_deref(), Some("u"));
    assert_eq!(real.port, 2200);
}

#[test]
fn an_alias_written_twice_is_one_host() {
    let parsed = parse("Host web\n  User a\nHost web\n  User b\n  Port 2200\n");
    assert_eq!(aliases(&parsed), ["web"]);
    let web = host(&parsed, "web");
    assert_eq!(web.user.as_deref(), Some("a"), "先出现的值生效");
    assert_eq!(web.port, 2200, "没取到的字段由后面的段补");
}

// ---------- 解析：第一个取到的值生效 ----------

#[test]
fn host_star_after_the_specific_host_only_fills_the_gaps() {
    let parsed = parse(
        "Host web\n  User deploy\nHost *\n  User fallback\n  Port 2200\n  IdentityFile ~/.ssh/default\n",
    );
    let web = host(&parsed, "web");
    assert_eq!(web.user.as_deref(), Some("deploy"), "具体的值在前，先取到");
    assert_eq!(web.port, 2200, "Host * 补上没写的端口");
    assert!(web
        .identity_file
        .as_deref()
        .is_some_and(|path| path.ends_with("default")));
}

#[test]
fn host_star_before_the_specific_host_wins_like_openssh() {
    let parsed =
        parse("Host *\n  User fallback\n  Port 2200\nHost web\n  User deploy\n  Port 22\n");
    let web = host(&parsed, "web");
    assert_eq!(
        web.user.as_deref(),
        Some("fallback"),
        "写在前面的 Host * 覆盖后面的具体值"
    );
    assert_eq!(web.port, 2200);
}

#[test]
fn identity_file_takes_the_first_one_across_blocks() {
    let parsed =
        parse("Host web\n  IdentityFile ~/.ssh/web_key\nHost *\n  IdentityFile ~/.ssh/default\n");
    let web = host(&parsed, "web");
    assert!(web
        .identity_file
        .as_deref()
        .is_some_and(|path| path.ends_with("web_key")));
    assert_eq!(web.notes, [HostNote::ExtraIdentityFiles(1)]);
}

#[test]
fn negated_patterns_exclude_a_host_from_a_star_block() {
    let parsed = parse("Host * !internal\n  User shared\nHost web\n  HostName w.example.com\nHost internal\n  HostName i.example.com\n");
    assert_eq!(host(&parsed, "web").user.as_deref(), Some("shared"));
    assert_eq!(
        host(&parsed, "internal").user,
        None,
        "被 ! 排除的主机不吃这一段"
    );
}

#[test]
fn settings_before_the_first_host_apply_to_every_host() {
    let parsed = parse(
        "User everyone\nPort 2201\n\nHost web\n  HostName w.example.com\nHost db\n  Port 5432\n",
    );
    assert_eq!(host(&parsed, "web").user.as_deref(), Some("everyone"));
    assert_eq!(host(&parsed, "web").port, 2201);
    assert_eq!(host(&parsed, "db").port, 2201, "文件开头的设置在前，先取到");
}

#[test]
fn host_patterns_are_case_insensitive() {
    let parsed = parse("Host WEB\n  User a\nHost web\n  Port 2200\n");
    assert_eq!(aliases(&parsed), ["WEB", "web"]);
    assert_eq!(host(&parsed, "web").user.as_deref(), Some("a"));
}

// ---------- 解析：Include ----------

#[test]
fn include_with_wildcard_reads_files_in_name_order_relative_to_the_ssh_dir() {
    let parsed = parse_files(&[
        ("config", "Include conf.d/*\nHost last\n  User l\n"),
        ("conf.d/20-b", "Host b\n  User b\n"),
        ("conf.d/10-a", "Host a\n  User a\n"),
        ("conf.d/.hidden", "Host hidden\n"),
        ("conf.d/sub/deep", "Host deep\n"),
    ]);
    assert_eq!(
        aliases(&parsed),
        ["a", "b", "last"],
        "按文件名排序；隐藏文件和子目录不算"
    );
}

#[test]
fn include_accepts_several_arguments_absolute_and_tilde_paths() {
    let mut fs = fs_with(&[("config", "")]);
    let extra = PathBuf::from("/other/extra.conf");
    fs.files.insert(extra.clone(), "Host abs\n".to_owned());
    fs.files.insert(at("one"), "Host one\n".to_owned());
    fs.files
        .insert(home().join("cfg").join("two"), "Host two\n".to_owned());
    fs.files.insert(
        at("config"),
        format!(
            "Include one \"{}\" ~/cfg/two missing\nHost tail\n",
            extra.to_string_lossy().replace('\\', "/")
        ),
    );
    let parsed = parse_config(&fs, &at("config"), &options()).expect("读配置");
    assert_eq!(aliases(&parsed), ["one", "abs", "two", "tail"]);
    assert!(
        parsed.notices.is_empty(),
        "不存在的 Include 文件悄悄跳过：{:?}",
        parsed.notices
    );
}

#[test]
fn included_settings_sit_at_the_include_line() {
    let parsed = parse_files(&[
        ("config", "Host web\n  Include extra\n  User later\n"),
        ("extra", "User earlier\n"),
    ]);
    assert_eq!(
        host(&parsed, "web").user.as_deref(),
        Some("earlier"),
        "被包含文件里的设置接在 Include 那一行的位置"
    );
}

#[test]
fn a_host_line_inside_an_included_file_does_not_leak_back() {
    let parsed = parse_files(&[
        ("config", "Host a\n  Include inc\n  Port 2222\n"),
        ("inc", "Host x\n  User xu\n"),
    ]);
    assert_eq!(host(&parsed, "a").port, 2222);
    assert_eq!(
        host(&parsed, "x").port,
        22,
        "include 之后回到 a 的范围，Port 不属于 x"
    );
    assert_eq!(host(&parsed, "x").user.as_deref(), Some("xu"));
}

#[test]
fn an_include_loop_stops_with_a_notice() {
    let parsed = parse_files(&[
        ("config", "Host a\n  User a\nInclude b\n"),
        ("b", "Host b\n  User b\nInclude config\n"),
    ]);
    assert_eq!(aliases(&parsed), ["a", "b"]);
    assert!(parsed
        .notices
        .iter()
        .any(|notice| matches!(notice, ConfigNotice::IncludeLoop { file } if file == "config")));
}

#[test]
fn includes_nest_at_most_sixteen_deep() {
    // 主文件 + 16 层 Include 可以；再深一层停下并提示。
    let mut files: Vec<(String, String)> = vec![("config".into(), "Include f1\n".into())];
    for level in 1..=20 {
        files.push((
            format!("f{level}"),
            format!("Host h{level}\n  User u\nInclude f{}\n", level + 1),
        ));
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let parsed = parse_files(&borrowed);
    assert_eq!(parsed.hosts.len(), 16, "f1..f16 读到了");
    assert!(host_exists(&parsed, "h16") && !host_exists(&parsed, "h17"));
    assert!(parsed
        .notices
        .iter()
        .any(|notice| matches!(notice, ConfigNotice::IncludeTooDeep { file } if file == "f17")));
}

fn host_exists(parsed: &ParsedConfig, alias: &str) -> bool {
    parsed.hosts.iter().any(|host| host.alias == alias)
}

#[test]
fn an_unreadable_include_is_a_notice_not_a_failure() {
    let mut fs = fs_with(&[
        ("config", "Include secret\nHost web\n"),
        ("secret", "Host s\n"),
    ]);
    fs.denied.push(at("secret"));
    let parsed = parse_config(&fs, &at("config"), &options()).expect("读配置");
    assert_eq!(aliases(&parsed), ["web"]);
    assert!(matches!(
        parsed.notices.as_slice(),
        [ConfigNotice::IncludeUnreadable { file, .. }] if file == "secret"
    ));
}

#[test]
fn a_missing_main_file_is_reported_as_not_found() {
    let fs = MemFs::default();
    assert_eq!(
        parse_config(&fs, &at("config"), &options()),
        Err(ReadError::NotFound)
    );
}

#[test]
fn parsing_only_reads_the_config_and_what_it_includes() {
    // 只读：除了配置文件和被 Include 的文件，什么都不读（尤其不读 IdentityFile 指向的私钥）。
    let mut fs = fs_with(&[
        (
            "config",
            "Host web\n  IdentityFile ~/.ssh/id_ed25519\n  Include more\n",
        ),
        ("more", "Host extra\n"),
        ("id_ed25519", "-----BEGIN-----\n"),
    ]);
    fs.denied.push(at("id_ed25519"));
    let parsed = parse_config(&fs, &at("config"), &options()).expect("读配置");
    assert_eq!(parsed.hosts.len(), 2);
    let reads: Vec<String> = fs.reads.borrow().iter().map(|path| native(path)).collect();
    assert_eq!(reads, [native(&at("config")), native(&at("more"))]);
}

// ---------- 解析：Match、不支持的关键字、非法值 ----------

#[test]
fn a_match_block_is_skipped_until_the_next_host() {
    let parsed = parse(
        "Match host foo\n  User from_match\n  Port 9999\nHost web\n  HostName w.example.com\n",
    );
    let web = host(&parsed, "web");
    assert_eq!(
        (web.user.as_deref(), web.port),
        (None, 22),
        "Match 段里的设置不参与"
    );
    assert_eq!(
        parsed.notices,
        [ConfigNotice::MatchSkipped {
            file: "config".into(),
            line: 1
        }]
    );
}

#[test]
fn a_match_after_a_host_ends_that_hosts_settings() {
    let parsed = parse("Host web\n  User w\nMatch all\n  User z\n");
    assert_eq!(host(&parsed, "web").user.as_deref(), Some("w"));
    assert_eq!(parsed.notices.len(), 1);
}

#[test]
fn unsupported_keywords_are_listed_once_in_order_of_appearance() {
    let parsed = parse(
        "Host web\n  ProxyCommand ssh -W %h:%p gw\n  LocalForward 8080 localhost:80\n  ForwardAgent yes\n  proxycommand other\n\
         Host *\n  ServerAliveInterval 30\n  ForwardAgent no\n",
    );
    assert_eq!(
        host(&parsed, "web").unsupported,
        [
            "ProxyCommand",
            "LocalForward",
            "ForwardAgent",
            "ServerAliveInterval"
        ]
    );
}

#[test]
fn an_invalid_port_skips_that_host_with_the_reason() {
    let parsed =
        parse("Host a\n  Port abc\nHost b\n  Port 0\nHost c\n  Port 70000\nHost d\n  Port 22\n");
    assert_eq!(aliases(&parsed), ["d"]);
    assert_eq!(
        parsed.skipped,
        [
            SkippedEntry {
                alias: "a".into(),
                reason: SkipReason::BadPort("abc".into())
            },
            SkippedEntry {
                alias: "b".into(),
                reason: SkipReason::BadPort("0".into())
            },
            SkippedEntry {
                alias: "c".into(),
                reason: SkipReason::BadPort("70000".into())
            },
        ]
    );
}

#[test]
fn an_invalid_host_name_skips_that_host() {
    let parsed = parse("Host a\n  HostName \"bad name\"\nHost b\n  HostName ok.example.com\n");
    assert_eq!(aliases(&parsed), ["b"]);
    assert_eq!(
        parsed.skipped,
        [SkippedEntry {
            alias: "a".into(),
            reason: SkipReason::BadAddress("bad name".into())
        }]
    );
}

#[test]
fn percent_h_in_host_name_becomes_the_alias_and_other_escapes_stay() {
    let parsed =
        parse("Host web\n  HostName %h.example.com\nHost db\n  HostName db.%p.example.com\n");
    let web = host(&parsed, "web");
    assert_eq!(web.address, "web.example.com");
    assert!(web.notes.is_empty());
    let db = host(&parsed, "db");
    assert_eq!(db.address, "db.%p.example.com", "其他 % 转义原样保留");
    assert_eq!(db.notes, [HostNote::PercentInHostName]);
}

// ---------- 解析：IdentityFile 和 ProxyJump ----------

#[test]
fn identity_file_paths_expand_tilde_and_relative_to_the_ssh_dir() {
    let parsed = parse(
        "Host a\n  IdentityFile ~/keys/a\nHost b\n  IdentityFile keys/b\nHost c\n  IdentityFile /abs/c\n\
         Host d\n  IdentityFile %d/.ssh/d\n",
    );
    assert_eq!(
        host(&parsed, "a").identity_file.as_deref(),
        Some(native(&home().join("keys").join("a")).as_str())
    );
    assert_eq!(
        host(&parsed, "b").identity_file.as_deref(),
        Some(native(&ssh_dir().join("keys").join("b")).as_str()),
        "相对路径相对 ssh_dir"
    );
    assert_eq!(host(&parsed, "c").identity_file.as_deref(), Some("/abs/c"));
    let d = host(&parsed, "d");
    assert_eq!(
        d.identity_file.as_deref(),
        Some("%d/.ssh/d"),
        "% 变量不展开"
    );
    assert_eq!(d.notes, [HostNote::PercentInIdentityFile]);
}

#[test]
fn proxy_jump_values() {
    let parsed = parse(
        "Host a\n  ProxyJump bastion\nHost b\n  ProxyJump one,two\nHost c\n  ProxyJump one, two ,three\n\
         Host d\n  ProxyJump none\nHost e\n  ProxyJump=bastion\nHost f\n  User x\n",
    );
    assert_eq!(
        host(&parsed, "a").proxy_jump,
        ProxyJump::Hops(vec!["bastion".into()])
    );
    assert_eq!(
        host(&parsed, "b").proxy_jump,
        ProxyJump::Hops(vec!["one".into(), "two".into()])
    );
    assert_eq!(
        host(&parsed, "c").proxy_jump,
        ProxyJump::Hops(vec!["one".into(), "two".into(), "three".into()])
    );
    assert_eq!(host(&parsed, "d").proxy_jump, ProxyJump::Disabled);
    assert_eq!(
        host(&parsed, "e").proxy_jump,
        ProxyJump::Hops(vec!["bastion".into()])
    );
    assert_eq!(host(&parsed, "f").proxy_jump, ProxyJump::Unset);
}

#[test]
fn a_proxy_jump_none_in_front_blocks_a_later_host_star_jump() {
    let parsed = parse("Host direct\n  ProxyJump none\nHost *\n  ProxyJump gw\nHost gw\n  HostName g.example.com\n");
    assert_eq!(host(&parsed, "direct").proxy_jump, ProxyJump::Disabled);
    assert_eq!(
        host(&parsed, "gw").proxy_jump,
        ProxyJump::Hops(vec!["gw".into()]),
        "Host * 的 ProxyJump 也落到跳板机自己头上，后面计划阶段当成「绕成圈」处理"
    );
}

// ---------- 计划 ----------

fn ctx<'a>(
    hosts: &'a [crate::model::Host],
    identities: &'a [crate::model::Identity],
) -> ImportContext<'a> {
    ImportContext {
        hosts,
        identities,
        default_user: "localuser",
    }
}

fn rows_of(text: &str) -> Vec<ImportRow> {
    build_rows(&parse(text), &ctx(&[], &[]))
}

fn row_index(rows: &[ImportRow], alias: &str) -> usize {
    rows.iter()
        .position(|row| row.alias == alias)
        .unwrap_or_else(|| panic!("没有 {alias}"))
}

fn all_selected(rows: &[ImportRow]) -> Vec<bool> {
    vec![true; rows.len()]
}

#[test]
fn a_missing_user_falls_back_to_the_local_user_then_to_root() {
    let parsed = parse("Host a\n  User x\nHost b\n  HostName b.example.com\n");
    let rows = build_rows(&parsed, &ctx(&[], &[]));
    assert_eq!(
        (rows[0].username.as_str(), rows[0].user_defaulted),
        ("x", false)
    );
    assert_eq!(
        (rows[1].username.as_str(), rows[1].user_defaulted),
        ("localuser", true)
    );

    let none = ImportContext {
        default_user: "  ",
        ..ctx(&[], &[])
    };
    assert_eq!(
        build_rows(&parsed, &none)[1].username,
        "root",
        "拿不到本机用户名就用表单的默认 root"
    );
}

fn db_with_hosts(hosts: &[(&str, &str, u16, &str)]) -> Database {
    let db = Database::open_in_memory().expect("内存库");
    for (name, address, port, user) in hosts {
        db.host_save(&draft(name, address, *port, user, None))
            .expect("建主机");
    }
    db
}

fn draft(name: &str, address: &str, port: u16, user: &str, jump: Option<i64>) -> HostDraft {
    HostDraft {
        id: None,
        group_id: None,
        name: name.into(),
        host: address.into(),
        port,
        username: user.into(),
        auth_method: AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: jump,
        proxy_mode: ProxyMode::Default,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
        protocol: None,
    }
}

#[test]
fn an_existing_host_with_the_same_address_port_and_user_is_marked_and_unselected_by_default() {
    let db = db_with_hosts(&[
        ("old web", "Web.Example.com", 2222, "deploy"),
        ("other port", "db.example.com", 22, "deploy"),
    ]);
    let hosts = db.host_list().expect("列主机");
    let parsed = parse(
        "Host web\n  HostName web.example.com\n  User deploy\n  Port 2222\n\
         Host db\n  HostName db.example.com\n  User deploy\n  Port 2200\n\
         Host db2\n  HostName db.example.com\n  User other\n",
    );
    let rows = build_rows(&parsed, &ctx(&hosts, &[]));
    assert!(rows[0].exists, "地址（不分大小写）、端口、用户名都相同");
    assert!(!rows[1].exists, "端口不同");
    assert!(!rows[2].exists, "用户名不同");
    assert_eq!(default_selection(&rows), [false, true, true]);
}

#[test]
fn a_single_jump_host_in_the_same_import_is_created_first() {
    let rows = rows_of("Host target\n  User t\n  ProxyJump bastion\nHost bastion\n  User b\n");
    let (target, bastion) = (row_index(&rows, "target"), row_index(&rows, "bastion"));
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(plan.rows[target].jump, JumpTarget::Row(bastion));
    assert_eq!(plan.order, [bastion, target], "先建跳板机再建它");
}

#[test]
fn a_jump_host_that_is_not_selected_falls_back_to_the_existing_host_of_that_name() {
    let db = db_with_hosts(&[("bastion", "gw.example.com", 22, "b")]);
    let hosts = db.host_list().expect("列主机");
    let rows = build_rows(
        &parse("Host target\n  User t\n  ProxyJump bastion\nHost bastion\n  User b\n"),
        &ctx(&hosts, &[]),
    );
    let target = row_index(&rows, "target");
    let plan = plan_import(&rows, &[true, false], &ctx(&hosts, &[]));
    assert_eq!(
        plan.rows[target].jump,
        JumpTarget::Existing {
            id: hosts[0].id,
            name: "bastion".into()
        }
    );
}

#[test]
fn a_jump_host_nobody_has_is_reported_and_left_unset() {
    let rows = rows_of(
        "Host a\n  User x\n  ProxyJump admin@gw.example.com:2222\nHost b\n  User x\n  ProxyJump none\n\
         Host c\n  User x\n  ProxyJump skipped\nHost skipped\n  User s\n",
    );
    let selected = [true, true, true, false];
    let plan = plan_import(&rows, &selected, &ctx(&[], &[]));
    assert_eq!(plan.rows[0].jump, JumpTarget::None);
    assert_eq!(
        plan.rows[0].notes,
        [PlanNote::JumpNotFound("admin@gw.example.com:2222".into())]
    );
    assert_eq!(plan.rows[1].notes, [PlanNote::JumpDisabled]);
    assert_eq!(
        plan.rows[2].notes,
        [PlanNote::JumpNotSelected("skipped".into())]
    );
    assert_eq!(plan.rows[2].jump, JumpTarget::None);
}

#[test]
fn a_multi_hop_proxy_jump_is_chained_through_the_jump_hosts() {
    let rows = rows_of(
        "Host target\n  User t\n  ProxyJump first,second\nHost first\n  User f\nHost second\n  User s\n",
    );
    let (target, first, second) = (
        row_index(&rows, "target"),
        row_index(&rows, "first"),
        row_index(&rows, "second"),
    );
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(plan.rows[target].jump, JumpTarget::Row(second));
    assert_eq!(
        plan.rows[second].jump,
        JumpTarget::Row(first),
        "second 经由 first"
    );
    assert_eq!(plan.rows[first].jump, JumpTarget::None);
    assert_eq!(
        plan.rows[second].notes,
        [PlanNote::WiredAsJump {
            via: "first".into()
        }]
    );
    assert_eq!(plan.order, [first, second, target]);
}

#[test]
fn a_chain_that_clashes_with_a_jump_hosts_own_jump_keeps_only_the_last_hop() {
    let rows = rows_of(
        "Host target\n  User t\n  ProxyJump first,second\nHost first\n  User f\n\
         Host second\n  User s\n  ProxyJump other\nHost other\n  User o\n",
    );
    let (target, second, other) = (
        row_index(&rows, "target"),
        row_index(&rows, "second"),
        row_index(&rows, "other"),
    );
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(plan.rows[target].jump, JumpTarget::Row(second));
    assert_eq!(
        plan.rows[second].jump,
        JumpTarget::Row(other),
        "second 自己的跳板机不动"
    );
    assert_eq!(
        plan.rows[target].notes,
        [PlanNote::ChainTruncated(ChainIssue::Conflict(
            "second".into()
        ))]
    );
}

#[test]
fn a_chain_with_a_missing_earlier_hop_keeps_only_the_last_hop() {
    let rows =
        rows_of("Host target\n  User t\n  ProxyJump nowhere,second\nHost second\n  User s\n");
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(plan.rows[0].jump, JumpTarget::Row(1));
    assert_eq!(
        plan.rows[0].notes,
        [PlanNote::ChainTruncated(ChainIssue::HopMissing(
            "nowhere".into()
        ))]
    );
    assert_eq!(
        plan.rows[1].jump,
        JumpTarget::None,
        "没有串链，second 不被改动"
    );
}

#[test]
fn jump_loops_are_broken_at_the_first_member() {
    let rows = rows_of("Host a\n  User x\n  ProxyJump b\nHost b\n  User x\n  ProxyJump a\n");
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(plan.rows[0].jump, JumpTarget::None);
    assert_eq!(plan.rows[0].notes, [PlanNote::JumpLoop]);
    assert_eq!(
        plan.rows[1].jump,
        JumpTarget::Row(0),
        "断开一处，另一处保留"
    );
    assert_eq!(plan.order, [0, 1]);
}

#[test]
fn a_host_star_proxy_jump_does_not_make_the_jump_host_its_own_jump() {
    let rows =
        rows_of("Host *\n  User x\n  ProxyJump gw\nHost gw\n  HostName g.example.com\nHost web\n");
    let (gw, web) = (row_index(&rows, "gw"), row_index(&rows, "web"));
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(
        plan.rows[gw].jump,
        JumpTarget::None,
        "自己当自己的跳板机：断开"
    );
    assert_eq!(plan.rows[gw].notes, [PlanNote::JumpLoop]);
    assert_eq!(plan.rows[web].jump, JumpTarget::Row(gw));
}

#[test]
fn chains_deeper_than_the_dial_limit_lose_their_jump() {
    // h0 直连，h1 经 h0，…… h5 的拨号链是 6 跳（含自己），超过上限 5。
    let mut text = String::new();
    for level in 0..6 {
        text.push_str(&format!("Host h{level}\n  User u\n"));
        if level > 0 {
            text.push_str(&format!("  ProxyJump h{}\n", level - 1));
        }
    }
    let rows = rows_of(&text);
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &[]));
    assert_eq!(plan.rows[4].jump, JumpTarget::Row(3), "5 跳刚好够");
    assert_eq!(plan.rows[5].jump, JumpTarget::None);
    assert_eq!(plan.rows[5].notes, [PlanNote::JumpTooDeep]);
}

#[test]
fn identities_are_shared_by_path_and_renamed_on_collision() {
    let db = Database::open_in_memory().expect("内存库");
    let existing = db
        .identity_save(&IdentityDraft {
            id: None,
            name: "id_ed25519".into(),
            kind: IdentityKind::KeyFile,
            key_path: Some(native(&home().join("keys").join("known"))),
            secret: None,
            passphrase: None,
        })
        .expect("建凭据");
    let identities = db.identity_list().expect("列凭据");
    let rows = rows_of(
        "Host a\n  User x\n  IdentityFile ~/keys/known\nHost b\n  User x\n  IdentityFile ~/keys/new/id_ed25519\n\
         Host c\n  User x\n  IdentityFile ~/keys/new/id_ed25519\nHost d\n  User x\n  IdentityFile ~/other/id_ed25519\n\
         Host e\n  User x\n",
    );
    let plan = plan_import(&rows, &all_selected(&rows), &ctx(&[], &identities));
    assert_eq!(plan.rows[0].auth, PlanAuth::ExistingKey(existing.id));
    assert_eq!(plan.rows[1].auth, PlanAuth::NewKey(0));
    assert_eq!(plan.rows[2].auth, PlanAuth::NewKey(0), "同一个路径只建一条");
    assert_eq!(plan.rows[3].auth, PlanAuth::NewKey(1));
    assert_eq!(
        plan.rows[4].auth,
        PlanAuth::Password,
        "没写 IdentityFile：和表单不选凭据一致"
    );
    let names: Vec<&str> = plan
        .identities
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["id_ed25519 (2)", "id_ed25519 (3)"],
        "和已有凭据重名加后缀"
    );
}

#[test]
fn unselected_rows_create_no_identity() {
    let rows = rows_of(
        "Host a\n  User x\n  IdentityFile ~/keys/a\nHost b\n  User x\n  IdentityFile ~/keys/b\n",
    );
    let plan = plan_import(&rows, &[false, true], &ctx(&[], &[]));
    assert_eq!(plan.identities.len(), 1);
    assert_eq!(plan.order, [1]);
}

// ---------- 落库 ----------

fn import_all(db: &Database, text: &str) -> (Vec<ImportRow>, ImportPlan, ImportOutcome) {
    let hosts = db.host_list().expect("列主机");
    let identities = db.identity_list().expect("列凭据");
    let context = ctx(&hosts, &identities);
    let rows = build_rows(&parse(text), &context);
    let selected = default_selection(&rows);
    let plan = plan_import(&rows, &selected, &context);
    let outcome = db.import_ssh_hosts(&rows, &plan, None).expect("导入");
    (rows, plan, outcome)
}

#[test]
fn imports_hosts_keys_and_jump_chains_into_the_database() {
    let db = Database::open_in_memory().expect("内存库");
    let (_, _, outcome) = import_all(
        &db,
        "Host target\n  HostName t.example.com\n  User t\n  Port 2200\n  IdentityFile ~/keys/t\n  ProxyJump first,second\n\
         Host first\n  HostName f.example.com\n  User f\nHost second\n  HostName s.example.com\n  User s\n  IdentityFile ~/keys/t\n",
    );
    assert_eq!(outcome.hosts_created, 3);
    assert_eq!(outcome.identities_created, 1, "同一把钥匙只建一条凭据");

    let hosts = db.host_list().expect("列主机");
    let by_name = |name: &str| hosts.iter().find(|host| host.name == name).expect(name);
    let target = by_name("target");
    assert_eq!(
        (target.host.as_str(), target.port, target.username.as_str()),
        ("t.example.com", 2200, "t")
    );
    assert_eq!(target.auth_method, AuthMethod::Key);
    assert_eq!(
        target.proxy_mode,
        ProxyMode::None,
        "设了跳板机的主机不用代理"
    );
    assert_eq!(by_name("first").auth_method, AuthMethod::Password);
    assert_eq!(by_name("first").identity_id, None);
    assert_eq!(
        by_name("first").proxy_mode,
        ProxyMode::Default,
        "直连的主机跟随默认代理"
    );

    let chain: Vec<String> = db
        .host_chain(target.id)
        .expect("拨号链")
        .into_iter()
        .map(|host| host.name)
        .collect();
    assert_eq!(chain, ["first", "second", "target"], "目标→second→first");

    let identities = db.identity_list().expect("列凭据");
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].kind, IdentityKind::KeyFile);
    assert_eq!(identities[0].name, "t");
    assert_eq!(
        identities[0].key_path.as_deref(),
        Some(native(&home().join("keys").join("t")).as_str())
    );
    assert!(!identities[0].has_secret, "只记路径，没有存任何私钥内容");
}

#[test]
fn importing_again_reuses_the_key_credential_and_marks_everything_as_existing() {
    let db = Database::open_in_memory().expect("内存库");
    let text = "Host web\n  HostName w.example.com\n  User u\n  IdentityFile ~/keys/k\n";
    import_all(&db, text);
    let hosts = db.host_list().expect("列主机");
    let identities = db.identity_list().expect("列凭据");
    let rows = build_rows(&parse(text), &ctx(&hosts, &identities));
    assert!(rows[0].exists);
    // 用户手动勾上，照样新建一台；凭据按路径复用。
    let plan = plan_import(&rows, &[true], &ctx(&hosts, &identities));
    let outcome = db.import_ssh_hosts(&rows, &plan, None).expect("导入");
    assert_eq!((outcome.hosts_created, outcome.identities_created), (1, 0));
    assert_eq!(db.host_list().expect("列主机").len(), 2);
    assert_eq!(db.identity_list().expect("列凭据").len(), 1);
}

#[test]
fn imported_hosts_go_into_the_chosen_group() {
    let db = Database::open_in_memory().expect("内存库");
    let group = db
        .group_save(&crate::model::GroupDraft {
            id: None,
            name: "from ssh".into(),
            parent_id: None,
            sort: 0,
        })
        .expect("建分组");
    let context = ctx(&[], &[]);
    let rows = build_rows(&parse("Host a\n  User x\nHost b\n  User x\n"), &context);
    let plan = plan_import(&rows, &all_selected(&rows), &context);
    db.import_ssh_hosts(&rows, &plan, Some(group.id))
        .expect("导入");
    assert!(db
        .host_list()
        .expect("列主机")
        .iter()
        .all(|host| host.group_id == Some(group.id)));
}

#[test]
fn a_failure_halfway_rolls_back_every_host_and_credential() {
    let db = Database::open_in_memory().expect("内存库");
    let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = notified.clone();
    db.set_change_listener(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let context = ctx(&[], &[]);
    let mut rows = build_rows(
        &parse(
            "Host a\n  User x\n  IdentityFile ~/keys/a\nHost b\n  User x\n  ProxyJump a\nHost c\n  User x\n",
        ),
        &context,
    );
    let plan = plan_import(&rows, &all_selected(&rows), &context);
    // 最后一台的端口不合法（计划之后才出的问题）：前面已经建好的都要撤销。
    rows[2].port = 0;
    let err = db.import_ssh_hosts(&rows, &plan, None);
    assert!(err.is_err(), "最后一台建失败，整体应该失败");
    assert!(db.host_list().expect("列主机").is_empty(), "一台都不留");
    assert!(
        db.identity_list().expect("列凭据").is_empty(),
        "凭据也一起撤销"
    );
    assert_eq!(
        notified.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "失败不通知同步"
    );

    // 成功的一次只通知一次（不是每台一次）。
    rows[2].port = 22;
    db.import_ssh_hosts(&rows, &plan, None).expect("导入");
    assert_eq!(db.host_list().expect("列主机").len(), 3);
    assert_eq!(notified.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_missing_group_fails_the_whole_import() {
    let db = Database::open_in_memory().expect("内存库");
    let context = ctx(&[], &[]);
    let rows = build_rows(
        &parse("Host a\n  User x\n  IdentityFile ~/keys/a\n"),
        &context,
    );
    let plan = plan_import(&rows, &all_selected(&rows), &context);
    assert!(db.import_ssh_hosts(&rows, &plan, Some(9999)).is_err());
    assert!(db.host_list().expect("列主机").is_empty());
    assert!(db.identity_list().expect("列凭据").is_empty());
}

// ---------- 通配符 ----------

#[test]
fn glob_matching_covers_star_question_mark_and_backtracking() {
    use super::syntax::glob_match;
    assert!(glob_match("*", ""));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("web-?", "web-1"));
    assert!(!glob_match("web-?", "web-10"));
    assert!(glob_match("*.example.com", "a.b.example.com"));
    assert!(!glob_match("*.example.com", "example.com"));
    assert!(glob_match("a*b*c", "aXXbYYbZc"), "要能回溯");
    assert!(!glob_match("a*b*c", "aXXbYY"));
    assert!(glob_match("WEB*", "web-prod"), "不分大小写");
    // 病态模式不会指数级回溯。
    let pattern = format!("{}b", "a*".repeat(30));
    assert!(!glob_match(&pattern, &"a".repeat(60)));
}

// ---------- 用户选的文件：`~` 是真实的家目录 ----------

#[test]
fn a_picked_file_expands_tilde_to_the_real_home_not_to_its_own_folder() {
    // 配置副本放在家目录之外（比如桌面或备份盘），里面写的 `~/.ssh/...` 仍然指向真实的 ~/.ssh。
    let picked = PathBuf::from("/backup/ssh-copy/config");
    let options = picked_config_location(&picked, Some(home()));
    assert_eq!(
        options.ssh_dir,
        PathBuf::from("/backup/ssh-copy"),
        "相对路径以文件所在目录为基准"
    );
    assert_eq!(options.home, home());

    let mut fs = MemFs::default();
    fs.files.insert(
        picked.clone(),
        "Include ~/.ssh/conf.d/*\nInclude local\nHost web\n  IdentityFile ~/.ssh/id_ed25519\n  IdentityFile keys/other\n"
            .to_owned(),
    );
    fs.files
        .insert(at("conf.d/more"), "Host from-home\n  User h\n".to_owned());
    fs.files.insert(
        PathBuf::from("/backup/ssh-copy/local"),
        "Host from-copy\n".to_owned(),
    );
    let parsed = parse_config(&fs, &picked, &options).expect("读配置");
    assert_eq!(aliases(&parsed), ["from-home", "from-copy", "web"]);
    assert_eq!(
        host(&parsed, "web").identity_file.as_deref(),
        Some(native(&home().join(".ssh").join("id_ed25519")).as_str()),
        "私钥路径指向真实家目录，不是配置副本旁边的 .ssh"
    );
}

#[test]
fn a_picked_file_without_a_known_home_falls_back_to_its_own_folder() {
    let picked = PathBuf::from("/backup/ssh-copy/config");
    for home in [None, Some(PathBuf::new())] {
        let options = picked_config_location(&picked, home);
        assert_eq!(options.home, PathBuf::from("/backup/ssh-copy"));
    }
}

#[test]
fn the_sandbox_keeps_tilde_inside_the_files_own_folder() {
    let options = sandboxed_config_location(Path::new("/dev/data/config"));
    assert_eq!(
        (options.ssh_dir, options.home),
        (PathBuf::from("/dev/data"), PathBuf::from("/dev/data")),
        "开发钩子的沙箱：不会顺着 ~ 去读真实的家目录"
    );
}
