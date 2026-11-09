//! 临时 Bash / Zsh 集成脚本：让 shell 在每次提示符前后发出 OSC 133（提示符 / 输入起点 / 命令开始 / 命令结束）
//! 和 OSC 7（当前目录），输入跟踪才能确认"现在停在输入行上"。
//!
//! 约束（方案 6.1 节）：
//! - 只影响**当前这个 shell 进程**，不改任何 rc 文件，断开连接自然结束；
//! - 保留原有的 prompt 和 hook：Bash 的 `PROMPT_COMMAND` 字符串 / 数组 / 未设置三种形态都原样保留，
//!   Zsh 的 `precmd` / `preexec` 链只是往里加一项，不覆盖已有函数；
//! - 自带卸载函数 `__zeeterm_off`：只移除这次注册的那一项，期间用户自己新加的 hook 不动；
//! - 失败自带清理：安装函数返回非 0 就立刻卸载；
//! - 启用握手：脚本发出去之后，等到第一个 `133;B`（输入起点）就算就绪；最多等 [`HANDSHAKE_TIMEOUT`]。
//!
//! 脚本以一个空格开头发送：Bash 的 `HISTCONTROL` 含 `ignorespace`（或 `ignoreboth`）、Zsh 开了
//! `HIST_IGNORE_SPACE` 时，这一行不会进 shell 历史；否则会进（这是 shell 自己的设置，这里不去改它，
//! 也不去删历史条目，免得误删用户自己的命令）。
//!
//! 脚本只用到 Bash 3.2 就有的语法（`PS0` / 数组形态的 `PROMPT_COMMAND` 在新版本里才起作用，旧版本上
//! 缺少的部分自然没有效果）；不用 `case` / here-doc，每行都是完整的语句或者以 `{` / `then` / `do` / `else`
//! 结尾，方便压成一行发送（见 [`to_single_line`]）。

use std::time::Duration;

/// 握手最长等待：脚本发出去之后这么久还没收到输入起点标记，就说明启用失败。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 卸载函数的名字。启用失败、尚无可靠 Prompt 时不会自动往远端补发它，界面提示用户手动运行。
pub const CLEANUP_COMMAND: &str = "__zeeterm_off";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShellKind {
    Bash,
    Zsh,
}

impl ShellKind {
    pub fn all() -> [ShellKind; 2] {
        [ShellKind::Bash, ShellKind::Zsh]
    }
}

const BASH_SCRIPT: &str = r#"# ZeeTerm: temporary shell integration for this session only (Bash).
# It emits OSC 133 prompt marks and OSC 7 cwd, keeps your PROMPT_COMMAND and PS1,
# edits no rc file, and is removed by running: __zeeterm_off
__zeeterm_cwd() {
local LC_ALL=C __zp=$PWD __zo= __zi __zc
for ((__zi = 0; __zi < ${#__zp}; __zi++)); do
__zc=${__zp:__zi:1}
if [[ $__zc == [A-Za-z0-9/._~-] ]]; then
__zo=$__zo$__zc
else
__zo=$__zo$(printf '%%%02X' "'$__zc")
fi
done
printf '\033]7;file://%s%s\007' "${HOSTNAME-}" "$__zo"
}
__zeeterm_pc() {
local __zs=$?
if [ -n "${__zeeterm_first-}" ]; then
__zeeterm_first=
else
printf '\033]133;D;%s\007' "$__zs"
fi
printf '\033]133;A\007'
__zeeterm_cwd
return "$__zs"
}
__zeeterm_off() {
local __zi __zn=() __zv
if [ "${__zeeterm_pc_kind-}" = array ]; then
for __zi in "${PROMPT_COMMAND[@]}"; do
[ "$__zi" = __zeeterm_pc ] || __zn+=("$__zi")
done
if [ "${#__zn[@]}" -gt 0 ]; then
PROMPT_COMMAND=("${__zn[@]}")
else
unset PROMPT_COMMAND
fi
elif [ -n "${__zeeterm_pc_kind-}" ]; then
if [[ ${PROMPT_COMMAND-} == *__zeeterm_pc\;* ]]; then
__zv=${PROMPT_COMMAND/__zeeterm_pc;/}
else
__zv=${PROMPT_COMMAND/__zeeterm_pc/}
fi
if [ -n "$__zv" ]; then
printf -v PROMPT_COMMAND '%s' "$__zv"
else
unset PROMPT_COMMAND
fi
fi
if [ -n "${__zeeterm_ps1_added-}" ]; then
printf -v PS1 '%s' "${PS1/"$__zeeterm_ps1_added"/}"
fi
if [ -n "${__zeeterm_ps0_added-}" ]; then
printf -v PS0 '%s' "${PS0/"$__zeeterm_ps0_added"/}"
fi
unset __zeeterm_first __zeeterm_pc_kind __zeeterm_ps1_added __zeeterm_ps0_added
unset -f __zeeterm_pc __zeeterm_cwd __zeeterm_on
unset -f __zeeterm_off
}
__zeeterm_on() {
__zeeterm_first=1
printf -v PS1 '%s' "$PS1"$'\\[\\033]133;B\\007\\]' || return 1
__zeeterm_ps1_added=$'\\[\\033]133;B\\007\\]'
if ((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4))); then
printf -v PS0 '%s' "${PS0-}"$'\033]133;C\007' || return 1
__zeeterm_ps0_added=$'\033]133;C\007'
fi
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
PROMPT_COMMAND=(__zeeterm_pc "${PROMPT_COMMAND[@]}") || return 1
__zeeterm_pc_kind=array
elif [ -n "${PROMPT_COMMAND-}" ]; then
printf -v PROMPT_COMMAND '%s' "__zeeterm_pc;${PROMPT_COMMAND}" || return 1
__zeeterm_pc_kind=string
else
printf -v PROMPT_COMMAND '%s' __zeeterm_pc || return 1
__zeeterm_pc_kind=unset
fi
}
__zeeterm_on || __zeeterm_off
"#;

const ZSH_SCRIPT: &str = r#"# ZeeTerm: temporary shell integration for this session only (Zsh).
# It emits OSC 133 prompt marks and OSC 7 cwd, keeps your precmd/preexec hooks and PS1,
# edits no rc file, and is removed by running: __zeeterm_off
__zeeterm_cwd() {
emulate -L zsh
local LC_ALL=C __zp=$PWD __zo= __zi __zc
for ((__zi = 1; __zi <= ${#__zp}; __zi++)); do
__zc=${__zp[$__zi]}
if [[ $__zc == [A-Za-z0-9/._~-] ]]; then
__zo=$__zo$__zc
else
__zo=$__zo$(printf '%%%02X' "'$__zc")
fi
done
printf '\033]7;file://%s%s\007' "${HOST-}" "$__zo"
}
__zeeterm_precmd() {
local __zs=$?
emulate -L zsh
if [[ -n ${__zeeterm_first-} ]]; then
__zeeterm_first=
else
printf '\033]133;D;%s\007' "$__zs"
fi
printf '\033]133;A\007'
__zeeterm_cwd
return $__zs
}
__zeeterm_preexec() {
printf '\033]133;C\007'
}
__zeeterm_off() {
emulate -L zsh
precmd_functions=(${precmd_functions:#__zeeterm_precmd})
preexec_functions=(${preexec_functions:#__zeeterm_preexec})
if [[ -n ${__zeeterm_ps1_added-} ]]; then
PS1=${PS1//"$__zeeterm_ps1_added"/}
fi
unset __zeeterm_first __zeeterm_ps1_added
unfunction __zeeterm_precmd __zeeterm_preexec __zeeterm_cwd __zeeterm_on 2>/dev/null
unfunction __zeeterm_off 2>/dev/null
}
__zeeterm_on() {
emulate -L zsh
__zeeterm_first=1
precmd_functions=(__zeeterm_precmd ${precmd_functions[@]})
preexec_functions+=(__zeeterm_preexec)
__zeeterm_ps1_added=$'%{\033]133;B\007%}'
PS1=$PS1$__zeeterm_ps1_added
}
__zeeterm_on || __zeeterm_off
"#;

/// 如果上一次启用的卸载函数还在，先卸掉再重新安装（重复启用不叠加）。
const PRECLEAN: &str = "type __zeeterm_off >/dev/null 2>&1 && __zeeterm_off";

/// 可读的多行脚本（"复制脚本手动启用"复制这一份，带注释）。
pub fn script(kind: ShellKind) -> String {
    let body = match kind {
        ShellKind::Bash => BASH_SCRIPT,
        ShellKind::Zsh => ZSH_SCRIPT,
    };
    format!("{PRECLEAN}\n{body}")
}

/// 把多行脚本压成一行：丢掉注释行和空行；上一行以 `{`、`then`、`do`、`else` 结尾的用空格接，
/// 其余用 `; ` 接。脚本按这个约定写（见模块说明），所以压出来的和多行版本语义一致。
pub fn to_single_line(script: &str) -> String {
    let mut out = String::new();
    for line in script.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !out.is_empty() {
            out.push_str(if opens_a_block(&out) { " " } else { "; " });
        }
        out.push_str(line);
    }
    out
}

/// 已经拼好的部分是不是以块的开头结束（后面接命令要用空格，不能加分号）。
fn opens_a_block(text: &str) -> bool {
    if text.ends_with('{') {
        return true;
    }
    ["then", "do", "else"].iter().any(|word| {
        text.strip_suffix(word).is_some_and(|rest| {
            // 必须是独立的单词（`undo` / `send` 不算）。
            !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_')
        })
    })
}

/// 往终端里发的那一行（不含结尾的回车）：以空格开头，见模块说明里的历史说明。
pub fn inject_line(kind: ShellKind) -> String {
    format!(" {}", to_single_line(&script(kind)))
}

/// 给用户看的卸载命令（带前导空格，同样不想进历史）。
pub fn cleanup_line() -> String {
    format!(" {CLEANUP_COMMAND}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_injected_line_is_one_line_that_starts_with_a_space() {
        for kind in ShellKind::all() {
            let line = inject_line(kind);
            assert!(line.starts_with(' '), "{kind:?}：前导空格，不想进历史");
            assert!(!line.contains('\n') && !line.contains('\r'), "{kind:?}");
            assert!(!line.contains("# ZeeTerm"), "{kind:?}：注释行必须丢掉");
            assert!(line.contains("__zeeterm_off"), "{kind:?}");
            assert!(line.len() < 4096, "{kind:?}：{} 字节", line.len());
        }
    }

    #[test]
    fn single_line_joining_follows_the_block_keywords() {
        let joined =
            to_single_line("# comment\nf() {\na=1\nif [ x ]; then\nb=2\nelse\nc=3\nfi\n}\nf\n");
        assert_eq!(joined, "f() { a=1; if [ x ]; then b=2; else c=3; fi; }; f");
        let looped = to_single_line("for i in 1 2; do\necho $i\ndone\n");
        assert_eq!(looped, "for i in 1 2; do echo $i; done");
        // 以 do / then 结尾的单词（如变量名 undo）不算块关键字。
        assert_eq!(to_single_line("undo\nx\n"), "undo; x");
        assert_eq!(to_single_line("x=send\ny\n"), "x=send; y");
    }

    #[test]
    fn scripts_leave_a_way_out_and_never_touch_rc_files() {
        for kind in ShellKind::all() {
            let text = script(kind);
            assert!(text.contains("__zeeterm_off()"), "{kind:?}：自带卸载函数");
            assert!(
                text.contains("|| __zeeterm_off"),
                "{kind:?}：安装失败自带清理"
            );
            assert!(text.contains(PRECLEAN), "{kind:?}：重复启用先卸旧的");
            for rc in [
                ".bashrc",
                ".zshrc",
                ".profile",
                ".bash_profile",
                ".zprofile",
                "/etc/",
            ] {
                assert!(!text.contains(rc), "{kind:?}：不能碰 {rc}");
            }
            for danger in ["history -d", "history -c", "fc -", "rm "] {
                assert!(
                    !text.contains(danger),
                    "{kind:?}：不能删历史或文件：{danger}"
                );
            }
            assert!(text.contains("133;A") && text.contains("133;B"), "{kind:?}");
            assert!(text.contains("133;C") && text.contains("133;D"), "{kind:?}");
            assert!(text.contains("]7;file://"), "{kind:?}");
        }
    }

    #[test]
    fn the_bash_script_preserves_all_three_prompt_command_shapes() {
        let text = script(ShellKind::Bash);
        assert!(text.contains("declare -a"), "数组形态");
        assert!(text.contains("__zeeterm_pc_kind=string"), "字符串形态");
        assert!(text.contains("__zeeterm_pc_kind=unset"), "未设置形态");
    }

    #[test]
    fn the_zsh_script_chains_hooks_instead_of_overwriting_them() {
        let text = script(ShellKind::Zsh);
        assert!(text.contains("precmd_functions=(__zeeterm_precmd ${precmd_functions[@]})"));
        assert!(text.contains("preexec_functions+=(__zeeterm_preexec)"));
        assert!(!text.contains("\nprecmd()"), "不覆盖已有的 precmd 函数");
        assert!(!text.contains("\npreexec()"), "不覆盖已有的 preexec 函数");
    }

    #[test]
    fn the_handshake_waits_at_most_ten_seconds() {
        assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(10));
    }
}
