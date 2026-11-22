//! 离线演示脚本：写死的回合脚本、脚本事件和演示工具。
//!
//! 演示**不启动任何智能体进程、不调用模型、不发起网络请求**；窗口按 100ms tick
//! 调用 [`TurnScript::tick`] 推进一步，确认按钮通过 [`TurnScript::resolve`] 放行 / 拒绝。

use super::*;

/// 脚本里的固定文案。测试和界面都从这里取，避免两处各写一份。
pub(super) const REASONING: &str = "先确认当前目录，再决定是否写入。";
pub(super) const FIRST_TEXT: &str = "我先看一下当前目录。";
pub(super) const PWD_OUTPUT: &str = "/srv/app";
pub(super) const FIRST_DONE_TEXT: &str = "目录是 /srv/app。";
pub(super) const SECOND_TEXT: &str = "接下来写入配置文件。";
pub(super) const WRITE_OUTPUT: &str = "已写入 12 字节";
pub(super) const WRITE_DONE_TEXT: &str = "写入完成。";
pub(super) const FIRST_DENIED_TEXT: &str = "已取消。";
pub(super) const SECOND_DENIED_TEXT: &str = "已取消写入。";
pub(super) const DENIED_ERROR: &str = "用户拒绝执行";
pub(super) const STOPPED_ERROR: &str = "已停止";

/// 「Markdown 示例」追加的静态助手消息；只演示渲染，不经过脚本，也不调用模型。
pub(super) const MARKDOWN_SAMPLE: &str = r#"## Markdown 渲染示例

助手消息现在按 Markdown 渲染，下面的块都可以选中并复制原文：

- 支持**粗体**、*斜体*和 `行内代码`
- 支持列表和链接：[示例站点](https://example.com/)
- 远程图片不会加载，只显示占位

| 功能 | 说明 |
| --- | --- |
| 代码块 | 右上角有「复制」按钮 |
| 表格 | 下方有「复制表格」按钮 |

```rust
fn main() {
    println!("hello, zssh");
}
```
"#;

/// 演示脚本里的两个工具调用。
pub(super) const PWD_INPUT: DemoInput = DemoInput::Command("pwd");
pub(super) const WRITE_INPUT: DemoInput = DemoInput::WriteFile {
    path: "/srv/app/app.conf",
    content: "mode=prod\n",
};

/// 演示脚本里的工具；真实模式的工具名直接用驱动上报的字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DemoTool {
    TerminalExec,
    WriteFile,
}

impl DemoTool {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::TerminalExec => "terminal_exec",
            Self::WriteFile => "write_file",
        }
    }

    /// 演示脚本里的 `terminal_exec` 只跑 `pwd`，按只读命令处理。
    pub(super) fn is_read_only(self) -> bool {
        match self {
            Self::TerminalExec => true,
            Self::WriteFile => false,
        }
    }
}

/// 工具输入；只保留界面和确认卡片需要的字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DemoInput {
    Command(&'static str),
    WriteFile {
        path: &'static str,
        content: &'static str,
    },
}

impl DemoInput {
    /// 工具卡片上的一行输入摘要。
    pub(super) fn summary(self) -> String {
        match self {
            Self::Command(command) => format!("$ {command}"),
            Self::WriteFile { path, .. } => format!("路径：{path}"),
        }
    }

    /// 演示脚本的确认请求：形状和真实桥给的 [`ApprovalRequest`] 一样，
    /// 这样演示和真实模式走同一张确认卡片。
    pub(super) fn approval_request(self) -> ApprovalRequest {
        let (command, file) = match self {
            Self::Command(command) => (command.to_owned(), None),
            Self::WriteFile { path, content } => (
                format!("write_file {path}"),
                Some(FileApproval {
                    path: path.to_owned(),
                    is_new: true,
                    new_bytes: content.len(),
                    diff: make_diff(None, content),
                }),
            ),
        };
        ApprovalRequest {
            scope: "demo".to_owned(),
            exec_id: "demo".to_owned(),
            target_id: "host-0".to_owned(),
            session_id: None,
            host_label: "演示主机".to_owned(),
            command,
            danger: DangerLevel::Safe,
            reasons: Vec::new(),
            file,
        }
    }
}

/// 当前模式下这个工具要不要先确认。
pub(super) fn requires_approval(tool: DemoTool, mode: ExecMode) -> bool {
    match mode {
        ExecMode::Auto => false,
        ExecMode::ReadOnly => !tool.is_read_only(),
        ExecMode::Manual => true,
    }
}

/// 脚本每一步发出的事件；界面只认这些事件，不直接读脚本内部状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnEvent {
    ReasoningDelta(&'static str),
    TextDelta(&'static str),
    ToolStart {
        call_id: &'static str,
        tool: DemoTool,
        input: DemoInput,
        needs_approval: bool,
    },
    ToolEnd {
        call_id: &'static str,
        outcome: Result<&'static str, &'static str>,
    },
    TurnEnd,
}

/// 脚本位置。确认等待是独立的阶段，等待期间 tick 不会推进。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Reasoning,
    Intro,
    FirstTool,
    FirstWait,
    FirstRun,
    FirstAfter,
    FirstDenied,
    FirstDeniedText,
    SecondIntro,
    SecondTool,
    SecondWait,
    SecondRun,
    SecondAfter,
    SecondDenied,
    SecondDeniedText,
    End,
    Finished,
}

/// 写死的演示脚本。`tick` 每次只走一步，确认按钮通过 `resolve` 放行/拒绝。
#[derive(Clone, Copy, Debug)]
pub(super) struct TurnScript {
    stage: Stage,
}

impl TurnScript {
    pub(super) fn new() -> Self {
        Self {
            stage: Stage::Reasoning,
        }
    }

    /// 推进一步；等待确认时返回 `None`。模式只在 `ToolStart` 那一步读取。
    pub(super) fn tick(&mut self, mode: ExecMode) -> Option<TurnEvent> {
        match self.stage {
            Stage::Reasoning => {
                self.stage = Stage::Intro;
                Some(TurnEvent::ReasoningDelta(REASONING))
            }
            Stage::Intro => {
                self.stage = Stage::FirstTool;
                Some(TurnEvent::TextDelta(FIRST_TEXT))
            }
            Stage::FirstTool => {
                let needs_approval = requires_approval(DemoTool::TerminalExec, mode);
                self.stage = if needs_approval {
                    Stage::FirstWait
                } else {
                    Stage::FirstRun
                };
                Some(TurnEvent::ToolStart {
                    call_id: "call-1",
                    tool: DemoTool::TerminalExec,
                    input: PWD_INPUT,
                    needs_approval,
                })
            }
            Stage::FirstWait => None,
            Stage::FirstRun => {
                self.stage = Stage::FirstAfter;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-1",
                    outcome: Ok(PWD_OUTPUT),
                })
            }
            Stage::FirstAfter => {
                self.stage = Stage::SecondIntro;
                Some(TurnEvent::TextDelta(FIRST_DONE_TEXT))
            }
            Stage::FirstDenied => {
                self.stage = Stage::FirstDeniedText;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-1",
                    outcome: Err(DENIED_ERROR),
                })
            }
            Stage::FirstDeniedText => {
                self.stage = Stage::End;
                Some(TurnEvent::TextDelta(FIRST_DENIED_TEXT))
            }
            Stage::SecondIntro => {
                self.stage = Stage::SecondTool;
                Some(TurnEvent::TextDelta(SECOND_TEXT))
            }
            Stage::SecondTool => {
                let needs_approval = requires_approval(DemoTool::WriteFile, mode);
                self.stage = if needs_approval {
                    Stage::SecondWait
                } else {
                    Stage::SecondRun
                };
                Some(TurnEvent::ToolStart {
                    call_id: "call-2",
                    tool: DemoTool::WriteFile,
                    input: WRITE_INPUT,
                    needs_approval,
                })
            }
            Stage::SecondWait => None,
            Stage::SecondRun => {
                self.stage = Stage::SecondAfter;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-2",
                    outcome: Ok(WRITE_OUTPUT),
                })
            }
            Stage::SecondAfter => {
                self.stage = Stage::End;
                Some(TurnEvent::TextDelta(WRITE_DONE_TEXT))
            }
            Stage::SecondDenied => {
                self.stage = Stage::SecondDeniedText;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-2",
                    outcome: Err(DENIED_ERROR),
                })
            }
            Stage::SecondDeniedText => {
                self.stage = Stage::End;
                Some(TurnEvent::TextDelta(SECOND_DENIED_TEXT))
            }
            Stage::End => {
                self.stage = Stage::Finished;
                Some(TurnEvent::TurnEnd)
            }
            Stage::Finished => None,
        }
    }

    /// 放行/拒绝当前等待的确认；不在等待阶段时返回 `false`。
    pub(super) fn resolve(&mut self, allow: bool) -> bool {
        match self.stage {
            Stage::FirstWait => {
                self.stage = if allow {
                    Stage::FirstRun
                } else {
                    Stage::FirstDenied
                };
                true
            }
            Stage::SecondWait => {
                self.stage = if allow {
                    Stage::SecondRun
                } else {
                    Stage::SecondDenied
                };
                true
            }
            _ => false,
        }
    }
}

pub(super) fn start_in_real_mode(demo_hook: Option<&str>) -> bool {
    demo_hook != Some("1")
}

/// 离线演示的切换入口只在调试版出现。
pub(super) fn demo_toggle_available() -> bool {
    crate::dev_hooks::ENABLED
}
