ZeeTerm - SSH / SFTP / serial terminal client
=============================================

This is the portable tar.gz package for Linux x86_64 (glibc 2.35 or newer,
Wayland or X11, a Vulkan driver).  这是 Linux x86_64 的 tar.gz 安装包
（glibc 2.35 及以上，Wayland 或 X11，需要 Vulkan 驱动）。

Install / 安装
--------------
    ./install.sh                  install into ~/.local   装到 ~/.local
    ./install.sh --prefix DIR     install into DIR        装到 DIR

No root needed.  The app menu entry and the icon are installed too.  The
programs go to <prefix>/lib/zeeterm/ (zeeterm and the internal helper zssh-mcp);
the only command on your PATH is the symlink  <prefix>/bin/zeeterm .
不需要 root；会同时装上应用菜单项和图标。程序放在 <prefix>/lib/zeeterm/（zeeterm 和内部辅助程序
zssh-mcp），PATH 里只有软链接 <prefix>/bin/zeeterm。

You can also run it without installing:  ./zeeterm
也可以不安装，直接运行：./zeeterm

Uninstall / 卸载
----------------
    <prefix>/share/zeeterm/uninstall.sh

Your data (hosts, settings, credential store) is never removed.  It lives in
~/.local/share/ZeeTerm  (or $XDG_DATA_HOME/ZeeTerm, or the directory given by
ZSSH_DATA_DIR).  你的数据不会被删除。

Notes / 说明
------------
* zssh-mcp (the AI tool bridge) must stay in the same directory as the real
  zeeterm program, so do not move one without the other.  The built-in updater
  replaces both together.
  zssh-mcp（AI 工具桥）必须和真正的 zeeterm 程序在同一个目录，不要只移动其中一个；应用内更新会把两者一起替换。
* Saved passwords and keys are protected by the desktop keyring (Secret
  Service: GNOME Keyring, KeePassXC, KWallet ...).  When no such service is
  running at all, ZeeTerm keeps its key in a file in the data directory
  instead (the settings page says so).
  保存的密码和密钥由桌面钥匙串（Secret Service）保护；完全没有这类服务时，密钥改存数据目录里的文件（设置页会提示）。
* If the window stays blank, install a Vulkan driver (for example
  mesa-vulkan-drivers).  窗口一片空白时请安装 Vulkan 驱动。

Project / 项目:  https://github.com/MoeShinX/ZeeTerm
License / 许可:  GPL-3.0-or-later  (see LICENSE and NOTICE)
