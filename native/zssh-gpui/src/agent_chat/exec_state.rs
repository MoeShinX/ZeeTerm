//! 面板上显示的执行模式：和工具桥实际用的策略保持一致。
//!
//! 工具桥每次执行前都重新读设置库里的 `ai_exec_mode`，所以界面显示的值只能是"从库里读到的"
//! 或"刚写进库并且成功了的"，不能是"用户点了什么"：写失败时库里还是旧值（可能是全部放行），
//! 界面却显示成逐条确认，用户会以为后面的操作都会先问他。
//!
//! 几个面板（主窗口侧栏、独立窗口）各开一个库连接，别的窗口 / 设置页改了模式，这一份要跟着变：
//! 沿用主窗口刷新主机库的办法——`PRAGMA data_version` 在别的连接提交写操作时翻动，翻了才重读，
//! 再加一个低频兜底。

use zssh_core::ai::bridge::ExecMode;

/// 兜底重读的间隔（节拍数，节拍 100ms，即约 15 秒）：防止个别环境下 `data_version` 不翻。
const FALLBACK_TICKS: u32 = 150;

/// 真实模式下面板显示的执行模式，以及和它有关的错误。
#[derive(Debug)]
pub(crate) struct ExecState {
    /// 面板显示（也是面板自己当策略看待）的模式。读不出来时固定是逐条确认。
    pub(crate) mode: ExecMode,
    /// 读 / 写执行模式的错误，显示在面板底部；成功读到或写入后清掉。
    pub(crate) error: Option<String>,
    /// 最近一次读取失败：此时 `mode` 是保守的逐条确认，不是库里的值。
    unreadable: bool,
    /// 上次对齐时的库版本（`PRAGMA data_version`）。
    version: Option<i64>,
    ticks: u32,
}

impl ExecState {
    /// 打开面板时的初始状态。`version` 要在读模式**之前**取：读完到下一个节拍之间别的窗口写了，
    /// 下一个节拍就会看到版本不同而重读。
    pub(crate) fn new(version: Option<i64>, read: (ExecMode, Option<String>)) -> Self {
        let (mode, error) = read;
        Self {
            mode,
            unreadable: error.is_some(),
            error,
            version,
            ticks: 0,
        }
    }

    /// 没有设置库（打不开）：没法读也没法写，按逐条确认，原因由存储错误那一行显示。
    pub(crate) fn unavailable() -> Self {
        Self::new(None, (ExecMode::Manual, None))
    }

    /// 用户选了 `mode`，`result` 是写库的结果。只有写成功才改显示；失败时保持原值并记下错误。
    /// 返回显示的模式有没有变成 `mode`。
    pub(crate) fn commit(&mut self, mode: ExecMode, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => {
                self.mode = mode;
                self.error = None;
                self.unreadable = false;
                true
            }
            Err(err) => {
                self.error = Some(err);
                false
            }
        }
    }

    /// 每个节拍调一次。库版本变了（或到了兜底间隔）才调用 `read` 重读，并把显示对齐到读到的值。
    /// 读失败时显示逐条确认并带上原因（和工具桥读失败时的做法一致），不会留着旧的"全部放行"。
    /// 返回界面有没有变化。
    pub(crate) fn poll(
        &mut self,
        version: Option<i64>,
        read: impl FnOnce() -> (ExecMode, Option<String>),
    ) -> bool {
        self.ticks = self.ticks.wrapping_add(1);
        let moved = version != self.version;
        if !moved && !self.ticks.is_multiple_of(FALLBACK_TICKS) {
            return false;
        }
        self.version = version;
        let (mode, error) = read();
        let before = (self.mode, self.error.clone());
        match error {
            Some(err) => {
                self.mode = mode;
                self.error = Some(err);
                self.unreadable = true;
            }
            None => {
                // 读通了：以库里的值为准。上一次的读错误、以及对着旧模式写失败的提示都过期了。
                if self.unreadable || mode != self.mode {
                    self.error = None;
                }
                self.mode = mode;
                self.unreadable = false;
            }
        }
        before != (self.mode, self.error.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecMode, ExecState, FALLBACK_TICKS};
    use crate::agent_chat::approval::{load_exec_mode, save_exec_mode};
    use zssh_core::store::Database;

    fn ok(mode: ExecMode) -> (ExecMode, Option<String>) {
        (mode, None)
    }

    fn broken() -> (ExecMode, Option<String>) {
        (ExecMode::Manual, Some("读取失败".to_owned()))
    }

    #[test]
    fn a_failed_write_keeps_the_shown_mode_and_reports_the_error() {
        let mut state = ExecState::new(Some(1), ok(ExecMode::Auto));
        assert!(!state.commit(ExecMode::Manual, Err("写入失败".to_owned())));
        assert_eq!(
            state.mode,
            ExecMode::Auto,
            "库里还是全部放行，不能显示成逐条确认"
        );
        assert_eq!(state.error.as_deref(), Some("写入失败"));

        assert!(state.commit(ExecMode::Manual, Ok(())));
        assert_eq!(state.mode, ExecMode::Manual);
        assert_eq!(state.error, None, "写成功后旧错误清掉");
    }

    #[test]
    fn without_a_settings_db_the_mode_stays_manual_after_a_failed_write() {
        let mut state = ExecState::unavailable();
        assert!(!state.commit(ExecMode::Auto, Err("设置库不可用".to_owned())));
        assert_eq!(state.mode, ExecMode::Manual);
        assert!(state.error.is_some());
    }

    #[test]
    fn nothing_is_read_while_the_database_version_is_unchanged() {
        let mut state = ExecState::new(Some(7), ok(ExecMode::ReadOnly));
        for _ in 0..FALLBACK_TICKS - 1 {
            assert!(!state.poll(Some(7), || panic!("版本没变不该读库")));
        }
        assert_eq!(state.mode, ExecMode::ReadOnly);
    }

    #[test]
    fn a_version_change_pulls_the_mode_changed_elsewhere() {
        let mut state = ExecState::new(Some(7), ok(ExecMode::Manual));
        assert!(state.poll(Some(8), || ok(ExecMode::Auto)));
        assert_eq!(state.mode, ExecMode::Auto);
        // 同一个版本再来不重读。
        assert!(!state.poll(Some(8), || panic!("版本没变不该读库")));
        // 读到的值和显示一样：不算变化。
        assert!(!state.poll(Some(9), || ok(ExecMode::Auto)));
    }

    #[test]
    fn the_fallback_rereads_even_when_the_version_never_moves() {
        let mut state = ExecState::new(Some(7), ok(ExecMode::Manual));
        let mut changed = false;
        for _ in 0..FALLBACK_TICKS {
            changed |= state.poll(Some(7), || ok(ExecMode::Auto));
        }
        assert!(changed);
        assert_eq!(state.mode, ExecMode::Auto);
    }

    #[test]
    fn an_unreadable_setting_shows_manual_with_the_reason_never_auto() {
        let mut state = ExecState::new(Some(1), ok(ExecMode::Auto));
        assert!(state.poll(Some(2), broken));
        assert_eq!(state.mode, ExecMode::Manual, "读不出来按逐条确认");
        assert_eq!(state.error.as_deref(), Some("读取失败"));

        // 恢复读通之后，错误清掉，显示库里的值。
        assert!(state.poll(Some(3), || ok(ExecMode::ReadOnly)));
        assert_eq!(state.mode, ExecMode::ReadOnly);
        assert_eq!(state.error, None);
    }

    #[test]
    fn recovering_to_the_same_manual_value_still_clears_the_read_error() {
        let mut state = ExecState::new(Some(1), broken());
        assert_eq!(state.mode, ExecMode::Manual);
        assert!(state.error.is_some());
        assert!(state.poll(Some(2), || ok(ExecMode::Manual)));
        assert_eq!(state.error, None);
    }

    #[test]
    fn a_write_error_survives_unrelated_database_changes() {
        let mut state = ExecState::new(Some(1), ok(ExecMode::Auto));
        state.commit(ExecMode::Manual, Err("写入失败".to_owned()));
        // 别处写了别的设置，模式没变：提示还在（显示的模式仍然是真的）。
        assert!(!state.poll(Some(2), || ok(ExecMode::Auto)));
        assert_eq!(state.error.as_deref(), Some("写入失败"));
        // 模式真的被别处改了：那次写失败的提示过期。
        assert!(state.poll(Some(3), || ok(ExecMode::ReadOnly)));
        assert_eq!(state.error, None);
    }

    /// 两个连接指向同一个库文件，和"主窗口侧栏 + 独立窗口各开一个面板"一样：
    /// 一边写，另一边的 `data_version` 翻动，面板据此刷新。
    #[test]
    fn a_panel_follows_a_mode_written_through_another_connection() {
        let _dir_guard = tempfile::Builder::new()
            .prefix("zssh-agent-chat-exec-state-")
            .tempdir()
            .expect("临时目录");
        let dir = _dir_guard.path().to_path_buf();
        let path = dir.join("zssh.db");

        let panel_db = Database::open(&path).expect("面板的库连接");
        let other_db = Database::open(&path).expect("别的窗口的库连接");
        save_exec_mode(&other_db, ExecMode::Manual).expect("先写成逐条确认");

        let mut state = ExecState::new(panel_db.data_version().ok(), load_exec_mode(&panel_db));
        assert_eq!(state.mode, ExecMode::Manual);

        // 面板自己写（经 commit）：自己的写不翻自己的版本，也不需要重读。
        save_exec_mode(&panel_db, ExecMode::ReadOnly).expect("面板自己写");
        assert!(state.commit(ExecMode::ReadOnly, Ok(())));
        assert!(!state.poll(panel_db.data_version().ok(), || panic!(
            "自己的写不该触发重读"
        )));

        // 别的窗口改成全部放行：面板下一个节拍跟上。
        save_exec_mode(&other_db, ExecMode::Auto).expect("别的窗口写");
        assert!(state.poll(panel_db.data_version().ok(), || load_exec_mode(&panel_db)));
        assert_eq!(state.mode, ExecMode::Auto);

        drop((panel_db, other_db));
    }
}
