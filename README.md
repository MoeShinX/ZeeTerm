# ZeeTerm

**A native SSH workspace for Windows, built with Rust and GPUI.**

Terminals, SFTP, remote editing, and AI assistance — together in one desktop app.

[Download for Windows](https://github.com/MoeShinX/ZeeTerm/releases/latest) · [简体中文](README.zh-CN.md) · [Changelog](CHANGELOG.md) · [Report an issue](https://github.com/MoeShinX/ZeeTerm/issues)

---

ZeeTerm is the native GPUI edition of [Z-SSH](https://github.com/MoeShinX/Z-SSH). It shares its core functionality but installs separately, so the two applications can coexist. ZeeTerm runs without an Electron, WebView2, or Node.js runtime.

![ZeeTerm workspace showing example connections](docs/images/workspace.png)

*Example workspace with fictional connection data.*

## A workspace for remote work

- **Terminal** — Tabs, split panes, detachable windows, configurable scrollback, themes, keyword highlighting, and Chinese IME support.
- **Connections** — Organize hosts in groups, search and connect quickly, manage credentials and SSH keys, and use proxies, jump hosts, and port forwarding. Verify host fingerprints before connecting.
- **SFTP** — Browse local and remote files, transfer by drag and drop, follow the terminal directory, and manage a transfer queue with resume support.
- **Remote editor** — Edit files in a tab with syntax highlighting, save conflict checks, and recoverable drafts.
- **AI assistance** — Use ZeePilot or your own installed OpenCode, Codex, or Claude Code. Choose between per-command approval, automatic read-only mode, and full access.
- **Sync and backup** — Encrypt and sync your data using WebDAV, S3-compatible storage, GitHub Gist, or OneDrive; create encrypted local and cloud backups.
- **Server overview and app lock** — View server resource information and lock the app while sessions continue in the background.

## Install

Download `ZeeTerm-Setup-<version>.exe` from [GitHub Releases](https://github.com/MoeShinX/ZeeTerm/releases). ZeeTerm supports **Windows 10/11 x64** and installs for the current user without administrator rights. Your data is stored separately in `%APPDATA%\ZeeTerm`.

For existing Z-SSH users, the app offers an import flow with a preview and backup before applying changes. See the [Chinese guide](README.zh-CN.md) for more detail.

## Privacy and security

ZeeTerm stores its data locally by default. Saved passwords, private-key passphrases, and sync tokens are protected using Windows DPAPI. Host-key verification helps you review a server identity before connecting. Encrypted sync and backups use a master password.

AI operations may access remote systems according to the execution mode you select. The app lock helps protect against someone using an unattended screen; it is not a defense against malware running as the same Windows user.

## Build from source

Requirements: Windows 10/11 x64, the Rust MSVC toolchain specified in `rust-toolchain.toml`, and Visual Studio 2022 C++ Build Tools. Building the installer also requires NSIS. CMake and NASM are not required.

```powershell
# Build the native application
cargo build --manifest-path native/Cargo.toml --release --locked

# Run project checks
powershell -File scripts/gates.ps1

# Build the installer
pwsh native/installer/build.ps1
```

The repository has two Cargo workspaces: shared libraries in `crates/` and the native application in `native/`. Release and signing instructions are in [docs/RELEASING.md](docs/RELEASING.md).

## License and credits

ZeeTerm is licensed under [GPL-3.0-or-later](LICENSE). Third-party code and acknowledgements are documented in [NOTICE](NOTICE) and [native/NOTICE](native/NOTICE).
