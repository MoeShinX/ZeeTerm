//! 执行模式和审批：模式切换规则、「全部放行」确认、审批请求到卡片的映射。
//!
//! 执行模式读写的设置项和工具桥是同一个（`settings.ai_exec_mode`）。

use super::*;

/// 执行模式在 `ai_exec_mode` 设置项里的字符串值。模式类型本身用 core 的
/// [`ExecMode`]（工具桥判定放行用的就是它），窗口不再自己复制一份。
pub(super) fn mode_setting(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Manual => "manual",
        ExecMode::ReadOnly => "readonly",
        ExecMode::Auto => "auto",
    }
}

pub(super) fn mode_label(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Manual => t(ChatMsg::ModeManual),
        ExecMode::ReadOnly => t(ChatMsg::ModeReadOnly),
        ExecMode::Auto => t(ChatMsg::ModeAuto),
    }
}

/// 要不要把模式写进设置库。
/// 只有真实模式才写：设置库里的值是真实的审批策略，离线演示改的只是内存里的演示模式。
pub(super) fn mode_is_persisted(real: bool) -> bool {
    real
}

/// 选了一个执行模式之后该怎么办。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ModeSwitch {
    /// 直接改。
    Apply,
    /// 先问一句再改：从别的模式切到「全部放行」。
    Ask,
}

/// 「全部放行」是安全关键档位（AI 之后的任何命令都不再问用户，并且持久化），
/// 所以只要是从别的模式切过去就要先确认；切到另外两种、或者本来就是它，不问。
pub(super) fn plan_mode_switch(current: ExecMode, target: ExecMode) -> ModeSwitch {
    if target == ExecMode::Auto && current != ExecMode::Auto {
        ModeSwitch::Ask
    } else {
        ModeSwitch::Apply
    }
}

/// 「全部放行」确认框的标题和正文。
///
/// 对话窗口自己用 [`ChatMsg::AllowAllTitle`] / [`ChatMsg::AllowAllMessage`]；这两个中文常量
/// 留给还没走 `i18n` 的调用方（`main_window::docks` 的确认框），两种语言都从这里的中文原文取词。
pub(crate) const ALLOW_ALL_TITLE: &str = "切换到「全部放行」"; // i18n-allow: 供未走 i18n 的调用方，界面走 ChatMsg
pub(crate) const ALLOW_ALL_MESSAGE: &str = // i18n-allow: 供未走 i18n 的调用方，界面走 ChatMsg
    "全部放行后，AI 在服务器上执行任何命令都不再问你。确定吗？"; // i18n-allow: 供未走 i18n 的调用方

/// 独立窗口里的确认状态：问了还没答。答「取消」什么都不改，答「确定」才返回要写的模式。
#[derive(Debug, Default)]
pub(super) struct AllowAllGate {
    asking: bool,
}

impl AllowAllGate {
    pub(super) fn request(&mut self, current: ExecMode, target: ExecMode) -> ModeSwitch {
        let plan = plan_mode_switch(current, target);
        self.asking = plan == ModeSwitch::Ask;
        plan
    }

    pub(super) fn asking(&self) -> bool {
        self.asking
    }

    pub(super) fn cancel(&mut self) {
        self.asking = false;
    }

    /// 用户确认了：返回要应用的模式；没在问（比如重复点击）返回 `None`。
    pub(super) fn confirm(&mut self) -> Option<ExecMode> {
        std::mem::take(&mut self.asking).then_some(ExecMode::Auto)
    }
}

/// 嵌入式面板告诉主窗口的事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentChatEvent {
    /// 用户在下拉里选了「全部放行」：请主窗口弹确认框，确认后再调 `set_mode_explicit`。
    ConfirmAllowAll,
}

/// 审批请求 → 工具卡片：`run_command` 的卡片带命令全文，`write_file` 带路径和差异
/// （全文在 [`ApprovalDetail`] 里，这里只放标题行的一行摘要）。
pub(super) fn approval_input(request: &ApprovalRequest) -> (String, ToolInput) {
    let tool = if request.file.is_some() {
        "write_file"
    } else {
        "run_command"
    };
    let summary = match &request.file {
        Some(file) => format!("{}{}", t(ChatMsg::PathPrefix), redact_paths(&file.path)),
        None => format!("$ {}", one_line_summary(&request.command, PREVIEW_LIMIT)),
    };
    // 确认卡片本身也是一张工具卡片；存历史时把命令和主机带上（凭据类字段由 core 抹掉）。
    let input = match &request.file {
        Some(file) => json!({
            "host": &request.host_label,
            "command": &request.command,
            "path": &file.path,
            "is_new": file.is_new,
        }),
        None => json!({
            "host": &request.host_label,
            "command": &request.command,
        }),
    };
    (tool.to_owned(), ToolInput::Real { summary, input })
}

/// 读执行模式；失败时按逐条确认显示，原因交给底部。
pub(super) fn load_exec_mode(db: &Database) -> (ExecMode, Option<String>) {
    match db.ai_exec_mode() {
        Ok(raw) => (ExecMode::from_setting(Some(&raw)), None),
        Err(err) => (
            ExecMode::Manual,
            Some(format!("{}{err}", t(ChatMsg::ReadExecModeFailedPrefix))),
        ),
    }
}

/// 保存执行模式；失败只返回给界面，不 panic。
pub(super) fn save_exec_mode(db: &Database, mode: ExecMode) -> Result<(), String> {
    db.set_ai_exec_mode(mode_setting(mode))
        .map_err(|err| format!("{}{err}", t(ChatMsg::SaveExecModeFailedPrefix)))
}

/// 打开 `<数据目录>/zssh.db`（和主机库、AI 智能体窗口同一个库）。
pub(super) fn open_settings_db() -> Result<Database, String> {
    let dir = crate::settings::data_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("{}{err}", t(ChatMsg::CreateDataDirFailedPrefix)))?;
    Database::open(&dir.join("zssh.db"))
        .map_err(|err| format!("{}{err}", t(ChatMsg::OpenSettingsDbFailedPrefix)))
}
