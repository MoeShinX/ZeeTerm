//! 应用内的 SSH 提问对话框（V2-U29）：主机指纹确认、密码 / 口令 / keyboard-interactive 输入。
//!
//! 每个窗口（主窗口、拖出的终端窗口、独立文件窗口）各放一个 [`SshPromptLayer`] 当最上层的子元素：
//! 没有提问时它什么都不画；有提问并且轮到本窗口时盖一层半透明遮罩 + 居中卡片，样子和别的应用内对话框
//! 一致、跟着深浅主题走。提问怎么排队、弹在哪个窗口、锁屏时怎么办都在 [`crate::ssh_prompt`]，这里只管画和收键。
//!
//! 密码输入框里的内容不进日志、不进剪贴板；对话框一关（答完、取消、被撤回、锁屏）输入框先清空再丢掉。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ::gpui::{AnyElement, FocusHandle, KeyDownEvent, SharedString, StyleRefinement, Window, px};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ssh::{HostKeyInfo, PromptAnswers, PromptRequest};

use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::main_window::widgets::{
    ButtonLook, check_card, field, modal_card, modal_scrim, text_button,
};
use crate::ssh_prompt::{
    Answer, Ask, HostKeyButton, Hub, Shown, WindowKey, button_decision, enter_allowed,
    enter_button, host_key_buttons, host_key_text, hub, prompt_text, remember_answer,
};
use crate::typography::{Role, Typo as _};

/// 本文件（应用内 SSH 提问对话框）的界面文案。标题、说明、按钮文字来自 `ssh_prompt`；
/// 这里只有详情栏的字段名和底部按钮。主机名、密钥类型、指纹来自远端，照原样显示。
#[derive(Clone, Copy, Debug)]
enum SshPromptMsg {
    Host,
    KeyType,
    SavedType,
    SavedFingerprint,
    CurrentType,
    CurrentFingerprint,
    Sha256Fingerprint,
    Cancel,
    Ok,
}

impl Msg for SshPromptMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Host => "主机",
            Self::KeyType => "密钥类型",
            Self::SavedType => "已保存的类型",
            Self::SavedFingerprint => "已保存的指纹",
            Self::CurrentType => "现在的类型",
            Self::CurrentFingerprint => "现在的指纹",
            Self::Sha256Fingerprint => "SHA256 指纹",
            Self::Cancel => "取消",
            Self::Ok => "确定",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::KeyType => "Key Type",
            Self::SavedType => "Saved Type",
            Self::SavedFingerprint => "Saved Fingerprint",
            Self::CurrentType => "Current Type",
            Self::CurrentFingerprint => "Current Fingerprint",
            Self::Sha256Fingerprint => "SHA256 Fingerprint",
            Self::Cancel => "Cancel",
            Self::Ok => "OK",
        }
    }
}

/// 轮询间隔：对话框最迟这么久之后出现 / 收起。
const POLL: Duration = Duration::from_millis(100);

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// 正在显示的内容。
enum Body {
    HostKey(HostKeyInfo),
    Prompt(PromptForm),
}

struct PromptForm {
    request: PromptRequest,
    inputs: Vec<Entity<InputState>>,
    /// 「用这个密码更新保存的密码」勾上了没有；每次弹出都从不勾开始（要覆盖保险库，必须是用户这一次明确选的）。
    remember: bool,
}

struct View {
    id: u64,
    shown_at: Instant,
    body: Body,
    /// 对话框弹出前窗口里谁有键盘焦点，收起时还给它。
    restore: Option<FocusHandle>,
}

/// 一个窗口里的提问层。
pub struct SshPromptLayer {
    hub: Arc<Hub>,
    key: WindowKey,
    view: Option<View>,
    focus: FocusHandle,
}

impl SshPromptLayer {
    /// `main`：这是主窗口（找不到来源窗口的提问弹在它这里）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>, main: bool) -> Self {
        let hub = Arc::clone(hub());
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        hub.register(key, main);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                // 窗口没了 `update_in` 会失败，任务自己退出。
                if this
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            hub,
            key,
            view: None,
            focus: cx.focus_handle(),
        }
    }

    /// 现在这个窗口上有没有开着的提问对话框（窗口级快捷键据此让路）。
    pub fn is_open(&self) -> bool {
        self.view.is_some()
    }

    /// 对话框上会出现的文字（截图前的隐私检查要把它们算进画面）。
    #[cfg(debug_assertions)]
    pub fn privacy_texts(&self) -> Vec<String> {
        match self.view.as_ref().map(|view| &view.body) {
            Some(Body::HostKey(info)) => {
                let text = host_key_text(info);
                let mut texts = vec![text.host, text.key_type, text.fingerprint];
                texts.extend(text.stored);
                for (key_type, fingerprint) in text.saved {
                    texts.push(key_type);
                    texts.push(fingerprint);
                }
                texts
            }
            Some(Body::Prompt(form)) => {
                let text = prompt_text(&form.request);
                let mut texts = vec![text.title, text.notice, text.instructions];
                texts.extend(text.questions);
                texts.extend(
                    text.remember
                        .into_iter()
                        .flat_map(|(label, hint)| [label, hint]),
                );
                texts
            }
            None => Vec::new(),
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_window_active() {
            self.hub.note_active(self.key);
        }
        // 锁着时不弹，已经弹着的先收起（输入框清空），解锁后再来。
        let locked = crate::app_lock::is_locked();
        let shown = self.hub.poll(self.key, locked, Instant::now());
        match (shown, self.view.as_ref().map(|view| view.id)) {
            (Some(shown), Some(id)) if shown.id == id => {}
            (Some(shown), _) => self.show(shown, window, cx),
            (None, Some(_)) => self.close_view(window, cx),
            (None, None) => {}
        }
    }

    fn show(&mut self, shown: Shown, window: &mut Window, cx: &mut Context<Self>) {
        // 直接换成下一个请求时，上一个的输入框也要清掉。
        let restore = match self.view.as_ref() {
            Some(old) => old.restore.clone(),
            None => window.focused(cx),
        };
        self.clear_inputs(window, cx);
        let body = match shown.ask {
            Ask::HostKey(info) => {
                window.focus(&self.focus, cx);
                Body::HostKey(info)
            }
            Ask::Prompt(request) => {
                let inputs: Vec<Entity<InputState>> = request
                    .prompts
                    .iter()
                    .map(|item| {
                        let masked = !item.echo;
                        cx.new(|cx| InputState::new(window, cx).masked(masked))
                    })
                    .collect();
                if let Some(first) = inputs.first() {
                    let handle = first.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                } else {
                    window.focus(&self.focus, cx);
                }
                Body::Prompt(PromptForm {
                    request,
                    inputs,
                    remember: false,
                })
            }
        };
        self.view = Some(View {
            id: shown.id,
            shown_at: shown.shown_at,
            body,
            restore,
        });
        cx.notify();
    }

    fn clear_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(View {
            body: Body::Prompt(form),
            ..
        }) = &self.view
        {
            for input in &form.inputs {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
    }

    /// 收起对话框：输入框清空，键盘还给原来的地方。
    fn close_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_inputs(window, cx);
        if let Some(view) = self.view.take()
            && let Some(handle) = view.restore
        {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// 把回答交给中转站并收起对话框。
    fn finish(&mut self, answer: Answer, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.view.as_ref().map(|view| view.id) else {
            return;
        };
        self.hub.answer(id, answer);
        self.close_view(window, cx);
    }

    /// 点了指纹对话框上的一个按钮。
    pub(crate) fn press_host_key(
        &mut self,
        button: HostKeyButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(View {
            body: Body::HostKey(info),
            ..
        }) = &self.view
        else {
            return;
        };
        let decision = button_decision(info.status, button);
        self.finish(Answer::HostKey(decision), window, cx);
    }

    /// 输入对话框的「确定」：把每个输入框的内容按顺序交回去。
    pub(crate) fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(View {
            body: Body::Prompt(form),
            ..
        }) = &self.view
        else {
            return;
        };
        let answers: Vec<String> = form
            .inputs
            .iter()
            .map(|input| input.read(cx).value().to_string())
            .collect();
        let remember = remember_answer(&form.request, form.remember);
        self.finish(
            Answer::Prompt(Some(PromptAnswers { answers, remember })),
            window,
            cx,
        );
    }

    fn set_remember(&mut self, ticked: bool, cx: &mut Context<Self>) {
        if let Some(View {
            body: Body::Prompt(form),
            ..
        }) = &mut self.view
        {
            form.remember = ticked;
            cx.notify();
        }
    }

    /// Esc / 「取消」：指纹按拒绝，输入按取消。
    pub(crate) fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = match self.view.as_ref().map(|view| &view.body) {
            Some(Body::HostKey(info)) => {
                // 取消 / 关闭按钮的回答，吊销时同样是拒绝。
                Answer::HostKey(button_decision(info.status, HostKeyButton::Cancel))
            }
            Some(Body::Prompt(_)) => Answer::Prompt(None),
            None => return,
        };
        self.finish(answer, window, cx);
    }

    /// 对话框开着时的按键：Esc 取消，Enter 只触发安全的默认按钮（输入框则是提交）。
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.cancel(window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                match self.view.as_ref() {
                    Some(View {
                        body: Body::HostKey(info),
                        shown_at,
                        ..
                    }) => {
                        // 刚弹出的一小会儿 Enter 不算数：用户可能正在打字。
                        if enter_allowed(*shown_at, Instant::now()) {
                            let button = enter_button(info.status);
                            self.press_host_key(button, window, cx);
                        }
                    }
                    Some(View {
                        body: Body::Prompt(_),
                        ..
                    }) => self.submit_prompt(window, cx),
                    None => return,
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    // ---- 画 ----

    fn host_key_card(
        &self,
        info: &HostKeyInfo,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let text = host_key_text(info);
        let status = info.status;
        let tone = if text.danger {
            tokens.danger
        } else {
            tokens.accent
        };
        let id = self.view.as_ref().map_or(0, |view| view.id);
        let entity = cx.entity();

        let mut details = details_box(tokens);
        details = details.child(detail_row(
            t(SshPromptMsg::Host),
            plain_value(text.host, tokens),
            tokens,
        ));
        if text.saved.is_empty() {
            details = details.child(detail_row(
                t(SshPromptMsg::KeyType),
                plain_value(text.key_type.clone(), tokens),
                tokens,
            ));
        }
        if !text.saved.is_empty() {
            // 换了密钥类型：把已保存的（可能不止一种）和现在出示的并排给用户核对。
            for (index, (key_type, fingerprint)) in text.saved.iter().enumerate() {
                details = details
                    .child(detail_row(
                        t(SshPromptMsg::SavedType),
                        plain_value(key_type.clone(), tokens),
                        tokens,
                    ))
                    .child(detail_row(
                        t(SshPromptMsg::SavedFingerprint),
                        fingerprint_value(
                            ("ssh-fp-old", id * 16 + index as u64),
                            fingerprint,
                            tokens,
                        ),
                        tokens,
                    ));
            }
            details = details
                .child(detail_row(
                    t(SshPromptMsg::CurrentType),
                    plain_value(text.key_type.clone(), tokens),
                    tokens,
                ))
                .child(detail_row(
                    t(SshPromptMsg::CurrentFingerprint),
                    fingerprint_value(("ssh-fp-new", id), &text.fingerprint, tokens),
                    tokens,
                ));
        } else if let Some(stored) = text.stored {
            details = details
                .child(detail_row(
                    t(SshPromptMsg::SavedFingerprint),
                    fingerprint_value(("ssh-fp-old", id), &stored, tokens),
                    tokens,
                ))
                .child(detail_row(
                    t(SshPromptMsg::CurrentFingerprint),
                    fingerprint_value(("ssh-fp-new", id), &text.fingerprint, tokens),
                    tokens,
                ));
        } else {
            details = details.child(detail_row(
                t(SshPromptMsg::Sha256Fingerprint),
                fingerprint_value(("ssh-fp-new", id), &text.fingerprint, tokens),
                tokens,
            ));
        }

        let buttons = host_key_buttons(status)
            .iter()
            .enumerate()
            .map(|(index, button)| {
                let button = *button;
                let look = host_key_look(status, button);
                let entity = entity.clone();
                text_button(
                    format!("ssh-hostkey-button-{index}"),
                    button.label(),
                    look,
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.press_host_key(button, window, cx));
                    },
                )
            });

        self.card(tokens, cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(if text.danger {
                            Ico::ShieldAlert
                        } else {
                            Ico::ShieldCheck
                        })
                        .size_5()
                        .text_color(tone),
                    )
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .when(text.danger, |d| d.text_color(tokens.danger))
                            .child(text.title),
                    ),
            )
            .child(
                div()
                    .typo(Role::Body, tokens)
                    .when(text.danger, |d| {
                        d.text_color(tokens.danger).font_weight(FontWeight::MEDIUM)
                    })
                    .child(text.intro),
            )
            .when(text.show_details, |d| d.child(details))
            .child(h_flex().gap_2().justify_end().children(buttons))
    }

    fn prompt_card(
        &self,
        form: &PromptForm,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let text = prompt_text(&form.request);
        let entity = cx.entity();
        let cancel = entity.clone();
        let submit = entity.clone();
        let fields = form
            .inputs
            .iter()
            .zip(form.request.prompts.iter())
            .zip(text.questions.iter())
            .map(|((state, item), question)| {
                let input = Input::new(state);
                let input = if item.echo {
                    input
                } else {
                    input.mask_toggle()
                };
                field(question.trim(), None, input, tokens)
            });
        self.card(tokens, cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(Ico::KeyRound).size_5().text_color(tokens.accent))
                    .child(div().typo(Role::GroupTitle, tokens).child(text.title)),
            )
            .when(!text.target.is_empty(), |d| {
                d.child(details_box(tokens).child(detail_row(
                    t(SshPromptMsg::Host),
                    plain_value(text.target.clone(), tokens),
                    tokens,
                )))
            })
            // 服务器发来的内容（keyboard-interactive）：先声明「这是服务器说的」，再把它框起来，
            // 免得它的文字冒充成 ZeeTerm 自己的提示。
            .when(!text.notice.is_empty(), |d| {
                d.child(
                    div()
                        .id("ssh-prompt-notice")
                        .typo(Role::Caption, tokens)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.accent)
                        .child(SharedString::from(text.notice.clone())),
                )
            })
            .when(!text.instructions.is_empty(), |d| {
                let instructions = div()
                    .id("ssh-prompt-instructions")
                    .max_h(px(140.0))
                    .overflow_y_scroll()
                    .typo(Role::Body, tokens)
                    .child(SharedString::from(text.instructions.clone()));
                if text.notice.is_empty() {
                    d.child(instructions)
                } else {
                    d.child(details_box(tokens).child(instructions))
                }
            })
            .children(fields)
            .when_some(text.remember.clone(), |d, (label, hint)| {
                let toggle = entity.clone();
                d.child(check_card(
                    "ssh-prompt-remember",
                    &label,
                    Some(&hint),
                    form.remember,
                    tokens,
                    move |ticked, _window, cx| {
                        toggle.update(cx, |this, cx| this.set_remember(ticked, cx));
                    },
                ))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "ssh-prompt-cancel",
                        t(SshPromptMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.cancel(window, cx));
                        },
                    ))
                    .child(text_button(
                        "ssh-prompt-ok",
                        t(SshPromptMsg::Ok),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            submit.update(cx, |this, cx| this.submit_prompt(window, cx));
                        },
                    )),
            )
    }

    /// 两种对话框共用的卡片：收键盘、Esc / Enter 在这里处理。
    fn card(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Stateful<Div> {
        modal_card("ssh-prompt-card", 520.0, false, tokens)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
    }
}

/// 按钮的外观：安全的默认按钮用主色，危险动作用红色。
fn host_key_look(status: zssh_core::ssh::HostKeyStatus, button: HostKeyButton) -> ButtonLook {
    match button {
        HostKeyButton::UpdateAndConnect => ButtonLook::Danger,
        // 换了密钥类型：信任它要有明确的警惕，不是默认动作。
        HostKeyButton::TrustAndSave
            if status == zssh_core::ssh::HostKeyStatus::OtherKeyTypesSaved =>
        {
            ButtonLook::Danger
        }
        _ if button == enter_button(status) => ButtonLook::Primary,
        _ => ButtonLook::Outline,
    }
}

/// 浅底带边框的信息块（指纹框和输入框上的「主机」行共用）。
fn details_box(tokens: &Tokens) -> Div {
    v_flex()
        .gap_2()
        .p_3()
        .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
        .bg(tokens.subtle)
        .border_1()
        .border_color(tokens.border)
}

/// 一行「名称 + 值」，名称固定宽度，值占满剩下的地方并自动换行。
fn detail_row(label: &'static str, value: impl IntoElement, tokens: &Tokens) -> Div {
    h_flex()
        .gap_3()
        .items_start()
        .child(
            div()
                .w(px(88.0))
                .flex_none()
                .typo(Role::Caption, tokens)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(value))
}

fn plain_value(value: String, tokens: &Tokens) -> Div {
    div()
        .typo(Role::Body, tokens)
        .child(SharedString::from(value))
}

/// 指纹：等宽字体、可以选中复制，太长自动换行。
fn fingerprint_value(id: (&'static str, u64), fingerprint: &str, tokens: &Tokens) -> TextView {
    // 放进 Markdown 的行内代码里取等宽字体；指纹本身是 Base64，保险起见去掉反引号。
    let source = format!("`{}`", fingerprint.replace('`', ""));
    TextView::markdown(id, SharedString::from(source))
        .selectable(true)
        .text_size(px(13.0))
        .text_color(tokens.text)
        .inline_code_style(
            StyleRefinement::default()
                .font_family(crate::fonts::mono_text_family())
                .text_color(tokens.text)
                // 去掉行内代码默认的底色和内边距：这里只是要等宽字体，不是要一块高亮。
                .bg(::gpui::Hsla::transparent_black())
                .p_0(),
        )
}

impl Render for SshPromptLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let tokens = Tokens::current(cx);
        let card: AnyElement = match &view.body {
            Body::HostKey(info) => self.host_key_card(info, &tokens, cx).into_any_element(),
            Body::Prompt(form) => self.prompt_card(form, &tokens, cx).into_any_element(),
        };
        let entity = cx.entity();
        // 点遮罩不取消（指纹确认不能被一次误点丢掉），只把键盘焦点收回到对话框，Esc 才有人接。
        modal_scrim("ssh-prompt-scrim", false, move |window, cx| {
            entity.update(cx, |this, cx| {
                if let Some(View {
                    body: Body::HostKey(_),
                    ..
                }) = &this.view
                {
                    window.focus(&this.focus, cx);
                }
            });
        })
        .child(card)
        .into_any_element()
    }
}

impl Drop for SshPromptLayer {
    fn drop(&mut self) {
        // 窗口关了：正在这个窗口显示的请求按拒绝 / 取消回答，排队的转去别的窗口。
        self.hub.unregister(self.key);
    }
}

/// 截图驱动用的调试入口（只在 debug 构建里存在）。
#[cfg(debug_assertions)]
pub mod dev {
    use std::cell::RefCell;

    use zssh_core::ssh::{HostKeyDecision, HostKeyStatus, PromptItem, PromptKind};

    use super::*;
    use crate::ssh_prompt::Pending;

    thread_local! {
        /// 演示请求的“连接侧”：留着它请求才不会被撤回，也能读到用户的回答。
        static HELD: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    }

    /// 演示用的假主机：文档保留域名，指纹是随手编的。
    fn demo_info(status: HostKeyStatus) -> HostKeyInfo {
        HostKeyInfo {
            host: "demo.example.com".to_owned(),
            port: 22,
            key_type: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:Zk3q7Hn0pXw9eVdR4yTb1cLfJmA8sGuO2iNhPaKxB5E".to_owned(),
            status,
            stored_fingerprint: matches!(status, HostKeyStatus::Changed)
                .then(|| "SHA256:Qe8mT2vLx0RjH5cYbN7uWd3kPaZfS1oIgA9tXyC4nM6".to_owned()),
            saved_keys: if status == HostKeyStatus::OtherKeyTypesSaved {
                vec![zssh_core::ssh::SavedHostKey {
                    key_type: "ecdsa-sha2-nistp256".to_owned(),
                    fingerprint: "SHA256:Qe8mT2vLx0RjH5cYbN7uWd3kPaZfS1oIgA9tXyC4nM6".to_owned(),
                }]
            } else {
                Vec::new()
            },
        }
    }

    /// 放一个假的指纹确认请求：`unknown` / `other` / `changed` / `revoked`。
    pub fn host_key_demo(kind: &str) -> Result<(), String> {
        let status = match kind {
            "unknown" => HostKeyStatus::Unknown,
            "other" => HostKeyStatus::OtherKeyTypesSaved,
            "changed" => HostKeyStatus::Changed,
            "revoked" => HostKeyStatus::Revoked,
            _ => return Err("kind must be unknown / other / changed / revoked".to_owned()),
        };
        let pending = hub().submit(Ask::HostKey(demo_info(status)));
        HELD.with(|held| held.borrow_mut().push(pending));
        Ok(())
    }

    /// 放一个假的输入请求：`password`（一个密码框）或 `ki`（带说明和两个问题的 keyboard-interactive）。
    pub fn prompt_demo(kind: &str) -> Result<(), String> {
        let request = match kind {
            "password" => PromptRequest::single(
                PromptKind::Password,
                "ops@demo.example.com:22",
                "Enter your password",
                true,
            ),
            "ki" => PromptRequest {
                target: "ops@demo.example.com:22".to_owned(),
                kind: PromptKind::KeyboardInteractive,
                name: "Two-step verification".to_owned(),
                instructions: {
                    "Enter your login password and the code from your phone.\nThe code is valid for 30 seconds."
                        .to_owned()
                },
                prompts: vec![
                    PromptItem {
                        text: "Password: ".to_owned(),
                        echo: false,
                    },
                    PromptItem {
                        text: "Code: ".to_owned(),
                        echo: true,
                    },
                ],
                can_remember: false,
                reason: None,
            },
            "rejected" => {
                // 保存的密码被拒之后重新询问的那一次（原因由界面出文案，勾选框默认不勾）。
                let mut request = PromptRequest::single(
                    PromptKind::Password,
                    "ops@demo.example.com:22",
                    "Enter your password",
                    true,
                );
                request.reason = Some(zssh_core::ssh::PromptReason::SavedPasswordRejected);
                request
            }
            _ => return Err("kind must be password, rejected or ki".to_owned()),
        };
        let pending = hub().submit(Ask::Prompt(request));
        HELD.with(|held| held.borrow_mut().push(pending));
        Ok(())
    }

    /// 最早一个已经有回答的演示请求的回答，读走（`None` = 还没有人回答）。
    pub fn take_answer() -> Option<String> {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            let index = held.iter_mut().position(|p| p.peek().is_some())?;
            let mut pending = held.remove(index);
            Some(match pending.peek()? {
                Answer::HostKey(HostKeyDecision::TrustAndSave) => "trust_save".to_owned(),
                Answer::HostKey(HostKeyDecision::TrustOnce) => "trust_once".to_owned(),
                Answer::HostKey(HostKeyDecision::Reject) => "reject".to_owned(),
                Answer::Prompt(None) => "cancel".to_owned(),
                Answer::Prompt(Some(answers)) => format!(
                    "answers:{}{}",
                    answers.answers.join(","),
                    if answers.remember { ";remember" } else { "" }
                ),
            })
        })
    }

    /// 还没回答的演示请求有几个。
    pub fn waiting() -> usize {
        HELD.with(|held| {
            held.borrow_mut()
                .iter_mut()
                .map(|p| p.peek().is_none())
                .filter(|waiting| *waiting)
                .count()
        })
    }

    impl SshPromptLayer {
        /// 驱动脚本：按名字点一个指纹对话框上的按钮。
        pub fn dev_press(
            &mut self,
            label: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Result<(), String> {
            let status = match self.view.as_ref().map(|view| &view.body) {
                Some(Body::HostKey(info)) => info.status,
                _ => {
                    return Err("the open dialog is not a host-key confirmation dialog".to_owned());
                }
            };
            let button = host_key_buttons(status)
                .iter()
                .copied()
                .find(|button| button.label() == label)
                .ok_or_else(|| format!("the dialog has no button labeled {label}"))?;
            self.press_host_key(button, window, cx);
            Ok(())
        }

        /// 驱动脚本：勾 / 取消「更新保存的密码」（和点勾选框同一个方法）；对话框没有这个勾选框就报错。
        pub fn dev_set_remember(
            &mut self,
            ticked: bool,
            cx: &mut Context<Self>,
        ) -> Result<(), String> {
            let offered = match self.view.as_ref().map(|view| &view.body) {
                Some(Body::Prompt(form)) => crate::ssh_prompt::remember_offered(&form.request),
                _ => false,
            };
            if !offered {
                return Err("the open dialog has no update-saved-password checkbox".to_owned());
            }
            self.set_remember(ticked, cx);
            Ok(())
        }

        /// 驱动脚本：往输入对话框的第 `index` 个输入框填内容。
        pub fn dev_type(
            &mut self,
            index: usize,
            text: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Result<(), String> {
            let Some(View {
                body: Body::Prompt(form),
                ..
            }) = &self.view
            else {
                return Err("the open dialog is not an input dialog".to_owned());
            };
            let input = form.inputs.get(index).ok_or("no such input field")?;
            input.update(cx, |state, cx| state.set_value(text, window, cx));
            Ok(())
        }
    }
}
