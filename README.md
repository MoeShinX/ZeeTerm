<p align="center">
  <img src="native/zssh-gpui/assets/brand/zeeterm-logo.svg" width="88" height="88" alt="ZeeTerm logo" />
</p>

<h1 align="center">ZeeTerm</h1>

<p align="center">
  <strong>A native SSH workspace. Built with Rust and GPUI.</strong><br />
  Terminals, file transfers, remote editing, and AI assistance in one desktop application.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-native-DEA584?style=flat-square" alt="Built with Rust" />
  <img src="https://img.shields.io/badge/UI-GPUI-8B5CF6?style=flat-square" alt="GPUI native UI" />
  <img src="https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-0078D4?style=flat-square" alt="Windows 10 and 11 x64" />
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPL--3.0--or--later-22C55E?style=flat-square" alt="GPL-3.0-or-later license" /></a>
</p>

<p align="center">
  <a href="https://github.com/MoeShinX/ZeeTerm/releases/latest"><strong>Download</strong></a> ·
  <a href="README.zh-CN.md">简体中文</a> ·
  <a href="https://t.me/ZeeTerm">Telegram</a> ·
  <a href="https://zeeterm.app/">Website · coming soon</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

## ⚡ Rebuilt for native rendering

ZeeTerm is the **GPUI native rebuild of [Z-SSH](https://github.com/MoeShinX/Z-SSH)**. Its desktop interface and terminal rendering are implemented in Rust, with GPUI handling the UI and the Alacritty terminal engine maintaining terminal state.

**No Electron. No WebView2. No embedded browser UI.** The desktop application does not require a Node.js runtime. It retains the shared SSH, SFTP, storage, and synchronization core while replacing the Tauri + React frontend with native rendering.

ZeeTerm installs separately from Z-SSH, uses its own data directory, and can coexist with it.

![ZeeTerm workspace with example sessions](docs/images/workspace.png)

*Workspace preview using fictional connection data.*

## 🧰 One workspace for remote operations

| Capability | What you can do |
| :--- | :--- |
| **Terminal workspace** | Work across tabs and split panes, detach tabs into separate windows, customize scrollback, and use themes and keyword highlighting. |
| **Sessions & authentication** | Organize connections in a searchable session tree. Use passwords, SSH keys, or an SSH agent; connect through proxies and jump hosts; configure port forwarding. |
| **SFTP file management** | Browse local and remote files, drag to transfer, follow the terminal directory, and manage queued transfers with resume support. |
| **Remote editing** | Edit remote files in tabs with syntax highlighting, save conflict checks, and recoverable drafts. |
| **AI assistance** | Use the built-in **ZeePilot** assistant or integrate an installed OpenCode, Codex, or Claude Code. Select per-command approval, automatic read-only mode, or full access. |
| **Encrypted sync & backup** | Sync through WebDAV, S3-compatible storage, GitHub Gist, or OneDrive. Keep encrypted local backups and additional cloud backup targets. |
| **Server overview** | Inspect CPU, memory, disk, and network information alongside your terminal. |
| **App lock** | Lock the workspace manually or after inactivity while sessions and transfers continue in the background. |

SSH is the primary workflow; **Telnet and serial connections** are also available. External AI tools are optional and have their own installation, account, and runtime requirements.

## Get started

**Supported platform: Windows 10 / 11, x64.**

1. Download `ZeeTerm-Setup-<version>.exe` from [GitHub Releases](https://github.com/MoeShinX/ZeeTerm/releases/latest).
2. Run the installer. Installation is per user and does not require administrator privileges.
3. Add a session, choose an authentication method, and connect. For SSH, review the server fingerprint before trusting a new host.

Data is stored in `%APPDATA%\ZeeTerm` by default. Set `ZSSH_DATA_DIR` to use a different directory.

**Coming from Z-SSH?** Use the built-in import flow to preview your data and create a backup before applying changes.

## 🔐 Security & data ownership

- **Local storage by default.** Sessions and application data stay on your machine; you choose whether to configure cloud synchronization or AI services.
- **Protected saved credentials.** Passwords, stored private keys, passphrases, and synchronization secrets use Windows DPAPI, bound to the Windows user context.
- **Encrypted synchronization.** Sync packages use **Argon2id + XChaCha20-Poly1305** with a master password. Use encrypted export or sync when moving credentials between machines.
- **SSH host-key verification.** Review unknown hosts and changed fingerprints before connecting.
- **Verified updates.** The updater validates the release signature and signed version before running the installer.

The app lock protects an unattended workspace; it does not replace operating-system security. AI services receive the context needed for the operations you enable, so choose their permissions and provider accordingly.

## Under the hood

| Layer | Implementation |
| :--- | :--- |
| Desktop UI | Rust · GPUI · GPUI Kit |
| Terminal state | Alacritty terminal engine |
| SSH / SFTP | `russh` · `russh-sftp` |
| Async runtime | Tokio |
| Local storage | SQLite · Windows DPAPI |
| Encrypted sync | Argon2id · XChaCha20-Poly1305 |
| Distribution | NSIS installer · minisign update verification |

The repository uses two Cargo workspaces:

- **`crates/`** — Shared application core, runtime, Windows integration, and MCP tool bridge.
- **`native/`** — GPUI application, terminal and session components, editor, highlighting, updater, and installer.

## Build from source

Requirements: **Windows 10/11 x64**, the Rust MSVC toolchain pinned in [rust-toolchain.toml](rust-toolchain.toml), and **Visual Studio 2022 C++ Build Tools** with a Windows SDK. Building the installer also requires [NSIS](https://nsis.sourceforge.io/). The project uses the `ring` SSH backend; CMake and NASM are not required.

```powershell
git clone https://github.com/MoeShinX/ZeeTerm.git
cd ZeeTerm

# Build the native application
cargo build --manifest-path native/Cargo.toml --release --locked

# Run formatting, lint, test, and gate-profile checks
powershell -File scripts/gates.ps1

# Build the Windows installer (PowerShell 7)
pwsh -File native/installer/build.ps1
```

The application binary is written to `native/target/release/zssh-gpui.exe`; installers are written to `native/installer/out/`.

See [AGENTS.md](AGENTS.md) for development conventions and isolated test-data requirements, and [docs/RELEASING.md](docs/RELEASING.md) for release and signing instructions.

## 💬 Community & project links

- **[Telegram · ZeeTerm](https://t.me/ZeeTerm)** — Questions, feedback, and discussion.
- **[zeeterm.app](https://zeeterm.app/)** — The project website is planned; development is coming later. GitHub remains the source for downloads and project documentation.
- **[GitHub Issues](https://github.com/MoeShinX/ZeeTerm/issues)** — Bug reports and feature requests. Include the app version, reproduction steps, and sanitized screenshots or logs.
- **[Changelog](CHANGELOG.md)** — Release history and recent changes.

## License & acknowledgements

ZeeTerm is open source under **[GPL-3.0-or-later](LICENSE)**. Third-party components, adapted code, and acknowledgements are documented in [NOTICE](NOTICE) and [native/NOTICE](native/NOTICE).
