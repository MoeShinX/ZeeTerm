# 发版说明

ZeeTerm 的发布物放在 GitHub Release：Windows 安装包 `ZeeTerm-Setup-<版本>.exe`、Windows 免安装版 `ZeeTerm-<版本>-windows-x64-portable.zip`、Linux 的 `ZeeTerm-<版本>-linux-x86_64.deb` 和 `ZeeTerm-<版本>-linux-x86_64.tar.gz`、更新清单 `latest.json` 和校验文件 `SHA256SUMS.txt`。已安装的程序启动时（以及开着时每 6 小时）读取最新 Release 里的 `latest.json`，发现新版本后下载安装包，**用发布者的更新签名公钥验签通过才会安装**。这份文档说明怎么发版、签名密钥怎么管、怎么回滚、更新失败怎么查。

**发版的主路径是 GitHub Actions**：改好版本号、推上 main，再推一个 `v*` tag，工作流（`.github/workflows/release.yml`）自己编译、跑门禁、签名、发布 Release。更新签名私钥和口令存在 GitHub 的 `release` 环境 Secrets 里，发版人的电脑上不需要私钥，也不用输口令。本机的 `scripts/release.ps1` 保留作备用（GitHub 出问题时手动发版）。

## 一次性准备

### 1. 生成更新签名密钥

更新签名用 minisign 格式的密钥对，由发版人自己生成，**私钥不进仓库**。

```powershell
powershell -File scripts/updater-keygen.ps1                    # 私钥写到 %USERPROFILE%\.zeeterm\updater.key
powershell -File scripts/updater-keygen.ps1 -WriteUpdaterJson  # 同时把公钥写进 native/zssh-update/updater.json
```

脚本会提示给私钥设口令（可留空，不推荐），然后打印公钥。不需要安装 minisign.exe：签名和验签用的是程序里同一套代码。

1. **把公钥填进 `native/zssh-update/updater.json` 的 `pubkey`**（用了 `-WriteUpdaterJson` 就已经写好了），提交。公钥可以公开。公钥还是占位值（`PLACEHOLDER…`）的构建不会检查、下载或安装任何更新，更新窗口只显示"更新签名未配置"；发版工作流也会在最前面拒绝占位公钥。
2. **备份私钥文件和口令**（密码管理器或加密存储）。私钥不要放进仓库、不要发给任何人。它只应该存在于：你自己的加密备份，和 GitHub 的 `release` 环境 Secrets（下一节）。
3. 密钥目录里已经有 `updater.key` 时脚本拒绝再生成，避免覆盖。

> 公钥一旦随正式版发出去就不能换：换了公钥，装着旧版本的用户验不了新签名，收不到更新，只能手动重新下载安装。所以私钥丢失或泄露是最严重的事故，见下面"私钥轮换与泄露"。

### 2. 在 GitHub 上设置 `release` 环境和 Secrets

发版工作流的 job 声明了 `environment: release`，两个 Secret 放在这个环境里（不是仓库级 Secrets），只有这个 job 拿得到，而且只暴露给"写私钥文件"和"签名"两个步骤。

| Secret（环境 `release` 里） | 内容 |
| --- | --- |
| `ZEETERM_UPDATER_KEY` | 私钥文件 `updater.key` 的全文 |
| `ZEETERM_UPDATER_KEY_PASSWORD` | 私钥口令（私钥没设口令时可留空，但不推荐） |

在**你自己的终端**里设置（命令不会把私钥或口令打印出来；不要让别人代劳）：

```powershell
# 建环境（已有则无副作用）
gh api -X PUT repos/MoeShinX/ZeeTerm/environments/release
# 私钥：PowerShell 不支持 `<` 重定向，用管道（-Raw 保留整个文件内容）
Get-Content -Raw "$env:USERPROFILE\.zeeterm\updater.key" | gh secret set ZEETERM_UPDATER_KEY --env release -R MoeShinX/ZeeTerm
# 口令：交互输入
gh secret set ZEETERM_UPDATER_KEY_PASSWORD --env release -R MoeShinX/ZeeTerm
```

（在 cmd 或 Git Bash 里，私钥一行可以写成 `gh secret set ZEETERM_UPDATER_KEY --env release -R MoeShinX/ZeeTerm < ~/.zeeterm/updater.key`。）

建议再做两个限制（网页上设置）：

- Settings → Environments → release → **Deployment branches and tags**：选 *Selected branches and tags*，加一条 tag 规则 `v*`。手动重跑的 `workflow_dispatch` 是从 `main` 分支触发的（工作流里限制了只接受 `main`），所以还要把 `main` 也加进去。
- Settings → Rules：给 `v*` tag 加一条规则，限制谁能创建 / 删除 / 移动它们。能推 tag 的人就能触发带私钥的工作流，这和"能签名"是一回事。

## 发版步骤（主路径：GitHub Actions）

1. **写更新说明（两份）**：`docs/release-notes/<版本>.md`（中文）和 `docs/release-notes/<版本>.en.md`（英文），都面向用户（写法见下面"更新说明"一节）；开发者看的改动写进 `CHANGELOG.md` 的 `## [Unreleased]`。缺任何一份，`bump-version.ps1` 和发版工作流都会直接失败。
2. **改版本号**（在 `main` 最新代码上）：

   ```powershell
   powershell -File scripts/bump-version.ps1 -Version 1.0.0            # 加 -DryRun 先演练：照常检查和修改，结束后原样还原
   ```

   它检查：版本号格式正确且高于当前版本、`docs/release-notes/1.0.0.md` 和 `1.0.0.en.md` 都存在且不空、tag `v1.0.0` 还不存在、CHANGELOG 有 `[Unreleased]` 一节；然后改全部版本号来源（见下面"版本号的来源"）、把 CHANGELOG 的 `[Unreleased]` 内容切成 `## [1.0.0] - <今天>`，最后打印要执行的 git 命令。**不提交、不打 tag、不推送。**
3. **提交并推到 main**，等 main 上的 CI 通过：

   ```powershell
   git add Cargo.toml Cargo.lock native/Cargo.toml native/Cargo.lock CHANGELOG.md docs/release-notes/1.0.0.md docs/release-notes/1.0.0.en.md
   git commit -m "chore(release): v1.0.0"
   git push origin main
   ```
4. **打 tag 并推送**（这一步触发发版工作流）：

   ```powershell
   git tag v1.0.0
   git push origin v1.0.0
   ```

   tag 必须叫 `v<版本>`，并且指向已经在 `main` 上的提交。
5. **看 Actions 页**的 Release 工作流。平时只有两个任务（发版**不再编译**，直接用 main 上同一个提交的 CI 产物）：
   0. **校验**（`verify`，ubuntu）：tag 去掉 `v` 等于两个 `Cargo.toml` 的版本；tag 指向的提交在 `main` 上；`docs/release-notes/<版本>.md`（中文）和 `docs/release-notes/<版本>.en.md`（英文）都存在且不空；`updater.json` 的公钥不是占位值；这个 tag 的 Release 还不存在。然后**锁定那一次 CI 运行**：本仓库的 `ci.yml`、`push` 事件、`main` 分支、提交号和 tag 一致、整次运行成功，而且只能有一次（不认在 main 上手动触发的 CI——那种运行编的是触发时的 main）。再在这次运行里按名字找出三个产物 `zeeterm-windows`（安装包 + 免安装版 zip）、`zeeterm-linux`（deb + tar.gz）、`zeeterm-release-tool-windows`（签名工具），每个必须恰好有一个没过期、属于这次运行和这个提交，记下 artifact ID；缺任何一个就失败，**不会拿"最近一次成功的产物"凑数**。CI 已经在 Windows、Linux、macOS 上对同一个提交跑过 fmt / clippy / test / 完整 release 编译 / 打包（Linux 还在 `debian:12`、`ubuntu:22.04` 容器里装过 deb），所以第 3 步说的"等 CI 通过再打 tag"是硬性要求。
   1. **签名、发布**（`release`，windows-latest，`release` 环境，只有它拿得到私钥）：自己再核一遍提交 / tag / 版本 / 更新说明（两份）/ 公钥 / Release 不存在 → 按 **run ID + artifact ID** 取回四个包和签名工具，核对：文件清单恰好是这个版本的四个包和 `release_tool.exe`、都不是空文件；免安装版里的 `ZeeTerm.exe` 和签名工具是 x64 PE，tar.gz 里的 `ZeeTerm-<版本>-linux-x86_64/zeeterm` 是 x86-64 ELF（目录名里的版本也对得上）→ 把 Secret 里的私钥写到 runner 临时目录，调 `scripts/release-sign.ps1`（`ZEETERM_RELEASE_TOOL` 指向取回的签名工具，不装 Rust、不编译）：核对 `updater.json` 的公钥与私钥是一对 → 用**同一把私钥**给四个包各签一份名（签名里带 `version:` 和文件名）→ 生成 `latest.json`（`windows-x86_64` 加上 `windows-x86_64-portable`、`linux-x86_64-deb`、`linux-x86_64-tar`）→ 用公钥按应用里的验签代码再验一遍 → 算 SHA-256，写 `SHA256SUMS.txt` 和 Release 正文 → **无论成败都删掉临时私钥文件** → `gh release create` 上传六个文件（四个包、`latest.json`、`SHA256SUMS.txt`；同名 Release 已存在时失败，不覆盖）。**tag 带 `-` 后缀（`v1.2.0-rc.1` 这类）时是预发布**：Release 标成 `--prerelease`，且只上传五个文件，不传 `latest.json`（见下面“预发布版本”）。

   另外两个任务 `linux` / `windows`（**受控重建**）平时跳过，只在手动触发时勾选 `rebuild` 才跑：从 tag 指向的提交重新做完整 release 编译和打包（和以前的发版一样，约 25 分钟），代替 CI 产物。CI 的门禁照样要求通过。

   耗时：以前（1.0.1 及之前）串行加重跑门禁约 1 小时 20 分钟；1.0.2–1.0.4 在发版里重编约 20–30 分钟；改成取 CI 产物后目标是 5 分钟左右，以实际运行计时为准。从推 main 到发出 Release 的总时间主要是 main 上 CI 本身的时间。

   **演练**：Run workflow（从 `main`）勾选 `dry_run`，输入 tag，或者不填 tag、改填 `commit`（main 上的完整 40 位提交号）。校验、锁定 CI 运行和产物、取回、核对、签名、生成 `latest.json` 和自检全部照做，但签名用现场生成的临时密钥（不碰正式私钥），也不创建 Release；给已经发布过的 tag 演练也可以。改了发版工作流或签名脚本之后先演练一次。
6. **下载核对**：从 Release 页下载安装包，`Get-FileHash` 对一下 `SHA256SUMS.txt`（Linux 包用 `sha256sum -c SHA256SUMS.txt --ignore-missing`）；再用一个旧版本程序点「设置 → 关于 → 检查更新」，确认能发现、下载并校验通过（不必点安装）。

`latest.json` 里的下载地址是 `https://github.com/MoeShinX/ZeeTerm/releases/download/v<版本>/<安装包文件名>`，所以 tag 必须叫 `v<版本>`、安装包文件名不能改。程序读的更新源是 `https://github.com/MoeShinX/ZeeTerm/releases/latest/download/latest.json`（`native/zssh-update/updater.json` 的 `endpoints`），即最新 Release 里的 `latest.json`。

**预发布版本**（tag 写成 `vX.Y.Z-rc.1` 这样带 `-` 后缀，版本号文件里也是同一个后缀）：工作流照常取产物、签名、生成并自检 `latest.json`（验证的是同一套流程），但创建 Release 时加 `--prerelease`，并且**不把 `latest.json` 传上去**。原因有两层：GitHub 的 `releases/latest` 不会指向预发布，应用读的 `releases/latest/download/latest.json` 因此不会拿到它；再不上传 `latest.json`，是为了即使有人在网页上把这个 Release 改成正式版，或直接按 tag 地址取清单，预发布也不会被推送给全部用户。想让大家升级就发正式版（新 tag，比如 `v1.2.0`），不要把预发布“转正”；转正后 `releases/latest/download/latest.json` 会 404，客户端只会显示“读取更新说明失败”，不会装任何东西。预发布的安装包和 `SHA256SUMS.txt` 照常在 Release 里，供测试的人手动下载。本机备用路径 `scripts/release.ps1` 打印的 `gh release create` 命令对带后缀的版本号同样会换成 `--prerelease` 且不带 `latest.json`。

工作流日志里不会打印私钥和口令（GitHub 会对 Secret 打码，脚本也不 echo，签名工具不开详细输出）；第一次发版后顺手翻一遍"签名"步骤的日志确认。

### 失败了怎么办

- **校验任务失败**：什么都没发布。日志最后一行说明原因：
  - main 上的 CI 还没跑完或没通过：等 CI 通过后对这次发版 *Re-run*（或 *Run workflow* 输入这个 tag）；CI 失败就修好合进 main，发新的补丁版本。
  - **CI 产物过期了**（CI 产物保留 30 天）：在 Actions 页打开校验日志里给出的**那一次** CI 运行，点 *Re-run all jobs*——重跑会保留原来的提交和 ref，重新生成产物；通过后再重跑发版。**不要在 main 上另点 CI 的 Run workflow**：那会编当时最新的 main，不是 tag 指向的提交（而且发版只认 push 触发的那次运行，手动触发的也用不上）。GitHub 只允许重跑 30 天内的运行。
  - 原 CI 运行已经超过 30 天、不能重跑，或者那次运行早于"CI 上传发版产物"的改动（里面根本没有这些产物，重跑也只会按当时的 `ci.yml` 来）：用**受控重建**——*Run workflow*（从 `main`），输入 tag，勾选 `rebuild`。
- **受控重建的 Linux 或 Windows 编译任务失败**：签名发布任务不会开始，什么都没发布。看日志里是编译、打包自检（lintian、appstreamcli、glibc 基线）还是容器检查；修好合进 main 发补丁版本，或不改代码的话 *Re-run* 即可。
- **签名发布任务在"核对取回的包和签名工具"失败**：取回的文件清单、版本或平台不对，说明产物有问题，**不要绕过**；查清是哪次 CI 运行、哪个产物，必要时修好发补丁版本。
- **工作流在"创建 Release"之前失败**：还没有发布任何东西。代码有问题就修好合进 main，**发新的补丁版本**（最稳，不用动 tag）；只是 runner 抽风、Secrets 没设好之类，不改代码的话，在 Actions 页对该次运行点 *Re-run*，或者 *Run workflow*（从 `main` 触发，输入已有的 tag，如 `v1.0.0`）。
- **"创建 Release"失败或半途中断**：先到 Releases 页看有没有残留的 Release（含草稿）。有就整个删掉（附件一起），再重跑；没有就直接重跑。
- **Release 已经发布了才发现有问题**：见下面"回滚与事故"，不要在同一版本号下替换附件，发新的补丁版本。

## Linux 包

每个版本发两个 Linux 包（x86_64，glibc 2.35 及以上，Wayland / X11），文件名固定，`latest.json` 里的下载地址按它拼，所以不能改：

| 文件 | 内容 | `latest.json` 平台 key |
| --- | --- | --- |
| `ZeeTerm-<版本>-linux-x86_64.deb` | `/usr/lib/zeeterm/zeeterm`、`/usr/lib/zeeterm/zssh-mcp`（AI 工具桥，内部辅助程序，必须和主程序在同一目录，所以不放进 `/usr/bin`）、`/usr/bin/zeeterm`（指向 `../lib/zeeterm/zeeterm` 的相对软链接，`.desktop` 的 `Exec` / `TryExec` 用它）、`.desktop`、hicolor 图标（16 到 512）、AppStream metainfo、man 手册、`copyright`（GPL-3.0-or-later 加第三方声明） | `linux-x86_64-deb` |
| `ZeeTerm-<版本>-linux-x86_64.tar.gz` | 解压后一个同名目录：`zeeterm`、`zssh-mcp`、`install.sh`（免 root，默认装 `~/.local`，`--prefix` 可改；程序装到 `<前缀>/lib/zeeterm/`，`<前缀>/bin/zeeterm` 是软链接，布局和 deb 一致）、`uninstall.sh`、图标、`.desktop`、`LICENSE`、`NOTICE`、`README` | `linux-x86_64-tar` |

应用标识是 `app.zeeterm.ZeeTerm`：`.desktop` 文件名、`Icon=`、`StartupWMClass=`、图标文件名、AppStream 的 `<id>` 都是它，和窗口的 app_id / WM_CLASS（`native/zssh-gpui/src/window_frame.rs` 的 `APP_ID`）一致，有测试（`installer_manifest.rs`）核对。可执行文件名是 `zeeterm`（`zeeterm --version` 只打印版本，在任何窗口、数据目录、钥匙串初始化之前处理，装好后用来冒烟）。

**怎么打**：`scripts/package-linux.sh`（CI、`release.yml` 的 Linux 任务和本机都用它；`--out-dir` 指产物目录，`--profile gate` 本机试打包省编译时间，`--skip-build` 用已经编好的二进制）。它做完整 release 编译（native 工作区和根工作区的 `zssh-mcp`）、用 `zssh-icons --hicolor` 从品牌 SVG 渲染图标、用 `dpkg-shlibdeps` 算 deb 的 `Depends`（再补上运行时 dlopen、shlibdeps 看不出来的 `libvulkan1`、`libwayland-client0`、`libegl1`；`mesa-vulkan-drivers`、钥匙串提供者、`xdg-utils` 放 `Recommends`）、`dpkg-deb --root-owner-group` 打包，tar.gz 用固定排序和时间戳，最后自检：文件布局和元数据、`desktop-file-validate`、`appstreamcli validate`、`lintian --fail-on error`、`ldd` 无 `not found`、glibc 符号不超过 2.35、`zeeterm --version`、tar.gz 里的 `install.sh` / `uninstall.sh` 在临时前缀里装一遍再卸干净。CI 里设 `ZEETERM_PKG_REQUIRE_CHECKS=1`，lintian 等工具缺失也按失败处理；本机缺工具只提示。

**依赖基线**：`scripts/linux-container-check.sh <deb>` 在 `debian:12` 和 `ubuntu:22.04` 的干净容器里装 deb（不装 Recommends），确认 `ldd` 无 `not found`、`zeeterm --version` 能跑、桌面文件校验通过、卸载干净。发布包必须在 ubuntu-22.04 上编（glibc 2.35）；在更新的系统（例如 Debian 13）上本地试打包可以，产物不能发布。

**系统依赖**只有一份清单：`scripts/linux-build-deps.sh`（`build` 编译、`test` 测试需要的 xvfb、xauth、gnome-keyring、socat、dbus，`package` 打包检查工具）。CI 和 README 都引用它，改依赖只改这里。

**测试环境**：Linux 的测试必须在一次性的 D-Bus 会话 + 临时解锁的 `gnome-keyring` 里跑（本机秘密的密钥走 Secret Service），用 `scripts/linux-keyring-session.sh <命令>`；`scripts/gates.sh` 是 Linux 门禁（对应 `gates.ps1`，末行「Linux 门禁全部通过。」），i18n 检查需要 `pwsh`（CI 里设 `ZEETERM_REQUIRE_I18N=1`，没有就失败）。

**签名与 `latest.json`**：Linux 包由 Windows 任务里的 `release-sign.ps1` 用同一把更新私钥签名（和 Windows 安装包一样的 minisign 格式，受信注释里绑 `version:` 和文件名），`latest.json` 的 `platforms` 增加 `linux-x86_64-deb` 和 `linux-x86_64-tar` 两个条目（各自带 `url` 和 `signature`），`SHA256SUMS.txt` 列出三个文件。Windows 的条目一个字不变：老版本 Windows 客户端读这份清单，选中的仍是 `windows-x86_64-nsis` / `windows-x86_64`，Linux 条目被忽略（`native/zssh-update` 有测试钉住）。

**应用内更新（Linux）**：更新窗口认出 ZeeTerm 是 deb 装的（程序在 `/usr/` 下且 `dpkg -S` 认得）还是 tar.gz 装的，只选对应的 `latest.json` 条目（`native/zssh-update/src/linux.rs`，流程说明在 `native/zssh-gpui/src/update.rs` 顶部）。deb 经 `pkexec apt-get install` 安装；tar.gz 在程序所在目录可写时解压、核对新程序的 `--version`、原子替换 `lib/zeeterm` 下的两个程序（`install.sh` 布局另外更新图标、桌面文件和安装清单），失败整体回滚。**改打包布局时**，`TarLayout::detect`、`install.sh` / `uninstall.sh`、`scripts/package-linux.sh` 要一起改，`installer_manifest.rs` 里的 `the_tar_installer_and_the_in_app_updater_agree_on_the_layout` 会拦住不一致。

> 本机备用路径 `scripts/release.ps1` 在 Windows 上跑，**不带 Linux 包**：用它发版得到的 `latest.json` 没有 Linux 条目，Linux 客户端读到后按"没有适用的包"处理（提示去发布页下载），Windows 不受影响。需要 Linux 包时，用 GitHub 路径，或把别处打好的两个包用 `release-sign.ps1 -LinuxDebPath ... -LinuxTarPath ...` 签名并手动上传。

## Windows 免安装版

每个版本除了安装包，还发一个免安装版 `ZeeTerm-<版本>-windows-x64-portable.zip`（文件名固定，`latest.json` 里的下载地址按它拼，不能改）。`native/installer/build.ps1` 打安装包时顺手打它：平铺的 zip（没有外层目录），里面是 `ZeeTerm.exe`、`zssh-mcp.exe`（AI 工具桥，必须和主程序同目录）、`LICENSE`、`NOTICE`（和安装包里是同一份合并后的声明），再加一个**空的 `portable` 标记文件**。解压即用。

**行为**：程序旁边有 `portable` 文件，就是免安装版（`native/zssh-gpui/src/portable.rs`）：

- 数据放在 `<程序所在目录>\data`，不碰 `%APPDATA%`；`ZSSH_DATA_DIR` 仍然最优先。装在 `%APPDATA%\ZeeTerm` 的安装版完全不受影响。
- 单实例的互斥体名和窗口属性名带上数据目录路径的短哈希，所以免安装版和安装版、两份不同目录的免安装版可以同时开；同一个目录再开一份仍会被拦下。安装版的名字一个字没变。
- 不写注册表、没有卸载项；程序本来就没有开机自启功能。「关于」页的版本号后面标"免安装版 / Portable"，数据目录下面有一句提醒：已保存的密码用 DPAPI 绑定当前 Windows 用户，拷到别的电脑 / 别的 Windows 用户下会解不开，换电脑请用「离线备份文件」导出再导入。
- 程序所在目录不能写（Program Files、只读盘）时，启动就提示并退出（数据目录建不出来）。

**应用内更新**：`latest.json` 的 `platforms` 里多一项 `windows-x86_64-portable`（`url` + `signature`，格式和别的条目一样，同一把私钥签名，签名里绑版本和文件名）。老客户端只认 `windows-x86_64-nsis` / `windows-x86_64`，多出来的这一项对它们没有任何影响（`native/zssh-update` 有测试钉住）；免安装版自己只选这一项，绝不拿安装包去"更新"自己。流程：下载 zip → 用和安装包同一套 minisign 公钥验签 → 解压到程序所在目录里的临时目录（只解白名单上的四个文件，`data` 和 `portable` 永不碰）→ 把运行中的 `ZeeTerm.exe` 改名成 `ZeeTerm.exe.old`、新程序移进来（`zssh-mcp.exe`、许可文件同样），任何一步失败都回滚 → 用户点「立即重启」，新进程带 `--zeeterm-wait-pid=<旧进程号>` 启动，先等旧进程退出再抢单实例互斥体 → 下次启动清掉 `*.old`。程序所在目录不可写时不下载、不替换，只提示有新版本并引导到发布页；清单里没有 zip 条目（老版本发布的）时同样引导到发布页。

**改打包布局时**：`build.ps1`（zip 内容和文件名）、`release-sign.ps1` / `release-lib.ps1`（签名和 `latest.json` 条目）、`release.yml`（上传、核对、发布）、`zssh_update::portable`（白名单文件名 `ZeeTerm.exe` / `zssh-mcp.exe` / `LICENSE` / `NOTICE`）要一起改，`installer_manifest.rs` 里的 `the_portable_zip_is_built_signed_and_published_with_the_names_the_updater_expects` 会拦住不一致。

> 本机备用路径 `scripts/release.ps1` 也会带上免安装版：`build.ps1` 打好后，zip 在产物目录里就一起签名、写进 `latest.json`，下一步命令里的 `gh release create` 也带上它。

## macOS 包与 Homebrew

macOS（Apple Silicon）目前没有签名和公证，不进发版工作流、不进 `latest.json`（客户端在 macOS 上只提醒有新版本、指向发布页）：

1. 在 Mac 上打好 `ZeeTerm.app`，压成 `ZeeTerm-<版本>-macos-arm64.zip`，手动上传到这个版本的 Release。
2. 更新 Homebrew tap（`MoeShinX/homebrew-tap` 的 `Casks/zeeterm.rb`）：

   ```powershell
   powershell -File scripts/update-homebrew-cask.ps1 -Version <版本> -TapDir <homebrew-tap 本地克隆> -Commit
   ```

   脚本从 Release 下载上一步的 zip、算 sha256、改写 cask 的 `version` 和 `sha256` 并在 tap 仓库提交，不推送；确认后自己 `git push`。
   zip 还没传上去时脚本会失败，什么都不改。

## 备用路径：本机发版

GitHub 出问题（Actions 故障、Secrets 失效）时才用。**先停用工作流**，否则推 tag 会触发它、与手动建 Release 冲突：`gh workflow disable release.yml -R MoeShinX/ZeeTerm`，发完 `gh workflow enable release.yml -R MoeShinX/ZeeTerm`。

```powershell
powershell -File scripts/release.ps1 -Version 1.0.0
```

前提：在 `main` 分支、已跟踪的文件没有未提交改动、本机有私钥（`%USERPROFILE%\.zeeterm\updater.key`，有口令时会提示输入）、更新说明已写好。脚本依次做：校验版本号 / 分支 / 工作区 / tag → 核对 `updater.json` 的公钥与私钥是一对 → 统一改版本号 → 跑门禁（`-SkipGates` 可跳过）→ 现编安装包 → 签名 → 生成 `latest.json` 并验签 → 写 `SHA256SUMS.txt` 和 `RELEASE_NOTES.md`，产物放 `dist\<版本>\`。**不推送、不打 tag、不创建 Release**，最后打印下一步命令（提交、打 tag、`gh release create`）。`dist\<版本>\` 已有内容时一开始就报错退出，确认可以丢弃再加 `-Force`。

**release 与 gate 两个配置**：门禁用 `gate`（快，用来验证“无 `debug_assertions` 时钩子读不到环境变量”和无 warning），发版编译安装包用 `release`（完整 thin LTO、单个代码生成单元，exe 更小更快）。两者在根 `Cargo.toml` 和 `native/Cargo.toml` 里各有一份，改其中一个时同步另一个；`gate` 只能比 `release` 少 LTO / 多并行单元，不能改 `opt-level`、`debug-assertions`、`panic` 这类影响行为的项。

签名、`latest.json`、`SHA256SUMS.txt`、Release 正文的实现和工作流共用 `scripts/release-lib.ps1`，产物格式一致。本机路径不会改 CHANGELOG（要自己把 `[Unreleased]` 改成这个版本）。

### 演练

```powershell
powershell -File scripts/release-drill.ps1
```

用现生成的测试密钥（带随机口令，放 `C:\zssh-v2-data\r2-keys\`，跑完自动删）和一个充数文件，把 GitHub 路径的签名步骤（`release-sign.ps1`）和本机路径（`release.ps1 -DryRun`）各跑一遍，核对两边 `latest.json` / `SHA256SUMS.txt` / Release 正文的格式一致，再做几个反例（私钥与公钥对不上、口令不对、公钥是占位值、安装包被改……）确认都会失败。**Linux 资产也在里面演练**：用充数的 deb 和 tar.gz 走一遍带 `-LinuxDebPath` / `-LinuxTarPath` 的签名，核对 `latest.json` 恰好多出 `linux-x86_64-deb`、`linux-x86_64-tar` 两项而 Windows 项不变、三个文件各自验签通过（张冠李戴、改动、换私钥都验不过）、`SHA256SUMS.txt` 和 Release 正文含全部文件，并确认只给一个包、文件名不对、文件不存在都会失败。**免安装版也在里面演练**：用充数的 zip 走一遍带 `-PortablePath` 的签名（单独，以及和 Linux 包一起），核对 `latest.json` 恰好多出 `windows-x86_64-portable` 一项而安装包条目不变（老客户端只认自己的 key）、zip 验签通过（张冠李戴、改动都验不过）、`SHA256SUMS.txt` 和 Release 正文含它，并确认文件名不对、文件不存在、私钥对不上都会失败。**更新说明也在里面演练**：`latest.json` 的 `notes` 是中文、`notes_en` 是英文（顶层字段恰好是 `version`、`notes`、`notes_en`、`pub_date`、`platforms`），Release 正文是英文说明加一节 `### SHA-256`；缺英文说明、英文说明是空的、缺中文说明都会失败，错误里写清缺哪个文件。不读 `%USERPROFILE%\.zeeterm` 下的任何东西，也不会改仓库（`release.ps1 -DryRun` 改的版本号会原样还原）。

单跑本机路径的演练：

```powershell
powershell -File scripts/release.ps1 -Version 9.9.9 -DryRun -KeyPath <测试私钥> -OutRoot <临时目录> `
    -SkipGates -PrebuiltInstaller <任意文件> -NotesFile <任意说明文件>
```

测试密钥只放在临时目录，不要用正式密钥演练。

## 更新说明

**每次发版要准备两份更新说明**，都放在 `docs/release-notes/`，写法见那里的 `README.md`：

| 文件 | 语言 | 用在哪 |
| --- | --- | --- |
| `<版本>.md` | 中文 | `latest.json` 的 `notes`：更新窗口的中文界面显示它；老版本客户端只认这一个字段 |
| `<版本>.en.md` | 英文 | GitHub Release 页的正文（发版脚本在末尾自动附上 `### SHA-256` 一节，英文说明里不用写）；`latest.json` 的 `notes_en`：更新窗口的英文界面优先显示它 |

更新说明会原样显示在用户的更新窗口里（窗口会把它渲染成带标题、列表、粗体的版式）。**写给普通用户看**：一句话概述加几条要点，每条一行说清"能做什么 / 有什么变化"；不要写脚本名、内部实现和开发者才关心的细节（那些留在 `CHANGELOG.md`）。

`latest.json` 里 `notes`（中文）一直都在，`notes_en`（英文）是新增的可选字段：老客户端不认这个字段，解析时直接忽略；新客户端的英文界面优先显示 `notes_en`，清单里没有它（老版本发布的清单）或是空的就退回 `notes`，中文界面永远显示 `notes`。`latest.json` 本身没有签名——签名只绑在安装包上（签名里的 `version:` 和文件名），所以加这个字段不影响任何校验；GitHub Release 页的正文是脚本生成的文件，和 `latest.json` 里的说明各自独立写出，不互相读取。

GitHub 工作流**只认** `docs/release-notes/` 下这两份（任何一份没有或是空的，`verify` 任务就在最前面失败，什么都不编译；`release` 任务里还会再查一遍，签名那一步也会拒绝），不退回 CHANGELOG，也不用中文顶替英文。本机备用脚本 `release.ps1` 按下面的优先级取说明，取到哪份会在输出里打印出来：

1. 中文：`-NotesFile <文件>`（临时指定，优先级最高）；`docs/release-notes/<版本>.md`（推荐：每个版本一份面向用户的说明；文件存在但是空的会直接报错）；`CHANGELOG.md` 里的 `## [<版本>]` 一节（没有 `docs/release-notes/<版本>.md` 时的退路，内容偏技术，不推荐直接给用户看）。
2. 英文：`-NotesEnFile <文件>`（临时指定）；`docs/release-notes/<版本>.en.md`。**没有退路**，两样都没有就失败。

更新窗口只渲染安全的 Markdown：HTML 不执行、图片不加载（显示替代文字）、只有 `https://` 链接能点开（用系统浏览器打开），所以说明里别靠图片和 HTML 传达信息。

## 版本号的来源

版本号只在这几处，**统一由 `scripts/bump-version.ps1` 修改**（本机备用路径 `release.ps1` 用的是同一份实现，在 `scripts/release-lib.ps1`），不要手改：

| 位置 | 说明 |
| --- | --- |
| `Cargo.toml` 的 `[workspace.package] version` | 核心库工作区 |
| `native/Cargo.toml` 的 `[workspace.package] version` | 原生应用工作区；程序里显示的版本、更新比较用的"当前版本"都来自它（编译时写入） |
| Linux 包的版本 | `scripts/package-linux.sh` 读 `native/Cargo.toml`（deb 的 `Version`、文件名、AppStream 的 `<release>` 都由它生成，模板 `native/installer/linux/app.zeeterm.ZeeTerm.metainfo.xml.in` 里只有占位符），不用手改；预发布版本 `-rc.1` 在 deb 里写成 `~rc.1` |
| `Cargo.lock`、`native/Cargo.lock` | 跟随工作区版本的本仓库包（`zssh-gpui` 与 `crates/` 下的 4 个 crate）。`native/` 下的辅助 crate（`zssh-editor` / `zssh-protocols` / `zssh-regex` / `zssh-session` / `zssh-terminal` / `zssh-update` / `zssh-icons`）版本固定为 `0.0.0`，**不随发版号改**；它们不参与更新比较，`--locked` 构建也照常通过 |
| 安装包文件名、版本资源、"应用和功能"里的显示版本 | `build.ps1` 读 `native/Cargo.toml`，安装脚本里不写死版本 |

有单元测试（`every_place_that_carries_the_version_agrees`）核对这几处一致。

## 私钥轮换与泄露

公钥内置在已发布的程序里，应用只认这一把。**没有"平滑换钥"的办法**：换一对新密钥 = 装着旧版本的用户验不了新签名，收不到自动更新，必须手动下载安装一次带新公钥的版本。所以轮换只在泄露或丢失时做。

- **私钥或口令可能泄露**（Secrets 内容出现在日志 / 截图里、账号被盗、能推 tag 的人不可信……）：
  1. 马上在 Settings → Environments → release 里删除两个 Secret，并 `gh workflow disable release.yml -R MoeShinX/ZeeTerm`，让别人无法再用它签名。
  2. 检查 Releases 页和 Actions 运行记录，有不认识的 Release 就删掉。攻击者要让用户装到恶意包，还得能往本仓库的 Release 放文件（更新源是这里），仅 Secrets 泄露风险较小，但按最坏情况处理。
  3. 生成新密钥对：`scripts/updater-keygen.ps1 -KeyDir <新目录> -WriteUpdaterJson`（脚本拒绝覆盖现有密钥，所以要换目录）；提交新的 `updater.json` 公钥；把新私钥和口令重新写进 Secrets（命令见上）。
  4. 发一个新版本（用新私钥签名）。旧版本程序验不过它，用户要**手动**下载安装包装一次；在 Release 说明和你能触达的渠道里提示。装过新版之后自动更新恢复。
  5. 旧私钥彻底作废：备份里的也删掉。
- **私钥丢了（没有泄露）**：后果同上，只是不用紧急止血：生成新密钥、设 Secrets、发新版、让用户手动装一次。所以私钥和口令要备份好。
- **只想换口令**：`release_tool` 目前没有改口令的子命令；口令是对私钥文件的加密，不换私钥就换不了。口令没泄露、只是想定期换，不值得为此换钥。
- **GitHub 账号被盗**等同于私钥泄露（对方能改工作流并推 tag），处理见第一条，并先恢复账号安全。

## 回滚与事故

- **新版本有问题，想让用户停在上一版**：把该 Release 标成预发布或删除它，`releases/latest` 就会指回上一个正式 Release，程序读到的 `latest.json` 随之回到上一版；已经升级的用户不会被降级（程序只升不降）。要立刻止血又没有可回退的版本时，删除该 Release 即可，用户端只会显示"读取更新说明失败"，不会装任何东西。
- **安装包有问题但清单没问题**：删掉出问题的附件，用户端下载会失败并提示，不会安装。修好后发新的补丁版本，不要在同一个版本号下替换附件（已经下载过的用户会遇到新旧文件对不上的情况）。
- **私钥泄露或丢失**：见上一节“私钥轮换与泄露”。

## 更新失败排查

用户端的更新窗口会给出具体原因：

| 提示 | 常见原因 |
| --- | --- |
| 更新签名未配置 | 这个构建的 `updater.json` 公钥还是占位值；用 `updater-keygen.ps1` 生成密钥并填写后重新发版 |
| 读取更新说明失败 / HTTP 404 | 还没有任何 Release，或 `latest.json` 没作为附件上传；检查 Release 附件 |
| 读取更新说明失败 / 连不上 | 网络或代理问题；「设置 → 连接 → 软件网络」决定检查更新走哪个代理 |
| 签名校验未通过 | 安装包被改过或下载不完整，或不是用这把密钥签的；确认用的是发版产出的那份文件（GitHub Release 或 `release.ps1` 的产物） |
| 签名版本 X 与公告版本 Y 不一致 | `latest.json` 的 `version` 与签名里的 `version:` 不同（手动改过清单，或把别的版本的签名搬了过来） |
| 下载失败 | 网络中断、被限流，或附件被删；重试即可 |
| 没有适用于当前平台的安装包 | `latest.json` 的 `platforms` 里没有 `windows-x86_64`（Linux 上是没有 `linux-x86_64-deb` / `linux-x86_64-tar`；免安装版是没有 `windows-x86_64-portable`，这时窗口不报错，改成引导到发布页；例如用本机备用路径发的版本） |
| 免安装版提示"程序所在的文件夹不能写入" | 免安装版放在 Program Files、只读盘等不可写的位置：它不会自己更新，请到发布页下载新的 zip，解压后覆盖程序文件（保留 `portable` 文件和 `data` 文件夹） |

用户端永远不会安装验签没通过的包：包先整个下到内存、验签通过后才落盘，启动安装器前还会再核对一次文件哈希。
