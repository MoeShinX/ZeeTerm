//! W02：参数里没有敏感路径、输出里却有秘密的读取（getent shadow、jq env、history、
//! systemctl show-environment、递归搜索家目录……）必须"不自动执行 + 带敏感原因"。
//! 只测准入函数，不执行任何命令。

use super::*;
use crate::ai::readonly::is_read_only;
use crate::ai::reason::ApprovalReason;
use crate::ai::safety::{assess, DangerLevel};
use crate::error::reason_keys as rk;

/// 「只读自动」模式下命令会不会不问就执行（和桥里 `decide_with` 的条件一致）。
fn auto_runs(command: &str) -> bool {
    assess(command).level == DangerLevel::Safe
        && is_read_only(command)
        && sensitive_path_in_command(command).is_none()
}

fn kind_of(command: &str) -> Option<SensitiveKind> {
    sensitive_path_in_command(command).map(|hit| hit.kind)
}

#[test]
fn account_databases_other_than_the_public_ones_need_approval() {
    for command in [
        "getent shadow",
        "getent gshadow",
        "getent shadow root",
        "getent -s files shadow",
        "getent --service=files shadow root",
        "getent -i shadow",
        "getent SHADOW",
        "getent aliases",
        "getent sudoers",
        "getent not-a-database",
        "/usr/bin/getent shadow",
        "ls && getent gshadow",
    ] {
        assert!(!auto_runs(command), "不该自动执行：{command}");
        assert_eq!(
            kind_of(command),
            Some(SensitiveKind::AccountDatabase),
            "{command}"
        );
    }
}

#[test]
fn public_getent_databases_still_run_unasked() {
    for command in [
        "getent passwd",
        "getent passwd root",
        "getent group",
        "getent group docker",
        "getent hosts example.com",
        "getent ahosts example.com",
        "getent ahostsv4 example.com",
        "getent ahostsv6 example.com",
        "getent services ssh",
        "getent protocols tcp",
        "getent networks",
        "getent rpc",
        "getent ethers",
        "getent netgroup",
        "getent initgroups root",
        "getent -s files passwd",
        "getent --service=files passwd",
        "getent -i hosts example.com",
    ] {
        assert!(auto_runs(command), "应该自动执行：{command}");
    }
    // 主机解析会发 DNS 查询：参数里有展开就不自动执行，和 ping / dig 一致
    assert!(!is_read_only("getent hosts $SOME_VAR.example.com"));
}

#[test]
fn jq_filters_that_reach_the_environment_need_approval() {
    for command in [
        "jq -n env",
        "jq -n '$ENV'",
        "jq -n 'env.PATH'",
        "jq -n '$ENV.PATH'",
        "jq -n '[env|keys]'",
        "jq -nr 'env|to_entries[]|.value'",
        "jq -n '\"\\(env)\"'",
        "jq -n '$__prog_args' --args a b",
        "jq -n 'input_filename'",
        "jq -n 'include \"x\"; .'",
        "jq -n 'import \"x\" as x; .'",
        "jq -n '. as $x | env'",
        "jq env",
        "jq --null-input env",
        "jq -f prog.jq data.json",
        "jq --from-file prog.jq data.json",
        "jq -nf prog.jq",
        "jq --unknown-option . x.json",
        "jq -n env | head",
    ] {
        assert!(!auto_runs(command), "不该自动执行：{command}");
        assert_eq!(
            kind_of(command),
            Some(SensitiveKind::Environment),
            "{command}"
        );
    }
}

#[test]
fn ordinary_jq_runs_unasked() {
    for command in [
        "jq . file.json",
        "jq .foo file.json",
        "jq -r '.name' package.json",
        "jq '.[] | select(.id == 1)' data.json",
        "jq -n '1+1'",
        "jq -c '.items[]' a.json",
        "jq -S . a.json",
        "jq -s . a.json b.json",
        "jq --slurp . a.json",
        "jq --arg x 1 'select(.a == $x)' a.json",
        "jq -r --arg k name 'select(.a == $k)' a.json",
        "jq --argjson n 3 'select(.a == $n)' a.json",
        "jq '.environment' a.json",
        "jq '.a.env' a.json",
        "jq 'map(.version)' a.json",
        "jq --indent 4 . a.json",
        "docker ps | jq .",
    ] {
        assert!(auto_runs(command), "应该自动执行：{command}");
    }
    // 过滤器要 shell 展开才知道内容：不自动执行（路径检查先按"可能命中"拦下）
    assert!(!auto_runs("jq -n $FILTER"));
    assert!(!is_read_only("jq -n $FILTER"));
    // 读文件本身仍走敏感路径检查
    assert!(!auto_runs("jq . ~/.aws/credentials"));
    assert_eq!(
        kind_of("jq . ~/.aws/credentials"),
        Some(SensitiveKind::Path)
    );
}

#[test]
fn shell_history_needs_approval() {
    for command in [
        "history",
        "history 20",
        "fc -l",
        "fc -ln",
        "fc -lr 1 20",
        "fc -l -20",
        "ls && history",
        "history | grep ssh",
    ] {
        assert!(!auto_runs(command), "不该自动执行：{command}");
        assert_eq!(
            kind_of(command),
            Some(SensitiveKind::ShellHistory),
            "{command}"
        );
    }
    // 单独的 is_read_only 也不再放行
    assert!(!is_read_only("history"));
    assert!(!is_read_only("fc -l"));
}

#[test]
fn systemd_environment_and_unit_dumps_need_approval() {
    for command in [
        "systemctl --user show-environment",
        "systemctl show-environment",
        "systemctl cat sample.service",
        "systemctl --user cat sample.service",
        "systemctl show sample.service",
        "systemctl show",
        "systemctl -p Environment show sample.service",
        "systemctl show --property=Environment sample.service",
        "systemctl --no-pager cat sample.service",
    ] {
        assert!(!auto_runs(command), "不该自动执行：{command}");
        assert_eq!(
            kind_of(command),
            Some(SensitiveKind::Environment),
            "{command}"
        );
        assert!(!is_read_only(command), "{command}");
    }
    for command in [
        "systemctl status sample.service",
        "systemctl --user status sample.service",
        "systemctl is-active sample.service",
        "systemctl is-enabled sample.service",
        "systemctl list-units --type=service",
        "systemctl list-unit-files",
        "systemctl list-timers",
    ] {
        assert!(auto_runs(command), "应该自动执行：{command}");
    }
}

#[test]
fn recursive_searches_over_ancestors_of_sensitive_paths_need_approval() {
    for command in [
        "grep -r -A40 'PRIVATE KEY' /home",
        "grep -r 'PRIVATE KEY' /home/",
        "grep -R pass /",
        "grep --recursive x /root",
        "grep -rn x ~",
        "grep -rn x ~/",
        "grep -r x $HOME",
        "grep -r x ${HOME}",
        "grep -r x ~root",
        "grep -r x /home/*",
        "grep -rl x /home/alice",
        "grep -rl x /home/alice/",
        "grep -r x /home/alice/.config",
        "grep -r x /root/.config",
        "grep -r x /etc",
        "grep -r x /etc/",
        "grep -r x /etc/ssh",
        "grep -r x /etc/ssl",
        "grep -r x /etc/ssl/private",
        "grep -r x /proc",
        "grep -r x /proc/1",
        "grep -r x /proc/self/task",
        "grep -r x /proc/*",
        "grep -ri x /h*",
        "grep -ri x /*",
        "grep -r x /home/../etc",
        "grep -r x /var/log /home",
        "grep -d recurse x /home",
        "grep -drecurse x /home",
        "grep --directories=recurse x /home",
        "grep --directories recurse x /home",
        "grep --rec x /home",
        "grep --dereference-recursive x /home",
        "grep -nr x /home",
        "grep -rn --include='*.conf' x /home",
        "grep -r -e x /home",
        "grep -r -f patterns.txt /home",
        "grep -r -- x /home",
        "egrep -r x /home",
        "fgrep -r x /home",
        "zgrep -r x /home",
        "rg 'PRIVATE KEY' /home",
        "rg token /",
        "rg token ~",
        "rg -e token /etc",
        "rg -t py token /home",
        "rg -g '*.conf' x /etc",
        "rg -n -i --hidden token /root",
        "rg --files /home",
        "cat x | grep -r y /home",
        // 前面的 cd 决定相对路径从哪搜
        "cd /home && grep -r x .",
        "cd ~ && grep -r x .",
        "cd; grep -r x .",
        "cd ~; rg x",
        "cd /etc && grep -r pass .",
        "cd / && grep -r pass ./home",
        "cd /home; cd alice; grep -r x .",
    ] {
        assert!(!auto_runs(command), "不该自动执行：{command}");
        assert_eq!(
            kind_of(command),
            Some(SensitiveKind::RecursiveSearch),
            "{command}"
        );
    }
}

#[test]
fn recursive_searches_that_miss_sensitive_places_run_unasked() {
    for command in [
        "grep -r error /var/log",
        "grep -rn listen /etc/nginx",
        "grep -rn listen /etc/nginx 2>/dev/null",
        "grep -ri error /var/log/nginx",
        "grep -r x /etc/nginx/conf.d",
        "grep -r x /etc/ssl/certs",
        "grep -r x /home/alice/projects",
        "grep -r x ~/projects",
        "grep -r x $HOME/projects",
        "grep -r x /root/work",
        "grep -r x /proc/sys",
        "grep -r x /proc/net",
        "grep -r x /var/www",
        "grep -r x /opt/app",
        "rg error /var/log",
        "rg -n listen /etc/nginx",
        "rg -e error /var/log",
        // 不递归
        "grep error /var/log/syslog",
        "grep -n x /etc/passwd",
        "grep -i x /etc/hosts",
        "grep password /etc/nginx/nginx.conf",
        "grep -A40 -B3 x /var/log/syslog",
        // 模式不是路径
        "grep -r /home /var/log",
        "grep -r -e /home /var/log",
        // 不知道当前目录在哪：相对路径不猜
        "grep -r x .",
        "grep -rn x ./src",
        "rg foo",
        "rg foo src",
        // cd 之后在不敏感的地方
        "cd /var/log && grep -r error .",
        "cd /srv/app && rg TODO",
        "cd /home/alice/projects && grep -r x .",
        "cd ~/projects; grep -r x .",
        "cd /etc/nginx; grep -r x .",
    ] {
        assert!(auto_runs(command), "应该自动执行：{command}");
    }
}

#[test]
fn environment_dumps_were_and_stay_out_of_the_unasked_set() {
    // 这些本来就不是只读（不在白名单 / 不带变量名），补上敏感原因，卡片上才看得到为什么要问
    for command in [
        "env",
        "env -0",
        "printenv",
        "printenv --null",
        "printenv API_TOKEN",
        "printenv AWS_SECRET_ACCESS_KEY",
        "set",
        "export -p",
        "export",
        "declare -p",
        "declare -x",
        "typeset -px",
        "readonly -p",
    ] {
        assert!(!auto_runs(command), "不该自动执行：{command}");
        assert!(!is_read_only(command), "本身就不是只读：{command}");
        assert_eq!(
            kind_of(command),
            Some(SensitiveKind::Environment),
            "{command}"
        );
    }
    assert!(!auto_runs("cat /proc/self/environ"));
    assert_eq!(kind_of("cat /proc/self/environ"), Some(SensitiveKind::Path));
    assert!(!auto_runs("cat /proc/1234/environ"));
    // 不是整份环境的写法不加原因
    for command in [
        "printenv PATH",
        "printenv HOME",
        "env FOO=1 ls",
        "set -e",
        "export FOO=1",
    ] {
        assert_eq!(kind_of(command), None, "{command}");
    }
    assert!(auto_runs("printenv PATH"));
}

/// 确认卡上的原因：走 `ApprovalReason::Command`，key 登记在 `reason_keys`，中文原文里写明命中了什么。
#[test]
fn every_source_hit_carries_a_registered_reason_key_and_text() {
    for (command, key, shown) in [
        (
            "getent shadow",
            rk::CMD_SENSITIVE_ACCOUNT_DB,
            "getent shadow",
        ),
        ("jq -n env", rk::CMD_SENSITIVE_ENVIRONMENT, "jq"),
        ("history", rk::CMD_SENSITIVE_HISTORY, "history"),
        (
            "systemctl --user show-environment",
            rk::CMD_SENSITIVE_ENVIRONMENT,
            "systemctl show-environment",
        ),
        (
            "grep -r -A40 'PRIVATE KEY' /home",
            rk::CMD_SENSITIVE_RECURSIVE_SEARCH,
            "/home",
        ),
    ] {
        let hit = sensitive_path_in_command(command).expect(command);
        match hit.reason() {
            ApprovalReason::Command { rule, text } => {
                assert_eq!(rule, key, "{command}");
                assert!(
                    rk::ALL.contains(&rule.as_str()),
                    "{rule} 要登记在 reason_keys"
                );
                assert!(text.contains(shown), "{command}：{text}");
            }
            other => panic!("{command}：应是带 key 的原因，实际 {other:?}"),
        }
    }
    // 路径类的原因没变
    let path = sensitive_path_in_command("cat ~/.ssh/id_rsa").expect("路径命中");
    assert!(matches!(
        path.reason(),
        ApprovalReason::SensitiveRead { .. }
    ));
}

/// 命令名之外的 `cd`、管道和重定向不会让检查漏掉或误报。
#[test]
fn combined_commands_report_the_first_sensitive_part() {
    assert_eq!(
        kind_of("ls /var/log && getent shadow | head"),
        Some(SensitiveKind::AccountDatabase)
    );
    assert_eq!(
        kind_of("getent shadow 2>/dev/null"),
        Some(SensitiveKind::AccountDatabase)
    );
    assert_eq!(
        kind_of("getent shadow>/dev/null"),
        Some(SensitiveKind::AccountDatabase)
    );
    assert_eq!(
        kind_of("history>/dev/null"),
        Some(SensitiveKind::ShellHistory)
    );
    assert_eq!(kind_of("ls /var/log; df -h"), None);
}
