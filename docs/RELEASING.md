# 发版说明

ZeeTerm 的发布物放在 GitHub Release：Windows 安装包 `ZeeTerm-Setup-<版本>.exe`、更新清单 `latest.json` 和校验文件 `SHA256SUMS.txt`。已安装的程序启动时（以及开着时每 6 小时）读取最新 Release 里的 `latest.json`，发现新版本后下载安装包，**用发布者的更新签名公钥验签通过才会安装**。这份文档说明怎么发版、签名密钥怎么管、怎么回滚、更新失败怎么查。

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

1. **写更新说明**：`docs/release-notes/<版本>.md`，面向用户（写法见下面"更新说明"一节）；开发者看的改动写进 `CHANGELOG.md` 的 `## [Unreleased]`。
2. **改版本号**（在 `main` 最新代码上）：

   ```powershell
   powershell -File scripts/bump-version.ps1 -Version 1.0.0            # 加 -DryRun 先演练：照常检查和修改，结束后原样还原
   ```

   它检查：版本号格式正确且高于当前版本、`docs/release-notes/1.0.0.md` 存在且不空、tag `v1.0.0` 还不存在、CHANGELOG 有 `[Unreleased]` 一节；然后改全部版本号来源（见下面"版本号的来源"）、把 CHANGELOG 的 `[Unreleased]` 内容切成 `## [1.0.0] - <今天>`，最后打印要执行的 git 命令。**不提交、不打 tag、不推送。**
3. **提交并推到 main**，等 main 上的 CI 通过：

   ```powershell
   git add Cargo.toml Cargo.lock native/Cargo.toml native/Cargo.lock CHANGELOG.md docs/release-notes/1.0.0.md
   git commit -m "chore(release): v1.0.0"
   git push origin main
   ```
4. **打 tag 并推送**（这一步触发发版工作流）：

   ```powershell
   git tag v1.0.0
   git push origin v1.0.0
   ```

   tag 必须叫 `v<版本>`，并且指向已经在 `main` 上的提交。
5. **看 Actions 页**的 Release 工作流。它依次做：
   1. 校验：tag 去掉 `v` 等于两个 `Cargo.toml` 的版本；检出的提交就是 tag 指向的提交且在 `main` 上；`docs/release-notes/<版本>.md` 存在；`updater.json` 的公钥不是占位值；这个 tag 的 Release 还不存在。
   2. 装 Rust（按 `rust-toolchain.toml`），跑 `scripts/gates.ps1`（和本机同一套门禁；runner 上没有 `ZSSH_TEST_*`，联网用例会自己跳过）。门禁里 release 类的两步用 `gate` 配置（`Cargo.toml` 里 `inherits = "release"`，只差 LTO / 并行代码生成单元，`debug-assertions` 同为 false，测试钩子行为一致），为的是省编译时间；**安装包本身仍由第 3 步用完整的 release 编译**。
   3. 装 NSIS，打安装包 `native/installer/build.ps1`。
   4. 把 Secret 里的私钥写到 runner 临时目录的一个文件，调 `scripts/release-sign.ps1`：核对 `updater.json` 的公钥与私钥是一对（对不上直接失败）→ 签名（签名里带 `version:`）→ 生成 `latest.json` → 用公钥按应用里的验签代码再验一遍 → 算 SHA-256，写 `SHA256SUMS.txt` 和 Release 正文。
   5. **无论成败都删掉临时私钥文件**（`if: always()` 的清理步骤）。
   6. `gh release create`：上传安装包、`latest.json`、`SHA256SUMS.txt`，标题 `ZeeTerm <版本>`，正文是更新说明加安装包 SHA-256。同名 Release 已存在时失败，不覆盖。
6. **下载核对**：从 Release 页下载安装包，`Get-FileHash` 对一下 `SHA256SUMS.txt`；再用一个旧版本程序点「设置 → 关于 → 检查更新」，确认能发现、下载并校验通过（不必点安装）。

`latest.json` 里的下载地址是 `https://github.com/MoeShinX/ZeeTerm/releases/download/v<版本>/<安装包文件名>`，所以 tag 必须叫 `v<版本>`、安装包文件名不能改。程序读的更新源是 `https://github.com/MoeShinX/ZeeTerm/releases/latest/download/latest.json`（`native/zssh-update/updater.json` 的 `endpoints`），即最新 Release 里的 `latest.json`。

工作流日志里不会打印私钥和口令（GitHub 会对 Secret 打码，脚本也不 echo，签名工具不开详细输出）；第一次发版后顺手翻一遍"签名"步骤的日志确认。

### 失败了怎么办

- **工作流在"创建 Release"之前失败**：还没有发布任何东西。代码有问题就修好合进 main，**发新的补丁版本**（最稳，不用动 tag）；只是 runner 抽风、Secrets 没设好之类，不改代码的话，在 Actions 页对该次运行点 *Re-run*，或者 *Run workflow*（从 `main` 触发，输入已有的 tag，如 `v1.0.0`）。
- **"创建 Release"失败或半途中断**：先到 Releases 页看有没有残留的 Release（含草稿）。有就整个删掉（附件一起），再重跑；没有就直接重跑。
- **Release 已经发布了才发现有问题**：见下面"回滚与事故"，不要在同一版本号下替换附件，发新的补丁版本。

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

用现生成的测试密钥（带随机口令，放 `C:\zssh-v2-data\r2-keys\`，跑完自动删）和一个充数文件，把 GitHub 路径的签名步骤（`release-sign.ps1`）和本机路径（`release.ps1 -DryRun`）各跑一遍，核对两边 `latest.json` / `SHA256SUMS.txt` / Release 正文的格式一致，再做几个反例（私钥与公钥对不上、口令不对、公钥是占位值、安装包被改……）确认都会失败。不读 `%USERPROFILE%\.zeeterm` 下的任何东西，也不会改仓库（`release.ps1 -DryRun` 改的版本号会原样还原）。

单跑本机路径的演练：

```powershell
powershell -File scripts/release.ps1 -Version 9.9.9 -DryRun -KeyPath <测试私钥> -OutRoot <临时目录> `
    -SkipGates -PrebuiltInstaller <任意文件> -NotesFile <任意说明文件>
```

测试密钥只放在临时目录，不要用正式密钥演练。

## 更新说明

更新说明会原样显示在用户的更新窗口里（窗口会把它渲染成带标题、列表、粗体的版式），也是 GitHub Release 页的正文。**写给普通用户看**：一句话概述加几条要点，每条一行说清"能做什么 / 有什么变化"；不要写脚本名、内部实现和开发者才关心的细节（那些留在 `CHANGELOG.md`）。

GitHub 工作流**只认** `docs/release-notes/<版本>.md`（没有或是空的就在最前面失败，不退回 CHANGELOG）。本机备用脚本 `release.ps1` 按下面的优先级取说明，取到哪份会在输出里打印出来：

1. `-NotesFile <文件>`（临时指定，优先级最高）；
2. `docs/release-notes/<版本>.md`（推荐：每个版本一份面向用户的说明，格式见 `docs/release-notes/README.md`；文件存在但是空的会直接报错）；
3. `CHANGELOG.md` 里的 `## [<版本>]` 一节（没有 `docs/release-notes/<版本>.md` 时的退路，内容偏技术，不推荐直接给用户看）。

更新窗口只渲染安全的 Markdown：HTML 不执行、图片不加载（显示替代文字）、只有 `https://` 链接能点开（用系统浏览器打开），所以说明里别靠图片和 HTML 传达信息。

## 版本号的来源

版本号只在这几处，**统一由 `scripts/bump-version.ps1` 修改**（本机备用路径 `release.ps1` 用的是同一份实现，在 `scripts/release-lib.ps1`），不要手改：

| 位置 | 说明 |
| --- | --- |
| `Cargo.toml` 的 `[workspace.package] version` | 核心库工作区 |
| `native/Cargo.toml` 的 `[workspace.package] version` | 原生应用工作区；程序里显示的版本、更新比较用的"当前版本"都来自它（编译时写入） |
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
| 没有适用于当前平台的安装包 | `latest.json` 的 `platforms` 里没有 `windows-x86_64` |

用户端永远不会安装验签没通过的包：包先整个下到内存、验签通过后才落盘，启动安装器前还会再核对一次文件哈希。
