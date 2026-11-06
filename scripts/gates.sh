#!/usr/bin/env bash
# ZeeTerm 的 Linux 门禁，对应 scripts/gates.ps1（Windows）：核心库（根工作区）和原生应用（native 工作区）各自的
# fmt / clippy / test，再加原生应用的 release 类配置检查（gate 配置：dev_hooks 断言 + 构建，且编译输出不许有 warning）。
# 逐条报告退出码和耗时，任何一条失败整体退出码就是 1，全部通过时最后一行是「Linux 门禁全部通过。」。
#
# 测试跑在一次性的 D-Bus 会话 + 临时解锁的 gnome-keyring 里（scripts/linux-keyring-session.sh），
# 和本机编译机上的 vm-gates.sh、CI 是同一种做法。需要的系统包见 scripts/linux-build-deps.sh。
#
# 用法（任意目录都行）：scripts/gates.sh
#   ZEETERM_REQUIRE_I18N=1   没有 pwsh 时 i18n 覆盖检查按失败处理（CI 里设；本机没有 pwsh 默认只提示并跳过）
#   ZEETERM_KEYRING_SESSION  换掉钥匙串会话脚本（给脚本自己的测试用）
#
# 联网用例不在这里跑：没设 ZSSH_TEST_* 环境变量时它们会自己跳过并打印原因。
# 自动化一律不碰用户的真实数据：没设 ZSSH_DATA_DIR 时本脚本给它指一个临时目录，跑完删掉；
# 其余 ZSSH_* 环境变量一律清掉。
set -u
script_dir=$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$script_dir/.." || exit 2
keyring_session=${ZEETERM_KEYRING_SESSION:-$script_dir/linux-keyring-session.sh}

own_data_dir=""
keep_data_dir=${ZSSH_DATA_DIR:-}
for name in $(env | grep -oE '^ZSSH_[A-Za-z0-9_]+' || true); do
    [ "$name" = ZSSH_DATA_DIR ] || unset "$name"
done
if [ -z "$keep_data_dir" ]; then
    own_data_dir=$(mktemp -d "${TMPDIR:-/tmp}/zeeterm-gates-XXXXXXXX")
    export ZSSH_DATA_DIR=$own_data_dir
fi
log_file=$(mktemp "${TMPDIR:-/tmp}/zeeterm-gates-log-XXXXXXXX")
cleanup() {
    rm -f "$log_file"
    if [ -n "$own_data_dir" ]; then rm -rf "$own_data_dir"; fi
}
trap cleanup EXIT

failed=()

# run_step <名字> <deny-warnings 0|1> <钥匙串会话 0|1> <命令...>
run_step() {
    local name=$1 deny=$2 session=$3
    shift 3
    echo "==> ${name}: $*"
    local started=$SECONDS code warnings
    local -a cmd=("$@")
    if [ "$session" = 1 ]; then cmd=("$keyring_session" "$@"); fi
    if [ "$deny" = 1 ]; then
        "${cmd[@]}" 2>&1 | tee "$log_file"
        code=${PIPESTATUS[0]}
        warnings=$(grep -cE '^warning(\[[^]]*\])?:' "$log_file" || true)
        if [ "$code" -eq 0 ] && [ "${warnings:-0}" -gt 0 ]; then
            echo "编译输出里有 ${warnings} 条 warning，按失败处理。"
            code=1
        fi
    else
        "${cmd[@]}"
        code=$?
    fi
    echo "[${name}] 退出码 ${code}  耗时 $((SECONDS - started)) 秒"
    if [ "$code" -ne 0 ]; then failed+=("$name"); fi
}

total_started=$SECONDS

# 界面文案覆盖：写死的中文只能在 allow 清单里的文件里。检查脚本是 PowerShell，Linux 上靠 pwsh 跑。
echo "==> i18n 覆盖检查: scripts/check-i18n.ps1"
i18n_started=$SECONDS
if command -v pwsh >/dev/null 2>&1; then
    pwsh -NoProfile -File "$script_dir/check-i18n.ps1"
    i18n_code=$?
    echo "[i18n 覆盖检查] 退出码 ${i18n_code}  耗时 $((SECONDS - i18n_started)) 秒"
    if [ "$i18n_code" -ne 0 ]; then failed+=("i18n 覆盖检查"); fi
elif [ "${ZEETERM_REQUIRE_I18N:-0}" = 1 ]; then
    echo "[i18n 覆盖检查] 没有 pwsh，按失败处理（ZEETERM_REQUIRE_I18N=1）"
    failed+=("i18n 覆盖检查")
else
    echo "[i18n 覆盖检查] 条件缺失：没有 pwsh，跳过（CI 和 Windows 门禁会跑）"
fi

run_step '核心库 cargo fmt'    0 0 cargo fmt --all -- --check
run_step '核心库 cargo clippy' 0 0 cargo clippy --workspace --all-targets --locked -- -D warnings
run_step '核心库 cargo test'   0 1 cargo test --workspace --locked
run_step '原生 cargo fmt'      0 0 cargo fmt --manifest-path native/Cargo.toml --all -- --check
run_step '原生 cargo clippy'   0 0 cargo clippy --manifest-path native/Cargo.toml --workspace --all-targets --locked -- -D warnings
run_step '原生 cargo test'     0 1 cargo test --manifest-path native/Cargo.toml --workspace --locked
run_step '原生 gate dev_hooks' 1 1 cargo test --manifest-path native/Cargo.toml -p zssh-gpui --profile gate --locked dev_hooks
run_step '原生 gate 构建'      1 0 cargo build --manifest-path native/Cargo.toml --workspace --profile gate --locked

echo "总耗时 $((SECONDS - total_started)) 秒"
if [ "${#failed[@]}" -gt 0 ]; then
    names=$(printf '%s、' "${failed[@]}")
    echo "失败：${names%、}"
    exit 1
fi
echo 'Linux 门禁全部通过。'
exit 0
