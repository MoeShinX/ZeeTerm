//! 设置 → AI → 内置助手 →「添加接口」的预设列表，以及「用 Tokenway 连接」（V2-A5）。
//!
//! - 预设列表：Tokenway 在第一位（带crate::i18n::t(PresetMsg::Recommended)和一句介绍），后面是 OpenAI / Anthropic / DeepSeek /
//!   OpenRouter / Ollama 和"自定义"，一个都不隐藏。非 Tokenway 的预设只是帮用户把添加接口的表单预填好；
//! - 连接对话框：「用 Tokenway 连接」（浏览器授权 + PKCE + 本机回环，进度和取消都在这里）、
//!   「我已有 Key，直接粘贴」、「还没有账号？注册 Tokenway」；
//! - 断开：确认后通知 Tokenway 停用（失败也继续）、删保险库里的 Key、删接口配置。
//!
//! 业务（验签、白名单、授权、换令牌、存库）都在 core 的 `tokenway` 模块和 `agent_setup::tokenway`，
//! 这里只拼装和画。授权 / 换令牌 / 余额 / 停用的地址只来自验过签的清单或内置清单，界面上改不了。

use std::sync::Arc;

use ::gpui::{SharedString, Window};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tokio::sync::Notify;
use zssh_core::ai::agent::builtin::config::EndpointKind;
use zssh_core::ai::agent::builtin::tokenway::endpoint::PRESET_PASTED;
use zssh_core::ai::agent::builtin::tokenway::{AUTHORIZE_TIMEOUT, ConnectError, Stage};

use super::MainWindow;
use super::modal::{Confirm, ConfirmAction, Modal};
use super::widgets::{BadgeKind, ButtonLook, badge, error_note, field, note, text_button};
use crate::agent_setup::presets::{LocalPreset, PresetRow, preset_rows};
use crate::agent_setup::tokenway::{self, Connected};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// Tokenway 预设与接口连接向导的文案。
#[derive(Clone, Copy, Debug)]
enum PresetMsg {
    DevicePlaceholder,
    NoPreset,
    RuntimeDownConnect,
    RuntimeDownDisconnect,
    Cancelled,
    RegisterOpened,
    DisconnectTokenwayBody,
    DisconnectKeyBody,
    DisconnectTitle,
    DisconnectConfirm,
    DisconnectedOk,
    Recommended,
    AddEndpoint,
    AddIntro,
    ReconnectTokenway,
    ConnectTokenway,
    StaleAuth,
    BrowserIntro,
    BalanceNote,
    WaitingAuth,
    ReceivingToken,
    BrowserOpened,
    AlmostDone,
    DeviceName,
    DeviceNameHint,
    Cancel,
    Retry,
    ConnectWithTokenway,
    NoAccount,
    PasteKey,
    OpenDbFailedPrefix,
    DefaultModelSuffix,
    ConnectedPrefix,
    ConnectedSuffix,
    AuthSaveFailedPrefix,
    RegisterOpenFailedPrefix,
    DisconnectFailedPrefix,
    DisconnectedLocalPrefix,
    DisconnectedLocalSuffix,
}

impl crate::i18n::Msg for PresetMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::DevicePlaceholder => "例如：办公室笔记本",
            Self::NoPreset => "没有可用的 Tokenway 预设",
            Self::RuntimeDownConnect => "后台运行时没起来，不能连接",
            Self::RuntimeDownDisconnect => "后台运行时没起来，不能断开",
            Self::Cancelled => "已取消连接",
            Self::RegisterOpened => {
                "已用浏览器打开 Tokenway 的注册页；注册好后回来点「用 Tokenway 连接」"
            }
            Self::DisconnectTokenwayBody => {
                "会通知 Tokenway 停用这台电脑的授权令牌（网络不通时只删本地，之后可以在 Tokenway 网站的「已授权的应用」里停用），再从加密保险库里删掉令牌、删掉这个接口。同步到其他电脑时，那边也会删除。"
            }
            Self::DisconnectKeyBody => {
                "会从加密保险库里删掉这个 Key、删掉这个接口（不会通知 Tokenway；Key 本身要在网站上自己停用）。同步到其他电脑时，那边也会删除。"
            }
            Self::DisconnectTitle => "断开 Tokenway？",
            Self::DisconnectConfirm => "断开",
            Self::DisconnectedOk => "已断开 Tokenway",
            Self::Recommended => "推荐",
            Self::AddEndpoint => "添加接口",
            Self::AddIntro => {
                "选一个起点：Tokenway 可以在浏览器里一键授权，不用复制 Key；其他的帮你把地址和类型填好，Key 自己贴。"
            }
            Self::ReconnectTokenway => "重新连接 Tokenway",
            Self::ConnectTokenway => "连接 Tokenway",
            Self::StaleAuth => {
                "之前的授权已经失效。重新授权一次，ZeeTerm 会换上新的令牌，名字、默认模型这些设置都保留。"
            }
            Self::BrowserIntro => {
                "点「用 Tokenway 连接」会用系统浏览器打开 Tokenway 的授权页：在网页上登录并点「授权」，ZeeTerm 就会拿到这台电脑专用的令牌。不用复制 Key，也不用在 ZeeTerm 里输入 Tokenway 的密码。"
            }
            Self::BalanceNote => {
                "这个令牌只能使用充值余额，不使用赠送额度（赠送额度可以在网页和其他客户端试用）。授权可以随时在 Tokenway 网站的「已授权的应用」里停用。"
            }
            Self::WaitingAuth => "正在等待浏览器授权…",
            Self::ReceivingToken => "已收到授权，正在领取令牌…",
            Self::BrowserOpened => {
                "已用系统浏览器打开 Tokenway 的授权页。在网页上登录并点「授权」，然后回到这里；最多等 5 分钟，不想等可以取消。"
            }
            Self::AlmostDone => "马上就好。",
            Self::DeviceName => "设备名",
            Self::DeviceNameHint => "显示在 Tokenway 的授权页和「已授权的应用」里；不要填你的真名",
            Self::Cancel => "取消",
            Self::Retry => "重试",
            Self::ConnectWithTokenway => "用 Tokenway 连接",
            Self::NoAccount => "还没有账号？注册 Tokenway",
            Self::PasteKey => "我已有 Key，直接粘贴",
            Self::OpenDbFailedPrefix => "打开设置库失败：",
            Self::DefaultModelSuffix => "，默认模型 ",
            Self::ConnectedPrefix => "已连接 Tokenway",
            Self::ConnectedSuffix => "。令牌只存在这台电脑的加密保险库里",
            Self::AuthSaveFailedPrefix => "授权成功了，但没能保存：",
            Self::RegisterOpenFailedPrefix => "没能打开注册页：",
            Self::DisconnectFailedPrefix => "断开失败：",
            Self::DisconnectedLocalPrefix => {
                "已断开 Tokenway（本地的令牌和配置都删了；没能通知服务端停用："
            }
            Self::DisconnectedLocalSuffix => "，可以在网站的「已授权的应用」里停用）",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::DevicePlaceholder => "For example: Office laptop",
            Self::NoPreset => "No Tokenway preset is available",
            Self::RuntimeDownConnect => {
                "The background runtime is not running, so the connection cannot start"
            }
            Self::RuntimeDownDisconnect => {
                "The background runtime is not running, so disconnecting is not possible"
            }
            Self::Cancelled => "Connection cancelled",
            Self::RegisterOpened => {
                "The Tokenway sign-up page is open in your browser; after signing up, come back and click \"Connect with Tokenway\""
            }
            Self::DisconnectTokenwayBody => {
                "This asks Tokenway to revoke the token for this computer (if the network is down only the local copy is deleted, and you can revoke it later under \"Authorized apps\" on the Tokenway website), then deletes the token from the encrypted vault and removes this endpoint. Synced computers will remove it too."
            }
            Self::DisconnectKeyBody => {
                "This deletes the key from the encrypted vault and removes this endpoint (Tokenway is not notified; revoke the key on its website yourself). Synced computers will remove it too."
            }
            Self::DisconnectTitle => "Disconnect Tokenway?",
            Self::DisconnectConfirm => "Disconnect",
            Self::DisconnectedOk => "Disconnected from Tokenway",
            Self::Recommended => "Recommended",
            Self::AddEndpoint => "Add Endpoint",
            Self::AddIntro => {
                "Choose a starting point: Tokenway can be authorized in one click in your browser with no key to copy; the others prefill the address and type, and you paste the key yourself."
            }
            Self::ReconnectTokenway => "Reconnect Tokenway",
            Self::ConnectTokenway => "Connect Tokenway",
            Self::StaleAuth => {
                "The previous authorization is no longer valid. Authorize again and ZeeTerm will store a new token, keeping the name, default model and other settings."
            }
            Self::BrowserIntro => {
                "Click \"Connect with Tokenway\" to open the Tokenway authorization page in your system browser: sign in and click Authorize, and ZeeTerm receives a token dedicated to this computer. No key to copy and no Tokenway password to type into ZeeTerm."
            }
            Self::BalanceNote => {
                "This token can only use your purchased balance, not the free credit (use the free credit on the website or in other clients). You can revoke the authorization at any time under \"Authorized apps\" on the Tokenway website."
            }
            Self::WaitingAuth => "Waiting for browser authorization…",
            Self::ReceivingToken => "Authorization received; fetching the token…",
            Self::BrowserOpened => {
                "The Tokenway authorization page is open in your system browser. Sign in, click Authorize, then come back; this waits up to 5 minutes and you can cancel."
            }
            Self::AlmostDone => "Almost done.",
            Self::DeviceName => "Device name",
            Self::DeviceNameHint => {
                "Shown on the Tokenway authorization page and under \"Authorized apps\"; do not use your real name"
            }
            Self::Cancel => "Cancel",
            Self::Retry => "Retry",
            Self::ConnectWithTokenway => "Connect with Tokenway",
            Self::NoAccount => "No account yet? Sign up for Tokenway",
            Self::PasteKey => "I already have a key — paste it",
            Self::OpenDbFailedPrefix => "Failed to open the settings database: ",
            Self::DefaultModelSuffix => ", default model ",
            Self::ConnectedPrefix => "Connected to Tokenway",
            Self::ConnectedSuffix => {
                ". The token is stored only in this computer's encrypted vault"
            }
            Self::AuthSaveFailedPrefix => "Authorization succeeded, but saving failed: ",
            Self::RegisterOpenFailedPrefix => "Could not open the sign-up page: ",
            Self::DisconnectFailedPrefix => "Disconnect failed: ",
            Self::DisconnectedLocalPrefix => {
                "Disconnected from Tokenway (the local token and configuration were deleted, but the service could not be notified to revoke it: "
            }
            Self::DisconnectedLocalSuffix => {
                " — revoke it under \"Authorized apps\" on the website)"
            }
        }
    }
}

/// 「添加接口」表单的预填内容（来自某个预设）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefill {
    pub preset_id: Option<String>,
    pub name: String,
    pub kind: EndpointKind,
    pub base_url: String,
    pub no_key: bool,
}

impl From<&LocalPreset> for Prefill {
    fn from(preset: &LocalPreset) -> Self {
        Self {
            preset_id: preset.id.map(str::to_owned),
            // 「自定义」不预填名字（让用户自己起）。
            name: if preset.id.is_some() {
                preset.name.to_owned()
            } else {
                String::new()
            },
            kind: preset.kind,
            base_url: preset.base_url.to_owned(),
            no_key: preset.no_key,
        }
    }
}

/// 连接对话框所处的阶段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// 还没开始（或已取消）。
    Idle,
    /// 已用系统浏览器打开授权页，等用户在网页上点「授权」。
    Waiting,
    /// 收到授权码，正在换令牌。
    Exchanging,
    /// 失败了，原因在里面。
    Failed(String),
}

impl Phase {
    pub fn running(&self) -> bool {
        matches!(self, Self::Waiting | Self::Exchanging)
    }
}

pub struct TokenwayDialog {
    pub device: Entity<InputState>,
    pub phase: Phase,
    /// 每点一次「连接」加一；别的代的进度 / 结果（取消了、关了对话框）一律作废。
    pub generation: u64,
    pub cancel: Option<Arc<Notify>>,
    /// 重新连接（令牌失效后）：标题和说明不同。
    pub reconnect: bool,
    /// 中性的小提示（比如"已取消"）。
    pub note: Option<String>,
}

enum Flow {
    Stage(Stage),
    Finished(Box<Result<Connected, ConnectError>>),
}

/// 阶段的文字（界面和驱动脚本断言共用）。
#[cfg(any(debug_assertions, test))]
pub fn phase_name(phase: &Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Waiting => "waiting",
        Phase::Exchanging => "exchanging",
        Phase::Failed(_) => "failed",
    }
}

impl MainWindow {
    /// 「添加接口」：先选预设。
    pub(crate) fn open_ai_presets(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.modal = Some(Modal::AiPresets);
        cx.notify();
    }

    fn preset_rows_now(&self) -> Vec<PresetRow> {
        preset_rows(tokenway::tokenway_preset().as_ref())
    }

    /// 点了预设列表里的一行（下标对应 [`preset_rows`] 的顺序）。
    pub(crate) fn ai_pick_preset(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.preset_rows_now().into_iter().nth(index) else {
            return;
        };
        match row {
            PresetRow::Tokenway { .. } => self.open_tokenway_dialog(false, window, cx),
            PresetRow::Local(preset) => {
                self.open_ai_endpoint_dialog_with(None, Some(Prefill::from(&preset)), window, cx)
            }
        }
    }

    // ---- 连接对话框 ----

    pub(crate) fn open_tokenway_dialog(
        &mut self,
        reconnect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 正在连接的对话框别重复开（重新连接按钮在面板里可以连点）。
        if let Some(Modal::Tokenway(dialog)) = &self.modal
            && dialog.phase.running()
        {
            return;
        }
        let device = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(tokenway::default_device_name())
                .placeholder(crate::i18n::t(PresetMsg::DevicePlaceholder))
        });
        self.modal = Some(Modal::Tokenway(Box::new(TokenwayDialog {
            device,
            phase: Phase::Idle,
            generation: 0,
            cancel: None,
            reconnect,
            note: None,
        })));
        cx.notify();
    }

    fn with_tokenway_dialog(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut TokenwayDialog),
    ) {
        if let Some(Modal::Tokenway(dialog)) = &mut self.modal {
            change(dialog);
            cx.notify();
        }
    }

    /// 「用 Tokenway 连接」。
    pub(crate) fn tokenway_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Tokenway(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.phase.running() {
            return;
        }
        let device = dialog.device.read(cx).value().trim().to_owned();
        let Some(preset) = tokenway::tokenway_preset() else {
            dialog.phase = Phase::Failed(crate::i18n::t(PresetMsg::NoPreset).to_owned());
            cx.notify();
            return;
        };
        let transport = match tokenway::transport() {
            Ok(transport) => transport,
            Err(reason) => {
                dialog.phase = Phase::Failed(reason);
                cx.notify();
                return;
            }
        };
        let Ok(runtime) = crate::ssh_backend::runtime() else {
            dialog.phase = Phase::Failed(crate::i18n::t(PresetMsg::RuntimeDownConnect).to_owned());
            cx.notify();
            return;
        };
        let cancel = Arc::new(Notify::new());
        dialog.generation += 1;
        let generation = dialog.generation;
        dialog.cancel = Some(cancel.clone());
        dialog.phase = Phase::Waiting;
        dialog.note = None;

        let policy = tokenway::policy();
        let opener = tokenway::opener();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Flow>();
        let stage_tx = tx.clone();
        runtime.spawn(async move {
            let deps = tokenway::ConnectDeps {
                preset: &preset,
                policy: &policy,
                transport: transport.as_ref(),
                opener: opener.as_ref(),
                device: &device,
                timeout: AUTHORIZE_TIMEOUT,
            };
            let result = tokenway::run_connect(&deps, &cancel, &move |stage| {
                let _ = stage_tx.send(Flow::Stage(stage));
            })
            .await;
            let _ = tx.send(Flow::Finished(Box::new(result)));
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some(flow) = rx.recv().await {
                let finished = matches!(flow, Flow::Finished(_));
                let _ = this.update_in(cx, |this, window, cx| {
                    this.tokenway_flow_event(generation, flow, window, cx)
                });
                if finished {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn tokenway_flow_event(
        &mut self,
        generation: u64,
        flow: Flow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = matches!(&self.modal, Some(Modal::Tokenway(d)) if d.generation == generation);
        if !current {
            return;
        }
        match flow {
            Flow::Stage(Stage::WaitingForBrowser) => {
                self.with_tokenway_dialog(cx, |d| d.phase = Phase::Waiting);
            }
            Flow::Stage(Stage::Exchanging) => {
                self.with_tokenway_dialog(cx, |d| d.phase = Phase::Exchanging);
            }
            Flow::Finished(result) => match *result {
                Ok(connected) => self.tokenway_connected(connected, window, cx),
                Err(err) if err.is_user_choice() => {
                    self.with_tokenway_dialog(cx, |d| {
                        d.phase = Phase::Idle;
                        d.cancel = None;
                        d.note = Some(err.to_string());
                    });
                }
                Err(err) => {
                    self.with_tokenway_dialog(cx, |d| {
                        d.phase = Phase::Failed(err.to_string());
                        d.cancel = None;
                    });
                }
            },
        }
    }

    /// 授权成功：把令牌存进保险库、建一条接口配置，关掉对话框。
    fn tokenway_connected(
        &mut self,
        connected: Connected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = (|| {
            let preset = tokenway::tokenway_preset().ok_or(crate::i18n::t(PresetMsg::NoPreset))?;
            let db = crate::host_db::open_database()
                .map_err(|err| format!("{}{err}", crate::i18n::t(PresetMsg::OpenDbFailedPrefix)))?;
            tokenway::finish_connect(&db, &preset, &connected, &tokenway::policy())
        })();
        match saved {
            Ok(endpoint) => {
                crate::agent_setup::builtin::remote_forget(&endpoint.uid);
                crate::agent_setup::builtin::reload();
                self.ai_builtin.reload_rows();
                self.ai_builtin.note = None;
                self.modal = None;
                self.focus_content(window, cx);
                let model = endpoint
                    .default_model
                    .as_deref()
                    .map(|m| format!("{}{m}", crate::i18n::t(PresetMsg::DefaultModelSuffix)))
                    .unwrap_or_default();
                self.notice(
                    &format!(
                        "{}{model}{}",
                        crate::i18n::t(PresetMsg::ConnectedPrefix),
                        crate::i18n::t(PresetMsg::ConnectedSuffix)
                    ),
                    window,
                    cx,
                );
            }
            Err(reason) => {
                self.with_tokenway_dialog(cx, |d| {
                    d.phase = Phase::Failed(format!(
                        "{}{reason}",
                        crate::i18n::t(PresetMsg::AuthSaveFailedPrefix)
                    ));
                    d.cancel = None;
                });
            }
        }
    }

    /// 取消进行中的连接（回到未开始）。
    pub(crate) fn tokenway_cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.with_tokenway_dialog(cx, |d| {
            if let Some(cancel) = d.cancel.take() {
                cancel.notify_one();
            }
            // 作废这一代的进度 / 结果，界面立刻回到未开始。
            d.generation += 1;
            d.phase = Phase::Idle;
            d.note = Some(crate::i18n::t(PresetMsg::Cancelled).to_owned());
        });
    }

    /// Enter：没在连接就开始连接。
    pub(crate) fn tokenway_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idle = matches!(&self.modal, Some(Modal::Tokenway(d)) if !d.phase.running());
        if idle {
            self.tokenway_connect(window, cx);
        }
    }

    /// 关对话框（Esc、取消按钮）：进行中的连接一并取消，监听随之关闭。
    pub(crate) fn tokenway_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::Tokenway(dialog)) = &mut self.modal
            && let Some(cancel) = dialog.cancel.take()
        {
            cancel.notify_one();
        }
        self.modal = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 「我已有 Key，直接粘贴」：进添加接口表单，名称 / 地址按预设填好，Key 自己贴。
    pub(crate) fn tokenway_paste_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::Tokenway(dialog)) = &mut self.modal
            && let Some(cancel) = dialog.cancel.take()
        {
            cancel.notify_one();
        }
        let Some(preset) = tokenway::tokenway_preset() else {
            self.notice(crate::i18n::t(PresetMsg::NoPreset), window, cx);
            return;
        };
        let prefill = Prefill {
            preset_id: Some(PRESET_PASTED.to_owned()),
            name: preset.name.clone(),
            kind: EndpointKind::OpenAiChat,
            base_url: preset.openai_base.clone().unwrap_or_default(),
            no_key: false,
        };
        self.open_ai_endpoint_dialog_with(None, Some(prefill), window, cx);
    }

    /// 「还没有账号？注册 Tokenway」：打开清单里的注册页（带的邀请码也是清单给的）。
    pub(crate) fn tokenway_register(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match tokenway::open_register() {
            Ok(()) => self.with_tokenway_dialog(cx, |d| {
                d.note = Some(crate::i18n::t(PresetMsg::RegisterOpened).to_owned());
            }),
            Err(reason) => self.notice(
                &format!(
                    "{}{reason}",
                    crate::i18n::t(PresetMsg::RegisterOpenFailedPrefix)
                ),
                window,
                cx,
            ),
        }
    }

    // ---- 断开 ----

    pub(crate) fn ask_disconnect_tokenway(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(row) = self.ai_builtin.rows.iter().find(|row| row.id == id) else {
            return;
        };
        let authorized = row.preset_id.as_deref()
            == Some(zssh_core::ai::agent::builtin::tokenway::endpoint::PRESET_AUTHORIZED);
        let message = if authorized {
            crate::i18n::t(PresetMsg::DisconnectTokenwayBody)
        } else {
            crate::i18n::t(PresetMsg::DisconnectKeyBody)
        };
        self.modal = Some(Modal::Confirm(Confirm {
            title: crate::i18n::t(PresetMsg::DisconnectTitle).to_owned(),
            message: message.to_owned(),
            confirm_label: Some(crate::i18n::t(PresetMsg::DisconnectConfirm).to_owned()),
            destructive: true,
            action: ConfirmAction::DisconnectTokenway(id),
        }));
        cx.notify();
    }

    pub(super) fn do_disconnect_tokenway(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(runtime) = crate::ssh_backend::runtime() else {
            self.notice(crate::i18n::t(PresetMsg::RuntimeDownDisconnect), window, cx);
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let _ = tx.send(tokenway::disconnect_endpoint(id).await);
        });
        self.await_result(rx, window, cx, |this, result, window, cx| {
            this.ai_builtin.reload_rows();
            match result {
                Ok(report) => {
                    let text = match report.revoked {
                        Ok(()) => crate::i18n::t(PresetMsg::DisconnectedOk).to_owned(),
                        Err(err) => format!(
                            "{}{err}{}",
                            crate::i18n::t(PresetMsg::DisconnectedLocalPrefix),
                            crate::i18n::t(PresetMsg::DisconnectedLocalSuffix)
                        ),
                    };
                    this.notice(&text, window, cx);
                }
                Err(reason) => this.notice(
                    &format!(
                        "{}{reason}",
                        crate::i18n::t(PresetMsg::DisconnectFailedPrefix)
                    ),
                    window,
                    cx,
                ),
            }
            cx.notify();
        });
    }

    // ---- 节拍 ----

    /// 主窗口每个节拍调：面板里点了「重新连接」就开连接对话框；选中的 Tokenway 令牌该取余额时在后台取；
    /// 余额显示变了就让界面重画。
    pub(super) fn tokenway_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        if tokenway::take_reconnect_request() {
            self.open_tokenway_dialog(true, window, cx);
            changed = true;
        }
        // 面板开着、选中的是浏览器授权的 Tokenway 令牌：打开面板时和一轮对话结束后取余额。
        if self.docks.ai_open
            && let Some(chat) = self.docks.ai.clone()
            && let Some(id) = chat.read(cx).tokenway_endpoint()
        {
            tokenway::balance_poll(id);
        }
        // 设置页的接口列表里每个浏览器授权的令牌也显示余额。
        if self.nav.view == super::MainView::Settings {
            for row in &self.ai_builtin.rows {
                if row.preset_id.as_deref()
                    == Some(zssh_core::ai::agent::builtin::tokenway::endpoint::PRESET_AUTHORIZED)
                {
                    tokenway::balance_poll(row.id);
                }
            }
        }
        let version = tokenway::balance_version();
        if version != self.tokenway_seen {
            self.tokenway_seen = version;
            changed = true;
        }
        changed
    }

    // ---- 画 ----

    pub(super) fn ai_presets_body(&self, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let rows = self.preset_rows_now();
        let mut list = v_flex().w_full().gap_2();
        for (index, row) in rows.iter().enumerate() {
            let pick = entity.clone();
            let hover = tokens.hover;
            let recommended = row.recommended();
            list = list.child(
                h_flex()
                    .id(("ai-preset", index))
                    .w_full()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if recommended {
                        tokens.accent
                    } else {
                        tokens.border
                    })
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .on_click(move |_, window, cx| {
                        pick.update(cx, |this, cx| this.ai_pick_preset(index, window, cx));
                    })
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .typo(Role::ItemName, tokens)
                                            .child(SharedString::from(row.title().to_owned())),
                                    )
                                    .when(recommended, |d| {
                                        d.child(badge(
                                            crate::i18n::t(PresetMsg::Recommended),
                                            BadgeKind::Good,
                                            tokens,
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .typo(Role::Caption, tokens)
                                    .child(SharedString::from(row.tagline().to_owned())),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size_4()
                            .text_color(tokens.text_muted),
                    ),
            );
        }
        let close = entity.clone();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(crate::i18n::t(PresetMsg::AddEndpoint)),
                    )
                    .child(note(crate::i18n::t(PresetMsg::AddIntro), tokens)),
            )
            .child(list)
            .child(h_flex().justify_end().child(text_button(
                "ai-presets-cancel",
                crate::i18n::t(PresetMsg::Cancel),
                ButtonLook::Outline,
                move |_, window, cx| {
                    close.update(cx, |this, cx| this.close_modal(window, cx));
                },
            )))
            .into_any_element()
    }

    pub(super) fn tokenway_body(
        &self,
        dialog: &TokenwayDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let running = dialog.phase.running();
        let title = if dialog.reconnect {
            crate::i18n::t(PresetMsg::ReconnectTokenway)
        } else {
            crate::i18n::t(PresetMsg::ConnectTokenway)
        };
        let mut body = v_flex().gap_3();
        body = body.child(
            v_flex()
                .gap_1()
                .child(div().typo(Role::GroupTitle, tokens).child(title))
                .child(note(
                    if dialog.reconnect {
                        crate::i18n::t(PresetMsg::StaleAuth)
                    } else {
                        crate::i18n::t(PresetMsg::BrowserIntro)
                    },
                    tokens,
                )),
        );
        body = body.child(note(crate::i18n::t(PresetMsg::BalanceNote), tokens));

        match &dialog.phase {
            Phase::Waiting | Phase::Exchanging => {
                let text = if dialog.phase == Phase::Waiting {
                    crate::i18n::t(PresetMsg::WaitingAuth)
                } else {
                    crate::i18n::t(PresetMsg::ReceivingToken)
                };
                let detail = if dialog.phase == Phase::Waiting {
                    crate::i18n::t(PresetMsg::BrowserOpened)
                } else {
                    crate::i18n::t(PresetMsg::AlmostDone)
                };
                body = body.child(
                    v_flex()
                        .id("tokenway-progress")
                        .gap_1()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(tokens.border)
                        .bg(tokens.subtle)
                        .child(
                            div()
                                .typo(Role::FieldLabel, tokens)
                                .child(SharedString::from(text)),
                        )
                        .child(note(detail, tokens)),
                );
            }
            Phase::Idle | Phase::Failed(_) => {
                body = body.child(field(
                    crate::i18n::t(PresetMsg::DeviceName),
                    Some(crate::i18n::t(PresetMsg::DeviceNameHint).to_owned()),
                    Input::new(&dialog.device),
                    tokens,
                ));
            }
        }
        if let Phase::Failed(message) = &dialog.phase {
            body = body.child(div().id("tokenway-error").child(error_note(
                SharedString::from(message.clone()),
                tokens.danger,
            )));
        }
        if let Some(text) = &dialog.note {
            body = body.child(
                div()
                    .id("tokenway-note")
                    .child(note(SharedString::from(text.clone()), tokens)),
            );
        }

        // 按钮：进行中只有「取消」；否则「用 Tokenway 连接 / 重试」+「我已有 Key」+ 底部的注册链接和取消。
        let (connect, paste, cancel_flow, register, close) = (
            entity.clone(),
            entity.clone(),
            entity.clone(),
            entity.clone(),
            entity.clone(),
        );
        let actions = if running {
            h_flex().gap_2().justify_end().child(text_button(
                "tokenway-cancel",
                crate::i18n::t(PresetMsg::Cancel),
                ButtonLook::Outline,
                move |_, window, cx| {
                    cancel_flow.update(cx, |this, cx| this.tokenway_cancel(window, cx));
                },
            ))
        } else {
            let label = if matches!(dialog.phase, Phase::Failed(_)) {
                crate::i18n::t(PresetMsg::Retry)
            } else {
                crate::i18n::t(PresetMsg::ConnectWithTokenway)
            };
            // 注册链接单独一行放在左边（和按钮挤在一行宽度不够）。
            let register_row = h_flex().justify_start().child(text_button(
                "tokenway-register",
                crate::i18n::t(PresetMsg::NoAccount),
                ButtonLook::Ghost,
                move |_, window, cx| {
                    register.update(cx, |this, cx| this.tokenway_register(window, cx));
                },
            ));
            let buttons = h_flex()
                .gap_2()
                .justify_end()
                .child(text_button(
                    "tokenway-paste",
                    crate::i18n::t(PresetMsg::PasteKey),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        paste.update(cx, |this, cx| this.tokenway_paste_key(window, cx));
                    },
                ))
                .child(text_button(
                    "tokenway-close",
                    crate::i18n::t(PresetMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        close.update(cx, |this, cx| this.tokenway_close(window, cx));
                    },
                ))
                .child(text_button(
                    "tokenway-connect",
                    label,
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        connect.update(cx, |this, cx| this.tokenway_connect(window, cx));
                    },
                ));
            v_flex().gap_2().child(register_row).child(buttons)
        };
        body.child(actions).into_any_element()
    }
}

/// 设置列表里这一行是不是 Tokenway（两种：浏览器授权的令牌 / 自己粘贴的 Key）。
pub fn is_tokenway_row(preset_id: Option<&str>) -> bool {
    zssh_core::ai::agent::builtin::tokenway::endpoint::is_tokenway(preset_id)
}

/// 行上的余额小字（还没取到就没有）。
pub fn balance_text(endpoint_id: i64) -> Option<String> {
    tokenway::balance_view(endpoint_id).map(|view| view.label)
}

#[cfg(test)]
mod tests {
    use super::{Phase, Prefill, phase_name};
    use crate::agent_setup::presets::local_presets;
    use zssh_core::ai::agent::builtin::config::EndpointKind;

    #[test]
    fn prefill_follows_the_preset_and_custom_starts_blank() {
        let presets = local_presets();
        let ollama = presets.iter().find(|p| p.id == Some("ollama")).unwrap();
        let prefill = Prefill::from(ollama);
        assert_eq!(prefill.preset_id.as_deref(), Some("ollama"));
        assert_eq!(prefill.name, "Ollama");
        assert!(prefill.no_key);
        assert_eq!(prefill.kind, EndpointKind::OpenAiChat);
        let anthropic = presets.iter().find(|p| p.id == Some("anthropic")).unwrap();
        assert_eq!(Prefill::from(anthropic).kind, EndpointKind::Anthropic);
        assert!(!Prefill::from(anthropic).no_key);
        let custom = presets.iter().find(|p| p.id.is_none()).unwrap();
        let blank = Prefill::from(custom);
        assert!(blank.name.is_empty() && blank.base_url.is_empty() && blank.preset_id.is_none());
    }

    #[test]
    fn only_waiting_and_exchanging_count_as_running() {
        assert!(!Phase::Idle.running());
        assert!(Phase::Waiting.running());
        assert!(Phase::Exchanging.running());
        assert!(!Phase::Failed("x".into()).running());
        assert_eq!(phase_name(&Phase::Failed("x".into())), "failed");
    }
}
