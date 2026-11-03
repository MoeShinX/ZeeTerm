# 发版说明

ZeeTerm 的发布物放在 GitHub Release：Windows 安装包 `ZeeTerm-Setup-<版本>.exe` 和更新清单 `latest.json`。已安装的程序启动时（以及开着时每 6 小时）读取最新 Release 里的 `latest.json`，发现新版本后下载安装包，**用发布者的更新签名公钥验签通过才会安装**。这份文档说明签名密钥怎么管、怎么发版、怎么回滚、更新失败怎么查。

## 一次性准备：更新签名密钥

更新签名用 minisign 格式的密钥对，由发版人自己生成，**私钥不进仓库**。

```powershell
powershell -File scripts/updater-keygen.ps1                    # 私钥写到 %USERPROFILE%\.zeeterm\updater.key
powershell -File scripts/updater-keygen.ps1 -WriteUpdaterJson  # 同时把公钥写进 native/zssh-update/updater.json
```

脚本会提示给私钥设口令（可留空，不推荐），然后打印公钥。不需要安装 minisign.exe：签名和验签用的是程序里同一套代码。

1. **把公钥填进 `native/zssh-update/updater.json` 的 `pubkey`**（用了 `-WriteUpdaterJson` 就已经写好了），提交。公钥可以公开。公钥还是占位值（`PLACEHOLDER…`）的构建不会检查、下载或安装任何更新，更新窗口只显示"更新签名未配置"。
2. **备份私钥文件和口令**（密码管理器或加密存储）。私钥不要放进仓库、不要发给任何人、不要上传到 CI。
3. 密钥目录里已经有 `updater.key` 时脚本拒绝再生成，避免覆盖。

> 公钥一旦随正式版发出去就不能换：换了公钥，装着旧版本的用户验不了新签名，收不到更新，只能手动重新下载安装。所以私钥丢失或泄露是最严重的事故，见下面"回滚与事故"。

## 发版步骤

前提：在 `main` 分支、已跟踪的文件没有未提交改动、`CHANGELOG.md` 里有 `## [<版本>]` 一节（写给用户看的更新说明；也可以用 `-NotesFile` 单独指定）。

```powershell
powershell -File scripts/release.ps1 -Version 1.0.0
```

脚本依次做：

1. 校验版本号格式、必须高于当前版本、在 `main`、工作区干净、`v<版本>` tag 不存在。
2. 核对 `updater.json` 里的公钥与私钥是一对（对不上直接停，免得发出一个用户永远装不上的版本）。
3. **统一改版本号**（见下节）。
4. 跑门禁 `scripts/gates.ps1`（`-SkipGates` 可跳过，门禁刚跑过时用）。
5. 现编安装包：`native/installer/build.ps1`，产物 `ZeeTerm-Setup-<版本>.exe`。
6. 用私钥签名：签名的 trusted comment 里带 `version:<版本>`，程序验签时要求它与清单公告的版本一致；签完立即用 `updater.json` 的公钥自检。
7. 生成 `latest.json`（`version`、`notes`、`pub_date`、`platforms.windows-x86_64` 的 `url` 和 `signature`），再按程序里的验签逻辑把成品验一遍。
8. 产物写到 `dist\<版本>\`（`dist/` 已在 `.gitignore`）：安装包、`.sig`、`latest.json`、`RELEASE_NOTES.md`，并打印安装包的 SHA-256。

脚本**不推送、不打 tag、不创建 Release**。最后打印下一步命令，确认产物没问题后手动执行：

```powershell
git add Cargo.toml Cargo.lock native/Cargo.toml native/Cargo.lock CHANGELOG.md
git commit -m "chore(release): v1.0.0"
git tag v1.0.0
git push origin main v1.0.0
gh release create v1.0.0 dist/1.0.0/ZeeTerm-Setup-1.0.0.exe dist/1.0.0/latest.json `
    --repo MoeShinX/ZeeTerm --title "ZeeTerm 1.0.0" --notes-file dist/1.0.0/RELEASE_NOTES.md
```

`latest.json` 里的下载地址是 `https://github.com/MoeShinX/ZeeTerm/releases/download/v<版本>/<安装包文件名>`，所以 tag 必须叫 `v<版本>`、安装包文件名不能改。程序读的更新源是 `https://github.com/MoeShinX/ZeeTerm/releases/latest/download/latest.json`（`native/zssh-update/updater.json` 的 `endpoints`），即最新 Release 里的 `latest.json`。

发布后用一个旧版本程序点一次「设置 → 关于 → 检查更新」，确认能发现、下载并校验通过（不必点安装）。

### 演练

```powershell
powershell -File scripts/release.ps1 -Version 9.9.9 -DryRun -KeyPath <测试私钥> -OutRoot <临时目录> `
    -SkipGates -PrebuiltInstaller <任意文件>
```

演练用测试密钥和一个充数的文件走完整个流程：版本号改完立即还原（`git status` 看不到变化），不检查分支和工作区，不编译安装包。产物不能发布。测试密钥只放在临时目录，不要用正式密钥演练。

## 版本号的来源

版本号只在这几处，**统一由 `release.ps1` 修改**，不要手改：

| 位置 | 说明 |
| --- | --- |
| `Cargo.toml` 的 `[workspace.package] version` | 核心库工作区 |
| `native/Cargo.toml` 的 `[workspace.package] version` | 原生应用工作区；程序里显示的版本、更新比较用的"当前版本"都来自它（编译时写入） |
| `Cargo.lock`、`native/Cargo.lock` | 跟随工作区版本的本仓库包（`zssh-*`），否则 `--locked` 构建失败 |
| 安装包文件名、版本资源、"应用和功能"里的显示版本 | `build.ps1` 读 `native/Cargo.toml`，安装脚本里不写死版本 |

有单元测试（`every_place_that_carries_the_version_agrees`）核对这几处一致。

## 回滚与事故

- **新版本有问题，想让用户停在上一版**：把该 Release 标成预发布或删除它，`releases/latest` 就会指回上一个正式 Release，程序读到的 `latest.json` 随之回到上一版；已经升级的用户不会被降级（程序只升不降）。要立刻止血又没有可回退的版本时，删除该 Release 即可，用户端只会显示"读取更新说明失败"，不会装任何东西。
- **安装包有问题但清单没问题**：删掉出问题的附件，用户端下载会失败并提示，不会安装。修好后发新的补丁版本，不要在同一个版本号下替换附件（已经下载过的用户会遇到新旧文件对不上的情况）。
- **私钥泄露或丢失**：没有在线补救办法。公钥内置在已发布的程序里，只能发一个新的安装包（用新的密钥对、新公钥），让用户手动下载安装；旧版本程序不会自动更新到它。所以私钥要备份好、口令要妥善保管。

## 更新失败排查

用户端的更新窗口会给出具体原因：

| 提示 | 常见原因 |
| --- | --- |
| 更新签名未配置 | 这个构建的 `updater.json` 公钥还是占位值；用 `updater-keygen.ps1` 生成密钥并填写后重新发版 |
| 读取更新说明失败 / HTTP 404 | 还没有任何 Release，或 `latest.json` 没作为附件上传；检查 Release 附件 |
| 读取更新说明失败 / 连不上 | 网络或代理问题；「设置 → 连接 → 软件网络」决定检查更新走哪个代理 |
| 签名校验未通过 | 安装包被改过或下载不完整，或不是用这把密钥签的；确认用的是 `release.ps1` 产出的那份文件 |
| 签名版本 X 与公告版本 Y 不一致 | `latest.json` 的 `version` 与签名里的 `version:` 不同（手动改过清单，或把别的版本的签名搬了过来） |
| 下载失败 | 网络中断、被限流，或附件被删；重试即可 |
| 没有适用于当前平台的安装包 | `latest.json` 的 `platforms` 里没有 `windows-x86_64` |

用户端永远不会安装验签没通过的包：包先整个下到内存、验签通过后才落盘，启动安装器前还会再核对一次文件哈希。
