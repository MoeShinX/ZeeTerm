# 面向用户的更新说明

这个目录放每个版本的**更新说明**，**每个版本两份**：

| 文件 | 语言 | 用在哪 |
| --- | --- | --- |
| `<版本>.md`，例如 `0.4.0.md` | 中文 | `latest.json` 的 `notes`：更新窗口的中文界面显示它；老版本客户端只认这一个字段 |
| `<版本>.en.md`，例如 `0.4.0.en.md` | 英文 | GitHub Release 页的正文（末尾自动附上各安装包的 SHA-256）；`latest.json` 的 `notes_en`：更新窗口的英文界面优先显示它 |

发版时（GitHub 发版工作流，以及本机备用的 `scripts/release.ps1`）自动读这两份。**缺哪一份都发不了版**：`scripts/bump-version.ps1` 和 GitHub 发版工作流的校验任务直接报错（只有本机备用脚本在没有中文文件时会退回 `CHANGELOG.md` 里的 `## [<版本>]` 一节，英文没有退路），见 `docs/RELEASING.md` 的"更新说明"。

两份是各写各的同一件事：条目一一对应、结构一致，英文不是机器逐字翻译，按英文用户的习惯写就好。

## 怎么写

写给普通用户看：说清"能做什么 / 有什么变化"，几条要点就够了。不要写脚本名、内部实现、提交号，那些留在 `CHANGELOG.md`。

建议的结构：一句话概述，加 `### 新功能`、`### 改进`、`### 修复` 三节（没有的就省掉），每节几条，**每条一行**。英文版对应 `### New`、`### Improved`、`### Fixed`。

```markdown
这一版重新设计了更新窗口，并修复了几个连接问题。

### 新功能

- 更新窗口能看清更新内容，还可以选中复制。
- 新增「跳过此版本」。

### 改进

- 下载更新时显示已下载大小和速度。

### 修复

- 修复断网重连后终端偶尔不刷新的问题。
```

```markdown
This release redesigns the update window and fixes a few connection problems.

### New

- The update window shows the release notes clearly, and you can select and copy them.
- New "Skip this version" button.

### Improved

- The update window shows the downloaded size and speed.

### Fixed

- Fixed the terminal sometimes not refreshing after a dropped connection is restored.
```

## 能用什么

更新窗口把说明渲染成 Markdown：标题（`###`）、列表（可以嵌套）、**粗体**、`行内代码`、代码块、分隔线、表格都支持。出于安全：

- HTML 不会被执行，原样显示成文字；
- 图片不会加载（显示替代文字），不要靠图片说明问题；
- 只有 `https://` 链接能点开，用系统浏览器打开；`http://`、`mailto:` 等其他链接只显示文字。

GitHub Release 页是 GitHub 自己渲染的，所以英文版里的内容也要保持这个约束：更新窗口显示的是同一份文字。

## 注意

- 文件用 UTF-8 编码，带不带 BOM 都可以。
- 英文版不要自己写 SHA-256 一节：发版脚本会在正文末尾自动附上 `### SHA-256`，写了会重复。
- 已经发布的版本不要回头改这里的文件：更新窗口显示的是发布时写进 `latest.json` 的那一份。
