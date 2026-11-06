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
        "vault_key_missing" => {
            "The database key is not in the keychain; existing encrypted data is kept as is"
        }
        "vault_key_cancelled" => "Keychain authorization was cancelled; the key was not accessed",
        "vault_key_locked" => "The keychain is locked; the key was not accessed",
        "vault_key_failed" => "Failed to access the keychain (status {status})",
        "vault_key_exists" => {
            "The database key already exists in the keychain and was not overwritten"
        }
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
        "cmd.nested_too_deep" => "The command is nested too deeply to confirm it is safe",
        "cmd.fork_bomb" => "A fork bomb pattern was detected",
        "cmd.unpaired_substitution" => {
            "The command has an unpaired construct ($() or backticks) and cannot be parsed"
        }
        "cmd.unpaired_quotes" => "The command has unpaired quotes and cannot be parsed",
        "cmd.sudoers_file" => "The command touches the sudo authorization files",
        "cmd.sysrq_trigger" => "It triggers SysRq and may crash the system outright",
        "cmd.find_delete" => "find -delete deletes the matched files directly",
        "cmd.find_exec" => "find -exec runs delete or overwrite style commands in bulk",
        "cmd.shred" => "shred irrecoverably overwrites file or device contents",
        "cmd.truncate" => "truncate cuts or rewrites file contents",
        "cmd.mkfs" => "It formats a filesystem",
        "cmd.write_block_device" => "It writes straight to a block device and may destroy data",
        "cmd.wipefs" => "It erases filesystem signatures",
        "cmd.partition_write" => "It writes the partition table",
        "cmd.power" => "A shutdown / reboot command",
        "cmd.runlevel" => "It switches to a power-off / reboot / single-user runlevel",
        "cmd.ssh_service" => "It affects the SSH service and may lock you out",
        "cmd.chmod_system" => "It changes the permissions of a system path and may lock you out",
        "cmd.chown_system" => "It changes the owner of a system path and may lock you out",
        "cmd.userdel" => "Deleting a system account breaks services or locks logins outright",
        "cmd.passwd_clear" => "It clears or locks a system account's password and may lock you out",
        "cmd.account_lock" => "It locks a system account and may lock you out",
        "cmd.visudo" => "It edits the sudo authorization files",
        "cmd.firewall_flush" => "It flushes the firewall rules",
        "cmd.firewall_default_deny" => {
            "It sets the firewall default policy to deny and may lock you out"
        }
        "cmd.firewall_disable" => "It turns the firewall off",
        "cmd.network_down" => "It brings a network interface down and may lock you out",
        "cmd.eval" => "eval parses dynamic content again",
        "cmd.kill_init" => "Killing init (PID 1) or all processes crashes the system",
        "cmd.killall5" => "killall5 kills every process and crashes the system",
        "cmd.kill_critical" => "It killed a critical system process and may lock you out",
        "cmd.crontab_remove" => "crontab -r wipes the current user's scheduled jobs",
        "cmd.kernel_panic" => {
            "It changes kernel panic / SysRq behavior and may crash the system outright"
        }
        "cmd.swapoff_all" => "Turning off all swap affects system stability",
        "cmd.umount_system" => "Unmounting a system path breaks the running system",
        "cmd.move_system" => "It moves a system path",
        "cmd.python_inline" => "Python runs inline code with -c, which cannot be analysed reliably",
        "cmd.perl_inline" => "Perl runs inline code with -e, which cannot be analysed reliably",
        "cmd.ruby_inline" => "Ruby runs inline code with -e, which cannot be analysed reliably",
        "cmd.node_inline" => "Node runs inline code with -e/-p, which cannot be analysed reliably",
        "cmd.php_inline" => "PHP runs inline code with -r, which cannot be analysed reliably",
        "cmd.lua_inline" => "Lua runs inline code with -e, which cannot be analysed reliably",
        "cmd.awk_system" => "awk calls system(), which cannot be analysed reliably",
        "cmd.interactive_shell" => {
            "It started an interactive shell or an unknown script, which should not happen in the tool bridge"
        }
        "cmd.rm_no_target" => {
            "Recursive forced delete with no target; the impact cannot be determined"
        }
        "cmd.rm_system" => "Recursive forced delete would hit system paths or wildcards",
        "cmd.privilege_no_command" => "The privilege command is not followed by a command to run",
        "cmd.su" => "su switches to another user, so the execution environment is not controlled",
        "cmd.redirect_block_device" => "The redirect writes to a block device and may destroy data",
        "cmd.redirect_system_file" => "The redirect overwrites files under a system path",
        "sftp.limited" => "Exceeded a server limit: {message}",
        "sftp.unexpected_packet" => "SFTP protocol error (received an unexpected reply)",
        "sftp.request_timeout" => {
            "The server did not respond for {secs} seconds (SFTP request timed out)"
        }
        "sftp.realpath_empty" => "realpath returned no path",
        "sftp.readlink_empty" => "readlink returned no target",
        "sftp.posix_rename_reply" => "posix-rename received an unexpected reply",
        "sftp.copy_data_reply" => "copy-data received an unexpected reply",
        "sftp.copy_peer_closed" => {
            "The connection holding the copy source has been closed; reopen that tab and try again"
        }
        "sftp.exists_not_dir" => "{path} already exists but is not a directory",
        "sftp.task_symlink" => "The symlink resolution task crashed: {cause}",
        "sftp.task_name_check" => "The file name check task crashed: {cause}",
        "sftp.task_write" => "The write task crashed: {cause}",
        "sftp.task_read" => "The read task crashed: {cause}",
        "sftp.task_local_read" => "The local read task failed to run: {cause}",
        "sftp.task_local_write" => "The local write task failed to run: {cause}",
        "sftp.task_local_finish" => "The local finishing task failed to run: {cause}",
        "sftp.task_local_sync" => "The local flush task failed to run: {cause}",
        "sftp.read_offset_mismatch" => {
            "Read back at the wrong offset: expected {expected}, got {got}"
        }
        "sftp.source_ended_early" => "The source file ended early (read {read} of {total})",
        "sftp.owner_would_change" => "The owner would change from {from} to {to}",
        "sftp.name_empty" => "The name cannot be empty",
        "sftp.name_dots" => "The name cannot be . or ..",
        "sftp.name_unusable" => "\"{name}\" cannot be used as a file name: {fixes}",
        "sftp.download_no_name" => {
            "{path} has no usable file name, so it cannot be downloaded locally"
        }
        "sftp.target_outside" => {
            "Target path {target} is not inside the chosen directory {root}; refused"
        }
        "sftp.copy_into_self" => {
            "Cannot copy folder {source} into itself or one of its subfolders (the target's real location is {real_target})"
        }
        "sftp.copy_unsure" => {
            "Cannot confirm the target is outside the source folder (could not resolve the real location of {path}: {cause}); nothing was copied"
        }
        "sftp.copy_too_deep" => {
            "Folders are nested more than {max} levels deep; copying stopped (a symlink or alias may form a loop): {path}"
        }
        "sftp.copy_target_is_dir" => {
            "The target is a folder and cannot be overwritten with a file: {target}"
        }
        "sftp.copy_merge_needs_policy" => {
            "The target folder already exists; decide how to handle same-name files before merging: {target}"
        }
        "sftp.copy_target_is_file" => {
            "The target is a file and cannot be overwritten with a folder: {target}"
        }
        "sftp.copy_size_mismatch" => {
            "The sizes differ after copying (source {expected} bytes, copy {bytes} bytes): {target}"
        }
        "sftp.copy_verify_too_deep" => {
            "Folders are nested more than {max} levels deep; cannot confirm the copy is complete: {dir}"
        }
        "sftp.copy_verify_too_many" => {
            "More than {max} entries; cannot confirm the copy is complete: {root}"
        }
        "sftp.replace_exists_check" => "Failed to check whether {to} exists: {cause}",
        "sftp.replace_rename" => "Rename failed: {cause}",
        "sftp.replace_aside_lost" => {
            "The connection had a problem while moving the original file aside: {cause} (the original may still be at {to}, or already at {backup})"
        }
        "sftp.replace_aside_failed" => {
            "Failed to move the original file to the backup name {backup}; the original is untouched: {cause}"
        }
        "sftp.replace_in_lost" => {
            "The connection had a problem while renaming the new content to {to}: {cause} (the original is at the backup name {backup}; the new content is at {from} or already at {to})"
        }
        "sftp.replace_in_restored" => {
            "Failed to rename the new content to {to}; the original file has been restored: {cause}"
        }
        "sftp.replace_in_restore_failed" => {
            "Failed to rename the new content to {to}: {cause}; restoring the original from {backup} also failed: {restore}"
        }
        "xfer.note_empty_dir" => "Empty directory {dir} will be created too",
        "xfer.note_skipped_link" => "Skipped symlink / directory junction: {path}",
        "xfer.note_file_count" => "{count} files in total",
        "xfer.note_skipped_symlink" => "Skipped symlink: {path}",
        "xfer.note_non_utf8" => {
            "The name of {path} is not UTF-8; the local name uses the lossy version {local}"
        }
        "xfer.note_renamed" => "{remote} → {local} ({fixes})",
        "xfer.dir_needs_recursive" => "This is a directory: either tick recursive or choose a file",
        "xfer.symlink_no_relay" => {
            "Symlinks cannot be copied through the relay (they would be copied as the content they point to, which differs from cp -a)"
        }
        "xfer.note_no_resume_point" => "{target} has no confirmed resume point; starting over",
        "xfer.note_cannot_truncate" => {
            "The temporary file of {target} cannot be truncated back to the resume point; starting over"
        }
        "xfer.note_resume_mismatch" => "The resume point of {target} does not match; starting over",
        "xfer.note_target_appeared" => {
            "{target} was created by another program during the transfer; re-confirming how to handle it"
        }
        "xfer.note_reconnect" => "Connection lost (retry {attempt}): {cause}",
        "xfer.note_skip_existing" => "Target already exists; skipping {path}",
        "xfer.note_cannot_resume" => {
            "{path} cannot be resumed (the target is not smaller than the source); overwriting instead"
        }
        "xfer.note_renamed_to" => "Target already exists; renamed to {renamed}",
        "sftp.wrapped" => "{cause}",
        "archive.name_taken" => {
            "The archive name for \"{base}\" is still taken after trying up to (1000); nothing was compressed"
        }
        "archive.nothing_selected" => "No entries were selected to compress",
        "archive.bad_name" => {
            "\"{name}\" is not a valid name in the current directory; nothing was compressed"
        }
        "archive.dir_not_absolute" => {
            "The current directory is not an absolute path; nothing was compressed"
        }
        "archive.compress_timeout" => {
            "Compression timed out without finishing (the server may still be compressing)"
        }
        "archive.no_output" => "Compression did not produce an archive file",
        "archive.output_missing" => {
            "The compression command finished, but the archive cannot be found: {cause}"
        }
        "archive.rename_failed" => {
            "The archive could not be renamed to its final name \"{name}\": {cause}"
        }
        "archive.unknown_format" => {
            "Unrecognized archive type (supported: .tar.gz .tgz .tar .tar.xz .txz .tar.bz2 .tbz2 .zip)"
        }
        "archive.dest_is_file" => {
            "The extraction target \"{name}\" already exists, but it is a file, not a folder"
        }
        "archive.path_not_absolute" => "The path is not absolute; nothing was extracted",
        "archive.read_failed" => "Cannot read the archive: {cause}",
        "archive.is_folder" => "This is a folder, not an archive",
        "archive.inspect_timeout" => "Checking the archive timed out",
        "archive.command_missing" => "The {command} command was not found on the server",
        "archive.list_failed_code" => "Cannot read the archive (exit code {code}): {stderr}",
        "archive.list_failed" => "Cannot read the archive: {stderr}",
        "archive.too_many_entries" => {
            "The archive has too many entries to check each one for safety; extraction was refused"
        }
        "archive.extract_timeout" => {
            "Extraction timed out without finishing (the server may still be extracting)"
        }
        "archive.extract_warning" => "Warnings during extraction",
        "archive.extract_warning_detail" => "Warnings during extraction: {tail}",
        "archive.extract_failed" => "Extraction failed (exit code {code}): {stderr}",
        "archive.extract_failed_no_code" => "Extraction failed (no exit code): {stderr}",
        "archive.extract_no_exit" => {
            "The extraction command returned no exit code, so it is unknown whether it succeeded"
        }
        "archive.compress_failed" => "Compression failed (exit code {code}): {stderr}",
        "archive.compress_no_exit" => {
            "The compression command returned no exit code, so it is unknown whether it succeeded"
        }
        "archive.files_changed" => {
            "Some files changed while being archived; the archive holds what was read at that moment"
        }
        "archive.changed_after_check" => {
            "The archive changed after it was checked; please start again"
        }
        "archive.dest_changed" => {
            "The target folder changed after it was checked; please start again"
        }
        "archive.extract_rename_failed" => {
            "Extraction finished, but the temporary folder could not be renamed to \"{name}\": {cause}"
        }
        "archive.exec_failed" => "Cannot run the command on the server: {cause}",
        "sftp.cp_exec_unavailable" => "Cannot run commands on the server: {cause}",
        "sftp.cp_timeout" => {
            "The copy on the server timed out without finishing (it may still be copying on the server)"
        }
        "sftp.cp_not_found" => "The cp command was not found on the server",
        "sftp.cp_failed" => "cp on the server failed (exit code {code}): {stderr}",
        "sftp.cp_no_exit" => {
            "cp on the server returned no exit code, so it is unknown whether the copy succeeded"
        }
        "sftp.cp_read_source" => "Cannot read the source: {cause}",
        "sftp.cp_stat_target" => "Cannot inspect the target: {cause}",
        "sftp.cp_target_is_dir_merge" => {
            "The target already exists and is a folder; merging needs copy-data or the local relay"
        }
        "sftp.cp_tree_size_mismatch" => {
            "The sizes differ after copying on the server (source {src_files} files / {src_bytes} bytes, copy {dst_files} files / {dst_bytes} bytes)"
        }
        "sftp.cp_verify_failed" => "Failed to verify the sizes: {cause}",
        "sftp.cp_file_size_mismatch" => {
            "The sizes differ after copying on the server (source {expected} bytes, copy {bytes} bytes)"
        }
        "sftp.cp_verify_copy_failed" => "Failed to verify the size of the copy: {cause}",
        "sftp.name_bad" => "The name cannot contain /, and cannot be . or ..",
        "sync.storage_cause" => "{cause}",
        "sync.pkg_base64" => "The package has a field that is not valid base64",
        "sync.pkg_kdf_range" => {
            "The key derivation parameters are out of range (memory_kib={memory}, iterations={iterations}, parallelism={parallelism})"
        }
        "sync.argon2_params" => "Invalid Argon2 parameters: {cause}",
        "sync.argon2_failed" => "Argon2 key derivation failed: {cause}",
        "sync.encrypt_failed" => "Encryption failed",
        "sync.random_unavailable" => "The system random number generator is unavailable: {cause}",
        "sync.pkg_not_encrypted" => "Not a valid encrypted package: {cause}",
        "sync.pkg_not_sync" => "Not a ZeeTerm sync package (format={format})",
        "sync.pkg_salt_nonce" => "The salt or nonce has the wrong length",
        "github.api_no_host" => "The GitHub API address has no host name",
        "github.api_extras" => {
            "The GitHub API address cannot contain a query string, fragment or credentials"
        }
        "github.token_empty" => "Enter a GitHub token",
        "github.token_bad_chars" => {
            "The token contains invalid characters (spaces, line breaks or invisible characters); copy it again"
        }
        "github.api_http" => {
            "The GitHub API address is http://, so the token would be sent in plain text; confirm first to use it"
        }
        "github.gist_id_empty" => "Enter a Gist ID, or let ZeeTerm create one",
        "github.gist_id_bad_long" => {
            "The Gist ID is invalid (letters and digits only; copy the last part of the Gist address)"
        }
        "github.no_raw_url" => "The content was truncated, but GitHub did not give a raw_url",
        "github.raw_url_bad" => "The raw_url from GitHub is invalid",
        "github.raw_url_not_http" => "The raw_url from GitHub is not an http(s) address",
        "github.no_version" => "GitHub did not return the Gist's version information",
        "github.no_new_gist_id" => "GitHub did not return the ID of the new Gist",
        "github.gist_id_bad" => "The Gist ID is invalid",
        "github.no_user" => "GitHub did not return user information",
        "github.not_utf8" => "The sync package is not UTF-8 text",
        "github.bad_gist" => "The Gist returned by GitHub has the wrong format",
        "github.bad_json" => "The content returned by GitHub is not valid JSON",
        "github.gist_not_found" => {
            "Cannot find this Gist: the ID is wrong, or this token cannot see it"
        }
        "github.raw_404" => "Failed to read the full content: raw_url returned 404",
        "github.user_not_found" => "GitHub cannot find this user (is the API address right?)",
        "github.token_expired" => {
            "The GitHub token is no longer valid; enter it again (it may have expired, been revoked or been typed wrong)"
        }
        "github.write_denied" => {
            "Write failed: this Gist does not belong to the token's user, or the token has no Gist write permission"
        }
        "github.create_denied" => {
            "Create failed: the token has no permission to create Gists (classic tokens need the gist scope; fine-grained tokens need Gists read/write)"
        }
        "github.delete_failed" => {
            "Delete failed: this Gist was not found, or it does not belong to the token's user"
        }
        "github.forbidden" => {
            "GitHub refused the request: the token has no Gist permission (classic tokens need the gist scope; fine-grained tokens need Gists read/write), or the token's organization restricts it"
        }
        "github.rejected_422" => "GitHub rejected the request content (422): {message}",
        "github.status" => "GitHub returned {code}",
        "github.api_url_bad" => "The GitHub API address is invalid: {cause}",
        "github.api_scheme" => "The GitHub API address only supports http(s), got {scheme}://",
        "github.gist_too_big" => {
            "The sync file in the Gist is too large (over {mib} MiB); it does not look like a ZeeTerm sync package and was not downloaded"
        }
        "github.incomplete" => {
            "The content read is incomplete (expected {size} bytes, got {got} bytes); try again later"
        }
        "github.too_many_files" => {
            "This Gist has too many files (the API only returns the first 300), so the sync file cannot be found; use another Gist"
        }
        "net.proxy_auth" => {
            "The proxy requires authentication, or the proxy user name / password is wrong (check Connections → Default proxy)"
        }
        "net.server_returned" => "The server returned {code}",
        "net.server_returned_code" => "The server returned {status} ({code})",
        "net.connect_failed" => {
            "Cannot reach the storage service (the network is down or the proxy is unavailable): {cause}"
        }
        "net.request_failed" => "Request failed: {cause}",
        "net.proxy_invalid" => "Invalid proxy settings: {cause}",
        "net.client_build" => "Failed to create the HTTP client: {cause}",
        "s3.endpoint_bad" => "The S3 Endpoint is invalid: {cause}",
        "s3.endpoint_scheme" => "The S3 Endpoint only supports http(s), got {scheme}://",
        "s3.endpoint_no_host" => "The S3 Endpoint has no host name",
        "s3.bucket_bad" => "The bucket name is invalid",
        "s3.url_failed" => "Failed to build the S3 address: {cause}",
        "s3.region_empty" => "The Region cannot be empty",
        "s3.keys_empty" => "The Access Key and Secret cannot be empty",
        "s3.no_bucket_read" => "S3 read failed: the bucket does not exist",
        "s3.failed" => "S3 {action} failed: {reason}",
        "s3.signature" => {
            "The signature is wrong (the Secret, Region or address style may be wrong)"
        }
        "s3.no_access_key" => "The Access Key does not exist",
        "s3.no_bucket" => "The bucket does not exist",
        "s3.denied" => {
            "Permission denied (this is also returned when the object does not exist and the bucket gives no list permission)"
        }
        "s3.region_wrong" => "The Region or Endpoint is wrong",
        "s3.auth_failed" => "Authentication failed",
        "webdav.url_bad" => "The WebDAV address is invalid: {cause}",
        "webdav.http" => {
            "The WebDAV address is http://, so the password would be sent in plain text; confirm first to use it"
        }
        "webdav.scheme" => "The WebDAV address only supports http(s), got {scheme}://",
        "webdav.no_host" => "The WebDAV address has no host name",
        "webdav.url_invalid" => "The WebDAV address is invalid",
        "webdav.failed" => "WebDAV {action} failed: {reason}",
        "webdav.unauthorized" => "Wrong user name or password",
        "webdav.forbidden" => "Permission denied",
        "webdav.not_found" => "The address does not exist",
        "webdav.method_not_allowed" => {
            "The server does not allow this operation (the address may not be WebDAV)"
        }
        "webdav.insufficient" => "The server is out of space",
        "webdav.locked" => "The file is locked by the server",
        "onedrive.denied" => {
            "OneDrive refused the request (no permission). The authorization may have been revoked; sign in again"
        }
        "onedrive.not_found" => "OneDrive {action} failed: the app folder or file was not found",
        "onedrive.locked" => "OneDrive {action} failed: the file is locked; try again later",
        "onedrive.full" => "OneDrive is out of space (507); free up OneDrive space and try again",
        "onedrive.status" => "OneDrive returned {code}",
        "onedrive.failed_code" => "OneDrive {action} failed: the server returned {status}{detail}",
        "onedrive.too_big" => {
            "The sync file on OneDrive is over the 64 MiB limit; it does not look like a ZeeTerm sync package and was not downloaded"
        }
        "onedrive.empty_package" => "The sync package is empty; nothing was uploaded",
        "onedrive.reauth" => "OneDrive sign-in is no longer valid; please sign in again",
        "onedrive.no_refresh" => {
            "Microsoft did not return a refresh token (the offline_access permission is required), so the sign-in cannot be kept long term"
        }
        "onedrive.no_access_token" => "The Microsoft response has no access token",
        "onedrive.rejected" => "Microsoft rejected the request: {detail}",
        "onedrive.no_upload_url" => "OneDrive did not return an upload address",
        "onedrive.upload_url_bad" => "The upload address returned by OneDrive is invalid",
        "onedrive.upload_not_https" => "The upload address returned by OneDrive is not https",
        "backup.too_many" => "Too many backups were made within the same second",
        "backup.path_bad" => "The backup path is invalid",
        "backup.not_backup" => "This is not a ZeeTerm backup file",
        "backup.outside_dir" => "The backup file is not in the backup folder",
        "target.id_bad" => "The backup target ID is invalid",
        "target.exists" => "This backup target already exists",
        "target.none" => "There is no such backup target",
        "target.no_backup_yet" => "There is no backup on this backup target yet",
        "conflict.encoding" => "The conflict record's encoding is damaged",
        "conflict.not_utf8" => "The conflict record is not UTF-8",
        "conflict.gone" => "This conflict no longer exists",
        "conflict.no_other" => "This conflict has no other version to use",
        "conflict.host_gone" => "This host no longer exists",
        "conflict.group_gone" => "This group no longer exists",
        "conflict.identity_gone" => "This credential no longer exists",
        "conflict.proxy_gone" => "This proxy no longer exists",
        "conflict.endpoint_gone" => "This ZeePilot endpoint no longer exists",
        "service.settings_damaged" => "The sync settings saved on this computer are damaged",
        "service.password_damaged" => "The master password saved on this computer is damaged",
        "service.no_cloud_data" => "There is no sync data in the cloud yet; sync once first",
        "service.cloud_changed" => {
            "The cloud was just changed by another computer; sync once before changing the master password"
        }
        "target.login_first" => "Sign in to OneDrive first, then save this backup target",
        "target.max" => "At most {max} cloud backup targets are allowed",
        "service.master_short" => "The master password must be at least {min} characters",
        "service.new_master_short" => "The new master password must be at least {min} characters",
        "backup.password_short" => "The backup password must be at least {min} characters",
        "target.timeout" => {
            "It did not finish within {minutes} minutes, so this attempt was abandoned"
        }
        "x11.send_failed" => "Failed to send the request: {cause}",
        "x11.no_free_display" => "No free display number was found",
        "x11.random_unavailable" => "The system random number generator is unavailable",
        "x11.write_auth_failed" => "Failed to write the authorization file: {cause}",
        "x11.vcxsrv_exited" => "VcXsrv exited right after starting",
        "x11.vcxsrv_not_listening" => "VcXsrv did not start listening within 10 seconds",
        "x11.non_ascii_path" => {
            "The data folder path contains non-ASCII characters, so VcXsrv cannot read the authorization file and the system gave no short path name. Point ZSSH_DATA_DIR at an ASCII-only path, or choose a custom display in settings"
        }
        "x11.custom_display_bad" => {
            "The display address specified in settings is unusable: {cause}"
        }
        "x11.prepare_task_failed" => {
            "The task that prepares the local X server ended abnormally: {cause}"
        }
        "x11.max_registrations" => {
            "This connection already has {max} terminals using X11 forwarding"
        }
        "x11.cookie_unique_failed" => "Failed to generate a unique random cookie",
        "x11.wait_failed" => "{cause}",
        "conntest.jump_forward" => "Jump host forwarding failed: {reason}",
        "conntest.labeled" => "{label}: {inner}",
        "conntest.jump_timeout" => {
            "Connecting to {host} through the jump host got no response within {seconds} seconds"
        }
        "conntest.wrapped" => "{cause}",
        "conntest.proxy_failed" => "Proxy failed ({proxy}): {detail}",
        "conntest.proxy_dns" => "Proxy failed: cannot resolve the proxy address {host}",
        "conntest.proxy_timeout" => {
            "Proxy failed: connecting to the proxy {host}:{port} got no response within {seconds} seconds"
        }
        "conntest.proxy_connect" => {
            "Proxy failed: cannot connect to the proxy {host}:{port} ({io_reason})"
        }
        "conntest.dns" => {
            "Name resolution failed: {host} (check that the address is right and the local DNS works)"
        }
        "conntest.timeout" => {
            "Connection timed out: {host}:{port} did not respond within {seconds} seconds"
        }
        "conntest.refused" => {
            "Connection refused: nothing is listening on {host}:{port}, or a firewall rejected it"
        }
        "conntest.connect_failed" => "Cannot connect to {host}:{port} ({io_reason})",
        "conntest.connect_other" => "Cannot establish the connection: {cause}",
        "conntest.key_changed" => {
            "The host key does not match the saved one: saved {stored}, received {presented} (recorded in {stored_in})"
        }
        "conntest.key_revoked" => {
            "The host key is marked as revoked (@revoked): recorded {stored}, received {presented}"
        }
        "conntest.key_rejected" => "The host key was rejected (fingerprint {fingerprint})",
        "conntest.key_rekey" => {
            "The host key changed during the handshake: first {first}, now {again}"
        }
        "conntest.auth_failed" => "Authentication failed: wrong user name or password",
        "conntest.auth_all" => {
            "Authentication failed: tried {tried}; the server also supports {remaining}"
        }
        "conntest.auth_timeout" => "Authentication timed out (at most {seconds}s per step)",
        "conntest.key_load" => "Failed to read the private key: {detail}",
        "conntest.agent" => "ssh-agent is unavailable: {detail}",
        "conntest.rsa_sha1" => {
            "The server only accepts SHA-1 ssh-rsa signatures: use an Ed25519 / ECDSA key, or turn on legacy algorithm support"
        }
        "conntest.need_password" => {
            "Authentication failed: a password is needed but none was entered (Test Connection does not prompt)"
        }
        "conntest.need_passphrase" => {
            "Authentication failed: the private key has a passphrase but none is available (Test Connection does not prompt)"
        }
        "conntest.need_interactive" => {
            "The server asks for interactive verification (for example a one-time code), which Test Connection cannot answer for you; the server itself is reachable"
        }
        "conntest.cancelled" => "Cancelled",
        "conntest.no_algo" => {
            "Cannot negotiate a common {kind} algorithm with the server (for older servers, try legacy algorithm support)"
        }
        "conntest.handshake_timeout" => "The SSH handshake did not finish within {seconds} seconds",
        "conntest.disconnected" => {
            "The server disconnected during the handshake (the service on this address / port may not be SSH)"
        }
        "conntest.handshake_other" => "SSH handshake failed: {cause}",
        "conntest.ok" => "Connected in {elapsed} ms",
        "conntest.ok_version" => "Connected in {elapsed} ms, server {version}",
        "conntest.untrusted" => {
            "The host fingerprint {fingerprint} is not trusted yet; no login was tested. Connect once normally and verify the fingerprint first"
        }
        "conntest.untrusted_other" => {
            "This host only has keys of other types saved, and the fingerprint {fingerprint} is not trusted yet; no login was tested. Connect once normally and verify the fingerprint first"
        }
        "conntest.untrusted_hop" => {
            "{label}: the host fingerprint {fingerprint} is not trusted yet; no login was tested. Connect once normally and verify the fingerprint first"
        }
        "conntest.untrusted_other_hop" => {
            "{label}: this host only has keys of other types saved, and the fingerprint {fingerprint} is not trusted yet; no login was tested. Connect once normally and verify the fingerprint first"
        }
        "conntest.tcp_data" => {
            "Connected in {elapsed} ms (TCP is reachable, no login attempted; the peer responded)"
        }
        "conntest.tcp_silent" => {
            "Connected in {elapsed} ms (TCP is reachable, no login attempted; the peer sent nothing for 2 seconds, which is common for Telnet devices)"
        }
        "conntest.tcp_closed" => {
            "Connected in {elapsed} ms (TCP is reachable, no login attempted; but the peer closed the connection right away)"
        }
        "conntest.serial_ok" => {
            "Serial port {device} can be opened (closed again right away), took {elapsed} ms"
        }
        "conntest.cancelled_test" => "Test cancelled",
        "conntest.timeout_stage" => {
            "The connection test did not finish within {seconds} seconds (stuck at: {stage})"
        }
        "conntest.no_target" => "There is no target to connect to",
        "conntest.no_jump" => "There is no jump host",
        "conntest.reset_after" => "The connection was reset by the peer right after connecting",
        "conntest.read_failed" => "Reading failed after connecting: {cause}",
        "sync.record_no_uid" => "The {entity} record has no uid",
        "sync.host_protocol_unknown" => {
            "Host \"{name}\" uses protocol \"{protocol}\", which this version does not recognize; please update ZeeTerm first"
        }
        "sync.host_options_bad" => "The protocol options of host \"{name}\" have the wrong format",
        "sync.endpoint_kind_unknown" => {
            "ZeePilot endpoint \"{name}\" uses type \"{kind}\", which this version does not recognize; please update ZeeTerm first"
        }
        "sync.endpoint_options_bad" => {
            "The options of ZeePilot endpoint \"{name}\" have the wrong format"
        }
        "sync.endpoint_invalid" => {
            "The name or address of ZeePilot endpoint \"{name}\" is not valid"
        }
        _ => return None,
    })
}

/// 把 key 对应的英文模板填上参数；认不得的 key、对不上的参数返回 `None`（调用方退回中文原文）。
/// 参数里有 `ipv6_hint` 的（目标是 IPv6 地址、代理 / 跳板机连不过去）再补一句出口提示。
pub(crate) fn sentence(key: &str, params: &[(&'static str, String)]) -> Option<String> {
    // `fixes`（本地名字被改的原因们）核心给的是稳定代码，先换成英文说法再填模板。
    let mut expanded;
    let params = if params.iter().any(|(name, _)| {
        matches!(
            *name,
            "fixes" | "action" | "stage" | "io_reason" | "source" | "tried_codes" | "entity_code"
        )
    }) {
        expanded = params.to_vec();
        let agent = expanded
            .iter()
            .find(|(name, _)| *name == "agent")
            .map(|(_, value)| value.clone());
        let mut extra: Vec<(&'static str, String)> = Vec::new();
        for (name, value) in &mut expanded {
            match *name {
                "fixes" => *value = name_fixes_en(value),
                "action" => *value = action_en(value),
                "stage" => *value = stage_en(value),
                "entity_code" => extra.push((
                    "entity",
                    match value.as_str() {
                        "group" => "group",
                        "host" => "host",
                        "credential" => "credential",
                        "proxy" => "proxy",
                        _ => "ZeePilot endpoint",
                    }
                    .to_owned(),
                )),
                "io_reason" => *value = io_reason_en(value),
                "tried_codes" => extra.push((
                    "tried",
                    crate::error_text::tried_methods_en(value, agent.as_deref()),
                )),
                _ => {}
            }
        }
        if let Some((_, source)) = expanded.iter().find(|(name, _)| *name == "source") {
            let path = expanded
                .iter()
                .find(|(name, _)| *name == "source_path")
                .map(|(_, value)| value.as_str())
                .unwrap_or_default();
            extra.push((
                "stored_in",
                if source == "system" {
                    format!("the system file {path} (read-only for us)")
                } else {
                    "ZeeTerm's own known_hosts".to_owned()
                },
            ));
        }
        if let Some((_, codes)) = expanded.iter().find(|(name, _)| *name == "remaining_codes") {
            extra.push(("remaining", crate::error_text::offered_methods_en(codes)));
        }
        // 展开出来的值要盖掉同名的原参数（`fill` 取第一个同名的）。
        for (name, value) in extra {
            match expanded.iter_mut().find(|(key, _)| *key == name) {
                Some(slot) => slot.1 = value,
                None => expanded.push((name, value)),
            }
        }
        expanded.as_slice()
    } else {
        params
    };
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

/// 本地名字被改的原因们（`NameFix::codes` 的稳定代码，`\u{1f}` 分隔）→ 英文。
fn name_fixes_en(codes: &str) -> String {
    codes
        .split('\u{1f}')
        .map(|code| match code.split_once(':') {
            Some(("illegal", label)) => format!("illegal character {label} replaced with _"),
            Some(("reserved", name)) => {
                format!("{name} is a reserved Windows name, so a prefix was added")
            }
            Some(("clash_batch", candidate)) => format!(
                "same name as another entry in this batch (differs only by case), renamed to {candidate}"
            ),
            Some(("clash_file", candidate)) => {
                format!("a file with the same name already exists locally, renamed to {candidate}")
            }
            Some(("clash_dir", candidate)) => {
                format!("a folder with the same name already exists locally, renamed to {candidate}")
            }
            _ => match code {
                "trailing" => "trailing dots or spaces were removed".to_owned(),
                "empty" => "the name became empty, so _ is used".to_owned(),
                other => other.to_owned(),
            },
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// 存储操作的稳定代码（`net::action_code`）→ 英文动词。
fn action_en(code: &str) -> String {
    match code {
        "read" => "read",
        "write" => "write",
        "delete" => "delete",
        "mkcol" => "create directory",
        "upload" => "upload",
        "download" => "download",
        "session" => "create upload session",
        other => other,
    }
    .to_owned()
}

/// 连接测试的阶段代码（`TestStage::code`）→ 英文阶段名。
fn stage_en(code: &str) -> String {
    match code {
        "resolve" => "name resolution",
        "connect" => "connecting",
        "proxy" => "connecting through the proxy",
        "jump" => "the jump host",
        "handshake" => "SSH handshake",
        "host_key" => "host key check",
        "auth" => "authentication",
        "open" => "opening the serial port",
        other => other,
    }
    .to_owned()
}

/// 网络层原因：`kind:<名>` 是核心给的稳定代码，其余是系统错误原文（原样保留）。
fn io_reason_en(value: &str) -> String {
    match value {
        "kind:refused" => "connection refused",
        "kind:timed_out" => "timed out",
        "kind:host_unreachable" => "host unreachable",
        "kind:network_unreachable" => "network unreachable",
        "kind:reset" => "connection reset",
        other => other,
    }
    .to_owned()
}
