# ZeeTerm 开发约定

ZeeTerm 是 Z-SSH 的 GPUI 原生版：只面向 Windows 10/11 x64 的 SSH 客户端（终端、SFTP、远端编辑器、AI 运维助手），全部用 Rust 写，界面是 GPUI。本仓库只含原生版需要的部分：`crates/`（共用核心库）、`native/`（应用本体与安装包）、`scripts/`。

## 工作方式

- 任务以任务卡的形式交给你，卡里没写的功能不要加；觉得设计有问题，写进报告的"疑问与偏差"，不要自行改设计。
- 做不了的部分（比如依赖的库不支持），在报告里写清原因和替代方案，把其余做完。
- 没有实际跑过的东西，不能写成已完成；报告里写"已检查""已通过"的每一项都必须真做过。

## 许可与署名

- ZeeTerm 以 GPL-3.0-or-later 开源，`LICENSE` 是全文，`NOTICE`（共用）和 `native/NOTICE`（原生版额外使用的组件）列出第三方代码。
- **已有的来源注释一律不准删**，例如文件头的 `// Adapted from Netcatty ...`。
- 移植了 Netcatty 的新模块时，在 `NOTICE` 的 Netcatty 一节补上；新增的第三方依赖先查许可，只接受 MIT / BSD / ISC / Apache-2.0 这类与 GPL-3.0-or-later 兼容的，并记入 `NOTICE` 或 `native/NOTICE`。
- 界面上不出现 Netcatty 字样，致谢只放在 `NOTICE`、README 和关于页的"开源许可"里。
- 移植过来的文件，文件头加一行注释写明来源。

## 环境

- Windows 10/11 x64、Rust（版本由 `rust-toolchain.toml` 固定，MSVC 工具链）、VS 2022 C++ 生成工具。打安装包还要 NSIS。
- **构建不能依赖 CMake / NASM**：`russh` 必须关掉默认特性，用 `ring` 后端。
- cargo 下载失败时按自己的环境走本地代理（例如 `HTTPS_PROXY=http://127.0.0.1:<端口>`）。

## 代码约定

- 业务逻辑放在 `crates/zssh-core`，它不能依赖任何界面框架；`native/` 只做界面和转接。
- 错误用 `thiserror` 定义，库代码里不要 `unwrap()` / `expect()`（测试和示例里可以）。
- 日志用 `tracing`，不能打印密码、口令、私钥和 keyboard-interactive 的回答。
- 注释写为什么，不写做了什么。中文、英文都行，同一个文件里保持一致。
- 测试钩子：`ZSSH_*` 开头、用来让自动化跳过确认 / 直接打开某窗口 / 伪造状态的环境变量，只在 `cfg(debug_assertions)` 下读取，release 构建里不存在这条路径。`ZSSH_DATA_DIR` 是产品功能，例外。

## 可移植性

ZeeTerm 要能在**别人的 Windows 10/11** 上用，不能只照着开发这台电脑写。

- 要照顾的差异：不同的用户名（含空格、含中文，如 `C:\Users\张 三`）、不同的安装方式、不同版本、没装某个智能体、配置文件不存在或写法不同。
- 任务卡里写的"本机是 xx 版本 / 装在某处 / 配置里有 xx"只是**一个测试用例**，不是唯一要支持的情况：实现按"一般情况"写（版本按 `--help` / `--version` 探测，路径按候选清单找，配置按容错读）。
- 代码、注释、测试、样本文件、文档、报告里**不写开发机特有的信息**：用户名、用户目录路径、自己配的服务名或其他配置项名、IP、主机名。举例用中性的占位：`C:\Users\<用户>`、`web-1`、`my_mcp_server`。
- 依赖本机环境的测试（真实智能体、测试服务器、已登录的账号）在环境不满足时**跳过并打印原因**，不能失败；也不能断言只有这台电脑才成立的值。
- 处理用户文件（配置、凭据、目录）时按"读不了 = 不确定"处理：不崩、不猜、不漏，并且**只读不改**。

## 测试服务器

- 连接测试只从环境变量读取：`ZSSH_TEST_HOST`、`ZSSH_TEST_PORT`、`ZSSH_TEST_USER`，以及 `ZSSH_TEST_PASSWORD` 或 `ZSSH_TEST_KEY_PATH`（`ZSSH_TEST_KEY_PASSPHRASE`）；可选 `ZSSH_TEST_SOCKS5`（测代理）。没设置就跳过对应测试并打印原因。
- 可选 `ZSSH_TEST_REMOTE_ROOT`：测试服务器上专给联网测试用的远端临时目录（绝对路径，不能是 `/`、`/tmp`，不能含引号、换行或 `..`）。几台机器共用一台测试服务器时设它，各自的临时目录和清扫互不影响；没设或不合格就打印原因、照旧用 `/tmp`，不会让测试失败。
- **任何密码、私钥、IP、主机名都不能写进代码、测试、提交、文档和报告。**报告里写"测试服务器"就行。
- 完整联网档（SFTP 全套、断线续传、AI 驱动、认证、代理 / 跳板、连接池等）在 `common::full_live()` 里检查 `ZSSH_LIVE_FULL=1`；日常检查不设它。

## 数据目录与用户的真实环境

- 自动化脚本、测试、截图一律用独立数据目录：通过 `ZSSH_DATA_DIR` 覆盖（默认是 `%APPDATA%\ZeeTerm`）。**不碰用户的真实数据目录**，没设置就报错退出，不要回退到真实目录。
- 不碰用户的 `~/.ssh/known_hosts`、`~/.claude` 等个人配置。
- **不要把真实的 Claude Code 登录凭据（`.credentials.json`）复制进别的配置目录去跑**：刷新令牌是轮换的，副本刷新后原件会失效，用户自己的终端会掉登录。会拉起真实智能体的测试必须显式开启（`ZSSH_AGENT_E2E=1`），默认的 `cargo test` 和 CI 不拉起它。
- 需要已登录 claude 的真实测试读环境变量 `ZSSH_AGENT_E2E_CLAUDE_CONFIG_DIR`：开发者事先为测试单独准备的配置目录（`CLAUDE_CONFIG_DIR=<该目录> claude` 登录一次，和日常终端里的登录互不相干），测试直接把它当 `CLAUDE_CONFIG_DIR` 用，**不复制、不改、不删**；没设置就跳过并打印原因。
- 用户机器上可能正开着他自己的 ZeeTerm：只结束自己启动的进程，不弹会挡住桌面的模态框给自己点。

## 截图隐私

截图前必须清理画面：不能出现测试服务器的地址、主机名、任何公网 IP、能看出服务器用途的进程名，以及本机用户名和 `C:\Users\...` 这类用户目录路径（设置页"关于"会显示数据目录，最容易被拍进去）。截图要有真正生效的检查，不能只靠人工看一眼：`scripts/native-shot.ps1` 起程序前自己校验 `-DataDir` 必须在 `C:\zssh-v2-data\` 或系统临时目录之下、且路径里没有当前 Windows 用户名，不满足就报错退出；画面文字的检查在应用里（`shot:<名字>` 步骤先查公网 IP、`Last login` 行、用户目录路径和 `ZSSH_SHOT_FORBID` 里的禁用片段，命中就不写 `.ready`、不会有截图）。数据目录用不含用户名的路径（例如 `C:\zssh-v2-data\<场景>`）。

## 每次提交前的检查

```
powershell -File scripts/gates.ps1
```

它依次跑核心库（根工作区）和原生应用（`native/`）各自的 `cargo fmt --check`、`cargo clippy --all-targets -D warnings`、`cargo test`，再加两步 gate 配置的检查——`cargo test -p zssh-gpui --profile gate dev_hooks`（无 `debug_assertions` 时测试钩子读不到环境变量的断言）和一次原生应用的 gate 构建，这两步的编译输出里出现任何 warning 也算失败（无 `debug_assertions` 时才冒出来的"从未使用"：只给 debug 调试脚本用的函数要用 `#[cfg(debug_assertions)]` 门住，不要用 `allow(dead_code)` 糊）。逐条报告退出码和耗时，任何一条非 0 整体就失败。

`gate` 是 `Cargo.toml`（根和 `native/` 各一份）里的自定义配置：`inherits = "release"`，只把 LTO 关掉、代码生成单元放开到 16，其余（`opt-level`、`debug-assertions = false`……）与 release 相同，所以钩子行为、`cfg(debug_assertions)` 判断和出货版一致，编译却快得多。**门禁用 gate，正式发版（`native/installer/build.ps1`、`release.yml`、`scripts/release.ps1`）仍用完整的 release**；两个配置要保持同步，只差 LTO / 并行单元。

平时改完代码可以只跑相关的 `cargo test -p <crate> <过滤>`，加 `cargo fmt` 和 `cargo clippy`；动了共享库、持久化、同步、安装包，或一次改动很大时跑全套。`target/` 会越积越大，定期 `cargo clean`。

## Git 与公开前检查

- 小步提交，一个提交做一件事，提交信息用 Conventional Commits（`feat(core): ...`），说明为什么。
- 只 `git add` 自己改过的具体文件，不用 `git add -A` / `git add .`；测试产物、截图输出、日志（`.log`）不进仓库。
- 不改写已有提交的历史。
- 公开或推送到别处之前跑 `powershell -File scripts/privacy-scan.ps1`：它查开发机的用户名、用户目录、计算机名、`ZSSH_TEST_*` 里的值、令牌 / 私钥样式、公网 IPv4 和邮箱，有命中就不能公开。还要查自己不能写进仓库的具体值（别的用户名、私人 IP、内部域名……）时，把它们逐行写进仓库外面的一个文件，用 `-ExtraPatternsFile <文件>` 传给它；这类宽模式命中中性占位时靠 `scripts/privacy-allow.txt` 放行——不给 `-AllowListFile` 时它默认就加载这份，对内置的用户名 / 用户目录命中也生效，但放行必须把整段命中都盖住，占位里含本机真实用户目录或用户名时一律不放行；里面只能放明显不属于任何真人的占位（`x`、`ab` 这种太短的名字不收）。
