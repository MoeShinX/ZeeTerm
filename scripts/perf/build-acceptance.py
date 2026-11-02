# V2-G13a: build docs/reports/V2-G13a/acceptance.csv from the three read-only source tables.
#
#   python scripts/perf/build-acceptance.py <legacy-acceptance.csv> <acceptance-matrix.csv> <feature-triage.csv> <out.csv>
#
# The source tables are only read. Every row gets one of five states:
#   pass      -> "通过"     : the report/test/scenario named in `evidence` actually ran this behaviour
#   todo      -> "未做"     : the native app does not have it (see the difference table)
#   manual    -> "需手测"   : implemented, but real input / OS / device / real service is what is left to check
#   missing   -> "条件缺失" : no condition to test it (real cloud account, MFA server, second host, paused real-agent runs ...)
#   na        -> "不适用"   : with the reason in `note`
# `evidence` names the V2 report (and scenario/screenshot/test) that holds the proof; "G04/G06" = the
# earlier native-prototype reports under the codex report folder (not in this repository).
import csv
import sys

P, T, M, C, N = "通过", "未做", "需手测", "条件缺失", "不适用"

# ---------------------------------------------------------------------------------------------- legacy
LEG = {
    # 1 install / first start
    1: (M, "V2-G12 §三 静默安装", "静默安装/升级/卸载全跑过；图形向导各页没点过"),
    2: (P, "V2-G07a 01-main-dark.png", "启动后只有主机页、没有空终端标签；图标是占位图标"),
    3: (M, "V2-G07a §五", "标题栏按钮/拖动/双击最大化只验了命中判定，没真点"),
    4: (P, "V2-G06x a1-appearance 场景（新数据目录）", "新数据目录默认深色 + Dracula + Tokyo Night，字号 12 / 行高 1.3 / 回滚 10000，五条默认高亮规则有；场景里逐项断言"),
    5: (P, "V2-G12 §一 关于页", "名称/版本/开源许可都对（旧文档写的 0.1.0 已过期）"),
    6: (N, "REL-02", "ZeeTerm 不依赖 WebView2，没有引导项；本卡实测进程树里只有一个进程"),
    7: (P, "V2-G12 §三 用例 3", "静默卸载后数据目录与 zssh.db 保留；从「应用和功能」图形卸载见手测清单"),
    # 2 host vault
    8: (P, "V2-G07b 窗口场景", "分组新建/子分组/重命名/删除（主机移到未分组）"),
    9: (P, "V2-G07b 窗口场景", ""),
    10: (P, "V2-G07b 窗口场景", "删除二次确认"),
    11: (P, "V2-G07b 窗口场景 02-vault-list.png", "网格/列表、搜索名称/地址/用户/标签"),
    12: (P, "V2-G07b 窗口场景", "user@host[:port] 回车走快速连接并先问密码"),
    13: (P, "V2-G07b 04-keychain.png / V2-F3", "被使用的凭据拒绝删除并指出主机"),
    14: (P, "V2-G07b 05-proxies.png / V2-F3", ""),
    15: (P, "V2-G07a §二", "重启后主机库、分组、凭据、代理都在"),
    # 3 connect/auth
    16: (P, "G04.2.2 主机指纹", "首次弹确认、信任后写 known_hosts、第二次不再问（原生用系统消息框）"),
    17: (T, "native ssh_interact.rs", "密码输入框没有「保存密码」勾选；只能在主机/凭据里预先存密码"),
    18: (M, "—", "输错密码的提示和「错密码不被保存」没有专门场景（因为输入框本来就不保存）"),
    19: (M, "V2-G07b §四", "清除指纹按钮没点过"),
    20: (C, "V2-F6 §五 / G04.2.2", "需要第二台服务器或重装密钥；只有 ssh_interact 单元测试"),
    21: (P, "G04.2.2；V2-G13a perf（密钥登录的联网会话）", "测试服务器只接受密钥登录，所有联网场景都走它"),
    22: (C, "—", "没有带口令的测试私钥（口令测试变量未配）"),
    23: (C, "—", "没有 agent 测试条件"),
    24: (C, "G04.2.2", "没有两步验证服务器；多题输入框界面做了，没联调"),
    25: (P, "见 LEG-044/045", ""),
    # 4 proxy / jump
    26: (P, "G04.2.2；V2-G07b 转发联网验证", "经 SOCKS5 连接（本卡全部联网场景都走 SOCKS5）"),
    27: (C, "—", "测试服务器只能经代理访问，直连没法验"),
    28: (C, "—", "没有 HTTP 代理；core 有单测"),
    29: (M, "—", "代理不可用时的报错速度没有专门场景"),
    30: (C, "G04.2.2 阻塞清单", "只有一台测试服务器，跳板机真实链路没验"),
    # 5 terminal
    31: (P, "G04.2.2 / V2-F1", "输入命令、回车、Ctrl+C"),
    32: (M, "G04.2.2 less；V2-F1", "less 验过；vim/htop/tmux 没验（测试服务器没装）"),
    33: (P, "G04.2.2 窗口缩放", "stty size 跟着窗口变"),
    34: (M, "TRM-03 单测", "中文显示有单测；中文输入法候选框位置要真人看"),
    35: (M, "—", "微软拼音/百度拼音的自动切英文只能手测"),
    36: ("PERF36", "V2-G13a perf", ""),
    37: (P, "G06 g06-copy-select / paste / bracket 截图", "复制粘贴、选中即复制、右键粘贴、多行确认、括号粘贴"),
    38: (N, "V2-G06x §一-4", "0.2.14 没有终端内搜索（Ctrl+Shift+F 是文件面板，计划 G06 第 1 条不新增）；按 0.2.14 不做，不是缺口。滚动回看有"),
    39: (P, "G06 g06-tabs-*；G04.2.2 同连接多标签", "新建/切换/关闭标签，第二个标签复用连接不再问密码"),
    40: (P, "V2-G06x p1/p2/p3 场景 + 01~03 截图", "Ctrl+Shift+D 左右、Ctrl+Shift+E 上下，可接着拆；四窗格各自输入输出、广播、拖分隔条 / 双击平分、关中间窗格后布局；窗格复用已认证连接（同一条连接上开 shell）；真鼠标拖分隔条走 gpui 的鼠标分发"),
    41: (P, "V2-G06x a1/s1 场景 + 04~10 截图", "界面主题 / 皮肤 / 终端配色 / 字体 / 字号 / 行高 / 回滚行数 / 光标样式都有，已开的终端立即生效；光标只有方块和下划线（0.2.14 另有竖线）；设置页控件的真鼠标点击没有逐个点（驱动调用的是和按钮同一批方法，滑块用真鼠标拖过）"),
    42: (M, "—", "长时间 vim/less 有无错位只有真人看得出"),
    # 6 disconnect
    43: (M, "V2-F1 §四", "真断网弹遮罩只有回环/杀 shell 的等价场景；真断网没做"),
    44: (P, "G04.2.2 重连；V2-F1", "原窗格重连、之前输出还在；按键是 Ctrl+Shift+R 或标签右键，不是 R（差异）"),
    45: (P, "G04.2.2 重连", "远端 exit 后显示断开，重连成功"),
    # 7 status bar
    46: (P, "V2-G11 §一-4 / V2-G07a", "状态栏和服务器概览（联网 Debian）"),
    47: (M, "—", "服务器上跑 yes 看 CPU 变化没做"),
    48: (M, "—", "断线后状态栏不显示过期数据没有专门场景"),
    # 8 AI
    49: (P, "V2-G07c 03-ai-panel-chat.png", ""),
    50: (C, "V2-G10a §五 / V2-G10b §四", "真实 opencode 经共用桥执行工具没跑；纯对话跑过"),
    51: (C, "V2-G10a §五", "本机没有 codex"),
    52: (C, "V2-00 真实 Claude 测试暂停", "不再复制真实凭据跑真实 Claude"),
    53: (P, "V2-G10b 场景 b1（假智能体+真终端）", "手动确认：卡片、同意/拒绝；真实智能体见 LEG-050~052"),
    54: (P, "V2-G10a / V2-G10b 场景 b1", "自动模式；切回手动恢复确认"),
    55: (P, "V2-G10b §一-3 / b1", "@ 另一台主机、目标标签"),
    56: (P, "V2-G10b §一-2 / b1", "命令在用户终端里执行；不适合的情况写明原因改走后台通道"),
    57: (M, "V2-G10b §四", "停止按钮→stop_tools 走真桥，窗口里没真点过停止"),
    58: (P, "V2-G10b §一-1 / b1 04-write-approval-with-diff.png", "读/写文件、写入确认带差异；写系统路径与属主两行只有纯函数测试"),
    59: (P, "V2-G07c", "新对话清空"),
    60: (M, "V2-G10a §四", "未登录提示用假世界验过；额度用完/断网没真测"),
    61: (P, "V2-G10b 01-folded-group.png / 02-expanded-cards.png", "折叠成一行、展开成卡片"),
    62: (P, "V2-G10b b1/b2", "含失败标「其中 N 条失败」"),
    63: (P, "V2-G10b 04-write-approval-with-diff.png / 03-running-with-tab-dot.png", "等批准的和正在执行的不折叠"),
    64: (M, "V2-G10b", "历史会话回看折叠没有专门场景（继续会话要求有会话标识，F4）"),
    65: (M, "V2-F4 / V2-G07c", "Markdown 由 gpui-component 渲染；表格窄面板横向滚动、浅色深色可读性没单独看"),
    66: (M, "V2-F4", "代码块复制/整条消息复制没有专门场景"),
    67: (P, "V2-F4 §一", "链接点击只提示、不打开浏览器、图片不加载；但链接文字后不直接显示完整网址（见差异表）"),
    # 9.1 SFTP
    68: (P, "V2-G08a 09-dock / 05-queue", "双栏浏览、进入、返回"),
    69: (P, "V2-G08a 11-delete-dialog / 26-rename-dialog", ""),
    70: (P, "V2-G08a 联网（sha256 对账）", ""),
    71: (P, "V2-G08a 联网", "含空目录"),
    72: (P, "V2-G08a 联网", "中文和空格文件名；非 UTF-8 名 20-non-utf8-row.png"),
    73: (P, "V2-G08a 13-same-name-panel.png", "覆盖/跳过/重命名/续传 + 本批其余都这样"),
    # 9.2 dock
    74: (P, "V2-G08b follow-on-alpha.png", "跟随终端目录；手动换目录后不再跟随"),
    75: (T, "V2-G08b 差异清单", "标签拖到终端标签上停靠的手势没做，只有按钮；拖标签出窗口也没做"),
    # 9.3 drag
    76: (M, "V2-G08b §四", "gpui 的 FileDrop 到上传全路径验了；系统 OLE 拖放这一步没验"),
    77: (T, "V2-G08b 差异清单", "文件栏拖到终端插入远端路径有；资源管理器拖到终端插本地路径没做"),
    78: (M, "V2-G08b dnd-done.png", "驱动里的鼠标按下/移动/抬起走通；真实鼠标手感没测"),
    79: (M, "V2-G08a §四", "「选择文件夹…」是系统模态对话框，没点"),
    # 9.4 save safety
    80: (P, "V2-G08a 17-rename-overwrite-ask.png", ""),
    81: (P, "V2-G09 注入故障单测；core 原子保存", "没有真拔网线；注入故障验过上传失败不弄残、无 .zssh-tmp 残留"),
    82: (P, "V2-G09 recovery 场景", "启动恢复提示：重新上传/另存/放弃"),
    83: (P, "V2-G09", "稍后处理再回来"),
    84: (M, "—", "meta.json 同名文件没有专门场景（原生草稿格式与 0.2.14 不同，理论上不受影响）"),
    85: (C, "V2-G09 §五 / V2-F3 §四", "测试服务器是 root，属主保不住的场景没法真触发"),
    86: (M, "core 共用", "两个标签同时上传同一路径的保护在 core；原生窗口里没单独场景"),
    87: (C, "—", "没有不支持 posix-rename 的服务器"),
    88: (P, "V2-G09 edit-conflict", "编辑冲突不悄悄覆盖（按内容 SHA-256 比较）"),
    89: (P, "V2-G09", "另存为"),
    # 9.5 big files
    90: (M, "V2-G08a 21-large-upload-reconciled.png", "大文件上传对账验过；几百 MB 的速度/界面流畅度没单独测"),
    91: (M, "V2-F3 §四", "续传只验了按钮条件和决策；真断网续传没做"),
    92: (P, "V2-G08a 22-transfer-paused / 取消", "取消后不留半截文件"),
    # 9.6 menu
    93: (P, "V2-G08a 01-menu-file-dark.png", ""),
    94: (P, "V2-G08a 02-menu-blank-dark.png", ""),
    95: (P, "V2-G08a 菜单置灰原因", "原因直接写在项右边（不是悬停提示）"),
    96: (M, "V2-G08a", "窗口缩小/贴边的菜单摆放没专门验"),
    97: (M, "V2-G08a", "菜单键盘导航没专门验"),
    98: (P, "V2-G08b b4cdaa8 修复；V2-G08a 真键盘", "文件栏 Ctrl+C 与终端互不抢"),
    # 9.7 editor
    99: (P, "V2-G09 窗口场景", "标签、语法高亮、状态栏；再双击切到已开标签"),
    100: (M, "V2-G09 §二", "高亮是 syntect，十一种语言的逐个核对没做"),
    101: (P, "V2-G09 §二/§五", "搜索/替换/跳行/换行；查找面板没有正则/整词"),
    102: (P, "V2-G09 联网", "保存后服务器上是新内容（场景里保存走方法，Ctrl+S 按键本身没按）"),
    103: (P, "V2-G09 EDI-04 测试", "CRLF 与 BOM 保持"),
    104: (P, "V2-G09 edit-refused.png", "GBK/二进制/超大都拒绝并写原因"),
    105: (P, "V2-G09 edit-conflict", ""),
    106: (P, "V2-F3 / V2-G09", "保存/不保存/取消三选一"),
    107: (T, "V2-G09 差异清单 #4", "关闭 SFTP 连接时不为编辑器询问（编辑器自带通道，不受影响）；退出时询问有"),
    108: (P, "V2-G09 recovery", "草稿恢复"),
    109: (M, "V2-G09 §五", "断线只有注入故障单测，窗口里没点过「重新连接」"),
    110: (T, "V2-G09 差异清单 #7", "本地文件没有内置编辑器"),
    # 9.8 copy/paste
    111: (P, "V2-G08b 联网 paste-cp", "服务器端复制 + 提示条"),
    112: (P, "V2-G08b", "同目录粘贴默认重命名，「名字 - 副本」"),
    113: (P, "V2-G08b", ""),
    114: (P, "V2-G08b", "不能粘贴进自己"),
    115: (C, "V2-G08b §四", "只有一台测试服务器；同机两条连接 + 假服务器身份验了整条中转路径"),
    116: (P, "V2-G08b", "源连接关闭的提示"),
    117: (P, "V2-G08b", "远端与本地之间粘贴灰着并写原因"),
    118: (P, "V2-G08b", ""),
    119: (P, "V2-G08b", "没有剪切"),
    120: (C, "—", "没有受限 shell 的服务器"),
    # 9.9 properties
    121: (P, "V2-G08a 04-properties-dark.png", ""),
    122: (P, "V2-G08a 14 / 18-size-calculation-cancelled.png", ""),
    123: (P, "V2-G08a", ""),
    124: (P, "V2-G08a", ""),
    # 10 sync
    125: (P, "V2-G11 w1（WebDAV 桩）", "桩，不是真实 WebDAV"),
    126: (C, "V2-G11 §五", "没有真实 https WebDAV"),
    127: (M, "V2-G11", "http 的橙色警告和勾选确认没有专门场景"),
    128: (C, "V2-F5 §四", "桩不验登录；密码错只验了状态机和分类"),
    129: (C, "V2-G11 §二", "S3 只在桩上验路径风格；虚拟主机风格和真实 S3 没测"),
    130: (C, "V2-G11 §二", "桩验了 Access Key 错；真实 S3 没测"),
    131: (C, "—", "没有 GitHub 经典令牌"),
    132: (C, "—", "没有 GitHub 细粒度令牌"),
    133: (P, "V2-G11 w3（Gist 桩）", "令牌被撤销→状态条「同步已暂停：令牌已失效」；权限不够的细分提示没验"),
    134: (M, "—", "同步请求走默认代理没有专门场景"),
    135: (P, "V2-G11 w1", "主密码、强度条、勾选「我知道忘记就无法恢复」"),
    136: (P, "V2-G11 w1/w3", "立即同步；密文检查在桩上"),
    137: (M, "V2-F5", "记住主密码只有单测，没有重启场景"),
    138: (P, "V2-G11 w1", "改主密码"),
    139: (P, "V2-G11 w2（桩里的另一台电脑）", "第二台解开已有数据"),
    140: (M, "—", "第二台第一次同步不产生多余备份没有专门核对"),
    141: (P, "V2-G11 w1 并发冲突", "两边最终一致"),
    142: (M, "V2-G11 §五", "自动同步调度器在窗口里没跑过一拍（只验闸门）"),
    143: (P, "V2-G11 w2", "防误删：取消/继续/用云端覆盖"),
    144: (P, "V2-G11 w2", "冲突列表：改用另一份 / 我知道了"),
    145: (P, "V2-G11 w1/w2", "本地备份、两步确认恢复、保留份数"),
    146: (M, "—", "基线损坏只有状态条文案单测"),
    147: (P, "V2-G11 w3", "令牌失效后暂停自动同步、不再反复重试"),
    148: (M, "—", "断网后同步失败原因与自动恢复没专门场景"),
    149: (P, "V2-G11 w3 / 单测", "429 + Retry-After 到点前 0 个新请求"),
    150: (P, "V2-G11 单测", "主密码不对按「主密码不对」分类"),
    # 10.6 OneDrive
    151: (C, "V2-G11 w4（桩授权）", "没有 Azure client ID 和账号；桩验了授权页被打开、PKCE 参数、换令牌"),
    152: (C, "—", "同意页上的权限只能看真实微软页面"),
    153: (C, "V2-G11 §二", "登录之后的 Graph 读写没做（桩没有文件接口）"),
    154: (P, "V2-G11 w4", "取消后回调端口关闭、不留设置"),
    155: (P, "V2-G11 w4", "拒绝授权的提示和重试"),
    156: (P, "V2-G11 w4", "5 分钟超时（桩里缩短）"),
    157: (P, "V2-G11 w4", "没有内置 client ID 时按钮不可用，「高级」里可填自己的"),
    158: (C, "—", "需要真实微软账号"),
    159: (C, "V2-G11 §二", "登录失效只验了状态条文案"),
    160: (C, "—", "断开连接需要先真实登录"),
    161: (C, "—", "OneDrive 走代理没测"),
    162: (C, "—", "回调同时监听 127.0.0.1 和 [::1] 没有 IPv6 机器验"),
    # 11 window / layout
    163: (P, "V2-G07c 面板把手拖动", "AI/文件面板宽度拖动（真实鼠标）"),
    164: (T, "V2-G07a 差异清单", "只记住窗口化时的位置尺寸；最大化状态、多屏变化后的位置修正没做"),
    165: (T, "V2-G07a 差异清单", "最大化状态不保存"),
    166: (M, "V2-G07a §五", "高 DPI 没测"),
    167: (M, "V2-G07a §五 / V2-G12", "关闭到托盘的真实路径没手点；托盘不能被驱动"),
    168: (M, "—", "首次缩到托盘的系统通知"),
    169: (M, "V2-G12 §八", "托盘左键/双击/右键菜单没点过；单实例唤回见 G04"),
    170: (P, "V2-G07c 09-quit-confirm-ai-turn.png / V2-G11", "退出程序模式：有 AI 回合/传输时合并确认"),
    171: (M, "V2-G07c §四", "托盘「退出」走同一个 request_quit，托盘本身不能驱动"),
    172: (N, "V2-G12 §一", "试用版关闭自动更新，托盘永远没有「重启安装」"),
    173: (P, "V2-G08b files-close-confirm.png", "关文件窗口只关它"),
    174: (M, "V2-G07a §五", "窗口缩在托盘期间后台任务继续没有场景"),
    175: (M, "—", "缩在托盘时有提问叫出窗口没有场景"),
    # 12 settings
    176: (T, "V2-G06x / V2-G07a 差异清单", "主题/字体/字号/行高/回滚行数/光标/选中即复制/右键粘贴都有（回滚行数 V2-G06x 补上）；SFTP 传输设置不全"),
    177: (P, "V2-G10a §二", "默认智能体/执行模式"),
    178: (M, "V2-F5", "执行模式不跟着同步走没有两台电脑的场景"),
    179: (P, "V2-G12 about 截图", "版本、数据目录（脱敏）、打开所在文件夹"),
    180: (P, "V2-G07a / V2-G10a", "重启后设置还在"),
    # 13 quit
    181: (P, "V2-G07c 09-quit-confirm-ai-turn.png / 10-close-sftp-tab-confirm.png", "有 AI 回合/传输时关闭会提示"),
    182: (P, "V2-G12 §三 / V2-F6", "安装版退出后无 ZeeTerm 进程残留；Job Object 兜底"),
    183: (P, "V2-G13a startup 批次（每次强制结束后重启）", "ZeeTerm 连续 20 次强制结束后再启动都正常；库 integrity_check = ok、主机仍在；没有进程残留"),
    # 14 update
    184: (N, "V2-G12 §一", "试用版：检查更新只显示说明，没有更新请求；切换前再验（见手测清单「接管前」）"),
    185: (N, "V2-G12 §一", "同上"),
    186: (N, "V2-G12 §一", "同上；更新窗口流程 F2 有本地桩测试"),
    187: (N, "V2-G12 §一", "同上"),
    188: (N, "V2-G12 §一", "同上"),
    189: (N, "V2-G12 §一", "同上；安装器侧 /UPDATE /R 静默验过"),
    190: (N, "V2-G12 §一", "同上"),
    191: (N, "V2-G12 §一", "同上"),
    192: (N, "V2-G12 §一", "同上"),
    193: (N, "V2-G12 §一", "同上"),
    194: (N, "V2-G12 §一", "同上；验签/版本绑定的单测见 UPT-02/03"),
    # 15 resources
    195: ("PERF195", "V2-G13a perf", ""),
    196: (M, "V2-G13a perf（只做一个周期）", "多个小时/多周期不单调增长没做"),
}

# ---------------------------------------------------------------------------------------------- matrix
MAT = {
    "SCOPE-01": (P, "G00 + 本表", "本卡对 196 项逐条给出 ZeeTerm 状态"),
    "SCOPE-02": (N, "—", "流程规则，不是可测功能"),
    "LIC-01": (P, "V2-R1 §二", "LICENSE/NOTICE 核对；ZeeTerm 专属条目在 native/NOTICE"),
    "LIC-02": (M, "G00 reuse-ledger.csv", "需要人工做许可审查"),
    "FRAME-01": (P, "G01/G02；V2-G13a release 构建", "本卡 release 构建通过"),
    "FRAME-02": (P, "G00 framework-patch-review.csv", "补丁台账在；上游状态本卡没重新核实"),
    "BUILD-01": (P, "V2-G13a startup/idle 进程树", "进程树只有 ZeeTerm 一个进程，没有 msedgewebview2"),
    "BUILD-02": (P, "G00 / V2-G13a release 构建", ""),
    "BUILD-03": (C, "V2-G12 §一-5", "native.yml 在，但没推送、没在 GitHub 上跑过，不能说「必需检查」已生效"),
    "DATA-01": (P, "全部脚本 ZSSH_DATA_DIR", "本卡脚本拒绝 C:\\zssh-v2-data 之外的目录"),
    "DATA-02": (T, "V2-G07b §三", "库文件与 Tauri 版分开（zssh.db / data.db），没有导入或共享路径；同步和离线备份格式同一份 core"),
    "RUN-01": (P, "G04", ""),
    "RUN-02": (M, "V2-F4 §四 / V2-F6", "应用级 runtime 有；托盘退出时异步 shutdown 靠 Job Object 兜底，没实测"),
    "TRM-01": (P, "G04.2.2 live", ""),
    "TRM-02": (M, "G02/G04 less/鼠标上报", "vim/tmux 没测"),
    "TRM-03": (P, "G02 g02-*.png / 单测", "中文、emoji、宽字符；字体回退列宽只有单测"),
    "TRM-04": (P, "G03 / G04 账本测试", ""),
    "TRM-05": (P, "V2-F1 回环刷屏回归", "ACK 与关闭绕过写队列"),
    "TRM-06": ("PERF_TRM06", "V2-G13a perf", ""),
    "TRM-07": ("PERF_TRM07", "V2-G13a perf", ""),
    "TRM-08": (P, "G06 g06-bracket-*.png / V2-F1", ""),
    "TRM-09": (P, "V2-G06x p1/p2/p3", "标签复制/关闭/重连、输入广播、左右 + 上下分屏、分隔条拖动 / 双击恢复平分都保持"),
    "TRM-10": (P, "G05 g05-links.png", ""),
    "TRM-11": (P, "V2-G06x a1/s1/s2 + 单测", "字体 / 字号 / 行高 / 光标 / 皮肤 / 回滚行数都有；老的 ui-settings.json 里存过的值不被新默认覆盖（单测）；会话日志没动"),
    "TRM-12": (P, "G06 g06-highlight-*.png", "高亮规则编辑见 V2-G07a 差异清单（设置页只有总开关）"),
    "TRM-13": (P, "V2-G07c 01 / 02 截图", ""),
    "TRM-14": ("PERF_TRM14", "V2-G13a perf", ""),
    "WIN-01": (M, "—", "微软/百度拼音只能手测"),
    "WIN-02": (M, "V2-G07b §四", "敏感剪贴板排除格式/30 秒清除有单测；会改用户剪贴板的场景没跑"),
    "WIN-03": (M, "—", "普通复制不受清理影响没有场景"),
    "WIN-04": (M, "V2-G12 §八", "托盘不能驱动"),
    "WIN-05": (M, "—", "Explorer 重启后托盘恢复没测"),
    "WIN-06": (N, "V2-G12 §一", "ZeeTerm 先以独立身份并存试用，不共用数据；接管前再验"),
    "WIN-07": (M, "—", "唤回/首次缩托盘通知没手点"),
    "WIN-08": (M, "V2-G07a §五", "无边框的拖动/最大化/多屏 DPI/几何恢复只验了命中判定"),
    "WIN-09": (P, "V2-G07c / V2-G11 窗口场景", "合并退出确认；取消不丢工作"),
    "WIN-10": (M, "V2-G12 §三（空格和中文安装路径）", "中文/空格的 Windows 用户名、普通权限账号、系统文件对话框没测"),
    "HOST-01": (P, "V2-G07a / V2-G07b", ""),
    "HOST-02": (C, "G04.2.2 / V2-G07b", "密码、私钥验过；agent/交互/指纹变化/revoked 没条件"),
    "NET-01": (C, "V2-G07b 转发联网验证", "SOCKS5 + 本地转发验过；HTTP 代理、多级跳板、远程转发没条件"),
    "NET-02": (M, "—", "软件网络代理用于同步/下载没有专门场景"),
    "SFTP-01": (P, "V2-G08a", ""),
    "SFTP-02": (P, "V2-G08a", "队列/递归/冲突/暂停/取消；断网续传见手测清单"),
    "SFTP-03": (M, "V2-G08b §四", "系统拖放那一步没验"),
    "SFTP-04": (P, "V2-G08a 15 / 14 截图", ""),
    "SFTP-05": (M, "V2-G08a §四", "tar 验过；zip/unzip 服务器上没有"),
    "SFTP-06": (P, "V2-G08b", ""),
    "SFTP-07": (P, "V2-G08b", "标签在两个窗口间交接；拖标签出窗口的手势没做"),
    "EDI-01": (P, "V2-G09", ""),
    "EDI-02": (P, "V2-G09", ""),
    "EDI-03": (P, "V2-G09 §二", "查找面板没有正则/整词"),
    "EDI-04": (P, "V2-G09", ""),
    "EDI-05": (M, "V2-G09 §五", "外部编辑按 0.2.14 已移除；属主保不住没条件；断线重连没真断网"),
    "AI-01": (C, "V2-G10a §五", "真实 Claude/Codex 暂停或没装；opencode 纯对话与假世界验过"),
    "AI-02": (P, "V2-G10a / V2-G10b 窗口场景", "假智能体"),
    "AI-03": (P, "V2-G10b b1", ""),
    "AI-04": (P, "V2-G10b", "历史继续要求有会话标识"),
    "AI-05": (M, "V2-F4 / V2-G07c", "表格/代码块视觉没单独核对"),
    "SYN-01": (C, "V2-G11 §二", "WebDAV/S3/Gist 在桩上验；真实服务和 OneDrive 登录后读写没条件"),
    "SYN-02": (P, "V2-G11 w1-w3", ""),
    "SYN-03": (M, "V2-G11 离线备份", "与 Tauri 版互导没验"),
    "STS-01": (P, "V2-G11 §一-4", ""),
    "UPT-01": (P, "V2-F2 单测", ""),
    "UPT-02": (P, "V2-F2 单测", ""),
    "UPT-03": (P, "V2-F2 单测", ""),
    "UPT-04": (N, "V2-G12 §一", "试用版关闭自动更新"),
    "UPT-05": (P, "V2-G12 静默 /UPDATE /R", "安装器侧；应用内触发见试用版说明"),
    "UPT-06": (N, "V2-G12", "试用版只有安装包，没有免安装 zip"),
    "UPT-07": (M, "V2-G12 §八", "真实旧版→新版由用户手测"),
    "UPT-08": (P, "V2-F2 §一", "安装器调用受限、等待父进程"),
    "REL-01": (P, "V2-G12 §三", "安装包 6 个文件、快捷方式、卸载键；没有字体文件"),
    "REL-02": (P, "V2-G12 §一 / V2-G13a 进程树", "不引导/不卸载 WebView2；进程树里没有 WebView2"),
    "REL-03": (T, "本表汇总", "仍有未做和条件缺失项，不能算发行通过"),
    "REL-04": (T, "—", "备份→升级→恢复→回退演练没做"),
    "REL-05": ("REL05", "V2-G13a native 检查", ""),
    "MET-01": ("MET01", "V2-G13a perf", ""),
    "MET-02": (P, "V2-G13a perf 报告", "首次/后续启动分列，标明不是 OS 冷"),
    "MET-03": ("MET03", "V2-G13a perf", ""),
    "MET-04": (M, "V2-G13a perf（一个周期）", "多周期单调性、GPU 内存没做"),
    "PRV-01": (T, "V2-G11 §五", "截图的隐私检查读终端文字，GPUI 自绘的概览文字没覆盖"),
    "DEC-01": (N, "—", "G03 的继续/停止决策，非 G13 范围"),
}


def load(path):
    with open(path, encoding="utf-8-sig", newline="") as f:
        return list(csv.DictReader(f))


def main():
    legacy_p, matrix_p, triage_p, out_p = sys.argv[1:5]
    overrides = {}
    if len(sys.argv) > 5:
        # name=status|note pairs filled in after the perf run, e.g. PERF36=通过|...
        for arg in sys.argv[5:]:
            k, v = arg.split("=", 1)
            st, note = v.split("|", 1)
            overrides[k] = (st, note)
    legacy = load(legacy_p)
    matrix = load(matrix_p)
    triage = {r["id"]: r for r in load(triage_p)}
    rows = []
    for r in legacy:
        n = int(r["id"].split("-")[1])
        status, evidence, note = LEG[n]
        if status in overrides:
            status, note = overrides[status]
        text = " ".join(r["legacy_text"].split())
        rows.append(("历史196", r["id"], r["section"], text[:70], status, evidence, note, triage[r["id"]]["native_phase"]))
    for r in matrix:
        status, evidence, note = MAT[r["id"]]
        if status in overrides:
            status, note = overrides[status]
        rows.append(("迁移矩阵", r["id"], r["area"], " ".join(r["acceptance"].split())[:70], status, evidence, note, r["phase"]))
    with open(out_p, "w", encoding="utf-8-sig", newline="") as f:
        w = csv.writer(f)
        w.writerow(["来源", "编号", "区域", "条目", "ZeeTerm状态", "证据", "说明", "计划阶段"])
        w.writerows(rows)
    from collections import Counter

    for src in ("历史196", "迁移矩阵"):
        c = Counter(x[4] for x in rows if x[0] == src)
        print(src, len([x for x in rows if x[0] == src]), dict(c))
    c = Counter(x[4] for x in rows)
    print("合计", len(rows), dict(c))


if __name__ == "__main__":
    main()
