//! 用户选择保存且认证成功的新密码，只能写回拨号前绑定的主机链。
//! 核心库在同一个写事务里复核整条链和原凭据；同步/编辑改变身份后整批跳过。

use zssh_core::ssh::{Connection, PromptKind};
use zssh_core::store::{
    Database,
    passwords::{PasswordReplacement, PasswordSnapshot},
};

/// 跳板机在前、目标在后。不要给明文结果增加日志或 Debug 输出。
pub(crate) fn collect(connection: &Connection) -> Vec<Option<String>> {
    connection
        .remembered_secrets()
        .into_iter()
        .map(|secret| {
            secret
                .filter(|secret| secret.kind == PromptKind::Password)
                .map(|secret| secret.secret.clone())
        })
        .collect()
}

/// 只消费握手前捕获的快照，不重新按当前跳板链的位置猜测密码属于谁。
/// 存储失败不影响已经认证成功的连接；日志不含秘密。
pub(crate) fn write_back(snapshot: PasswordSnapshot, secrets: Vec<Option<String>>) {
    if secrets.iter().all(Option::is_none) || secrets.len() != snapshot.hosts().len() {
        return;
    }
    let replacements: Vec<_> = snapshot
        .hosts()
        .zip(secrets)
        .map(|(host, secret)| {
            secret.map(|password| {
                PasswordReplacement::new(
                    password,
                    crate::main_window::host_form::default_password_name(&host.name),
                )
            })
        })
        .collect();
    let host_id = snapshot.target_id();
    let outcome = Database::open(&crate::settings::data_dir().join("zssh.db"))
        .and_then(|db| db.save_authenticated_passwords(&snapshot, &replacements));
    match outcome {
        Ok(count) => tracing::info!(host_id, count, "已按用户的选择更新保存的密码"),
        Err(err) => {
            tracing::warn!(host_id, error = %crate::error_text::error_text(&err), "更新保存的密码没有成功")
        }
    }
}
