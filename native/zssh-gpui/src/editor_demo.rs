//! 编辑器最短链路：本地文件 -> GPUI Editor -> 版本校验保存 -> 冲突提示。
//!
//! 远端 SFTP 版之后接同一组 `zssh_editor` 类型；这里先用本地文件证明
//! 打开、修改、保存、外部改动冲突这四步。

use std::path::PathBuf;

use ::gpui::{FocusHandle, KeyDownEvent};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_editor::{FileVersion, SaveOutcome, read, save};

pub struct EditorDemo {
    editor: Entity<EditorState>,
    path: PathBuf,
    version: FileVersion,
    status: String,
    focus: FocusHandle,
}

impl EditorDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dir = std::env::temp_dir().join(format!("zssh-editor-demo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("note.txt");
        if !path.exists() {
            std::fs::write(
                &path,
                "ZeeTerm \u{7f16}\u{8f91}\u{5668}\u{6f14}\u{793a}\n\n\
                 \u{4fee}\u{6539}\u{540e}\u{70b9}\u{300c}\u{4fdd}\u{5b58}\u{300d}\u{ff0c}\u{518d}\u{70b9}\u{300c}\u{6a21}\u{62df}\u{5916}\u{90e8}\u{4fee}\u{6539}\u{300d}\u{540e}\u{91cd}\u{8bd5}\u{3002}\n",
            )
            .ok();
        }

        // 演示窗口（只由调试钩子 / 旧演示窗口打开）：文件就是上面刚写的，读不出来说明临时目录
        // 坏了，没有可展示的内容也造不出假的版本号，直接 panic。
        #[allow(clippy::expect_used)]
        let snapshot = read(&path).expect("读取示例文件");
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(true)
                .default_value(snapshot.text.clone())
        });

        Self {
            editor,
            path,
            version: snapshot.version,
            status: "\u{5df2}\u{52a0}\u{8f7d}".to_owned(),
            focus: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        match save(&self.path, &text, self.version) {
            Ok(SaveOutcome::Saved(version)) => {
                self.version = version;
                self.status = "\u{5df2}\u{4fdd}\u{5b58}".to_owned();
            }
            Ok(SaveOutcome::Conflict { .. }) => {
                self.status =
                    "\u{6587}\u{4ef6}\u{5df2}\u{88ab}\u{5916}\u{90e8}\u{4fee}\u{6539}\u{ff0c}\u{4fdd}\u{5b58}\u{88ab}\u{62d2}\u{7edd}"
                        .to_owned();
            }
            Err(err) => {
                self.status = format!("\u{4fdd}\u{5b58}\u{5931}\u{8d25}\u{ff1a}{err}");
            }
        }
        cx.notify();
    }

    fn simulate_external_change(&mut self, cx: &mut Context<Self>) {
        let mut text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.push_str("\n// external change\n");
        if std::fs::write(&self.path, text).is_ok() {
            self.status =
                "\u{5df2}\u{5728}\u{5916}\u{90e8}\u{4fee}\u{6539}\u{6587}\u{4ef6}\u{ff0c}\u{73b0}\u{5728}\u{4fdd}\u{5b58}\u{4f1a}\u{51b2}\u{7a81}"
                    .to_owned();
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.control && event.keystroke.key.eq_ignore_ascii_case("s") {
            self.save(cx);
        }
    }
}

impl Render for EditorDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let save_entity = entity.clone();
        let external_entity = entity.clone();

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_lg()
                            .child("\u{7f16}\u{8f91}\u{5668}\u{539f}\u{578b}"),
                    )
                    .child(div().text_sm().child(self.path.display().to_string()))
                    .child(Button::new("save").label("\u{4fdd}\u{5b58}").on_click(
                        move |_, _, cx| {
                            save_entity.update(cx, |this, cx| this.save(cx));
                        },
                    ))
                    .child(
                        Button::new("external")
                            .label("\u{6a21}\u{62df}\u{5916}\u{90e8}\u{4fee}\u{6539}")
                            .on_click(move |_, _, cx| {
                                external_entity
                                    .update(cx, |this, cx| this.simulate_external_change(cx));
                            }),
                    ),
            )
            .child(div().px_2().pb_1().text_sm().child(self.status.clone()))
            .child(
                div()
                    .flex_1()
                    .min_h(::gpui::px(240.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(Editor::new(&self.editor)),
            )
    }
}
