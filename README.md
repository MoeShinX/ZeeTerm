# ZeeTerm

ZeeTerm 是 Z-SSH 的 GPUI 原生版：面向 Windows 10/11（x64）的 SSH 客户端，集终端、SFTP 文件管理、远端编辑器和 AI 运维助手于一体。界面和终端完全用 Rust 原生实现（[GPUI](https://github.com/zed-industries/zed) + Alacritty 终端状态机），不依赖 Electron、WebView 或 Node.js 运行时。

它与 Z-SSH（Tauri + React 版）共用同一套核心库（SSH、SFTP、同步、AI 智能体驱动、本地存储），但是**独立的产品**：产品名、安装目录、卸载键、主程序名、默认数据目录都与 Z-SSH 不同，两者可以并存。

## 功能概览

以下只列当前代码里已经做出来、有自动化测试覆盖的部分。

- **终端**：多标签，分屏，可配置回滚行数，浅色 / 深色多套皮肤与终端配色，关键词高亮，标签可拖出成独立窗口或拖到别的窗口合并，中文输入法。
- **主机管理**：主机树（分组、标签、搜索、排序）、快速连接（`user@host:port`）、凭据库（密码、私钥、口令，生成密钥、一键安装公钥）、代理与跳板机、端口转发，连接前核对主机指纹。
- **SFTP**：主窗口里的文件栏，也可以开成独立文件窗口；复制 / 粘贴、拖放、跟随终端当前目录、传输队列与断点续传、压缩与解压。
- **编辑器**：远端文件直接在标签里编辑，语法高亮，保存时检查版本冲突并原子写回，异常退出后可恢复草稿。
- **AI 运维助手**：接入你自己安装并登录的 opencode、Codex、Claude Code；命令在你的终端里执行，可逐条确认、只读自动或全部放行；能读写远端文件；主机长期记忆与历史会话保存在本机。
- **同步与备份**：主机库、凭据、代理、快捷命令和设置端到端加密后同步到 WebDAV、S3 兼容存储、GitHub Gist 或 OneDrive；本地自动加密快照与离线备份文件。
- **服务器概览**：连上后显示 CPU、内存、磁盘、网络等状态。
- **打包**：按用户安装的 NSIS 安装包，不需要管理员权限，不带 WebView2。

### 隐私

没有账号、没有统计、没有崩溃上报；数据默认存在 `%APPDATA%\ZeeTerm`（可用环境变量 `ZSSH_DATA_DIR` 改到别处）。密码、私钥口令和同步令牌用 Windows 数据保护接口（DPAPI）加密。程序只连接你自己添加的服务器、你配置的代理和同步存储，以及你启用的智能体需要的地址。

当前是试用阶段：**自动更新是关着的**（编译期开关），新版本请重新下载安装包覆盖安装，数据不受影响。

## 构建

需要：

- Windows 10/11 x64
- Rust 工具链（MSVC）。`rust-toolchain.toml` 固定了版本，`rustup` 会自动按它安装
- Visual Studio 2022 C++ 生成工具
- 打安装包还需要 [NSIS](https://nsis.sourceforge.io/)（`makensis.exe` 在 `PATH` 里，或装在 `Program Files\NSIS`）

构建**不需要** CMake 和 NASM（`russh` 关掉了默认特性，用 `ring` 后端）。

```powershell
# 编译程序（产物 native\target\release\zssh-gpui.exe）
cargo build --manifest-path native/Cargo.toml --release --locked

# 全部检查：fmt、clippy、测试、release 构建，逐条报告退出码和耗时
powershell -File scripts/gates.ps1

# 打安装包（产物在 native\installer\out\）
pwsh native/installer/build.ps1
```

依赖测试服务器的用例只在设置了 `ZSSH_TEST_HOST`、`ZSSH_TEST_PORT`、`ZSSH_TEST_USER` 以及密码或 `ZSSH_TEST_KEY_PATH` 时运行，没设置会跳过并打印原因。

## 目录结构

| 目录 | 内容 |
| --- | --- |
| `crates/zssh-core` | 核心库：SSH / SFTP、AI 智能体驱动与工具桥、同步与备份、本地存储。不依赖任何界面框架 |
| `crates/zssh-app-runtime` | 应用运行层：任务登记 / 取消、类型化事件 |
| `crates/zssh-platform-windows` | Windows 平台能力：剪贴板、输入法、单实例、系统主题 |
| `crates/zssh-mcp` | AI 工具桥的 MCP 进程，随安装包一起分发 |
| `native/zssh-gpui` | ZeeTerm 应用本体（GPUI 界面） |
| `native/zssh-terminal`、`zssh-session`、`zssh-editor`、`zssh-regex`、`zssh-update` | 终端模型、会话泵、编辑器文件契约、高亮正则引擎、更新验签 |
| `native/installer` | NSIS 安装脚本、打包脚本和静默安装测试脚本 |
| `scripts` | `gates.ps1`（全部检查）、`privacy-scan.ps1` 与它的放行清单 `privacy-allow.txt`（公开前的隐私扫描） |

`crates/` 是一个 Cargo 工作区，`native/` 是另一个（它用路径依赖引用 `crates/` 里的库），各有自己的 `Cargo.lock`。

## 许可

GPL-3.0-or-later，全文见 `LICENSE`。项目移植和使用了其他开源项目的代码，署名与许可见 `NOTICE` 和 `native/NOTICE`（Netcatty 等）。
