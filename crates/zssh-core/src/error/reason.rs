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

/// 一条会上屏的原因。
#[derive(Clone, PartialEq, Eq)]
pub struct Reason {
    key: &'static str,
    zh: String,
    params: Vec<(&'static str, String)>,
}

impl Reason {
    /// `key` 取自 [`keys`]；`zh` 是这条原因的中文原文（可以是 `format!` 出来的）。
    pub fn new(key: &'static str, zh: impl Into<String>) -> Self {
        Self {
            key,
            zh: zh.into(),
            params: Vec::new(),
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
        // @generated-keys-end
    }
}
