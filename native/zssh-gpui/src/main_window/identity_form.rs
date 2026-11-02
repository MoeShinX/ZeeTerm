//! 凭据面板的纯逻辑（0.2.14 `authIdentity.ts` + `KeychainPanel.tsx` 里的规则）：保存时秘密怎么
//! 传、私钥口令怎么传、改类型会不会清掉已存的秘密；以及 V2-F3 的两条：内联私钥不被一行字替换、
//! 删除前列出还在用的主机。不依赖 GPUI。

use zssh_core::model::{Identity, IdentityDraft, IdentityKind};

/// 类型下拉里的四项，顺序同 0.2.14。
pub const KINDS: [(IdentityKind, &str); 4] = [
    (IdentityKind::Password, "密码"),
    (IdentityKind::KeyFile, "私钥文件"),
    (IdentityKind::KeyInline, "导入的私钥"),
    (IdentityKind::Agent, "ssh-agent"),
];

pub fn kind_label(kind: IdentityKind) -> &'static str {
    KINDS
        .iter()
        .find(|(item, _)| *item == kind)
        .map_or("", |(_, label)| *label)
}

/// 保存凭据时要传给存储层的 `secret`：`None` 表示不改，`Some("")` 表示清掉，其他是新内容。
///
/// - 填了就用填的（导入的私钥看私钥正文框，agent 恒为空，其余看密码 / 口令框）；
/// - 新建时留空就是没有；
/// - 编辑时留空：类型没改 = 不改；改了类型又没给新内容 = 清掉（原来存的秘密对新类型没有意义，
///   带过去还可能被当成密码发出去）。
pub fn secret_to_send(
    editing: Option<IdentityKind>,
    kind: IdentityKind,
    secret: &str,
    inline_key: &str,
) -> Option<String> {
    let typed = match kind {
        IdentityKind::KeyInline => inline_key,
        IdentityKind::Agent => "",
        _ => secret,
    };
    if !typed.is_empty() {
        return Some(typed.to_owned());
    }
    match editing {
        None => Some(String::new()),
        Some(old) if old == kind => None,
        Some(_) => Some(String::new()),
    }
}

/// 导入的私钥的口令：`None` 保留原口令，`Some("")` 清掉，其他是新口令。口令框留空的意思是「不改」——
/// 不能因为重新粘贴了私钥就顺手把原口令清掉；要去掉口令得明确勾选「清除口令」。
pub fn inline_passphrase_to_send(
    kind: IdentityKind,
    passphrase: &str,
    clear: bool,
) -> Option<String> {
    if kind != IdentityKind::KeyInline {
        return None;
    }
    if clear {
        return Some(String::new());
    }
    (!passphrase.is_empty()).then(|| passphrase.to_owned())
}

/// 编辑已有凭据时改了类型，且原来存过秘密：保存会清掉它，界面要提前说。
pub fn kind_change_clears_secret(editing: Option<&Identity>, kind: IdentityKind) -> bool {
    editing.is_some_and(|item| item.has_secret && item.kind != kind)
}

/// 凭据表单的值。
#[derive(Clone, Debug)]
pub struct IdentityInput<'a> {
    pub name: &'a str,
    pub kind: IdentityKind,
    pub key_path: &'a str,
    pub secret: &'a str,
    pub passphrase: &'a str,
    pub clear_passphrase: bool,
    pub inline_key: &'a str,
}

/// 检查表单并拼出保存草稿。`editing` 是正在编辑的已有凭据。
pub fn build_draft(
    input: &IdentityInput<'_>,
    editing: Option<&Identity>,
) -> Result<IdentityDraft, String> {
    if input.name.trim().is_empty() {
        return Err("凭据名称不能为空".to_owned());
    }
    Ok(IdentityDraft {
        id: editing.map(|item| item.id),
        name: input.name.trim().to_owned(),
        kind: input.kind,
        key_path: (input.kind == IdentityKind::KeyFile).then(|| input.key_path.trim().to_owned()),
        secret: secret_to_send(
            editing.map(|item| item.kind),
            input.kind,
            input.secret,
            input.inline_key,
        ),
        passphrase: inline_passphrase_to_send(input.kind, input.passphrase, input.clear_passphrase),
    })
}

/// 已有导入私钥、类型没改：存储层会沿用原口令，「留空不改」和「清除口令」才有意义。
pub fn can_keep_passphrase(editing: Option<&Identity>, kind: IdentityKind) -> bool {
    editing.is_some_and(|item| {
        item.kind == IdentityKind::KeyInline && item.has_secret && kind == IdentityKind::KeyInline
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(kind: IdentityKind, has_secret: bool) -> Identity {
        Identity {
            id: 3,
            name: "deploy".to_owned(),
            kind,
            key_path: None,
            has_secret,
        }
    }

    fn input<'a>(kind: IdentityKind) -> IdentityInput<'a> {
        IdentityInput {
            name: "deploy",
            kind,
            key_path: "",
            secret: "",
            passphrase: "",
            clear_passphrase: false,
            inline_key: "",
        }
    }

    #[test]
    fn a_new_identity_with_nothing_typed_stores_an_empty_secret() {
        assert_eq!(
            secret_to_send(None, IdentityKind::Password, "", ""),
            Some(String::new()),
            "新建留空就是没有（agent 凭据本来也没有秘密）"
        );
        assert_eq!(
            secret_to_send(None, IdentityKind::Password, "pw", ""),
            Some("pw".to_owned())
        );
    }

    #[test]
    fn leaving_the_box_empty_keeps_the_stored_secret_unless_the_kind_changed() {
        assert_eq!(
            secret_to_send(Some(IdentityKind::Password), IdentityKind::Password, "", ""),
            None,
            "留空 = 不改"
        );
        assert_eq!(
            secret_to_send(Some(IdentityKind::Password), IdentityKind::KeyFile, "", ""),
            Some(String::new()),
            "改了类型又没给新内容：清掉，免得旧密码被当成私钥口令"
        );
        assert_eq!(
            secret_to_send(
                Some(IdentityKind::Password),
                IdentityKind::KeyFile,
                "phrase",
                ""
            ),
            Some("phrase".to_owned())
        );
    }

    #[test]
    fn an_imported_key_reads_the_key_box_and_an_agent_never_has_a_secret() {
        assert_eq!(
            secret_to_send(
                None,
                IdentityKind::KeyInline,
                "ignored",
                "-----BEGIN KEY-----"
            ),
            Some("-----BEGIN KEY-----".to_owned())
        );
        assert_eq!(
            secret_to_send(Some(IdentityKind::Agent), IdentityKind::Agent, "typed", ""),
            None
        );
    }

    #[test]
    fn the_inline_passphrase_is_kept_unless_changed_or_explicitly_cleared() {
        assert_eq!(
            inline_passphrase_to_send(IdentityKind::KeyInline, "", false),
            None
        );
        assert_eq!(
            inline_passphrase_to_send(IdentityKind::KeyInline, "new", false),
            Some("new".to_owned())
        );
        assert_eq!(
            inline_passphrase_to_send(IdentityKind::KeyInline, "ignored", true),
            Some(String::new()),
            "勾了清除就清掉，不管框里有什么"
        );
        assert_eq!(
            inline_passphrase_to_send(IdentityKind::Password, "x", true),
            None
        );
    }

    #[test]
    fn changing_the_kind_of_a_stored_secret_is_flagged_beforehand() {
        let stored = identity(IdentityKind::Password, true);
        assert!(kind_change_clears_secret(
            Some(&stored),
            IdentityKind::KeyFile
        ));
        assert!(!kind_change_clears_secret(
            Some(&stored),
            IdentityKind::Password
        ));
        assert!(!kind_change_clears_secret(
            Some(&identity(IdentityKind::Password, false)),
            IdentityKind::KeyFile
        ));
        assert!(!kind_change_clears_secret(None, IdentityKind::KeyFile));
    }

    #[test]
    fn the_passphrase_controls_only_show_for_an_unchanged_stored_imported_key() {
        let key = identity(IdentityKind::KeyInline, true);
        assert!(can_keep_passphrase(Some(&key), IdentityKind::KeyInline));
        assert!(!can_keep_passphrase(Some(&key), IdentityKind::Password));
        assert!(!can_keep_passphrase(None, IdentityKind::KeyInline));
        assert!(!can_keep_passphrase(
            Some(&identity(IdentityKind::KeyInline, false)),
            IdentityKind::KeyInline
        ));
    }

    #[test]
    fn the_draft_needs_a_name_and_only_key_files_carry_a_path() {
        let mut form = input(IdentityKind::Password);
        form.name = "  ";
        assert_eq!(build_draft(&form, None).unwrap_err(), "凭据名称不能为空");

        let mut form = input(IdentityKind::KeyFile);
        form.key_path = "  C:\\keys\\id_ed25519 ";
        let draft = build_draft(&form, None).expect("合法");
        assert_eq!(draft.key_path.as_deref(), Some("C:\\keys\\id_ed25519"));
        let draft = build_draft(&input(IdentityKind::Password), None).expect("合法");
        assert_eq!(draft.key_path, None);
        assert_eq!(draft.id, None);
    }

    #[test]
    fn editing_keeps_the_id_and_a_blank_secret_means_unchanged() {
        let stored = identity(IdentityKind::Password, true);
        let draft = build_draft(&input(IdentityKind::Password), Some(&stored)).expect("合法");
        assert_eq!(draft.id, Some(3));
        assert_eq!(draft.secret, None);
        assert_eq!(draft.passphrase, None);
    }

    #[test]
    fn labels_cover_every_kind() {
        for (kind, label) in KINDS {
            assert_eq!(kind_label(kind), label);
        }
    }

    /// 把表单草稿真的交给存储层：「留空 = 不改」「改类型清掉旧秘密」是存储层的语义，这里核对界面传的值对得上。
    #[test]
    fn the_drafts_mean_what_the_form_says_once_the_database_applies_them() {
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");
        let mut form = input(IdentityKind::Password);
        form.secret = "first";
        let created = db
            .identity_save(&build_draft(&form, None).expect("合法"))
            .expect("新建");
        assert!(created.has_secret, "新建填了密码就存了");

        // 编辑时密码框留空：不改，还是存着的。
        let form = input(IdentityKind::Password);
        let kept = db
            .identity_save(&build_draft(&form, Some(&created)).expect("合法"))
            .expect("改名");
        assert!(kept.has_secret, "留空表示不改");

        // 改了类型又没给新内容：旧秘密清掉（不然旧密码会被当私钥口令用）。
        let mut form = input(IdentityKind::KeyFile);
        form.key_path = r"C:\keys\id_ed25519";
        let changed = db
            .identity_save(&build_draft(&form, Some(&kept)).expect("合法"))
            .expect("改类型");
        assert_eq!(changed.kind, IdentityKind::KeyFile);
        assert!(!changed.has_secret, "改类型后旧秘密不再带着");
        assert_eq!(changed.key_path.as_deref(), Some(r"C:\keys\id_ed25519"));
    }
}
