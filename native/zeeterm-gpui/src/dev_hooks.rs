//! 测试 / 手动验证用的环境变量钩子，统一从这里读。
//!
//! 这些变量（`ZSSH_SSH_TRUST_ONCE`、`ZSSH_SFTP_*`、`ZSSH_PASTE_NO_COPY_DATA`、`ZSSH_EDIT_*`、`ZSSH_AGENT_*`、启动即开窗口的
//! 那一批等）能跳过指纹确认、直接删/覆盖远端文件、替换更新公钥。发行版里用户或别的进程只要能
//! 设环境变量就能触发它们，所以只在 `debug_assertions` 下读取，release 构建恒返回"未设置"。
//!
//! 主窗口相关（`ZSSH_CONNECT_HOST`、`ZSSH_MAIN_VIEW`、`ZSSH_MAIN_SCRIPT`、`ZSSH_MAIN_LOG`、`ZSSH_SHOT_DIR`、
//! `ZSSH_SHOT_FORBID`、`ZSSH_SEED_TEST_HOST`、`ZSSH_SEED_PLACEHOLDERS`）和旧终端演示窗口
//! （`ZSSH_DEMO_TERMINAL`、`ZSSH_CONNECT_HOST_WINDOW`）的钩子也一样只在 debug 构建里读。
//!
//! 例外：`ZSSH_DATA_DIR` 是产品功能（便携/隔离数据目录），不走这里，见 `settings::data_dir`。
//! 代理类的标准变量（`HTTPS_PROXY` 等）也是产品行为，不走这里。

use std::ffi::OsString;

/// 钩子是否生效。放成常量，release 下编译器能把整条读取路径删掉。
pub const ENABLED: bool = cfg!(debug_assertions);

/// 把"开关"和"真正去读"拆开，开关为假时连读取闭包都不调用。
/// 这样 release 配置下的行为可以在 debug 构建里直接断言（见测试）。
fn gated<T>(enabled: bool, read: impl FnOnce() -> Option<T>) -> Option<T> {
    if enabled { read() } else { None }
}

/// 读一个钩子变量（要求是合法 Unicode）；release 下恒为 `None`。
pub fn var(name: &str) -> Option<String> {
    gated(ENABLED, || std::env::var(name).ok())
}

/// 读一个钩子变量的原始值；release 下恒为 `None`。
pub fn var_os(name: &str) -> Option<OsString> {
    gated(ENABLED, || std::env::var_os(name))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{gated, var, var_os};

    /// 只在这个模块里用的变量名，避免和别的用例争用。
    const PROBE: &str = "ZSSH_DEV_HOOKS_PROBE";

    #[test]
    fn disabled_gate_never_runs_the_reader() {
        let called = Cell::new(false);
        let got = gated(false, || {
            called.set(true);
            Some("value")
        });
        assert_eq!(got, None);
        assert!(!called.get(), "关闭时不应该去读环境变量");
    }

    #[test]
    fn enabled_gate_passes_the_value_through() {
        assert_eq!(gated(true, || Some(7)), Some(7));
        assert_eq!(gated(true, || None::<u8>), None);
    }

    /// release 配置下（`cargo test --release`）：即使变量真的设了，钩子也读不到。
    #[cfg(not(debug_assertions))]
    #[test]
    fn release_build_ignores_set_variables() {
        // SAFETY: 本模块只有这一个用例写这个变量，且用例内同步完成。
        unsafe { std::env::set_var(PROBE, "1") };
        assert_eq!(var(PROBE), None);
        assert_eq!(var_os(PROBE), None);
        unsafe { std::env::remove_var(PROBE) };
    }

    /// debug 配置下钩子照常工作，自动化才有用。
    #[cfg(debug_assertions)]
    #[test]
    fn debug_build_reads_set_variables() {
        // SAFETY: 本模块只有这一个用例写这个变量，且用例内同步完成。
        unsafe { std::env::set_var(PROBE, "1") };
        assert_eq!(var(PROBE).as_deref(), Some("1"));
        assert_eq!(
            var_os(PROBE).and_then(|v| v.into_string().ok()).as_deref(),
            Some("1")
        );
        unsafe { std::env::remove_var(PROBE) };
        assert_eq!(var(PROBE), None);
    }
}
