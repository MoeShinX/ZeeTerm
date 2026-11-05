//! 核心库「会上屏的原因」（`zssh_core::error::Reason`）的英文句子：每个 key 一条模板。
//!
//! 中文界面直接用核心库给的中文原文（`Display`），不在这里再抄一份；英文界面按 key 取模板、
//! 把参数填进去。参数里的用户数据（路径、主机名、凭据名）和第三方 / 系统错误原文原样保留。
//! 认不得的 key 或对不上的参数，调用方退回中文原文（见 `error_text`）。

/// key → 英文模板；没有这个 key 返回 `None`。
pub(crate) fn english(key: &str) -> Option<&'static str> {
    Some(match key {
        "store.jump_host_self" => "A host cannot be its own jump host",
        "store.jump_host_not_ssh_in_use" => {
            "This host is used as a jump host by {users}; only SSH hosts can be jump hosts"
        }
        "store.host_type_switch" => {
            "Serial hosts and network hosts (SSH / Telnet) cannot be converted into each other; create a new host instead"
        }
        "store.tags_serialize" => "Failed to serialize the tags: {cause}",
        "store.jump_chain_cycle" => "The jump host chain loops: host id {id} appears twice",
        "store.jump_host_protocol" => {
            "Jump host \"{name}\" is a {protocol} host; jump hosts must be SSH hosts"
        }
        "store.jump_chain_too_deep" => {
            "At most {max} levels of jump hosts are allowed; this host's chain is too deep"
        }
        "store.serial_no_login_info" => "Serial hosts have no network login information",
        "store.identity_is_ai_key" => {
            "This credential is a ZeePilot endpoint API key and cannot be used for a host."
        }
        "store.key_auth_without_identity" => {
            "Host \"{label}\" uses private-key authentication but has no credential selected"
        }
        "store.identity_no_key_path" => "Credential \"{name}\" has no private key file path",
        "store.identity_no_key_content" => "Credential \"{name}\" has no private key content",
        "store.identity_not_key" => {
            "Credential \"{name}\" is not a private key credential and cannot be used for key authentication"
        }
        "store.identity_not_password" => {
            "Credential \"{name}\" is not a password credential and cannot be used for password authentication (its content would be sent to the server as the password)"
        }
        "store.host_name_empty" => "The host's name cannot be empty",
        "input.port_range" => "The port must be between 1 and 65535",
        "input.username_empty" => "The user name cannot be empty",
        "input.address_empty" => "The address cannot be empty",
        "input.address_too_long" => "The address is too long",
        "input.address_bad_whitespace" => {
            "The address cannot contain whitespace or control characters: only a domain name, IPv4 or IPv6 address is allowed"
        }
        "input.address_bad_char" => {
            "The address cannot contain '{ch}': only a domain name, IPv4 or IPv6 address is allowed"
        }
        "store.group_name_empty" => "The group name cannot be empty",
        "store.group_own_parent" => "A group cannot be its own parent",
        "store.group_move_into_child" => "A group cannot be moved under its own sub-group",
        "input.name_too_long_64" => "The name can be at most 64 characters",
        "store.forward_bind_bad" => "Invalid listen address: {value}",
        "store.forward_dest_bad" => "The target address cannot be empty or contain spaces",
        "store.forward_remote_port_taken" => {
            "This host already has a remote forward listening on port {port}"
        }
        "input.unknown_forward_kind" => "Unknown forward type: {value}",
        "input.unknown_auth_method" => "Unknown authentication method: {value}",
        "input.unknown_identity_kind" => "Unknown credential type: {value}",
        "input.unknown_proxy_strategy" => "Unknown proxy strategy: {value}",
        "input.unknown_proxy_kind" => "Unknown proxy type: {value}",
        "input.unknown_protocol" => "Unknown connection protocol: {value}",
        "input.unknown_endpoint_kind" => "Unknown endpoint type: {value}",
        "store.proxy_name_empty" => "The proxy name cannot be empty",
        "store.proxy_address_empty" => "The proxy address cannot be empty",
        "store.proxy_password_not_utf8" => "The decrypted proxy password is not UTF-8: {cause}",
        "quick.no_whitespace" => "The address cannot contain whitespace",
        "quick.ipv6_missing_bracket" => "The IPv6 address is missing the closing bracket ']'",
        "quick.after_bracket" => "Only a port number may follow ']', got '{after}'",
        "quick.ipv6_needs_bracket" => {
            "An IPv6 address must be wrapped in brackets, e.g. root@[::1]:2222"
        }
        "ai.endpoint_name_bad" => "The endpoint name cannot be empty or longer than 100 characters",
        "ai.endpoint_url_bad" => {
            "The endpoint URL must start with http:// or https:// and cannot contain spaces or control characters"
        }
        "ai.default_model_bad" => "The default model name is invalid",
        "ai.model_name_bad" => "A model name is invalid (too long or contains control characters)",
        "ai.models_too_many" => "Too many manually entered models (at most 200)",
        "ai.overrides_too_many" => "Too many manual capability overrides (at most 200)",
        "ai.override_model_bad" => "A manual capability override has an invalid model name",
        "ai.no_key_only_openai" => "Only OpenAI-compatible endpoints can omit the API key",
        "ai.preset_id_bad" => "The preset ID is invalid",
        "protocol.serial_no_identity" => "Serial hosts cannot use a credential",
        "protocol.serial_no_proxy" => "Serial hosts cannot use a proxy",
        "protocol.serial_no_jump" => "Serial hosts cannot use a jump host",
        "protocol.password_only" => {
            "A {protocol} host only supports password authentication (you can leave the credential empty and be asked when connecting)"
        }
        "protocol.credential_not_password" => {
            "Credential \"{name}\" is not a password credential; a {protocol} host can only use a password credential"
        }
        "protocol.terminal_type_bad" => {
            "The terminal type must be 1-{max} visible ASCII characters (for example {example})"
        }
        "protocol.auto_login_needs_patterns" => {
            "With auto-login on, both the user name prompt and the password prompt need at least one match rule"
        }
        "protocol.auto_login_needs_user" => "With auto-login on, the user name cannot be empty",
        "protocol.serial_device_empty" => "Enter the serial device name (for example COM3)",
        "protocol.serial_device_bad" => {
            "The serial device name can be at most {max} characters and cannot contain control characters"
        }
        "protocol.baud_range" => "The baud rate must be between {min} and {max}",
        "protocol.serial_five_two" => "5 data bits cannot be combined with 2 stop bits",
        "protocol.serial_one_half" => "1.5 stop bits can only be combined with 5 data bits",
        "protocol.username_patterns_too_many" => {
            "At most {max} user name prompt patterns are allowed"
        }
        "protocol.username_pattern_bad" => {
            "Each user name prompt pattern can be at most {max} characters and cannot contain control characters"
        }
        "protocol.password_patterns_too_many" => {
            "At most {max} password prompt patterns are allowed"
        }
        "protocol.password_pattern_bad" => {
            "Each password prompt pattern can be at most {max} characters and cannot contain control characters"
        }
        "protocol.x11_ssh_only" => {
            "X11 forwarding is only available for SSH hosts; a {protocol} host cannot enable it"
        }
        "protocol.no_telnet_options" => "A {protocol} host cannot carry Telnet options",
        "protocol.no_serial_options" => "A {protocol} host cannot carry serial options",
        "protocol.jump_must_be_ssh" => {
            "The jump host must be an SSH host, but a {protocol} host is selected"
        }
        "protocol.options_serialize" => "Failed to serialize the protocol options: {cause}",
        "store.identity_name_empty" => "The credential name cannot be empty",
        "store.keyfile_needs_path" => "A private key file credential needs a path",
        "store.identity_kind_change_needs_secret" => {
            "When changing the credential type, enter the password or private key again; the saved content is not carried over to the new type"
        }
        "store.this_identity_no_key_path" => "This credential has no private key file path",
        "store.private_key_file_unreadable" => "Cannot read the private key file {path}: {cause}",
        "store.identity_no_public_key" => {
            "This credential is not a private key, so it has no public key"
        }
        "store.identity_not_imported_key" => {
            "Credential \"{name}\" is not an imported private key and cannot store a passphrase"
        }
        "store.identity_no_key_content_yet" => {
            "This credential has no imported private key content yet; paste the private key before setting a passphrase"
        }
        "store.identity_secret_not_utf8" => "The decrypted credential is not UTF-8: {cause}",
        "store.order_duplicate_id" => "The order list contains id {id} twice",
        "store.order_hosts_same_group" => "Hosts can only be reordered within the same group",
        "store.order_groups_same_parent" => {
            "Groups can only be reordered under the same parent group"
        }
        "store.ssh_user_required" => {
            "An SSH host's user name cannot be empty; fill in the user name first"
        }
        "store.dpapi_protect_failed" => "CryptProtectData failed: {cause}",
        "store.dpapi_unprotect_failed" => "CryptUnprotectData failed: {cause}",
        "store.data_too_long" => "The data is too long: {len} bytes",
        "store.no_saved_password" => "This host has no saved password",
        "store.key_login_no_password" => {
            "This host uses key authentication, so there is no password to copy"
        }
        "store.agent_login_no_password" => {
            "This host logs in with the SSH agent, so there is no password to copy"
        }
        "hint.ipv6_egress" => {
            "The target is an IPv6 address, and the proxy or jump host may have no IPv6 egress: enter the target's IPv4 address or a domain name instead, or relay through a jump host that has IPv6"
        }
        "keygen.generate_failed" => "Failed to generate the key: {cause}",
        "keygen.export_public_failed" => "Failed to export the public key: {cause}",
        "keygen.encrypt_failed" => "Failed to encrypt the private key: {cause}",
        "keygen.export_private_failed" => "Failed to export the private key: {cause}",
        "ssh.private_key_unreadable" => {
            "Cannot read this private key (wrong passphrase, or not a supported private key format): {cause}"
        }
        "keygen.public_key_format_bad" => "The public key format is invalid",
        "keygen.install_not_confirmed" => {
            "The server did not confirm that the public key was installed (not a POSIX shell, or no permission to write ~/.ssh)"
        }
        "ssh.key_passphrase_still_wrong" => {
            "Still cannot open the private key with this passphrase: {cause}"
        }
        "ssh.agent_unreachable" => {
            "Could not connect to the OpenSSH agent named pipe or to Pageant"
        }
        "ssh.agent_list_failed" => "Failed to list the keys in the agent: {cause}",
        "ssh.answer_count_mismatch" => {
            "The number of answers ({answers}) does not match the number of prompts ({prompts})"
        }
        "forward.local_port_in_use" => "Local port {port} is already in use by another program",
        "forward.local_port_denied" => "No permission to listen on local port {port}",
        "forward.local_listen_failed" => "Failed to listen on {host}:{port}: {cause}",
        "ssh.connection_lost" => "The SSH connection was lost",
        "known_hosts.mkdir_failed" => "Failed to create the folder: {cause}",
        "known_hosts.open_failed" => "Failed to open known_hosts: {cause}",
        "known_hosts.write_failed" => "Failed to write known_hosts: {cause}",
        "known_hosts.flush_failed" => "Failed to flush known_hosts: {cause}",
        "known_hosts.read_failed" => "Failed to read known_hosts: {cause}",
        "known_hosts.unreadable" => {
            "Cannot read {path}: {cause}. Setup cannot confirm whether this host's fingerprint is registered or revoked, so the connection was refused; check this file's permissions and try again"
        }
        "ssh.jump_tcp_forwarding_prohibited" => {
            "The server prohibits TCP forwarding (administratively prohibited), so it cannot be used as a jump host"
        }
        "ssh.jump_cannot_reach_target" => {
            "The jump host cannot reach the target address (connect failed)"
        }
        "ssh.chain_empty" => "The connection chain needs at least one hop",
        "ssh.tcp_forwarding_prohibited" => {
            "The server prohibits TCP forwarding (AllowTcpForwarding no)"
        }
        "ssh.server_cannot_reach" => "The server cannot reach {host}:{port} (connect failed)",
        "ssh.remote_listen_denied" => {
            "The server refused to listen on {host}:{port} (the port is in use, or the server has forwarding turned off)"
        }
        "ssh.connection_closed" => "The connection is closed",
        "ssh.sftp_subsystem_timeout" => "Timed out waiting for the sftp subsystem's reply",
        "ssh.exec_failed" => "Failed to run the command: {cause}",
        "ssh.exec_nonzero" => "The command exited with a non-zero status: {stderr}",
        "ssh.exec_cancelled" => "Cancelled; the command was not sent",
        "ssh.remote_port_claimed" => {
            "A remote forward on this connection is already listening on port {port}"
        }
        "shell.pty_request_failed" => "Failed to request a PTY: {cause}",
        "shell.request_failed" => "Failed to request a shell: {cause}",
        "shell.refused" => "The server refused to open a shell",
        "shell.too_many_messages" => {
            "The server sent too many messages before replying (while waiting for the reply to the {what} request, more than {max})"
        }
        "shell.too_much_data" => {
            "The server sent too much data before replying (while waiting for the reply to the {what} request, more than {max} bytes)"
        }
        "shell.closed_while_opening" => {
            "The server closed the connection while the terminal was being opened (while waiting for the reply to the {what} request)"
        }
        "shell.reply_timeout" => {
            "Timed out waiting for the server's reply to the {what} request ({seconds}s)"
        }
        "shell.write_failed" => "Failed to write to the shell: {cause}",
        "shell.resize_failed" => "Failed to resize the window: {cause}",
        "shell.eof_failed" => "Failed to send EOF: {cause}",
        "shell.close_failed" => "Failed to close the shell: {cause}",
        "proxy.handshake_timeout" => "The handshake took longer than {seconds} seconds",
        "proxy.target_empty" => "The target host is empty",
        "proxy.target_bad_ipv6" => {
            "The target host \"{host}\" is not a valid IPv6 address, and SOCKS5 cannot carry an address with a zone id"
        }
        "proxy.target_bad_char" => {
            "The target host \"{host}\" contains whitespace or control characters and cannot be put into a CONNECT request"
        }
        "proxy.target_brackets" => "The brackets in the target host \"{host}\" are not paired",
        "proxy.socks_unreachable" => "Cannot reach the proxy server",
        "proxy.socks_password_rejected" => {
            "The proxy did not accept the user name or password (sub-negotiation returned {code})"
        }
        "proxy.socks_no_auth_method" => {
            "The proxy does not accept the authentication methods we offer"
        }
        "proxy.socks_auth_required" => "The proxy requires authentication",
        "proxy.socks_connection_refused" => "The proxy refused this target (connection refused)",
        "proxy.socks_host_unreachable" => "The proxy says the target host is unreachable",
        "proxy.socks_network_unreachable" => "The proxy says the network is unreachable",
        "proxy.socks_ruleset" => "The proxy's rules do not allow connecting to this target",
        "proxy.socks_command_unsupported" => "The proxy does not support CONNECT",
        "proxy.socks_address_type" => "The proxy does not support this address type",
        "proxy.other" => "{cause}",
        "proxy.http_connect_send_failed" => "Failed to send the CONNECT request: {cause}",
        "proxy.http_auth_bad" => {
            "The proxy requires authentication ({status}): the user name or password is wrong"
        }
        "proxy.http_auth_needed" => {
            "The proxy requires authentication ({status}): this proxy needs a user name and password"
        }
        "proxy.http_status" => "The proxy returned {status}",
        "proxy.http_head_too_long" => "The proxy's response header is too long",
        "proxy.http_read_failed" => "Failed to read the proxy's response: {cause}",
        "proxy.http_closed_empty" => "The proxy closed the connection without sending anything",
        "lock.on_start_needs_password" => {
            "Set a lock screen password first, then turn on \"Lock on start\""
        }
        "ai.preset_no_endpoint" => "The preset has no endpoint address",
        "ai.endpoint_address_rejected" => "The endpoint address is not acceptable: {cause}",
        "x11.display_bad_text" => {
            "The display address contains whitespace or control characters, or is too long"
        }
        "x11.display_unix_socket" => {
            "Windows has no Unix sockets; write an address like localhost:0"
        }
        "x11.display_format" => {
            "The display address {spec} is not valid; it should look like localhost:0 or :0"
        }
        "x11.display_number" => "The display number in the display address {spec} is not a number",
        "x11.screen_number" => "The screen number in the display address {spec} is not a number",
        "x11.port_range" => "The port in the display address {spec} is out of range",
        _ => return None,
    })
}

/// 把 key 对应的英文模板填上参数；认不得的 key、对不上的参数返回 `None`（调用方退回中文原文）。
/// 参数里有 `ipv6_hint` 的（目标是 IPv6 地址、代理 / 跳板机连不过去）再补一句出口提示。
pub(crate) fn sentence(key: &str, params: &[(&'static str, String)]) -> Option<String> {
    let mut text = crate::error_text::fill(english(key)?, params)?;
    if params
        .iter()
        .any(|(name, value)| *name == "ipv6_hint" && value == "1")
    {
        text.push_str(". ");
        text.push_str(english("hint.ipv6_egress")?);
    }
    Some(text)
}
