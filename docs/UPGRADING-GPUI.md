# 升级 GPUI 与 gpui-component

ZeeTerm 的界面建在 GPUI 和 gpui-component 上。这份文档说明它们现在怎么固定版本、什么时候值得升级、升级时怎么做。

## 现在怎么固定

| 依赖 | 写法 | 位置 |
|---|---|---|
| GPUI | `gpui = { package = "gpui-pre", version = "=0.3.10" }` | `native/zeeterm-gpui/Cargo.toml` |
| gpui-component | `gpui-kit = { version = "=0.8.1" }`，实际编译的是仓库里的副本 `native/vendor/gpui-component-0.8.1`（通过 `native/Cargo.toml` 的 `[patch.crates-io]`） | `native/zeeterm-gpui/Cargo.toml`、`native/Cargo.toml` |

- 版本号用 `=` 写死，`native/Cargo.lock` 提交在仓库里，CI 的编译和测试都带 `--locked`。上游发多少新版本，ZeeTerm 编出来的东西都不变，**只有我们主动升级才会变**。
- `gpui-pre` 不是 Zed 官方发布的 crate，是从 Zed 仓库打的快照包（0.3.10 的描述写明是 `zed@08cbb88`），源码仓库是 `zed-industries/zed`。
- gpui-component（上游仓库 `longbridge/gpui-kit`）的副本上有本地改动：键盘焦点在按钮上时画清楚的焦点框（V2-U57）。来源、校验值、改了什么、为什么不能在应用里做，都写在 `native/vendor/gpui-component-0.8.1/PROVENANCE`；完整改动见同目录的 `LOCAL-CHANGES.patch`。

## 什么时候升级

不追每个新版本。GPUI 还没到 1.0，几乎每次升级都有不兼容的改动；应用里有一百多个文件直接用到 GPUI 和 gpui-component，升一次的成本不小。有下面任意一条理由才升：

- 上游修了 ZeeTerm 碰到的问题，比如输入法、渲染、窗口、Linux / macOS 平台上的毛病；
- 出了安全公告：CI 的「依赖审计」会报出来，登记和放行规则见 `docs/known-advisories.md`；
- 需要的功能只有新版有。

`gpui-pre` 和 gpui-component 要**一起升**：gpui-component 的每个版本都绑定一个特定的 GPUI 版本，只升其中一个通常编译不过。

## 升级步骤

每次升级单独一张任务卡、单独一个 PR，不和功能改动混在一起，出了问题好定位、好回退。

1. **选版本**：先选 gpui-component 的目标版本，看它的 `Cargo.toml` 依赖哪个 `gpui-pre`，两个一起定。读两边从当前版本到目标版本的更新记录，列出不兼容的改动。
2. **换 gpui-component 副本**：
   - 下载目标版本的 `.crate`，解压到 `native/vendor/gpui-component-<新版本>`，记下它的 SHA-256。
   - 先看上游是否已经给按钮画了可见的键盘焦点框。已经有了，就不再需要本地改动，直接删掉 vendor 目录和 `[patch.crates-io]` 里那一行，改用 crates.io 上的原版。
   - 还没有的话，把 `LOCAL-CHANGES.patch` 打到新副本上（上游代码变了就手动移植），重新生成 `LOCAL-CHANGES.patch`，按旧格式更新 `PROVENANCE`（来源、版本、SHA-256、改了什么、怎么核对）。
   - 删掉旧版本的副本目录，同步改 `native/Cargo.toml` 的 `exclude` 和 `[patch.crates-io]` 路径。
3. **改版本号**：只有 `native/zeeterm-gpui` 直接依赖它们。改它 `Cargo.toml` 里所有 `gpui-pre` / `gpui-kit` 的版本号（包括 `[dev-dependencies]` 里带 `test-support` 的那份），然后在 `native/` 下执行 `cargo update -p gpui-pre -p gpui-kit` 更新 `Cargo.lock`。
4. **查许可**：看新版本带进来的新依赖。只接受与 GPL-3.0-or-later 兼容的许可（MIT / BSD / ISC / Apache-2.0 这类），需要记的写进 `native/NOTICE`。
5. **修编译**：按第 1 步列出的不兼容改动修应用代码，跑 `powershell -File scripts/gates.ps1`，最后一行必须是「全部通过。」。Linux / macOS 专用的代码本机编译不到，靠 PR 上的 CI 在三个平台各编一遍。
6. **手动冒烟**：GPUI 的改动很多测不出来，要实际用一遍。至少覆盖：
   - 中文输入法：在终端、编辑器、普通输入框里打字和选字；
   - 终端：显示、选中、复制粘贴、滚动、半透明背景；
   - 窗口：多窗口、托盘、最小化恢复、高分屏缩放；
   - 对话框：模态框、文件选择框、右键菜单；
   - 键盘：快捷键、Tab 切换焦点、按钮的焦点框；
   - 关键页面：截图和升级前对比（截图按 `AGENTS.md` 的截图隐私要求来）。
7. **发版**：和普通改动一样合进 main，跟下一个版本一起发。发布后出问题，下一个版本把这个 PR 回退掉即可。

## 如果 gpui-pre 不再更新

`gpui-pre` 不是 Zed 官方发布的包，以后可能不再更新。到那时有两条退路：

- 改成直接引用 Zed 仓库的某个固定提交（`git = "https://github.com/zed-industries/zed", rev = "<提交>"`）；
- 像 gpui-component 一样，把某个版本拷进 `native/vendor/` 自己维护，写好 `PROVENANCE`。

两条路都要先确认 gpui-component 能跟着用同一份 GPUI，也就是要同时调整它的依赖。
