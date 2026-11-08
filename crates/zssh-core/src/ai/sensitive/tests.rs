use super::*;

fn path_hit(path: &str) -> bool {
    sensitive_file_path(path).is_some()
}

fn cmd_hit(command: &str) -> Option<SensitiveHit> {
    sensitive_path_in_command(command)
}

/// 表里的每一项：`~`、`$HOME`、`${HOME}`、绝对路径几种写法都要命中，大小写不敏感。
#[test]
fn every_entry_of_the_table_is_hit_in_each_path_form() {
    let tail_forms = [
        // 目录类（目录本身和里面的文件）
        ".ssh",
        ".ssh/id_rsa",
        ".ssh/config",
        ".gnupg/private-keys-v1.d/x.key",
        ".gnupg",
        ".aws/credentials",
        ".aws",
        ".azure/accessTokens.json",
        ".config/gcloud/credentials.db",
        ".config/gcloud",
        // 文件类
        ".kube/config",
        ".docker/config.json",
        ".netrc",
        ".pgpass",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
    ];
    for tail in tail_forms {
        for prefix in [
            "~/",
            "$HOME/",
            "${HOME}/",
            "/home/alice/",
            "/root/",
            "./",
            "",
        ] {
            let path = format!("{prefix}{tail}");
            assert!(path_hit(&path), "应该命中：{path}");
            assert!(path_hit(&path.to_uppercase()), "大小写不敏感：{path}");
        }
    }
    for path in [
        "/etc/shadow",
        "/etc/shadow-",
        "/etc/gshadow",
        "/etc/sudoers",
        "/etc/sudoers.d/90-cloud-init",
        "/ETC/SHADOW",
        "/mnt/backup/etc/shadow",
    ] {
        assert!(path_hit(path), "应该命中：{path}");
    }
}

#[test]
fn file_name_rules_cover_env_extensions_and_key_prefixes() {
    for path in [
        ".env",
        "/srv/app/.env",
        "/srv/app/.env.production",
        "~/proj/.ENV.local",
        "/etc/ssl/private/site.pem",
        "/etc/ssl/private/site.PEM",
        "/srv/tls/server.key",
        "/srv/tls/client.p12",
        "/srv/tls/client.pfx",
        "/home/u/vault.kdbx",
        "/srv/backup/id_rsa",
        "id_ed25519",
        "/srv/x/id_ecdsa.bak",
        "/srv/x/id_dsa",
        "/srv/x/id_rsa.pub",
    ] {
        assert!(path_hit(path), "应该命中：{path}");
    }
}

#[test]
fn ordinary_paths_are_not_hit() {
    for path in [
        "/etc/hosts",
        "/etc/passwd",
        "/etc/ssh/sshd_config",
        "/var/log/syslog",
        "~/.bashrc",
        "~/notes.txt",
        "keyboard.txt",
        "/srv/docs/keyboard.txt",
        "/srv/docs/environment.md",
        "/srv/app/.envrc",
        "/srv/app/env",
        "/srv/app/monkey.txt",
        "/srv/app/pem.txt",
        "/srv/ssh/readme",
        "/home/u/.sshd-notes/readme",
        "/home/u/my.ssh/readme",
        "/home/u/awsome/x",
        "/opt/gnupg-docs/readme",
        "/etc/shadowsocks.json.d",
        "relative/dir/file.txt",
        "",
        "/",
    ] {
        assert!(!path_hit(path), "不该命中：{path}");
    }
}

#[test]
fn commands_reading_sensitive_files_are_hit_with_the_path_shown() {
    let hit = cmd_hit("cat ~/.ssh/id_rsa").expect("命中");
    assert_eq!(hit.path, "~/.ssh/id_rsa");
    assert!(hit.exact);
    assert_eq!(hit.reason().text(), "读取敏感文件：~/.ssh/id_rsa");

    for command in [
        "base64 ~/.aws/credentials",
        "head -n 5 \"$HOME/.ssh/id_rsa\"",
        "cat < ~/.ssh/id_rsa",
        "cat </etc/shadow",
        "grep -r token ~/.aws",
        "cat '/home/u/.kube/config'",
        "cat ~/.ss\"\"h/id_rsa",
        r"cat ~/.ss\h/id_rsa",
        "cd ~/.ssh && cat id_rsa",
        "ls -la /home/u/.gnupg",
        "tail -n 3 /srv/app/.env",
        "cat --file=/etc/shadow",
        "strings /srv/tls/server.key | head",
        "wc -c /etc/sudoers",
        "sort /etc/sudoers.d/90-x",
        "cat /etc/hosts /etc/gshadow",
        "cat ~/.docker/config.json",
        "cat ~/.config/gcloud/credentials.db",
        "cat ~/.git-credentials ~/.netrc",
    ] {
        assert!(cmd_hit(command).is_some(), "应该命中：{command}");
    }
}

#[test]
fn wildcards_and_variables_that_may_reach_a_sensitive_file_are_hit() {
    for command in [
        "cat ~/.s*/*",
        "cat ~/.ss?/id_rsa",
        "cat ~/.[s]sh/id_rsa",
        "cat /home/*/.ssh/id_rsa",
        "cat /etc/sh*",
        "cat id_*",
        "cat ~/.ssh/id_*",
        "cat *.pem",
        "cat ~/{.ssh,.cache}/id_rsa",
        "cat ~/{a,b}/{c,.aws}/credentials",
        "cat $KEYFILE",
        "cat \"$KEYFILE\"",
        "head ${F}",
        "cat /srv/$APP/config",
        "cat ~/{1..3}",
    ] {
        let hit = cmd_hit(command).unwrap_or_else(|| panic!("应该命中：{command}"));
        let _ = hit.reason();
    }
    // 含通配符 / 变量、没法确定具体文件的，原因里要说明。
    let hit = cmd_hit("cat ~/.s*/*").expect("命中");
    assert!(!hit.exact);
    assert!(hit.reason().text().contains("无法确定"), "{}", hit.reason());
    let exact = cmd_hit("cat ~/.ssh/id_rsa").expect("命中");
    assert!(!exact.reason().text().contains("无法确定"));
}

#[test]
fn ordinary_read_only_commands_are_not_hit() {
    for command in [
        "cat /etc/hosts",
        "cat /etc/passwd",
        "ls -la /var/log",
        "ls *",
        "ls /etc/*",
        "grep -r error /var/log/*.log",
        "tail -n 100 /var/log/syslog",
        "head -n 5 keyboard.txt",
        "cat environment.md",
        "cat $HOME/notes.txt",
        "cat ${HOME}/notes.txt",
        "cat ~/notes.txt",
        "grep -E 'error$' /var/log/syslog",
        "grep 'a$|b$' /var/log/syslog",
        "grep \"^root:\" /etc/passwd",
        "df -h | grep sda",
        "ps aux | grep ssh",
        "ls /srv/{a,b}",
        "echo ~/.ssh/id_rsa",
        "printf '%s' /etc/shadow",
        "uname -a",
        "journalctl -u sshd -n 20",
        "ss -tlnp",
        "ls ~/.config/",
        "ls ~/.cache",
    ] {
        assert!(cmd_hit(command).is_none(), "不该命中：{command}");
    }
}

#[test]
fn braces_expand_up_to_a_limit() {
    assert_eq!(
        expand_braces("a{b,c}d").expect("展开"),
        vec!["abd".to_owned(), "acd".to_owned()]
    );
    assert_eq!(expand_braces("plain").expect("无大括号"), vec!["plain"]);
    assert!(expand_braces("{1..3}").is_none());
    assert!(expand_braces("{unclosed,a").is_none());
    let big = format!("{}{}", "{a,b}".repeat(7), "");
    assert!(expand_braces(&big).is_none(), "2^7 > 64");
}

#[test]
fn glob_matcher_basics() {
    assert!(glob_match(".s*", ".ssh"));
    assert!(glob_match("id_*", "id_rsa"));
    assert!(glob_match("?sh", "ssh"));
    assert!(glob_match(".[s]sh", ".ssh"));
    assert!(!glob_match(".s*", "ssh"));
    assert!(!glob_match("id_?", "id_rsa"));
}

#[test]
fn dot_dot_segments_cannot_hide_a_sensitive_path() {
    for path in [
        "/etc/x/../shadow",
        "/home/u/a/../.ssh/id_rsa",
        "/etc/./shadow",
        "~/./.ssh/../.ssh/config",
    ] {
        assert!(path_hit(path), "应该命中：{path}");
    }
    assert!(
        !path_hit("/home/u/.ssh/../notes.txt"),
        "折叠后已经离开 .ssh"
    );
    assert!(cmd_hit("cat /etc/x/../shadow").is_some());
}

/// V2-C7a S5：补上审查指出的几类凭据 / 秘密文件。
#[test]
fn sshd_host_private_keys_are_hit_but_the_public_ones_are_not() {
    for path in [
        "/etc/ssh/ssh_host_rsa_key",
        "/etc/ssh/ssh_host_ed25519_key",
        "/etc/ssh/ssh_host_ecdsa_key",
        "/etc/ssh/ssh_host_dsa_key",
        "/etc/ssh/SSH_HOST_RSA_KEY",
        "/etc/ssh/ssh_host_rsa_key.bak",
        "/mnt/backup/etc/ssh/ssh_host_ed25519_key-old",
        "ssh_host_ed25519_key",
    ] {
        assert!(path_hit(path), "应该命中：{path}");
    }
    for path in [
        "/etc/ssh/ssh_host_rsa_key.pub",
        "/etc/ssh/ssh_host_ed25519_key.pub",
        "/etc/ssh/sshd_config",
        "/etc/ssh/ssh_config",
        "/etc/ssh/moduli",
        "/etc/ssh/ssh_host_notes.txt",
    ] {
        assert!(!path_hit(path), "不该命中：{path}");
    }
    let hit = cmd_hit("cat /etc/ssh/ssh_host_ed25519_key").expect("命令里的主机私钥");
    assert!(hit.exact);
    assert!(cmd_hit("cat /etc/ssh/ssh_host_ed25519_key.pub").is_none());
    // 通配符能匹配到主机私钥的名字，按"可能命中"。
    assert!(cmd_hit("cat /etc/ssh/ssh_host_*_key").is_some_and(|hit| !hit.exact));
    assert!(cmd_hit("cat /etc/ssh/ssh_host_*").is_some_and(|hit| !hit.exact));
}

#[test]
fn proc_environ_files_are_hit_in_every_form() {
    for path in [
        "/proc/self/environ",
        "/proc/1/environ",
        "/proc/12345/environ",
        "/proc/thread-self/environ",
        "/proc/self/task/42/environ",
        "/PROC/SELF/ENVIRON",
        "/proc/./self/environ",
        "/proc/self/../1/environ",
        "/host/proc/1/environ",
    ] {
        assert!(path_hit(path), "应该命中：{path}");
    }
    // 其它 /proc 文件和名字相近的文件不受影响。
    for path in [
        "/proc/self/status",
        "/proc/self/cmdline",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/1/environment",
        "/proc/self/environ.txt",
        "/srv/docs/environ",
        "/srv/proc/notes/environ",
        "/proc/net/environ",
    ] {
        assert!(!path_hit(path), "不该命中：{path}");
    }
    assert!(cmd_hit("cat /proc/self/environ").is_some_and(|hit| hit.exact));
    assert!(cmd_hit("tr '\0' '\n' < /proc/1/environ").is_some_and(|hit| hit.exact));
    // 进程号是通配符 / 变量：无法确定，按可能命中。
    assert!(cmd_hit("cat /proc/*/environ").is_some_and(|hit| !hit.exact));
    assert!(cmd_hit("cat /proc/$PID/environ").is_some_and(|hit| !hit.exact));
    assert!(cmd_hit("cat /proc/self/status").is_none());
}

#[test]
fn shell_and_database_history_files_are_hit() {
    for name in [
        ".bash_history",
        ".zsh_history",
        ".mysql_history",
        ".psql_history",
        ".python_history",
    ] {
        for prefix in ["~/", "$HOME/", "/home/alice/", "/root/", "./", ""] {
            let path = format!("{prefix}{name}");
            assert!(path_hit(&path), "应该命中：{path}");
            assert!(path_hit(&path.to_uppercase()), "大小写不敏感：{path}");
        }
        assert!(path_hit(&format!("/root/{name}.bak")), "备份：{name}");
    }
    assert!(cmd_hit("tail -n 50 ~/.bash_history").is_some_and(|hit| hit.exact));
    assert!(cmd_hit("grep password ~/.mysql_history").is_some());
    for path in [
        "/root/bash_history_notes.txt",
        "/home/alice/.bashrc",
        "/home/alice/.profile",
        "/home/alice/history.md",
        "/var/log/history.log",
        "/home/alice/.bash_historyx",
    ] {
        assert!(!path_hit(path), "不该命中：{path}");
    }
    // `history` 命令本身不是读文件（路径检查不管它），但它打印的就是 shell 历史，按命令来源命中
    // （见 sources.rs）。
    assert_eq!(
        cmd_hit("history").map(|hit| hit.kind),
        Some(SensitiveKind::ShellHistory)
    );
}

#[test]
fn vault_tokens_and_cloud_credential_files_are_hit() {
    for path in [
        "~/.vault-token",
        "/root/.vault-token",
        "$HOME/.vault-token",
        "/srv/app/credentials.json",
        "credentials.json",
        "/srv/app/CREDENTIALS.JSON",
        "/srv/app/credentials.json.bak",
        "/srv/gcp/service-account.json",
        "/srv/gcp/service-account-prod.json",
        "/srv/gcp/service-account-key-2026.json",
        "/srv/gcp/service_account.json",
    ] {
        assert!(path_hit(path), "应该命中：{path}");
    }
    for path in [
        "/srv/app/credentials.txt",
        "/srv/app/credentials.jsonl-notes.md",
        "/srv/app/my-credentials.json",
        "/srv/gcp/service-accounts.md",
        "/srv/gcp/service-account.yaml",
        "/srv/app/package.json",
        "/srv/app/vault-token-howto.md",
    ] {
        assert!(!path_hit(path), "不该命中：{path}");
    }
    assert!(cmd_hit("cat ~/.vault-token").is_some_and(|hit| hit.exact));
    assert!(cmd_hit("cat service-account.json").is_some_and(|hit| hit.exact));
    // 名字里带通配符、又能匹配到这些文件：按可能命中。
    assert!(cmd_hit("cat service-account*.json").is_some_and(|hit| !hit.exact));
    assert!(cmd_hit("cat cred*.json").is_some_and(|hit| !hit.exact));
}
