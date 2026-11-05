//! 会上屏的错误原因：稳定的 key + 结构化参数，外加一份中文原文。
//!
//! 核心库不依赖界面框架，也不替界面挑语言：它只说"这是哪一种原因、有哪些参数"
//! （`PrivateKeyUnreadable { path }` 这类），界面按当前语言生成整句英文 / 中文。
//! 中文原文（[`Reason::zh`]）留给 `Display`：日志、诊断、测试断言和中文界面都读它，
//! 所以中文界面上用户看到的话和以前逐字相同。
//!
//! key 一旦发布就是契约：只能新增、不能改名（同 `ErrorWithCode::code`）。参数里放用户数据
//! （路径、主机名、凭据名）和第三方库 / 系统错误的原文：原样保留，界面不翻译它们。
//! 界面不认得的 key 退回中文原文，宁可显示中文，也不丢信息。

use std::fmt;

use super::ErrorWithCode;

/// 一条会上屏的原因。
#[derive(Clone, PartialEq, Eq)]
pub struct Reason {
    key: &'static str,
    zh: String,
    params: Vec<(&'static str, String)>,
    /// 这条原因是由别的错误引起的（"检查 X 时失败：<内层错误>"）：名字对应模板里的占位符
    /// （`cause`、`restore`……）。界面按当前语言把内层也生成整句，不用拼好的中文。
    causes: Vec<(&'static str, Box<Captured>)>,
}

/// 把一个错误的「码 + 参数 + 中文原文 + 内层」抓下来，好放进 [`Reason`]（`Reason` 要能
/// `Clone` / 比较，所以不直接装 `Box<dyn ErrorWithCode>`）。
#[derive(Clone, PartialEq, Eq)]
pub struct Captured {
    code: &'static str,
    params: Vec<(&'static str, String)>,
    zh: String,
    cause: Option<Box<Captured>>,
    causes: Vec<(&'static str, Box<Captured>)>,
}

impl Captured {
    fn of(err: &dyn ErrorWithCode) -> Self {
        Self {
            code: err.code(),
            params: err.params(),
            zh: err.to_string(),
            cause: err.cause().map(|inner| Box::new(Self::of(inner))),
            causes: err
                .causes()
                .into_iter()
                .map(|(name, inner)| (name, Box::new(Self::of(inner))))
                .collect(),
        }
    }
}

impl fmt::Display for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.zh)
    }
}

impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.zh)
    }
}

impl ErrorWithCode for Captured {
    fn code(&self) -> &'static str {
        self.code
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        self.params.clone()
    }

    fn cause(&self) -> Option<&dyn ErrorWithCode> {
        self.cause
            .as_deref()
            .map(|inner| inner as &dyn ErrorWithCode)
    }

    fn causes(&self) -> Vec<(&'static str, &dyn ErrorWithCode)> {
        self.causes
            .iter()
            .map(|(name, inner)| (*name, inner.as_ref() as &dyn ErrorWithCode))
            .collect()
    }
}

impl Reason {
    /// `key` 取自 [`keys`]；`zh` 是这条原因的中文原文（可以是 `format!` 出来的）。
    pub fn new(key: &'static str, zh: impl Into<String>) -> Self {
        Self {
            key,
            zh: zh.into(),
            params: Vec::new(),
            causes: Vec::new(),
        }
    }

    /// 没有结构化 key 的文字：界面照原样显示。只给"用户永远看不到"的内部诊断用
    /// （比如断言式的"理论上到不了"），会上屏的原因都应该有自己的 key。
    pub fn text(zh: impl Into<String>) -> Self {
        Self::new(keys::TEXT, zh)
    }

    /// 加一个参数。数字等先转成十进制文本：界面只做"把值填进模板"。
    #[must_use]
    pub fn with(mut self, name: &'static str, value: impl ToString) -> Self {
        self.params.push((name, value.to_string()));
        self
    }

    /// 记下引起这条原因的内层错误（中文原文里已经拼进了它的 `Display`；英文由界面按它的
    /// 码和参数重新生成，模板里用 `{cause}`）。
    #[must_use]
    pub fn caused_by(self, err: &dyn ErrorWithCode) -> Self {
        self.caused_by_as("cause", err)
    }

    /// 同上，占位符名字自己取（一条原因里有两个内层错误时用）。
    #[must_use]
    pub fn caused_by_as(mut self, name: &'static str, err: &dyn ErrorWithCode) -> Self {
        self.causes.push((name, Box::new(Captured::of(err))));
        self
    }

    /// 内层错误们（见 [`Reason::caused_by`]）。
    pub fn causes(&self) -> Vec<(&'static str, &dyn ErrorWithCode)> {
        self.causes
            .iter()
            .map(|(name, inner)| (*name, inner.as_ref() as &dyn ErrorWithCode))
            .collect()
    }

    /// 在中文原文后面接一段（只改 `Display`；英文由界面按参数补，见 `ipv6_hint`）。
    #[must_use]
    pub fn append_zh(mut self, suffix: &str) -> Self {
        self.zh.push_str(suffix);
        self
    }

    pub fn key(&self) -> &'static str {
        self.key
    }

    pub fn params(&self) -> &[(&'static str, String)] {
        &self.params
    }

    /// 中文原文。
    pub fn zh(&self) -> &str {
        &self.zh
    }

    pub fn into_zh(self) -> String {
        self.zh
    }

    /// 一次加好几个参数。
    #[must_use]
    pub fn with_all(mut self, params: impl IntoIterator<Item = (&'static str, String)>) -> Self {
        self.params.extend(params);
        self
    }

    /// 中文原文是不是以这段开头（断言用）。
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.zh.starts_with(prefix)
    }

    /// 中文原文里有没有这段文字（断言和日志筛选用）。
    pub fn contains(&self, needle: &str) -> bool {
        self.zh.contains(needle)
    }
}

/// `Reason` 自己也能当「内层错误」被嵌套：界面按它的 key 出整句。
impl ErrorWithCode for Reason {
    fn code(&self) -> &'static str {
        "core.reason"
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![("reason", self.key.to_owned()), ("detail", self.zh.clone())];
        out.extend(self.params.iter().cloned());
        out
    }

    fn causes(&self) -> Vec<(&'static str, &dyn ErrorWithCode)> {
        Reason::causes(self)
    }
}

/// 序列化成中文原文那一个字符串（和 `X11Failure` 以前的 `String` 字段在 JSON 里一模一样）。
impl serde::Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.zh)
    }
}

impl PartialEq<str> for Reason {
    fn eq(&self, other: &str) -> bool {
        self.zh == other
    }
}

impl PartialEq<&str> for Reason {
    fn eq(&self, other: &&str) -> bool {
        self.zh == *other
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.zh)
    }
}

impl fmt::Debug for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.zh)
    }
}

/// 测试和旧调用点：只有文字、没有 key 的原因。
impl From<String> for Reason {
    fn from(zh: String) -> Self {
        Self::text(zh)
    }
}

impl From<&str> for Reason {
    fn from(zh: &str) -> Self {
        Self::text(zh)
    }
}

/// 被引用的对象类别（`ZsshError::NotFound` 用）。界面按语言出词（"主机" / "host"），
/// 不再把中文名词放进错误里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    Group,
    Host,
    Identity,
    Proxy,
    PortForward,
    AiEndpoint,
}

impl Entity {
    /// 稳定的英文标识，作为参数交给界面。
    pub fn key(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Host => "host",
            Self::Identity => "identity",
            Self::Proxy => "proxy",
            Self::PortForward => "port_forward",
            Self::AiEndpoint => "ai_endpoint",
        }
    }

    /// 中文名词（`Display` 用，和以前的写死文本逐字相同）。
    pub fn zh(self) -> &'static str {
        match self {
            Self::Group => "分组",
            Self::Host => "主机",
            Self::Identity => "凭据",
            Self::Proxy => "代理",
            Self::PortForward => "端口转发",
            Self::AiEndpoint => "ZeePilot 接口",
        }
    }
}

/// 拨号链上某一跳失败时，是哪一步出的错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopStage {
    /// 建立到这一跳的网络连接（直连或经代理）。
    Connect,
    /// SSH 握手或认证。
    Handshake,
    /// 经上一跳（`via`）开转发通道。
    Tunnel { via: String },
}

impl HopStage {
    pub fn key(&self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Handshake => "handshake",
            Self::Tunnel { .. } => "tunnel",
        }
    }

    /// 中文说法，接在"失败："前面。
    pub fn zh(&self) -> String {
        match self {
            Self::Connect => "连接".to_owned(),
            Self::Handshake => "SSH 握手或认证".to_owned(),
            Self::Tunnel { via } => format!("经跳板机 {via} 打开转发通道"),
        }
    }
}

/// 所有 key。新增只能往后加。
pub mod keys {
    macro_rules! keys {
        ($($name:ident = $key:literal;)*) => {
            $(pub const $name: &str = $key;)*
            /// 全部 key（测试用：界面要为每个 key 备好英文模板）。
            pub const ALL: &[&str] = &[$($key),*];
        };
    }

    keys! {
        TEXT = "text";
        // @generated-keys-begin
        HOST_JUMP_SELF = "store.jump_host_self";
        HOST_JUMP_NOT_SSH_IN_USE = "store.jump_host_not_ssh_in_use";
        HOST_TYPE_SWITCH = "store.host_type_switch";
        HOST_TAGS_SERIALIZE = "store.tags_serialize";
        JUMP_CHAIN_CYCLE = "store.jump_chain_cycle";
        JUMP_HOST_PROTOCOL = "store.jump_host_protocol";
        JUMP_CHAIN_TOO_DEEP = "store.jump_chain_too_deep";
        SERIAL_NO_LOGIN_INFO = "store.serial_no_login_info";
        IDENTITY_IS_AI_KEY = "store.identity_is_ai_key";
        HOST_KEY_AUTH_NO_IDENTITY = "store.key_auth_without_identity";
        IDENTITY_NO_KEY_PATH = "store.identity_no_key_path";
        IDENTITY_NO_KEY_CONTENT = "store.identity_no_key_content";
        IDENTITY_NOT_KEY = "store.identity_not_key";
        IDENTITY_NOT_PASSWORD = "store.identity_not_password";
        HOST_NAME_EMPTY = "store.host_name_empty";
        PORT_RANGE = "input.port_range";
        USERNAME_EMPTY = "input.username_empty";
        ADDRESS_EMPTY = "input.address_empty";
        ADDRESS_TOO_LONG = "input.address_too_long";
        ADDRESS_BAD_WHITESPACE = "input.address_bad_whitespace";
        ADDRESS_BAD_CHAR = "input.address_bad_char";
        GROUP_NAME_EMPTY = "store.group_name_empty";
        GROUP_OWN_PARENT = "store.group_own_parent";
        GROUP_MOVE_INTO_CHILD = "store.group_move_into_child";
        NAME_TOO_LONG_64 = "input.name_too_long_64";
        FORWARD_BIND_BAD = "store.forward_bind_bad";
        FORWARD_DEST_BAD = "store.forward_dest_bad";
        FORWARD_REMOTE_PORT_TAKEN = "store.forward_remote_port_taken";
        UNKNOWN_FORWARD_KIND = "input.unknown_forward_kind";
        UNKNOWN_AUTH_METHOD = "input.unknown_auth_method";
        UNKNOWN_IDENTITY_KIND = "input.unknown_identity_kind";
        UNKNOWN_PROXY_STRATEGY = "input.unknown_proxy_strategy";
        UNKNOWN_PROXY_KIND = "input.unknown_proxy_kind";
        UNKNOWN_PROTOCOL = "input.unknown_protocol";
        UNKNOWN_ENDPOINT_KIND = "input.unknown_endpoint_kind";
        PROXY_NAME_EMPTY = "store.proxy_name_empty";
        PROXY_ADDRESS_EMPTY = "store.proxy_address_empty";
        PROXY_PASSWORD_NOT_UTF8 = "store.proxy_password_not_utf8";
        QUICK_NO_WHITESPACE = "quick.no_whitespace";
        QUICK_IPV6_NO_BRACKET = "quick.ipv6_missing_bracket";
        QUICK_AFTER_BRACKET = "quick.after_bracket";
        QUICK_IPV6_NEEDS_BRACKET = "quick.ipv6_needs_bracket";
        ENDPOINT_NAME_BAD = "ai.endpoint_name_bad";
        ENDPOINT_URL_BAD = "ai.endpoint_url_bad";
        DEFAULT_MODEL_BAD = "ai.default_model_bad";
        MODEL_NAME_BAD = "ai.model_name_bad";
        MODELS_TOO_MANY = "ai.models_too_many";
        OVERRIDES_TOO_MANY = "ai.overrides_too_many";
        OVERRIDE_MODEL_BAD = "ai.override_model_bad";
        NO_KEY_ONLY_OPENAI = "ai.no_key_only_openai";
        PRESET_ID_BAD = "ai.preset_id_bad";
        SERIAL_NO_IDENTITY = "protocol.serial_no_identity";
        SERIAL_NO_PROXY = "protocol.serial_no_proxy";
        SERIAL_NO_JUMP = "protocol.serial_no_jump";
        PASSWORD_ONLY = "protocol.password_only";
        PROTOCOL_CREDENTIAL_NOT_PASSWORD = "protocol.credential_not_password";
        TERMINAL_TYPE_BAD = "protocol.terminal_type_bad";
        AUTO_LOGIN_NEEDS_PATTERNS = "protocol.auto_login_needs_patterns";
        AUTO_LOGIN_NEEDS_USER = "protocol.auto_login_needs_user";
        SERIAL_DEVICE_EMPTY = "protocol.serial_device_empty";
        SERIAL_DEVICE_BAD = "protocol.serial_device_bad";
        BAUD_RANGE = "protocol.baud_range";
        SERIAL_FIVE_TWO = "protocol.serial_five_two";
        SERIAL_ONE_HALF = "protocol.serial_one_half";
        USER_PATTERNS_TOO_MANY = "protocol.username_patterns_too_many";
        USER_PATTERN_BAD = "protocol.username_pattern_bad";
        PASS_PATTERNS_TOO_MANY = "protocol.password_patterns_too_many";
        PASS_PATTERN_BAD = "protocol.password_pattern_bad";
        X11_SSH_ONLY = "protocol.x11_ssh_only";
        NO_TELNET_OPTIONS = "protocol.no_telnet_options";
        NO_SERIAL_OPTIONS = "protocol.no_serial_options";
        JUMP_MUST_BE_SSH = "protocol.jump_must_be_ssh";
        PROTOCOL_OPTIONS_SERIALIZE = "protocol.options_serialize";
        IDENTITY_NAME_EMPTY = "store.identity_name_empty";
        KEYFILE_NEEDS_PATH = "store.keyfile_needs_path";
        IDENTITY_KIND_CHANGE_NEEDS_SECRET = "store.identity_kind_change_needs_secret";
        THIS_IDENTITY_NO_KEY_PATH = "store.this_identity_no_key_path";
        PRIVATE_KEY_FILE_UNREADABLE = "store.private_key_file_unreadable";
        IDENTITY_NO_PUBLIC_KEY = "store.identity_no_public_key";
        IDENTITY_NOT_IMPORTED_KEY = "store.identity_not_imported_key";
        IDENTITY_NO_KEY_CONTENT_YET = "store.identity_no_key_content_yet";
        IDENTITY_SECRET_NOT_UTF8 = "store.identity_secret_not_utf8";
        ORDER_DUPLICATE_ID = "store.order_duplicate_id";
        ORDER_HOSTS_SAME_GROUP = "store.order_hosts_same_group";
        ORDER_GROUPS_SAME_PARENT = "store.order_groups_same_parent";
        SSH_USER_REQUIRED = "store.ssh_user_required";
        DPAPI_PROTECT_FAILED = "store.dpapi_protect_failed";
        DPAPI_UNPROTECT_FAILED = "store.dpapi_unprotect_failed";
        DATA_TOO_LONG = "store.data_too_long";
        NO_SAVED_PASSWORD = "store.no_saved_password";
        KEY_LOGIN_NO_PASSWORD = "store.key_login_no_password";
        AGENT_LOGIN_NO_PASSWORD = "store.agent_login_no_password";
        HINT_IPV6_EGRESS = "hint.ipv6_egress";
        KEYGEN_GENERATE_FAILED = "keygen.generate_failed";
        KEYGEN_EXPORT_PUBLIC_FAILED = "keygen.export_public_failed";
        KEYGEN_ENCRYPT_FAILED = "keygen.encrypt_failed";
        KEYGEN_EXPORT_PRIVATE_FAILED = "keygen.export_private_failed";
        PRIVATE_KEY_UNREADABLE = "ssh.private_key_unreadable";
        PUBLIC_KEY_FORMAT_BAD = "keygen.public_key_format_bad";
        KEY_INSTALL_NOT_CONFIRMED = "keygen.install_not_confirmed";
        KEY_PASSPHRASE_STILL_WRONG = "ssh.key_passphrase_still_wrong";
        AGENT_PIPE_UNREACHABLE = "ssh.agent_unreachable";
        AGENT_LIST_FAILED = "ssh.agent_list_failed";
        ANSWER_COUNT_MISMATCH = "ssh.answer_count_mismatch";
        LOCAL_PORT_IN_USE = "forward.local_port_in_use";
        LOCAL_PORT_DENIED = "forward.local_port_denied";
        LOCAL_LISTEN_FAILED = "forward.local_listen_failed";
        CONNECTION_LOST = "ssh.connection_lost";
        KH_MKDIR_FAILED = "known_hosts.mkdir_failed";
        KH_OPEN_FAILED = "known_hosts.open_failed";
        KH_WRITE_FAILED = "known_hosts.write_failed";
        KH_FLUSH_FAILED = "known_hosts.flush_failed";
        KH_READ_FAILED = "known_hosts.read_failed";
        KH_UNREADABLE = "known_hosts.unreadable";
        JUMP_TCP_FORWARDING_PROHIBITED = "ssh.jump_tcp_forwarding_prohibited";
        JUMP_CANNOT_REACH_TARGET = "ssh.jump_cannot_reach_target";
        CHAIN_EMPTY = "ssh.chain_empty";
        TCP_FORWARDING_PROHIBITED = "ssh.tcp_forwarding_prohibited";
        SERVER_CANNOT_REACH = "ssh.server_cannot_reach";
        REMOTE_LISTEN_DENIED = "ssh.remote_listen_denied";
        CONNECTION_CLOSED = "ssh.connection_closed";
        SFTP_SUBSYSTEM_TIMEOUT = "ssh.sftp_subsystem_timeout";
        EXEC_FAILED = "ssh.exec_failed";
        EXEC_NONZERO = "ssh.exec_nonzero";
        EXEC_CANCELLED = "ssh.exec_cancelled";
        REMOTE_PORT_CLAIMED = "ssh.remote_port_claimed";
        SHELL_PTY_REQUEST_FAILED = "shell.pty_request_failed";
        SHELL_REQUEST_FAILED = "shell.request_failed";
        SHELL_REFUSED = "shell.refused";
        SHELL_TOO_MANY_MESSAGES = "shell.too_many_messages";
        SHELL_TOO_MUCH_DATA = "shell.too_much_data";
        SHELL_CLOSED_WHILE_OPENING = "shell.closed_while_opening";
        SHELL_REPLY_TIMEOUT = "shell.reply_timeout";
        SHELL_WRITE_FAILED = "shell.write_failed";
        SHELL_RESIZE_FAILED = "shell.resize_failed";
        SHELL_EOF_FAILED = "shell.eof_failed";
        SHELL_CLOSE_FAILED = "shell.close_failed";
        PROXY_HANDSHAKE_TIMEOUT = "proxy.handshake_timeout";
        PROXY_TARGET_EMPTY = "proxy.target_empty";
        PROXY_TARGET_BAD_IPV6 = "proxy.target_bad_ipv6";
        PROXY_TARGET_BAD_CHAR = "proxy.target_bad_char";
        PROXY_TARGET_BRACKETS = "proxy.target_brackets";
        SOCKS_UNREACHABLE = "proxy.socks_unreachable";
        SOCKS_PASSWORD_REJECTED = "proxy.socks_password_rejected";
        SOCKS_NO_AUTH_METHOD = "proxy.socks_no_auth_method";
        SOCKS_AUTH_REQUIRED = "proxy.socks_auth_required";
        SOCKS_CONNECTION_REFUSED = "proxy.socks_connection_refused";
        SOCKS_HOST_UNREACHABLE = "proxy.socks_host_unreachable";
        SOCKS_NETWORK_UNREACHABLE = "proxy.socks_network_unreachable";
        SOCKS_RULESET = "proxy.socks_ruleset";
        SOCKS_COMMAND_UNSUPPORTED = "proxy.socks_command_unsupported";
        SOCKS_ADDRESS_TYPE = "proxy.socks_address_type";
        PROXY_OTHER = "proxy.other";
        HTTP_CONNECT_SEND_FAILED = "proxy.http_connect_send_failed";
        HTTP_AUTH_BAD = "proxy.http_auth_bad";
        HTTP_AUTH_NEEDED = "proxy.http_auth_needed";
        HTTP_STATUS = "proxy.http_status";
        HTTP_HEAD_TOO_LONG = "proxy.http_head_too_long";
        HTTP_READ_FAILED = "proxy.http_read_failed";
        HTTP_CLOSED_EMPTY = "proxy.http_closed_empty";
        LOCK_ON_START_NEEDS_PASSWORD = "lock.on_start_needs_password";
        PRESET_NO_ENDPOINT = "ai.preset_no_endpoint";
        ENDPOINT_ADDRESS_REJECTED = "ai.endpoint_address_rejected";
        X11_DISPLAY_BAD_TEXT = "x11.display_bad_text";
        X11_DISPLAY_UNIX_SOCKET = "x11.display_unix_socket";
        X11_DISPLAY_FORMAT = "x11.display_format";
        X11_DISPLAY_NUMBER = "x11.display_number";
        X11_SCREEN_NUMBER = "x11.screen_number";
        X11_PORT_RANGE = "x11.port_range";
        CMD_NESTED_TOO_DEEP = "cmd.nested_too_deep";
        CMD_FORK_BOMB = "cmd.fork_bomb";
        CMD_UNPAIRED_SUBSTITUTION = "cmd.unpaired_substitution";
        CMD_UNPAIRED_QUOTES = "cmd.unpaired_quotes";
        CMD_SUDOERS_FILE = "cmd.sudoers_file";
        CMD_SYSRQ = "cmd.sysrq_trigger";
        CMD_FIND_DELETE = "cmd.find_delete";
        CMD_FIND_EXEC = "cmd.find_exec";
        CMD_SHRED = "cmd.shred";
        CMD_TRUNCATE = "cmd.truncate";
        CMD_MKFS = "cmd.mkfs";
        CMD_WRITE_BLOCK_DEVICE = "cmd.write_block_device";
        CMD_WIPEFS = "cmd.wipefs";
        CMD_PARTITION_WRITE = "cmd.partition_write";
        CMD_POWER = "cmd.power";
        CMD_RUNLEVEL = "cmd.runlevel";
        CMD_SSH_SERVICE = "cmd.ssh_service";
        CMD_CHMOD_SYSTEM = "cmd.chmod_system";
        CMD_CHOWN_SYSTEM = "cmd.chown_system";
        CMD_USERDEL = "cmd.userdel";
        CMD_PASSWD_CLEAR = "cmd.passwd_clear";
        CMD_ACCOUNT_LOCK = "cmd.account_lock";
        CMD_VISUDO = "cmd.visudo";
        CMD_FIREWALL_FLUSH = "cmd.firewall_flush";
        CMD_FIREWALL_DEFAULT_DENY = "cmd.firewall_default_deny";
        CMD_FIREWALL_DISABLE = "cmd.firewall_disable";
        CMD_NETWORK_DOWN = "cmd.network_down";
        CMD_EVAL = "cmd.eval";
        CMD_KILL_INIT = "cmd.kill_init";
        CMD_KILLALL5 = "cmd.killall5";
        CMD_KILL_CRITICAL = "cmd.kill_critical";
        CMD_CRONTAB_REMOVE = "cmd.crontab_remove";
        CMD_KERNEL_PANIC = "cmd.kernel_panic";
        CMD_SWAPOFF_ALL = "cmd.swapoff_all";
        CMD_UMOUNT_SYSTEM = "cmd.umount_system";
        CMD_MOVE_SYSTEM = "cmd.move_system";
        CMD_PYTHON_INLINE = "cmd.python_inline";
        CMD_PERL_INLINE = "cmd.perl_inline";
        CMD_RUBY_INLINE = "cmd.ruby_inline";
        CMD_NODE_INLINE = "cmd.node_inline";
        CMD_PHP_INLINE = "cmd.php_inline";
        CMD_LUA_INLINE = "cmd.lua_inline";
        CMD_AWK_SYSTEM = "cmd.awk_system";
        CMD_INTERACTIVE_SHELL = "cmd.interactive_shell";
        CMD_RM_NO_TARGET = "cmd.rm_no_target";
        CMD_RM_SYSTEM = "cmd.rm_system";
        CMD_PRIVILEGE_NO_COMMAND = "cmd.privilege_no_command";
        CMD_SU = "cmd.su";
        CMD_REDIRECT_BLOCK_DEVICE = "cmd.redirect_block_device";
        CMD_REDIRECT_SYSTEM_FILE = "cmd.redirect_system_file";
        SFTP_LIMITED = "sftp.limited";
        SFTP_UNEXPECTED_PACKET = "sftp.unexpected_packet";
        SFTP_REQUEST_TIMEOUT = "sftp.request_timeout";
        SFTP_REALPATH_EMPTY = "sftp.realpath_empty";
        SFTP_READLINK_EMPTY = "sftp.readlink_empty";
        SFTP_POSIX_RENAME_REPLY = "sftp.posix_rename_reply";
        SFTP_COPY_DATA_REPLY = "sftp.copy_data_reply";
        SFTP_COPY_PEER_CLOSED = "sftp.copy_peer_closed";
        SFTP_EXISTS_NOT_DIR = "sftp.exists_not_dir";
        SFTP_TASK_SYMLINK = "sftp.task_symlink";
        SFTP_TASK_NAME_CHECK = "sftp.task_name_check";
        SFTP_TASK_WRITE = "sftp.task_write";
        SFTP_TASK_READ = "sftp.task_read";
        SFTP_TASK_LOCAL_READ = "sftp.task_local_read";
        SFTP_TASK_LOCAL_WRITE = "sftp.task_local_write";
        SFTP_TASK_LOCAL_FINISH = "sftp.task_local_finish";
        SFTP_TASK_LOCAL_SYNC = "sftp.task_local_sync";
        SFTP_READ_OFFSET_MISMATCH = "sftp.read_offset_mismatch";
        SFTP_SOURCE_ENDED_EARLY = "sftp.source_ended_early";
        SFTP_OWNER_WOULD_CHANGE = "sftp.owner_would_change";
        SFTP_NAME_EMPTY = "sftp.name_empty";
        SFTP_NAME_DOTS = "sftp.name_dots";
        SFTP_NAME_UNUSABLE = "sftp.name_unusable";
        SFTP_DOWNLOAD_NO_NAME = "sftp.download_no_name";
        SFTP_TARGET_OUTSIDE = "sftp.target_outside";
        SFTP_COPY_INTO_SELF = "sftp.copy_into_self";
        SFTP_COPY_UNSURE = "sftp.copy_unsure";
        SFTP_COPY_TOO_DEEP = "sftp.copy_too_deep";
        SFTP_COPY_TARGET_IS_DIR = "sftp.copy_target_is_dir";
        SFTP_COPY_MERGE_NEEDS_POLICY = "sftp.copy_merge_needs_policy";
        SFTP_COPY_TARGET_IS_FILE = "sftp.copy_target_is_file";
        SFTP_COPY_SIZE_MISMATCH = "sftp.copy_size_mismatch";
        SFTP_COPY_VERIFY_TOO_DEEP = "sftp.copy_verify_too_deep";
        SFTP_COPY_VERIFY_TOO_MANY = "sftp.copy_verify_too_many";
        SFTP_REPLACE_EXISTS_CHECK = "sftp.replace_exists_check";
        SFTP_REPLACE_RENAME = "sftp.replace_rename";
        SFTP_REPLACE_ASIDE_LOST = "sftp.replace_aside_lost";
        SFTP_REPLACE_ASIDE_FAILED = "sftp.replace_aside_failed";
        SFTP_REPLACE_IN_LOST = "sftp.replace_in_lost";
        SFTP_REPLACE_IN_RESTORED = "sftp.replace_in_restored";
        SFTP_REPLACE_IN_RESTORE_FAILED = "sftp.replace_in_restore_failed";
        XFER_NOTE_EMPTY_DIR = "xfer.note_empty_dir";
        XFER_NOTE_SKIPPED_LINK = "xfer.note_skipped_link";
        XFER_NOTE_FILE_COUNT = "xfer.note_file_count";
        XFER_NOTE_SKIPPED_SYMLINK = "xfer.note_skipped_symlink";
        XFER_NOTE_NON_UTF8 = "xfer.note_non_utf8";
        XFER_NOTE_RENAMED = "xfer.note_renamed";
        XFER_DIR_NEEDS_RECURSIVE = "xfer.dir_needs_recursive";
        XFER_SYMLINK_NO_RELAY = "xfer.symlink_no_relay";
        XFER_NOTE_NO_RESUME_POINT = "xfer.note_no_resume_point";
        XFER_NOTE_CANNOT_TRUNCATE = "xfer.note_cannot_truncate";
        XFER_NOTE_RESUME_MISMATCH = "xfer.note_resume_mismatch";
        XFER_NOTE_TARGET_APPEARED = "xfer.note_target_appeared";
        XFER_NOTE_RECONNECT = "xfer.note_reconnect";
        XFER_NOTE_SKIP_EXISTING = "xfer.note_skip_existing";
        XFER_NOTE_CANNOT_RESUME = "xfer.note_cannot_resume";
        XFER_NOTE_RENAMED_TO = "xfer.note_renamed_to";
        SFTP_WRAPPED = "sftp.wrapped";
        ARCHIVE_NAME_TAKEN = "archive.name_taken";
        ARCHIVE_NOTHING_SELECTED = "archive.nothing_selected";
        ARCHIVE_BAD_NAME = "archive.bad_name";
        ARCHIVE_DIR_NOT_ABSOLUTE = "archive.dir_not_absolute";
        ARCHIVE_COMPRESS_TIMEOUT = "archive.compress_timeout";
        ARCHIVE_NO_OUTPUT = "archive.no_output";
        ARCHIVE_OUTPUT_MISSING = "archive.output_missing";
        ARCHIVE_RENAME_FAILED = "archive.rename_failed";
        ARCHIVE_UNKNOWN_FORMAT = "archive.unknown_format";
        ARCHIVE_DEST_IS_FILE = "archive.dest_is_file";
        ARCHIVE_PATH_NOT_ABSOLUTE = "archive.path_not_absolute";
        ARCHIVE_READ_FAILED = "archive.read_failed";
        ARCHIVE_IS_FOLDER = "archive.is_folder";
        ARCHIVE_INSPECT_TIMEOUT = "archive.inspect_timeout";
        ARCHIVE_COMMAND_MISSING = "archive.command_missing";
        ARCHIVE_LIST_FAILED_CODE = "archive.list_failed_code";
        ARCHIVE_LIST_FAILED = "archive.list_failed";
        ARCHIVE_TOO_MANY_ENTRIES = "archive.too_many_entries";
        ARCHIVE_EXTRACT_TIMEOUT = "archive.extract_timeout";
        ARCHIVE_EXTRACT_WARNING = "archive.extract_warning";
        ARCHIVE_EXTRACT_WARNING_DETAIL = "archive.extract_warning_detail";
        ARCHIVE_EXTRACT_FAILED = "archive.extract_failed";
        ARCHIVE_EXTRACT_FAILED_NO_CODE = "archive.extract_failed_no_code";
        ARCHIVE_EXTRACT_NO_EXIT = "archive.extract_no_exit";
        ARCHIVE_COMPRESS_FAILED = "archive.compress_failed";
        ARCHIVE_COMPRESS_NO_EXIT = "archive.compress_no_exit";
        ARCHIVE_FILES_CHANGED = "archive.files_changed";
        ARCHIVE_CHANGED_AFTER_CHECK = "archive.changed_after_check";
        ARCHIVE_DEST_CHANGED = "archive.dest_changed";
        ARCHIVE_EXTRACT_RENAME_FAILED = "archive.extract_rename_failed";
        ARCHIVE_EXEC_FAILED = "archive.exec_failed";
        SFTP_CP_EXEC_UNAVAILABLE = "sftp.cp_exec_unavailable";
        SFTP_CP_TIMEOUT = "sftp.cp_timeout";
        SFTP_CP_NOT_FOUND = "sftp.cp_not_found";
        SFTP_CP_FAILED = "sftp.cp_failed";
        SFTP_CP_NO_EXIT = "sftp.cp_no_exit";
        SFTP_CP_READ_SOURCE = "sftp.cp_read_source";
        SFTP_CP_STAT_TARGET = "sftp.cp_stat_target";
        SFTP_CP_TARGET_IS_DIR_MERGE = "sftp.cp_target_is_dir_merge";
        SFTP_CP_TREE_SIZE_MISMATCH = "sftp.cp_tree_size_mismatch";
        SFTP_CP_VERIFY_FAILED = "sftp.cp_verify_failed";
        SFTP_CP_FILE_SIZE_MISMATCH = "sftp.cp_file_size_mismatch";
        SFTP_CP_VERIFY_COPY_FAILED = "sftp.cp_verify_copy_failed";
        SFTP_NAME_BAD = "sftp.name_bad";
        SYNC_STORAGE_CAUSE = "sync.storage_cause";
        SYNC_PKG_BASE64 = "sync.pkg_base64";
        SYNC_PKG_KDF_RANGE = "sync.pkg_kdf_range";
        SYNC_ARGON2_PARAMS = "sync.argon2_params";
        SYNC_ARGON2_FAILED = "sync.argon2_failed";
        SYNC_ENCRYPT_FAILED = "sync.encrypt_failed";
        SYNC_RANDOM_UNAVAILABLE = "sync.random_unavailable";
        SYNC_PKG_NOT_ENCRYPTED = "sync.pkg_not_encrypted";
        SYNC_PKG_NOT_SYNC = "sync.pkg_not_sync";
        SYNC_PKG_SALT_NONCE = "sync.pkg_salt_nonce";
        GH_API_NO_HOST = "github.api_no_host";
        GH_API_EXTRAS = "github.api_extras";
        GH_TOKEN_EMPTY = "github.token_empty";
        GH_TOKEN_BAD_CHARS = "github.token_bad_chars";
        GH_API_HTTP = "github.api_http";
        GH_GIST_ID_EMPTY = "github.gist_id_empty";
        GH_GIST_ID_BAD_LONG = "github.gist_id_bad_long";
        GH_NO_RAW_URL = "github.no_raw_url";
        GH_RAW_URL_BAD = "github.raw_url_bad";
        GH_RAW_URL_NOT_HTTP = "github.raw_url_not_http";
        GH_NO_VERSION = "github.no_version";
        GH_NO_NEW_GIST_ID = "github.no_new_gist_id";
        GH_GIST_ID_BAD = "github.gist_id_bad";
        GH_NO_USER = "github.no_user";
        GH_NOT_UTF8 = "github.not_utf8";
        GH_BAD_GIST = "github.bad_gist";
        GH_BAD_JSON = "github.bad_json";
        GH_GIST_NOT_FOUND = "github.gist_not_found";
        GH_RAW_404 = "github.raw_404";
        GH_USER_NOT_FOUND = "github.user_not_found";
        GH_TOKEN_EXPIRED = "github.token_expired";
        GH_WRITE_DENIED = "github.write_denied";
        GH_CREATE_DENIED = "github.create_denied";
        GH_DELETE_FAILED = "github.delete_failed";
        GH_FORBIDDEN = "github.forbidden";
        GH_REJECTED_422 = "github.rejected_422";
        GH_STATUS = "github.status";
        GH_API_URL_BAD = "github.api_url_bad";
        GH_API_SCHEME = "github.api_scheme";
        GH_GIST_TOO_BIG = "github.gist_too_big";
        GH_INCOMPLETE = "github.incomplete";
        GH_TOO_MANY_FILES = "github.too_many_files";
        NET_PROXY_AUTH = "net.proxy_auth";
        NET_SERVER_RETURNED = "net.server_returned";
        NET_SERVER_RETURNED_CODE = "net.server_returned_code";
        NET_CONNECT_FAILED = "net.connect_failed";
        NET_REQUEST_FAILED = "net.request_failed";
        NET_PROXY_INVALID = "net.proxy_invalid";
        NET_CLIENT_BUILD = "net.client_build";
        S3_ENDPOINT_BAD = "s3.endpoint_bad";
        S3_ENDPOINT_SCHEME = "s3.endpoint_scheme";
        S3_ENDPOINT_NO_HOST = "s3.endpoint_no_host";
        S3_BUCKET_BAD = "s3.bucket_bad";
        S3_URL_FAILED = "s3.url_failed";
        S3_REGION_EMPTY = "s3.region_empty";
        S3_KEYS_EMPTY = "s3.keys_empty";
        S3_NO_BUCKET_READ = "s3.no_bucket_read";
        S3_FAILED = "s3.failed";
        S3_SIGNATURE = "s3.signature";
        S3_NO_ACCESS_KEY = "s3.no_access_key";
        S3_NO_BUCKET = "s3.no_bucket";
        S3_DENIED = "s3.denied";
        S3_REGION_WRONG = "s3.region_wrong";
        S3_AUTH_FAILED = "s3.auth_failed";
        DAV_URL_BAD = "webdav.url_bad";
        DAV_HTTP = "webdav.http";
        DAV_SCHEME = "webdav.scheme";
        DAV_NO_HOST = "webdav.no_host";
        DAV_URL_INVALID = "webdav.url_invalid";
        DAV_FAILED = "webdav.failed";
        DAV_UNAUTHORIZED = "webdav.unauthorized";
        DAV_FORBIDDEN = "webdav.forbidden";
        DAV_NOT_FOUND = "webdav.not_found";
        DAV_METHOD_NOT_ALLOWED = "webdav.method_not_allowed";
        DAV_INSUFFICIENT = "webdav.insufficient";
        DAV_LOCKED = "webdav.locked";
        OD_DENIED = "onedrive.denied";
        OD_NOT_FOUND = "onedrive.not_found";
        OD_LOCKED = "onedrive.locked";
        OD_FULL = "onedrive.full";
        OD_STATUS = "onedrive.status";
        OD_FAILED_CODE = "onedrive.failed_code";
        OD_TOO_BIG = "onedrive.too_big";
        OD_EMPTY_PACKAGE = "onedrive.empty_package";
        OD_REAUTH = "onedrive.reauth";
        OD_NO_REFRESH = "onedrive.no_refresh";
        OD_NO_ACCESS_TOKEN = "onedrive.no_access_token";
        OD_REJECTED = "onedrive.rejected";
        OD_NO_UPLOAD_URL = "onedrive.no_upload_url";
        OD_UPLOAD_URL_BAD = "onedrive.upload_url_bad";
        OD_UPLOAD_NOT_HTTPS = "onedrive.upload_not_https";
        BK_TOO_MANY = "backup.too_many";
        BK_PATH_BAD = "backup.path_bad";
        BK_NOT_BACKUP = "backup.not_backup";
        BK_OUTSIDE_DIR = "backup.outside_dir";
        BT_ID_BAD = "target.id_bad";
        BT_EXISTS = "target.exists";
        BT_NONE = "target.none";
        BT_NO_BACKUP_YET = "target.no_backup_yet";
        CF_ENCODING = "conflict.encoding";
        CF_NOT_UTF8 = "conflict.not_utf8";
        CF_GONE = "conflict.gone";
        CF_NO_OTHER = "conflict.no_other";
        CF_HOST_GONE = "conflict.host_gone";
        CF_GROUP_GONE = "conflict.group_gone";
        CF_IDENTITY_GONE = "conflict.identity_gone";
        CF_PROXY_GONE = "conflict.proxy_gone";
        CF_ENDPOINT_GONE = "conflict.endpoint_gone";
        SV_SETTINGS_DAMAGED = "service.settings_damaged";
        SV_PASSWORD_DAMAGED = "service.password_damaged";
        SV_NO_CLOUD_DATA = "service.no_cloud_data";
        SV_CLOUD_CHANGED = "service.cloud_changed";
        BT_LOGIN_FIRST = "target.login_first";
        BT_MAX_TARGETS = "target.max";
        SV_MASTER_SHORT = "service.master_short";
        SV_NEW_MASTER_SHORT = "service.new_master_short";
        BK_PASSWORD_SHORT = "backup.password_short";
        BT_TIMEOUT = "target.timeout";
        X11_SEND_FAILED = "x11.send_failed";
        X11_NO_FREE_DISPLAY = "x11.no_free_display";
        X11_RANDOM = "x11.random_unavailable";
        X11_WRITE_AUTH = "x11.write_auth_failed";
        X11_EXITED = "x11.vcxsrv_exited";
        X11_NO_LISTEN = "x11.vcxsrv_not_listening";
        X11_NON_ASCII_PATH = "x11.non_ascii_path";
        X11_CUSTOM_BAD = "x11.custom_display_bad";
        X11_PREPARE_TASK = "x11.prepare_task_failed";
        X11_MAX_REG = "x11.max_registrations";
        X11_COOKIE_UNIQUE = "x11.cookie_unique_failed";
        X11_WAIT_FAILED = "x11.wait_failed";
        CT_JUMP_FORWARD = "conntest.jump_forward";
        CT_LABELED = "conntest.labeled";
        CT_JUMP_TIMEOUT = "conntest.jump_timeout";
        CT_WRAPPED = "conntest.wrapped";
        CT_PROXY_FAILED = "conntest.proxy_failed";
        CT_PROXY_DNS = "conntest.proxy_dns";
        CT_PROXY_TIMEOUT = "conntest.proxy_timeout";
        CT_PROXY_CONNECT = "conntest.proxy_connect";
        CT_DNS = "conntest.dns";
        CT_TIMEOUT = "conntest.timeout";
        CT_REFUSED = "conntest.refused";
        CT_CONNECT_FAILED = "conntest.connect_failed";
        CT_CONNECT_OTHER = "conntest.connect_other";
        CT_KEY_CHANGED = "conntest.key_changed";
        CT_KEY_REVOKED = "conntest.key_revoked";
        CT_KEY_REJECTED = "conntest.key_rejected";
        CT_KEY_REKEY = "conntest.key_rekey";
        CT_AUTH_FAILED = "conntest.auth_failed";
        CT_AUTH_ALL = "conntest.auth_all";
        CT_AUTH_TIMEOUT = "conntest.auth_timeout";
        CT_KEY_LOAD = "conntest.key_load";
        CT_AGENT = "conntest.agent";
        CT_RSA_SHA1 = "conntest.rsa_sha1";
        CT_NEED_PASSWORD = "conntest.need_password";
        CT_NEED_PASSPHRASE = "conntest.need_passphrase";
        CT_NEED_INTERACTIVE = "conntest.need_interactive";
        CT_CANCELLED = "conntest.cancelled";
        CT_NO_ALGO = "conntest.no_algo";
        CT_HANDSHAKE_TIMEOUT = "conntest.handshake_timeout";
        CT_DISCONNECTED = "conntest.disconnected";
        CT_HANDSHAKE_OTHER = "conntest.handshake_other";
        CT_OK = "conntest.ok";
        CT_OK_VERSION = "conntest.ok_version";
        CT_UNTRUSTED = "conntest.untrusted";
        CT_UNTRUSTED_OTHER = "conntest.untrusted_other";
        CT_UNTRUSTED_HOP = "conntest.untrusted_hop";
        CT_UNTRUSTED_OTHER_HOP = "conntest.untrusted_other_hop";
        CT_TCP_DATA = "conntest.tcp_data";
        CT_TCP_SILENT = "conntest.tcp_silent";
        CT_TCP_CLOSED = "conntest.tcp_closed";
        CT_SERIAL_OK = "conntest.serial_ok";
        CT_CANCELLED_TEST = "conntest.cancelled_test";
        CT_TIMEOUT_STAGE = "conntest.timeout_stage";
        CT_NO_TARGET = "conntest.no_target";
        CT_NO_JUMP = "conntest.no_jump";
        CT_RESET_AFTER = "conntest.reset_after";
        CT_READ_FAILED = "conntest.read_failed";
        SYNC_RECORD_NO_UID = "sync.record_no_uid";
        SYNC_HOST_PROTOCOL_UNKNOWN = "sync.host_protocol_unknown";
        SYNC_HOST_OPTIONS_BAD = "sync.host_options_bad";
        SYNC_ENDPOINT_KIND_UNKNOWN = "sync.endpoint_kind_unknown";
        SYNC_ENDPOINT_OPTIONS_BAD = "sync.endpoint_options_bad";
        SYNC_ENDPOINT_INVALID = "sync.endpoint_invalid";
        // @generated-keys-end
    }
}
