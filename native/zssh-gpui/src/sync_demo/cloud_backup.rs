//! 同步设置页的「云端备份」一节（U33）：一个主同步 + 最多 5 个只上传的备份目标。
//!
//! 主同步照旧（双向合并）；备份目标只做单向加密上传，对象名固定为 `zeeterm-backup.json`，不往回合并。
//! 主同步成功之后（手动或自动）依次上传到每个启用的目标，一个目标失败只记在它自己的状态里；另外每个目标
//! 有「立即备份」（不需要先配置主同步）和「从这里恢复」。逻辑在核心的 `backup_targets`，这里只做界面、
//! 把网络和数据库操作放进后台线程，以及保存前的表单校验（复用主同步表单的 `build_*`）。
//!
//! 凭据：表单里的密码 / Secret / 令牌和主同步表单一样不回显、留空 = 沿用已保存的；目标列表整体用本机
//! DPAPI 加密存放，不会进同步包，也不会出现在云端备份里。

use std::path::Path;
use std::sync::Arc;

use ::gpui::{AnyElement, App, AppContext as _, Context, Entity, Window};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::InputState;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tokio::sync::Notify;
use zssh_core::sync::backup::BackupInfo;
use zssh_core::sync::backup_targets::{
    self, BackupTarget, MAX_TARGETS, OutcomeStatus, TargetOutcome, provider_label,
};
use zssh_core::sync::offline::OfflineImportReport;
use zssh_core::sync::onedrive::config::{OneDriveConfig, builtin_client_id, is_valid_client_id};
use zssh_core::sync::sealer::{DpapiSealer, SecretSealer};
use zssh_core::sync::service::SyncContext;

use super::view::{banner, colored, field, muted, password_field};
use super::{
    ExclusiveKind, GITHUB_TOKEN_EMPTY, GithubForm, ONEDRIVE_LOGIN_TIMEOUT, PASSWORD_EMPTY,
    PASSWORD_SAVED, ProviderChoice, S3_SECRET_EMPTY, S3Form, StorageConfig, SyncDemo, SyncEvent,
    SyncInFlight, WebDavForm, build_github, build_s3, build_webdav, exclusive_busy_message,
    form_from_view, github_form_from_view, load_master_password_at, onedrive_endpoints,
    onedrive_opener, open_db_at, open_existing_db_at, s3_form_from_view, validate_master_password,
};
use crate::app_theme::Tokens;
use crate::settings_view::card;
use crate::typography::{Role, Typo as _};

/// 目标数已满时给用户看的话。
const FULL_HINT: &str = "云端备份目标最多 5 个";

/// 在跑的后台任务；同时只允许一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TargetBusy {
    /// 某个目标的「立即备份」。
    Backup(String),
    /// 某个目标的「从这里恢复」。
    Restore(String),
}

/// 一次操作的结果提示（成功 / 失败各一个颜色）。
#[derive(Debug, Clone)]
pub(super) struct Notice {
    pub ok: bool,
    pub text: String,
}

/// 「云端备份」一节的状态。
pub(super) struct CloudBackup {
    pub targets: Vec<BackupTarget>,
    /// 目标列表读不出来（换了 Windows 账户、内容损坏）：不能当成“没有目标”。
    pub load_error: Option<String>,
    pub form: Option<TargetForm>,
    pub busy: Option<TargetBusy>,
    pub notice: Option<Notice>,
    /// 「从这里恢复」「删除」「清空」的二次确认：一次只会有一个处于待确认。
    pub restore_armed: Option<String>,
    pub delete_armed: Option<String>,
    pub reset_armed: bool,
}

impl CloudBackup {
    /// 打开页面时读一次；库文件不存在就是没有目标，不为了读它新建库。
    pub(super) fn load(data_dir: &Path) -> Self {
        let mut this = Self {
            targets: Vec::new(),
            load_error: None,
            form: None,
            busy: None,
            notice: None,
            restore_armed: None,
            delete_armed: None,
            reset_armed: false,
        };
        this.reload(data_dir);
        this
    }

    pub(super) fn reload(&mut self, data_dir: &Path) {
        match load_targets_at(data_dir) {
            Ok(targets) => {
                self.targets = targets;
                self.load_error = None;
            }
            Err(message) => {
                self.targets = Vec::new();
                self.load_error = Some(message);
            }
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    fn disarm(&mut self) {
        self.restore_armed = None;
        self.delete_armed = None;
        self.reset_armed = false;
    }

    fn describe(&self, id: &str) -> String {
        self.targets
            .iter()
            .find(|target| target.id == id)
            .map(|target| format!("{} {}", provider_label(&target.config), target.describe()))
            .unwrap_or_else(|| "这个备份目标".to_owned())
    }
}

/// 添加 / 编辑表单。输入框只在打开表单时建，关掉就丢（密码不留在内存里的输入框中）。
pub(super) struct TargetForm {
    pub id: String,
    /// 编辑已有的目标（`false` = 新增）。
    pub editing: bool,
    pub provider: ProviderChoice,
    /// 编辑时：已保存的配置（密码留空沿用它）。
    saved: Option<StorageConfig>,
    has_secret: bool,
    url: Entity<InputState>,
    directory: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    s3_endpoint: Entity<InputState>,
    s3_region: Entity<InputState>,
    s3_bucket: Entity<InputState>,
    s3_access_key: Entity<InputState>,
    s3_secret: Entity<InputState>,
    s3_prefix: Entity<InputState>,
    gh_api_base: Entity<InputState>,
    gh_token: Entity<InputState>,
    gh_gist_id: Entity<InputState>,
    od_client_id: Entity<InputState>,
    allow_http: bool,
    gh_allow_http: bool,
    s3_path_style: bool,
    /// OneDrive：登录过的账号显示名（空串 = 登录过但没拿到名字）；`None` = 还没登录。
    od_account: Option<String>,
    od_advanced: bool,
    signing_in: bool,
    cancel: Option<Arc<Notify>>,
    pub error: Option<String>,
}

impl TargetForm {
    /// 新增：`existing` 为 `None`；编辑：带上要改的目标。
    fn new(
        existing: Option<&BackupTarget>,
        window: &mut Window,
        cx: &mut Context<SyncDemo>,
    ) -> Self {
        let id = existing
            .map(|target| target.id.clone())
            .unwrap_or_else(backup_targets::new_target_id);
        let saved = existing.map(|target| target.config.clone());
        let view = saved.as_ref().map(StorageConfig::view);
        let provider = view
            .as_ref()
            .and_then(|view| ProviderChoice::from_provider(&view.provider))
            .unwrap_or_default();
        let webdav = view.as_ref().map(form_from_view).unwrap_or_default();
        let s3 = view.as_ref().map(s3_form_from_view).unwrap_or_default();
        let github = view.as_ref().map(github_form_from_view).unwrap_or_default();
        let has_secret = view.as_ref().is_some_and(|view| view.has_secret);
        let od_account = match &saved {
            Some(StorageConfig::Onedrive(config)) => Some(config.account.clone()),
            _ => None,
        };
        let od_client = match &saved {
            Some(StorageConfig::Onedrive(config)) => config.client_id.clone(),
            _ => String::new(),
        };
        let make = |window: &mut Window,
                    cx: &mut Context<SyncDemo>,
                    value: &str,
                    placeholder: &'static str,
                    masked: bool| {
            cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .default_value(value.to_owned())
                    .placeholder(placeholder);
                if masked {
                    state = state.masked(true);
                }
                state
            })
        };
        let secret_hint = |empty: &'static str, on: bool| if on { PASSWORD_SAVED } else { empty };
        let secret_of = |p: ProviderChoice| has_secret && provider == p;
        Self {
            id,
            editing: existing.is_some(),
            provider,
            saved,
            has_secret,
            url: make(
                window,
                cx,
                &webdav.url,
                "https://dav.example.com/remote.php/dav/files/<用户>/",
                false,
            ),
            directory: make(window, cx, &webdav.directory, "存放目录（可空）", false),
            username: make(window, cx, &webdav.username, "用户名", false),
            password: make(
                window,
                cx,
                "",
                secret_hint(PASSWORD_EMPTY, secret_of(ProviderChoice::WebDav)),
                true,
            ),
            s3_endpoint: make(
                window,
                cx,
                &s3.endpoint,
                "https://s3.us-east-1.amazonaws.com 或 https://<账号>.r2.cloudflarestorage.com",
                false,
            ),
            s3_region: make(
                window,
                cx,
                &s3.region,
                "Region（例如 us-east-1；R2 填 auto）",
                false,
            ),
            s3_bucket: make(window, cx, &s3.bucket, "Bucket 名称", false),
            s3_access_key: make(window, cx, &s3.access_key, "Access Key", false),
            s3_secret: make(
                window,
                cx,
                "",
                secret_hint(S3_SECRET_EMPTY, secret_of(ProviderChoice::S3)),
                true,
            ),
            s3_prefix: make(window, cx, &s3.prefix, "对象前缀（可空）", false),
            gh_api_base: make(
                window,
                cx,
                &github.api_base,
                "https://api.github.com（留空用默认）",
                false,
            ),
            gh_token: make(
                window,
                cx,
                "",
                secret_hint(GITHUB_TOKEN_EMPTY, secret_of(ProviderChoice::Github)),
                true,
            ),
            gh_gist_id: make(
                window,
                cx,
                &github.gist_id,
                "留空 = 第一次备份时自动新建",
                false,
            ),
            od_client_id: make(
                window,
                cx,
                &od_client,
                "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx（留空用内置的）",
                false,
            ),
            allow_http: webdav.allow_insecure_http,
            gh_allow_http: github.allow_insecure_http,
            s3_path_style: s3.path_style,
            od_account,
            od_advanced: false,
            signing_in: false,
            cancel: None,
            error: None,
        }
    }

    /// 调试驱动（`sync_set:t_*`）往表单里填内容用的输入框。只在 debug 构建里编进去。
    #[cfg(debug_assertions)]
    pub(super) fn dev_input(&self, field: &str) -> Result<&Entity<InputState>, String> {
        Ok(match field {
            "t_url" => &self.url,
            "t_dir" => &self.directory,
            "t_user" => &self.username,
            "t_pass" => &self.password,
            "t_s3_endpoint" => &self.s3_endpoint,
            "t_s3_region" => &self.s3_region,
            "t_s3_bucket" => &self.s3_bucket,
            "t_s3_access" => &self.s3_access_key,
            "t_s3_secret" => &self.s3_secret,
            "t_s3_prefix" => &self.s3_prefix,
            "t_gh_api" => &self.gh_api_base,
            "t_gh_token" => &self.gh_token,
            "t_gh_gist" => &self.gh_gist_id,
            "t_od_client" => &self.od_client_id,
            _ => return Err("不认识的表单字段".to_owned()),
        })
    }

    #[cfg(debug_assertions)]
    pub(super) fn is_signing_in(&self) -> bool {
        self.signing_in
    }

    fn webdav_form(&self, cx: &App) -> WebDavForm {
        WebDavForm {
            url: self.url.read(cx).value().to_string(),
            directory: self.directory.read(cx).value().to_string(),
            username: self.username.read(cx).value().to_string(),
            password: self.password.read(cx).value().to_string(),
            allow_insecure_http: self.allow_http,
        }
    }

    fn s3_form(&self, cx: &App) -> S3Form {
        S3Form {
            endpoint: self.s3_endpoint.read(cx).value().to_string(),
            region: self.s3_region.read(cx).value().to_string(),
            bucket: self.s3_bucket.read(cx).value().to_string(),
            access_key: self.s3_access_key.read(cx).value().to_string(),
            secret_key: self.s3_secret.read(cx).value().to_string(),
            prefix: self.s3_prefix.read(cx).value().to_string(),
            path_style: self.s3_path_style,
        }
    }

    fn github_form(&self, cx: &App) -> GithubForm {
        GithubForm {
            api_base: self.gh_api_base.read(cx).value().to_string(),
            token: self.gh_token.read(cx).value().to_string(),
            gist_id: self.gh_gist_id.read(cx).value().to_string(),
            allow_insecure_http: self.gh_allow_http,
        }
    }

    /// 表单 → 配置：校验规则和主同步表单是同一套（`build_*`）。
    fn to_config(&self, cx: &App) -> Result<StorageConfig, String> {
        let saved = self.saved.as_ref();
        match self.provider {
            ProviderChoice::WebDav => build_webdav(&self.webdav_form(cx), saved),
            ProviderChoice::S3 => build_s3(&self.s3_form(cx), saved),
            ProviderChoice::Github => build_github(&self.github_form(cx), saved),
            ProviderChoice::OneDrive => {
                let Some(account) = &self.od_account else {
                    return Err("请先登录 OneDrive".to_owned());
                };
                Ok(StorageConfig::Onedrive(OneDriveConfig {
                    client_id: self.od_client_id.read(cx).value().trim().to_owned(),
                    account: account.clone(),
                }))
            }
        }
    }
}

/// 备份 / 恢复用哪个主密码：输入框里有就用输入的，没有就用本机记住的；都没有就让用户输。
/// `needs_confirm`：单独用云端备份（没配置主同步）又没记住主密码时，输的要和“再输一遍”一致——
/// 输错一个字，备份就解不开了。
pub(super) fn resolve_password(
    typed: &str,
    confirm: &str,
    remembered: Option<String>,
    needs_confirm: bool,
) -> Result<String, String> {
    let password = if !typed.is_empty() {
        typed.to_owned()
    } else {
        remembered.ok_or_else(|| "请先输入主密码（用来加密 / 解开云端备份）".to_owned())?
    };
    validate_master_password(&password)?;
    if needs_confirm && !typed.is_empty() && confirm != typed {
        return Err("两次输入的主密码不一样".to_owned());
    }
    Ok(password)
}

// ---------------------------------------------------------------------------
// 后台线程里的操作（各自开库；不复用界面里的连接）

fn load_targets_at(data_dir: &Path) -> Result<Vec<BackupTarget>, String> {
    if !data_dir.join("zssh.db").is_file() {
        return Ok(Vec::new());
    }
    let db = open_existing_db_at(data_dir)?;
    backup_targets::load_targets(&db, &DpapiSealer).map_err(|err| err.to_string())
}

fn add_target_at(data_dir: &Path, id: String, config: StorageConfig) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    backup_targets::add_target(&db, &DpapiSealer, Some(id), config, true)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn update_target_at(data_dir: &Path, id: &str, config: StorageConfig) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    backup_targets::update_target(&db, &DpapiSealer, id, config)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn delete_target_at(data_dir: &Path, id: &str) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    backup_targets::delete_target(&db, &DpapiSealer, id).map_err(|err| err.to_string())
}

fn set_enabled_at(data_dir: &Path, id: &str, enabled: bool) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    backup_targets::set_enabled(&db, &DpapiSealer, id, enabled).map_err(|err| err.to_string())
}

fn reset_targets_at(data_dir: &Path) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    backup_targets::reset_targets(&db, &DpapiSealer).map_err(|err| err.to_string())
}

/// 丢弃一个还没保存的新目标留下的 OneDrive 令牌。
fn discard_token_at(data_dir: &Path, id: &str) {
    if let Ok(db) = open_existing_db_at(data_dir) {
        let _ = backup_targets::delete_target(&db, &DpapiSealer, id);
    }
}

fn sign_in_target_at(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    id: &str,
    client_id: &str,
    cancel: &Notify,
) -> Result<OneDriveConfig, String> {
    let db = open_db_at(data_dir)?;
    let endpoints = onedrive_endpoints();
    let browser = onedrive_opener();
    handle
        .block_on(backup_targets::onedrive_sign_in_for_target(
            &db,
            &DpapiSealer,
            &endpoints,
            id,
            client_id,
            ONEDRIVE_LOGIN_TIMEOUT,
            browser.as_ref(),
            cancel,
        ))
        .map_err(|err| err.to_string())
}

fn context_for<'a>(
    db: &'a zssh_core::store::Database,
    sealer: &'a dyn SecretSealer,
    data_dir: &Path,
) -> Result<SyncContext<'a>, String> {
    let device_id = db
        .sync_device_id()
        .map_err(|err| format!("读取同步设备 ID 失败：{err}"))?;
    Ok(SyncContext {
        db,
        sealer,
        device_id,
        data_dir: data_dir.to_path_buf(),
        known_hosts_path: Some(data_dir.join("known_hosts")),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        onedrive_endpoints: onedrive_endpoints(),
    })
}

/// 把本机数据加密上传到备份目标。`only` 为 `None` = 主同步成功后的自动备份（启用的目标、
/// 没变化的跳过）；`Some(id)` = 这个目标的「立即备份」。主密码只进参数，不写日志。
pub(super) fn backup_with(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    password: &str,
    only: Option<&str>,
) -> Result<Vec<TargetOutcome>, String> {
    let db = open_db_at(data_dir)?;
    let context = context_for(&db, sealer, data_dir)?;
    handle
        .block_on(context.backup_to_targets(password, only))
        .map_err(|err| err.to_string())
}

/// 主同步成功之后顺带做的自动备份。失败不影响主同步：只记日志，每个目标的结果已经写进它自己的状态。
pub(super) fn backup_after_sync(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    password: &str,
) {
    match backup_with(handle, data_dir, sealer, password, None) {
        Ok(outcomes) => {
            let failed = outcomes
                .iter()
                .filter(|outcome| matches!(outcome.status, OutcomeStatus::Failed(_)))
                .count();
            if failed > 0 {
                tracing::warn!(failed, total = outcomes.len(), "有云端备份目标没有备份成功");
            }
        }
        Err(message) => tracing::warn!(%message, "云端备份没有开始"),
    }
}

pub(super) fn restore_with(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    id: &str,
    password: &str,
) -> Result<OfflineImportReport, String> {
    let db = open_db_at(data_dir)?;
    let context = context_for(&db, sealer, data_dir)?;
    handle
        .block_on(context.restore_from_target(id, password))
        .map_err(|err| err.to_string())
}

/// 上次结果的一行字：时间 + 成功 / 失败原因。
pub(super) fn last_text(target: &BackupTarget) -> (bool, String) {
    match &target.last {
        None => (true, "还没有备份过".to_owned()),
        Some(last) if last.ok => (
            true,
            format!("上次备份 {} 成功", super::format_stamp(last.at)),
        ),
        Some(last) => (
            false,
            format!(
                "上次备份 {} 失败：{}",
                super::format_stamp(last.at),
                last.message
            ),
        ),
    }
}

/// 恢复完成后给用户看的摘要。
fn restore_text(label: &str, report: &OfflineImportReport) -> String {
    let applied = &report.applied;
    format!(
        "已从 {label} 恢复：主机 新增 {} / 更新 {} / 删除 {}，凭据 新增 {}，代理 新增 {}。\
         恢复前的状态已自动备份为 {}，可以在「备份与恢复」里恢复回去",
        applied.hosts.inserted,
        applied.hosts.updated,
        applied.hosts.deleted,
        applied.identities.inserted,
        applied.proxies.inserted,
        backup_name(&report.safety_backup),
    )
}

fn backup_name(info: &BackupInfo) -> &str {
    &info.name
}

// ---------------------------------------------------------------------------
// 界面动作

impl SyncDemo {
    fn cloud_notice(&mut self, ok: bool, text: impl Into<String>) {
        self.cloud.notice = Some(Notice {
            ok,
            text: text.into(),
        });
    }

    fn reload_targets(&mut self) {
        self.cloud.reload(&crate::settings::data_dir());
    }

    /// 后台把结果写进目标自己的状态之后重读列表（调用方负责 `notify`）。
    pub(super) fn reload_targets_now(&mut self) {
        self.reload_targets();
    }

    /// 备份页从备份 / 离线文件写回了同步账号设置（或用户确认替换）：把这一页的显示换成本机现在的。
    /// 做的事和打开页面时（`build`）读一遍设置相同；输入框里没保存的草稿被丢掉——设置整体换过了。
    pub(super) fn reload_after_accounts_change(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let data_dir = crate::settings::data_dir();
        let (saved, view, status) = super::load_saved();
        self.saved = saved;
        self.status = status;
        self.refresh_secret_ui(window, cx);
        let provider = view
            .as_ref()
            .and_then(|view| ProviderChoice::from_provider(&view.provider))
            .unwrap_or_default();
        let webdav = view.as_ref().map(form_from_view).unwrap_or_default();
        let s3 = view.as_ref().map(s3_form_from_view).unwrap_or_default();
        let github = view.as_ref().map(github_form_from_view).unwrap_or_default();
        let onedrive = view
            .as_ref()
            .map(super::onedrive_form_from_view)
            .unwrap_or_default();
        let set = |input: &Entity<InputState>,
                   value: &str,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        };
        set(&self.url, &webdav.url, window, cx);
        set(&self.directory, &webdav.directory, window, cx);
        set(&self.username, &webdav.username, window, cx);
        set(&self.s3_endpoint, &s3.endpoint, window, cx);
        set(&self.s3_region, &s3.region, window, cx);
        set(&self.s3_bucket, &s3.bucket, window, cx);
        set(&self.s3_access_key, &s3.access_key, window, cx);
        set(&self.s3_prefix, &s3.prefix, window, cx);
        set(&self.github_api_base, &github.api_base, window, cx);
        set(&self.github_gist_id, &github.gist_id, window, cx);
        set(&self.onedrive_client_id, &onedrive.client_id, window, cx);
        for secret in [&self.password, &self.s3_secret, &self.github_token] {
            set(secret, "", window, cx);
        }
        self.allow_insecure_http = webdav.allow_insecure_http;
        self.github_allow_insecure_http = github.allow_insecure_http;
        self.drafts = super::ProviderDrafts {
            provider,
            webdav,
            s3,
            github,
            onedrive,
        };
        // 记住的主密码、自动同步、已和云端对上的标记、并发保护检测结果。
        let remembered = load_master_password_at(&data_dir).ok().flatten();
        self.remember = super::RememberPassword::from_saved(remembered.as_deref());
        set(
            &self.master_password,
            remembered.as_deref().unwrap_or_default(),
            window,
            cx,
        );
        set(&self.master_password_confirm, "", window, cx);
        self.auto = super::load_auto_panel(&data_dir);
        let interval = self.auto.settings.interval_minutes.to_string();
        set(&self.auto_interval, &interval, window, cx);
        self.ready = super::load_ready_at(&data_dir);
        self.baseline_broken = super::baseline_broken_at(&data_dir);
        self.concurrency = super::load_concurrency_at(&data_dir);
        self.probe = None;
        self.clear_confirm.disarm();
        self.logout_confirm.disarm();
        self.close_target_form_inner(true);
        self.cloud.disarm();
        self.reload_targets();
        cx.notify();
    }

    /// 打开添加表单（`edit` 为要改的目标 id）。
    pub(super) fn open_target_form(
        &mut self,
        edit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cloud.is_busy() {
            return;
        }
        self.cloud.disarm();
        self.cloud.notice = None;
        let existing = edit.and_then(|id| self.cloud.targets.iter().find(|t| t.id == id).cloned());
        if existing.is_none() && self.cloud.targets.len() >= MAX_TARGETS {
            self.cloud_notice(false, FULL_HINT);
            cx.notify();
            return;
        }
        self.close_target_form_inner(false);
        self.cloud.form = Some(TargetForm::new(existing.as_ref(), window, cx));
        cx.notify();
    }

    /// 关掉表单。新增时已经登录过 OneDrive 但没保存，登录留下的令牌一起删掉。
    pub(super) fn close_target_form(&mut self, cx: &mut Context<Self>) {
        if self.cloud.form.as_ref().is_some_and(|form| form.signing_in) {
            return;
        }
        self.close_target_form_inner(true);
        cx.notify();
    }

    fn close_target_form_inner(&mut self, discard: bool) {
        if let Some(form) = self.cloud.form.take()
            && discard
            && !form.editing
            && form.od_account.is_some()
        {
            discard_token_at(&crate::settings::data_dir(), &form.id);
        }
    }

    pub(super) fn set_target_provider(&mut self, provider: ProviderChoice, cx: &mut Context<Self>) {
        if let Some(form) = self.cloud.form.as_mut()
            && !form.signing_in
        {
            form.provider = provider;
            form.error = None;
            cx.notify();
        }
    }

    /// 保存表单：新增或改已有的。校验失败把原因显示在表单里。
    pub(super) fn save_target_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.cloud.form.as_ref() else {
            return;
        };
        if form.signing_in {
            return;
        }
        let config = match form.to_config(cx) {
            Ok(config) => config,
            Err(message) => {
                if let Some(form) = self.cloud.form.as_mut() {
                    form.error = Some(message);
                }
                cx.notify();
                return;
            }
        };
        let (id, editing) = (form.id.clone(), form.editing);
        let data_dir = crate::settings::data_dir();
        let result = if editing {
            update_target_at(&data_dir, &id, config)
        } else {
            add_target_at(&data_dir, id, config)
        };
        match result {
            Ok(()) => {
                // 保存成功：令牌属于这个目标了，关表单时不能再删。
                self.close_target_form_inner(false);
                self.reload_targets();
                self.cloud_notice(
                    true,
                    if editing {
                        "已保存。上次备份结果已清空，下一次备份会重新上传"
                    } else {
                        "已添加。主同步下一次成功之后会自动备份到这里，也可以点「立即备份」"
                    },
                );
            }
            Err(message) => {
                if let Some(form) = self.cloud.form.as_mut() {
                    form.error = Some(message);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn toggle_target(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.cloud.is_busy() {
            return;
        }
        let Some(enabled) = self
            .cloud
            .targets
            .iter()
            .find(|target| target.id == id)
            .map(|target| target.enabled)
        else {
            return;
        };
        if let Err(message) = set_enabled_at(&crate::settings::data_dir(), id, !enabled) {
            self.cloud_notice(false, message);
        }
        self.reload_targets();
        cx.notify();
    }

    /// 删除：第一次点只进入确认状态；云端已经上传的文件不动。
    pub(super) fn delete_target(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.cloud.is_busy() {
            return;
        }
        if self.cloud.delete_armed.as_deref() != Some(id) {
            self.cloud.disarm();
            self.cloud.delete_armed = Some(id.to_owned());
            self.cloud_notice(
                false,
                "再点一次「确认删除」会删掉本机保存的这个备份目标和它的登录信息（云端已经上传的备份文件不动）",
            );
            cx.notify();
            return;
        }
        self.cloud.disarm();
        match delete_target_at(&crate::settings::data_dir(), id) {
            Ok(()) => self.cloud_notice(true, "已删除这个备份目标（云端已经上传的备份文件没有动）"),
            Err(message) => self.cloud_notice(false, message),
        }
        self.reload_targets();
        cx.notify();
    }

    /// 目标列表读不出来时的出路：清空（两步确认）。
    pub(super) fn reset_targets(&mut self, cx: &mut Context<Self>) {
        if self.cloud.is_busy() {
            return;
        }
        if !self.cloud.reset_armed {
            self.cloud.disarm();
            self.cloud.reset_armed = true;
            cx.notify();
            return;
        }
        self.cloud.disarm();
        match reset_targets_at(&crate::settings::data_dir()) {
            Ok(()) => {
                self.cloud_notice(true, "已清空本机保存的云端备份设置（云端的备份文件没有动）")
            }
            Err(message) => self.cloud_notice(false, message),
        }
        self.reload_targets();
        cx.notify();
    }

    /// 备份 / 恢复用的主密码：输入框里有就用，没有就用本机记住的；单独用云端备份（没配置主同步）
    /// 又没有记住的主密码时，要输两遍（输错一个字，备份就解不开了）。
    fn target_password(&self, cx: &App) -> Result<String, String> {
        let typed = self.master_password.read(cx).value().to_string();
        let confirm = self.master_password_confirm.read(cx).value().to_string();
        let remembered = if typed.is_empty() {
            load_master_password_at(&crate::settings::data_dir())
                .ok()
                .flatten()
        } else {
            None
        };
        // 单独用云端备份（没配置主同步）又没有记住主密码：输的要再输一遍。
        let standalone = self.saved.is_none() && !self.remember.saved;
        resolve_password(&typed, &confirm, remembered, standalone)
    }

    /// 某个目标的「立即备份」。
    pub(super) fn start_target_backup(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.cloud.is_busy() || self.testing || self.syncing || self.signing_in {
            return;
        }
        self.cloud.disarm();
        let password = match self.target_password(cx) {
            Ok(password) => password,
            Err(message) => {
                self.cloud_notice(false, message);
                cx.notify();
                return;
            }
        };
        let guard = match SyncInFlight::acquire(ExclusiveKind::Sync) {
            Ok(guard) => guard,
            Err(holder) => {
                self.cloud_notice(
                    false,
                    exclusive_busy_message(ExclusiveKind::Sync, holder).to_owned(),
                );
                cx.notify();
                return;
            }
        };
        let (data_dir, sender, id) = (
            crate::settings::data_dir(),
            self.sender.clone(),
            id.to_owned(),
        );
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.cloud.busy = Some(TargetBusy::Backup(id.clone()));
                self.cloud.notice = None;
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    // 守卫活到这次备份结束：期间同步和恢复都进不来。
                    let _guard = guard;
                    let result =
                        backup_with(&handle, &data_dir, &DpapiSealer, &password, Some(&id))
                            .and_then(|outcomes| {
                                outcomes
                                    .into_iter()
                                    .next()
                                    .map(|outcome| outcome.status)
                                    .ok_or_else(|| "没有这个备份目标".to_owned())
                            });
                    let _ = sender.send(SyncEvent::TargetBackedUp { id, result });
                }));
            }
            Err(err) => self.cloud_notice(false, format!("启动备份任务失败：{err}")),
        }
        cx.notify();
    }

    /// 「从这里恢复」：第一次点只进入确认状态（确认框说明会覆盖本机数据）。
    pub(super) fn arm_target_restore(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.cloud.is_busy() {
            return;
        }
        self.cloud.disarm();
        self.cloud.notice = None;
        self.cloud.restore_armed = Some(id.to_owned());
        cx.notify();
    }

    pub(super) fn cancel_target_restore(&mut self, cx: &mut Context<Self>) {
        self.cloud.disarm();
        cx.notify();
    }

    /// 「确认恢复」：下载、用主密码解开、先做本地备份再导入（替换）。和同步 / 导入互斥。
    pub(super) fn confirm_target_restore(&mut self, cx: &mut Context<Self>) {
        if self.cloud.is_busy() || self.testing || self.syncing || self.signing_in {
            return;
        }
        let Some(id) = self.cloud.restore_armed.take() else {
            return;
        };
        self.cloud.disarm();
        let password = match self.target_password(cx) {
            Ok(password) => password,
            Err(message) => {
                self.cloud_notice(false, message);
                cx.notify();
                return;
            }
        };
        let guard = match SyncInFlight::acquire(ExclusiveKind::Restore) {
            Ok(guard) => guard,
            Err(holder) => {
                self.cloud_notice(
                    false,
                    exclusive_busy_message(ExclusiveKind::Restore, holder).to_owned(),
                );
                cx.notify();
                return;
            }
        };
        let (data_dir, sender) = (crate::settings::data_dir(), self.sender.clone());
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.cloud.busy = Some(TargetBusy::Restore(id.clone()));
                self.cloud.notice = None;
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    let _guard = guard;
                    let result = restore_with(&handle, &data_dir, &DpapiSealer, &id, &password)
                        .map(Box::new);
                    let _ = sender.send(SyncEvent::TargetRestored { id, result });
                }));
            }
            Err(err) => self.cloud_notice(false, format!("启动恢复任务失败：{err}")),
        }
        cx.notify();
    }

    /// 表单里的 OneDrive 登录：令牌存在这个目标自己的键下，不碰主同步的登录。
    pub(super) fn start_target_login(&mut self, cx: &mut Context<Self>) {
        if self.cloud.is_busy() || self.signing_in {
            return;
        }
        let Some(form) = self.cloud.form.as_mut() else {
            return;
        };
        if form.signing_in {
            return;
        }
        let client_id = form.od_client_id.read(cx).value().to_string();
        let (data_dir, sender, id) = (
            crate::settings::data_dir(),
            self.sender.clone(),
            form.id.clone(),
        );
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                let cancel = Arc::new(Notify::new());
                form.cancel = Some(cancel.clone());
                form.signing_in = true;
                form.error = None;
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    let result = sign_in_target_at(&handle, &data_dir, &id, &client_id, &cancel);
                    let _ = sender.send(SyncEvent::TargetSignedIn(result));
                }));
            }
            Err(err) => form.error = Some(format!("启动登录任务失败：{err}")),
        }
        cx.notify();
    }

    pub(super) fn cancel_target_login(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self
            .cloud
            .form
            .as_ref()
            .and_then(|form| form.cancel.clone())
        {
            cancel.notify_one();
            cx.notify();
        }
    }

    pub(super) fn toggle_target_http(&mut self, github: bool, cx: &mut Context<Self>) {
        if let Some(form) = self.cloud.form.as_mut() {
            if github {
                form.gh_allow_http = !form.gh_allow_http;
            } else {
                form.allow_http = !form.allow_http;
            }
            cx.notify();
        }
    }

    pub(super) fn toggle_target_path_style(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = self.cloud.form.as_mut() {
            form.s3_path_style = !form.s3_path_style;
            cx.notify();
        }
    }

    pub(super) fn toggle_target_advanced(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = self.cloud.form.as_mut() {
            form.od_advanced = !form.od_advanced;
            cx.notify();
        }
    }

    // ---- 后台结果 ----

    pub(super) fn on_target_backed_up(
        &mut self,
        id: &str,
        result: Result<OutcomeStatus, String>,
        cx: &mut Context<Self>,
    ) {
        self.cloud.busy = None;
        let label = self.cloud.describe(id);
        match result {
            Ok(OutcomeStatus::Uploaded) => self.cloud_notice(true, format!("已备份到 {label}")),
            Ok(OutcomeStatus::Unchanged) => {
                self.cloud_notice(true, format!("{label} 上的备份已经是最新的"))
            }
            Ok(OutcomeStatus::Failed(message)) | Err(message) => {
                self.cloud_notice(false, format!("备份到 {label} 失败：{message}"))
            }
        }
        self.reload_targets();
        cx.notify();
    }

    pub(super) fn on_target_restored(
        &mut self,
        id: &str,
        result: Result<Box<OfflineImportReport>, String>,
        cx: &mut Context<Self>,
    ) {
        self.cloud.busy = None;
        let label = self.cloud.describe(id);
        match result {
            Ok(report) => self.cloud_notice(true, restore_text(&label, &report)),
            Err(message) => self.cloud_notice(false, format!("从 {label} 恢复失败：{message}")),
        }
        self.reload_targets();
        cx.notify();
    }

    pub(super) fn on_target_signed_in(
        &mut self,
        result: Result<OneDriveConfig, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = self.cloud.form.as_mut() {
            form.signing_in = false;
            form.cancel = None;
            match result {
                Ok(config) => {
                    form.od_account = Some(config.account.trim().to_owned());
                    form.error = None;
                }
                Err(message) => form.error = Some(message),
            }
        }
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// 画法

impl SyncDemo {
    /// 「云端备份」卡片。
    #[inline(never)]
    pub(super) fn cloud_backup_card(
        &self,
        entity: &Entity<Self>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(error) = &self.cloud.load_error {
            rows.push(self.cloud_load_error(entity, error, tokens));
        }
        // 没配置主同步时，这里是唯一用到主密码的地方：就在这里输（配置了主同步，主密码在上面那张卡里）。
        if self.saved.is_none() && self.cloud.load_error.is_none() {
            rows.push(self.cloud_password_rows(tokens));
        }
        for target in &self.cloud.targets {
            rows.push(self.target_row(target, entity, tokens));
        }
        if self.cloud.targets.is_empty()
            && self.cloud.load_error.is_none()
            && self.cloud.form.is_none()
        {
            rows.push(
                div()
                    .id("cloud-empty")
                    .child(muted("还没有云端备份目标。", tokens))
                    .into_any_element(),
            );
        }
        if let Some(form) = &self.cloud.form {
            rows.push(self.target_form_panel(form, entity, tokens, cx));
        } else if self.cloud.load_error.is_none() {
            rows.push(self.cloud_add_row(entity, tokens));
        }
        if let Some(notice) = &self.cloud.notice {
            rows.push(
                div()
                    .id("cloud-notice")
                    .child(colored(
                        notice.text.clone(),
                        if notice.ok {
                            tokens.dot_connected
                        } else {
                            tokens.danger
                        },
                    ))
                    .into_any_element(),
            );
        }
        card(
            "sync-cloud-backup",
            "云端备份",
            Some(
                "主同步之外，可以再加几个只上传的备份：主同步每次成功之后，ZeeTerm 把同样的数据用主密码加密，\
                 覆盖上传到这里（只留一份，从不往回合并）。备份文件叫 zeeterm-backup.json，和同步包放在同一个\
                 目录 / 桶里也不会互相覆盖。这里的账号信息只保存在本机，不会进同步包，也不会出现在云端备份里。",
            ),
            tokens,
            rows,
        )
    }

    fn cloud_load_error(&self, entity: &Entity<Self>, error: &str, tokens: &Tokens) -> AnyElement {
        let reset = entity.clone();
        banner(
            tokens.danger,
            vec![
                colored(
                    format!("读不出本机保存的云端备份设置：{error}"),
                    tokens.danger,
                ),
                muted(
                    "可能是换了 Windows 账户，或者设置被破坏了。清空之后可以重新添加备份目标（云端已有的备份文件不会被删）。",
                    tokens,
                ),
                Button::new("cloud-reset")
                    .label(if self.cloud.reset_armed {
                        "再次点击「确认清空」"
                    } else {
                        "清空云端备份设置"
                    })
                    .compact()
                    .on_click(move |_, _, cx| {
                        reset.update(cx, |this, cx| this.reset_targets(cx));
                    })
                    .into_any_element(),
            ],
        )
    }

    fn cloud_password_rows(&self, tokens: &Tokens) -> AnyElement {
        let mut rows = vec![
            password_field("主密码", &self.master_password, tokens),
            password_field("再输入一次", &self.master_password_confirm, tokens),
            muted(
                "云端备份用主密码加密，至少 8 个字符；忘了就打不开，也没有找回的办法。这里不会保存它，每次备份 / \
                 恢复都要输入。配置了主同步之后，它在上面「主密码」那张卡里。",
                tokens,
            ),
        ];
        if !self.remember.saved {
            rows.push(muted(
                "没有配置主同步，所以主同步成功后的自动备份不会触发；点每个目标上的「立即备份」手动备份。",
                tokens,
            ));
        }
        v_flex()
            .id("cloud-password")
            .w_full()
            .gap_2()
            .children(rows)
            .into_any_element()
    }

    fn cloud_add_row(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let full = self.cloud.targets.len() >= MAX_TARGETS;
        let add = entity.clone();
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(
                Button::new("cloud-add")
                    .label("添加备份目标")
                    .compact()
                    .disabled(full || self.cloud.is_busy())
                    .on_click(move |_, window, cx| {
                        add.update(cx, |this, cx| this.open_target_form(None, window, cx));
                    }),
            )
            .child(muted(
                if full {
                    FULL_HINT.to_owned()
                } else {
                    format!("已添加 {} / {MAX_TARGETS} 个", self.cloud.targets.len())
                },
                tokens,
            ))
            .into_any_element()
    }

    #[inline(never)]
    fn target_row(
        &self,
        target: &BackupTarget,
        entity: &Entity<Self>,
        tokens: &Tokens,
    ) -> AnyElement {
        let id = target.id.clone();
        let busy = self.cloud.is_busy() || self.busy();
        let backing_up = self.cloud.busy == Some(TargetBusy::Backup(id.clone()));
        let (ok, last) = last_text(target);
        let toggle = entity.clone();
        let toggle_id = id.clone();
        let head = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .px_1p5()
                            .rounded_sm()
                            .bg(tokens.subtle)
                            .text_size(::gpui::px(11.0))
                            .text_color(tokens.text_muted)
                            .child(provider_label(&target.config)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(target.describe()),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(muted("启用", tokens))
                    .child(
                        Switch::new(("cloud-enabled", target_index(&self.cloud, &id)))
                            .checked(target.enabled)
                            .disabled(busy)
                            .accessibility_label("启用这个备份目标")
                            .on_click(move |_, _, cx| {
                                let id = toggle_id.clone();
                                toggle.update(cx, |this, cx| this.toggle_target(&id, cx));
                            }),
                    ),
            );
        let status_color = if ok { tokens.text_muted } else { tokens.danger };
        let mut lines: Vec<AnyElement> = vec![head.into_any_element()];
        lines.push(colored(last, status_color));
        if !target.enabled {
            lines.push(muted("已停用：主同步成功后不会自动备份到这里", tokens));
        }
        lines.push(self.target_buttons(&id, backing_up, busy, entity));
        if self.cloud.restore_armed.as_deref() == Some(id.as_str()) {
            lines.push(self.restore_warning(&id, entity, tokens, busy));
        }
        v_flex()
            .id(("cloud-target", target_index(&self.cloud, &id)))
            .w_full()
            .gap_1()
            .children(lines)
            .into_any_element()
    }

    fn target_buttons(
        &self,
        id: &str,
        backing_up: bool,
        busy: bool,
        entity: &Entity<Self>,
    ) -> AnyElement {
        let index = target_index(&self.cloud, id);
        let (backup_id, restore_id, edit_id, delete_id) =
            (id.to_owned(), id.to_owned(), id.to_owned(), id.to_owned());
        let delete_label = if self.cloud.delete_armed.as_deref() == Some(id) {
            "确认删除"
        } else {
            "删除"
        };
        h_flex()
            .gap_2()
            .items_center()
            .child(row_button(
                entity,
                "cloud-backup",
                index,
                if backing_up {
                    "备份中…"
                } else {
                    "立即备份"
                },
                busy,
                move |this, _, cx| this.start_target_backup(&backup_id, cx),
            ))
            .child(row_button(
                entity,
                "cloud-restore",
                index,
                "从这里恢复",
                busy,
                move |this, _, cx| this.arm_target_restore(&restore_id, cx),
            ))
            .child(row_button(
                entity,
                "cloud-edit",
                index,
                "编辑",
                busy,
                move |this, window, cx| this.open_target_form(Some(edit_id.clone()), window, cx),
            ))
            .child(row_button(
                entity,
                "cloud-delete",
                index,
                delete_label,
                busy,
                move |this, _, cx| this.delete_target(&delete_id, cx),
            ))
            .into_any_element()
    }

    /// 「从这里恢复」的确认框：写清会覆盖本机数据。
    fn restore_warning(
        &self,
        id: &str,
        entity: &Entity<Self>,
        tokens: &Tokens,
        busy: bool,
    ) -> AnyElement {
        let label = self.cloud.describe(id);
        let (confirm, cancel) = (entity.clone(), entity.clone());
        div()
            .id("cloud-restore-warning")
            .child(banner(
                tokens.dot_connecting,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.caution)
                        .child(format!("从 {label} 恢复，会覆盖本机的数据"))
                        .into_any_element(),
                    div()
                        .child(
                            "本机的主机、分组、凭据、代理和可同步的设置会变成和这份云端备份一样，备份里没有的会被删掉。\
                             恢复前会先自动做一份本地备份，可以在「备份与恢复」里恢复回去；不会改同步基线，\
                             也不会动这里的备份目标和主同步设置。",
                        )
                        .into_any_element(),
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("cloud-restore-confirm")
                                .label("确认恢复")
                                .compact()
                                .danger()
                                .disabled(busy)
                                .on_click(move |_, _, cx| {
                                    confirm.update(cx, |this, cx| this.confirm_target_restore(cx));
                                }),
                        )
                        .child(
                            Button::new("cloud-restore-cancel")
                                .label("取消")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cancel.update(cx, |this, cx| this.cancel_target_restore(cx));
                                }),
                        )
                        .into_any_element(),
                ],
            ))
            .into_any_element()
    }

    // ---- 添加 / 编辑表单 ----

    #[inline(never)]
    fn target_form_panel(
        &self,
        form: &TargetForm,
        entity: &Entity<Self>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        const CHOICES: [ProviderChoice; 4] = [
            ProviderChoice::WebDav,
            ProviderChoice::S3,
            ProviderChoice::Github,
            ProviderChoice::OneDrive,
        ];
        let current = form.provider;
        let pick = entity.clone();
        let mut rows: Vec<AnyElement> = vec![
            div()
                .typo(Role::ItemName, tokens)
                .child(if form.editing {
                    "编辑备份目标"
                } else {
                    "添加备份目标"
                })
                .into_any_element(),
            v_flex()
                .w_full()
                .gap_1()
                .child(div().typo(Role::FieldLabel, tokens).child("存储类型"))
                .child(crate::main_window::widgets::segmented_bar(
                    "cloud-provider",
                    CHOICES
                        .iter()
                        .map(|choice| (choice.label().to_owned(), *choice == current, None))
                        .collect(),
                    tokens,
                    move |index, _, cx| {
                        if let Some(choice) = CHOICES.get(index).copied() {
                            pick.update(cx, |this, cx| this.set_target_provider(choice, cx));
                        }
                    },
                ))
                .into_any_element(),
        ];
        rows.push(match current {
            ProviderChoice::WebDav => self.form_webdav(form, entity, tokens, cx),
            ProviderChoice::S3 => self.form_s3(form, entity, tokens),
            ProviderChoice::Github => self.form_github(form, entity, tokens, cx),
            ProviderChoice::OneDrive => self.form_onedrive(form, entity, tokens, cx),
        });
        if let Some(error) = &form.error {
            rows.push(
                div()
                    .id("cloud-form-error")
                    .child(colored(error.clone(), tokens.danger))
                    .into_any_element(),
            );
        }
        let (save, close) = (entity.clone(), entity.clone());
        rows.push(
            h_flex()
                .gap_2()
                .child(
                    Button::new("cloud-form-save")
                        .label("保存")
                        .compact()
                        .disabled(form.signing_in)
                        .on_click(move |_, _, cx| {
                            save.update(cx, |this, cx| this.save_target_form(cx));
                        }),
                )
                .child(
                    Button::new("cloud-form-cancel")
                        .label("取消")
                        .compact()
                        .disabled(form.signing_in)
                        .on_click(move |_, _, cx| {
                            close.update(cx, |this, cx| this.close_target_form(cx));
                        }),
                )
                .into_any_element(),
        );
        v_flex()
            .id("cloud-form")
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .children(rows)
            .into_any_element()
    }

    fn form_webdav(
        &self,
        form: &TargetForm,
        entity: &Entity<Self>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        let mut rows = vec![
            field("WebDAV 地址", &form.url, tokens),
            field("用户名", &form.username, tokens),
            password_field("密码", &form.password, tokens),
            field("存放目录（可选）", &form.directory, tokens),
            muted(
                "备份文件 zeeterm-backup.json 放在这个目录里，和同步包 zssh-sync.json 并排也不会互相覆盖；目录不存在会自动创建。",
                tokens,
            ),
        ];
        if form.has_secret && form.provider == ProviderChoice::WebDav {
            rows.push(muted("已保存密码；留空 = 继续用已保存的密码", tokens));
        }
        if super::view::is_plain_http(form.url.read(cx).value().as_ref()) {
            let allow = entity.clone();
            rows.push(banner(
                tokens.dot_connecting,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.caution)
                        .child("地址是 http://，密码会以明文发送")
                        .into_any_element(),
                    Checkbox::new("cloud-allow-http")
                        .checked(form.allow_http)
                        .label("我了解密码会以明文发送，仍然使用 http://")
                        .on_click(move |_, _, cx| {
                            allow.update(cx, |this, cx| this.toggle_target_http(false, cx));
                        })
                        .into_any_element(),
                ],
            ));
        }
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    fn form_s3(&self, form: &TargetForm, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let style = entity.clone();
        let mut rows = vec![
            field("Endpoint", &form.s3_endpoint, tokens),
            field("Region", &form.s3_region, tokens),
            field("Bucket", &form.s3_bucket, tokens),
            field("Access Key", &form.s3_access_key, tokens),
            password_field("Secret", &form.s3_secret, tokens),
            field("对象路径前缀（可选）", &form.s3_prefix, tokens),
            v_flex()
                .w_full()
                .gap_1()
                .child(div().typo(Role::FieldLabel, tokens).child("地址风格"))
                .child(
                    Button::new("cloud-s3-path-style")
                        .label(if form.s3_path_style {
                            "路径风格（endpoint/bucket/key）"
                        } else {
                            "虚拟主机风格（bucket.endpoint/key）"
                        })
                        .compact()
                        .on_click(move |_, _, cx| {
                            style.update(cx, |this, cx| this.toggle_target_path_style(cx));
                        }),
                )
                .into_any_element(),
        ];
        if form.has_secret && form.provider == ProviderChoice::S3 {
            rows.push(muted("已保存 Secret；留空 = 继续用已保存的 Secret", tokens));
        }
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    fn form_github(
        &self,
        form: &TargetForm,
        entity: &Entity<Self>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        let mut rows = vec![
            field("接口地址", &form.gh_api_base, tokens),
            password_field("个人访问令牌", &form.gh_token, tokens),
            field("Gist ID", &form.gh_gist_id, tokens),
            muted(
                "需要 gist 权限。Gist ID 留空 = 第一次备份时自动新建一个 secret Gist；也可以填主同步用的同一个 Gist，\
                 备份文件 zeeterm-backup.json 和同步包是两个文件，互不影响。secret Gist 拿到链接的人都能打开，但里面是密文。",
                tokens,
            ),
        ];
        if form.has_secret && form.provider == ProviderChoice::Github {
            rows.push(muted("已保存令牌；留空 = 继续用已保存的令牌", tokens));
        }
        if super::view::is_plain_http(form.gh_api_base.read(cx).value().as_ref()) {
            let allow = entity.clone();
            rows.push(banner(
                tokens.dot_connecting,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.caution)
                        .child("接口地址是 http://，令牌会以明文发送")
                        .into_any_element(),
                    Checkbox::new("cloud-gh-allow-http")
                        .checked(form.gh_allow_http)
                        .label("我了解令牌会以明文发送，仍然使用 http://")
                        .on_click(move |_, _, cx| {
                            allow.update(cx, |this, cx| this.toggle_target_http(true, cx));
                        })
                        .into_any_element(),
                ],
            ));
        }
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    fn form_onedrive(
        &self,
        form: &TargetForm,
        entity: &Entity<Self>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        let custom = form.od_client_id.read(cx).value().to_string();
        let allowed = super::view::can_login(builtin_client_id().is_some(), &custom);
        let custom_invalid = !custom.trim().is_empty() && !is_valid_client_id(&custom);
        let account = match &form.od_account {
            Some(account) if !account.trim().is_empty() => account.clone(),
            Some(_) => "已登录".to_owned(),
            None => "未登录".to_owned(),
        };
        let mut rows: Vec<AnyElement> = vec![
            muted(
                "用微软账号登录，备份放在 OneDrive 的应用专用文件夹里。可以和主同步用同一个账号，也可以用另一个；\
                 这里的登录信息和主同步的互相独立，断开其中一个不影响另一个。目前只支持个人微软账号。",
                tokens,
            ),
            h_flex()
                .gap_2()
                .child(muted("账号", tokens))
                .child(div().text_sm().child(account))
                .into_any_element(),
        ];
        if form.signing_in {
            let cancel = entity.clone();
            rows.push(banner(
                tokens.text_muted,
                vec![
                    div()
                        .id("cloud-od-logging-in")
                        .font_weight(FontWeight::MEDIUM)
                        .child("正在浏览器中登录…")
                        .into_any_element(),
                    div()
                        .child("请在刚刚打开的浏览器页面里登录并同意授权，最多等 5 分钟。")
                        .into_any_element(),
                    Button::new("cloud-od-login-cancel")
                        .label("取消登录")
                        .compact()
                        .on_click(move |_, _, cx| {
                            cancel.update(cx, |this, cx| this.cancel_target_login(cx));
                        })
                        .into_any_element(),
                ],
            ));
        } else {
            let login = entity.clone();
            rows.push(
                h_flex()
                    .child(
                        Button::new("cloud-od-login")
                            .label(if form.od_account.is_some() {
                                "重新登录"
                            } else {
                                "用微软账号登录"
                            })
                            .compact()
                            .disabled(!allowed)
                            .on_click(move |_, _, cx| {
                                login.update(cx, |this, cx| this.start_target_login(cx));
                            }),
                    )
                    .into_any_element(),
            );
            if !allowed && !custom_invalid {
                rows.push(colored(
                    "这个版本没有内置 OneDrive 的 client ID，需要在下面的「高级」里填你自己的。",
                    tokens.dot_connecting,
                ));
            }
            if custom_invalid {
                rows.push(colored(
                    "client ID 的格式不对，应该形如 xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx。",
                    tokens.dot_connecting,
                ));
            }
        }
        let toggle = entity.clone();
        rows.push(
            v_flex()
                .gap_1()
                .child(
                    h_flex().child(
                        Button::new("cloud-od-advanced")
                            .label(if form.od_advanced {
                                "高级 ▾"
                            } else {
                                "高级 ▸"
                            })
                            .ghost()
                            .compact()
                            .on_click(move |_, _, cx| {
                                toggle.update(cx, |this, cx| this.toggle_target_advanced(cx));
                            }),
                    ),
                )
                .when(form.od_advanced, |col| {
                    col.child(field("自己的 client ID", &form.od_client_id, tokens))
                })
                .into_any_element(),
        );
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }
}

/// 目标在列表里的位置（做元素 ID 用）。
fn target_index(cloud: &CloudBackup, id: &str) -> usize {
    cloud
        .targets
        .iter()
        .position(|target| target.id == id)
        .unwrap_or_default()
}

/// 目标行里的一个小按钮：点一下就在 `SyncDemo` 上调一个方法。
fn row_button(
    entity: &Entity<SyncDemo>,
    key: &'static str,
    index: usize,
    label: &'static str,
    disabled: bool,
    run: impl Fn(&mut SyncDemo, &mut Window, &mut Context<SyncDemo>) + 'static,
) -> Button {
    let entity = entity.clone();
    Button::new((key, index))
        .label(label)
        .compact()
        .disabled(disabled)
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| run(this, window, cx));
        })
}
