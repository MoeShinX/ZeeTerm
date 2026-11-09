//! 主窗口的配色：皮肤（界面色板 + 配套的终端色板）全部出自这一个文件。
//!
//! 色值取 0.2.14 前端的皮肤表（`ui/src/state/skins.ts`：浅色 7 套、深色 9 套），表由脚本从
//! 那个文件抽出，不手写；界面用到的几种派生色（悬停、选中底色、强调色上的字）也按
//! `skinTokens` 的公式算，两边看起来是同一个产品。主窗口的所有部件只从 [`Tokens`]
//! 取色，不在别处写死颜色；gpui-component 自带的按钮、输入框等通过 [`apply`]
//! 把同一套色值写进全局主题，所以也跟着变。
//!
//! 选择有三层，和 0.2.14 的设置一一对应：明暗（`set_mode`）、浅色和深色各自用哪个皮肤、
//! 终端配色（跟随皮肤，或固定成某一套皮肤的终端色板）。后两项记在全局 [`Look`] 里。

use ::gpui::{App, Global, Hsla, Rgba, px};
use gpui_kit::component::{Theme, ThemeColor, ThemeMode};

/// 一个皮肤的界面色板（对应 skins.ts 里的 `ui`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiColors {
    pub background: u32,
    /// 卡片、弹出层、侧栏、标题栏。
    pub surface: u32,
    /// 悬停、次要按钮、输入框底色。
    pub subtle: u32,
    pub border: u32,
    pub text: u32,
    pub text_muted: u32,
    pub accent: u32,
    pub danger: u32,
}

/// 终端色板（对应 skins.ts 里的 `terminal`）：自己的底色和前景，不一定等于界面的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermPalette {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    /// 终端里的选区底色。
    pub selection: u32,
    /// 终端 16 色：黑红绿黄蓝品青白，再是 8 个亮色。
    pub ansi: [u32; 16],
}

/// 皮肤显示名里需要翻译的部分（拉丁名原样显示）。
#[derive(Clone, Copy, Debug)]
enum ThemeMsg {
    PaperWhite,
    Graphite,
}

impl crate::i18n::Msg for ThemeMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::PaperWhite => "纸白",
            Self::Graphite => "石墨",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::PaperWhite => "Paper White",
            Self::Graphite => "Graphite",
        }
    }
}

/// 皮肤的显示名：中文名查 i18n 表，其余（拉丁名）原样。
pub fn skin_name(skin: &Skin) -> &'static str {
    match skin.name {
        "纸白" => crate::i18n::t(ThemeMsg::PaperWhite), // i18n-allow: 皮肤名的数据匹配
        "石墨" => crate::i18n::t(ThemeMsg::Graphite),   // i18n-allow: 皮肤名的数据匹配
        other => other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skin {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    pub ui: UiColors,
    pub terminal: TermPalette,
}

/// 浅色、深色各自的默认皮肤（浅色「GitHub」、深色「石墨」；V2-U17 起，只影响没设置过的新用户）。
pub const DEFAULT_LIGHT_SKIN: &str = "github-light";
pub const DEFAULT_DARK_SKIN: &str = "graphite";
/// 终端配色设置的特殊值：跟随当前皮肤。
pub const TERMINAL_FOLLOW_SKIN: &str = "skin";
/// 终端配色的默认值：跟随皮肤（V2-U17 起；之前是固定 Tokyo Night）。只影响没改过这项设置的人。
pub const DEFAULT_TERMINAL_THEME: &str = TERMINAL_FOLLOW_SKIN;

/// 聚焦控件外面的光晕圈：关（只留 1px 强调色边框）。见 `apply`。
pub const FOCUS_HALO: bool = false;

/// 圆润界面的逻辑像素；与系统缩放无关。输入框 / 按钮用 20，面板用 24。
pub const CONTROL_RADIUS: f32 = 20.0;
pub const PANEL_RADIUS: f32 = 24.0;
/// 多行内容块（AI 对话气泡、代码 / 日志 / 输出框、表格容器、带说明文字的卡片）的圆角。20 的控件圆角放在这类块上，
/// 首行 / 末行的字离圆弧只剩几个像素，字的左上角和右下角容易被裁；12 在 8–12px 的内边距下字不会碰到圆弧，
/// 而且比 `CONTROL_RADIUS` 小一档，一眼能看出是「内容块」不是按钮。单行控件仍用 `CONTROL_RADIUS`，小按钮 / 徽标用胶囊。
pub const CARD_RADIUS: f32 = 12.0;

/// 原有皮肤保留顺序，GitHub 后加入 Lavender（先浅色后深色）。
/// 原有色值由脚本从 `skins.ts` 抽出；`skin_ids_match_the_sync_whitelist` 核对 id 与同步白名单一致。
pub const SKINS: &[Skin] = &[
    Skin {
        // VS Code Light Modern
        id: "paper",
        name: "纸白", // i18n-allow: 显示名走 skin_name()
        dark: false,
        ui: UiColors {
            background: 0xFFFFFF,
            surface: 0xF8F8F8,
            subtle: 0xF0F0F0,
            border: 0xE5E5E5,
            text: 0x3B3B3B,
            text_muted: 0x6E6E6E,
            accent: 0x005FB8,
            danger: 0xC72E2E,
        },
        terminal: TermPalette {
            background: 0xFFFFFF,
            foreground: 0x3B3B3B,
            cursor: 0x005FB8,
            selection: 0xADD6FF,
            ansi: [
                0x000000, 0xCD3131, 0x107C10, 0x949800, 0x0451A5, 0xBC05BC, 0x0598BC, 0x555555,
                0x666666, 0xCD3131, 0x14CE14, 0xB5BA00, 0x0451A5, 0xBC05BC, 0x0598BC, 0xA5A5A5,
            ],
        },
    },
    Skin {
        // Solarized Light（Ethan Schoonover）
        id: "solarized-light",
        name: "Solarized",
        dark: false,
        ui: UiColors {
            background: 0xFDF6E3,
            surface: 0xEEE8D5,
            subtle: 0xF3EDDA,
            border: 0xE1DAC4,
            text: 0x4E636A,
            text_muted: 0x7B8B8E,
            accent: 0x268BD2,
            danger: 0xDC322F,
        },
        terminal: TermPalette {
            background: 0xFDF6E3,
            foreground: 0x657B83,
            cursor: 0x586E75,
            selection: 0xEEE8D5,
            ansi: [
                0x073642, 0xDC322F, 0x859900, 0xB58900, 0x268BD2, 0xD33682, 0x2AA198, 0xEEE8D5,
                0x002B36, 0xCB4B16, 0x586E75, 0x657B83, 0x839496, 0x6C71C4, 0x93A1A1, 0xFDF6E3,
            ],
        },
    },
    Skin {
        // Ayu Light
        id: "ayu-light",
        name: "Ayu",
        dark: false,
        ui: UiColors {
            background: 0xFCFCFC,
            surface: 0xF3F4F5,
            subtle: 0xECEEF0,
            border: 0xE2E5E8,
            text: 0x5C6166,
            text_muted: 0x7D848B,
            accent: 0xB86A00,
            danger: 0xE65050,
        },
        terminal: TermPalette {
            background: 0xFCFCFC,
            foreground: 0x5C6166,
            cursor: 0xFFAA33,
            selection: 0xD3E1F5,
            ansi: [
                0x000000, 0xEA6C6D, 0x6CBF43, 0xECA944, 0x3199E1, 0x9E75C7, 0x46BA94, 0xBABABA,
                0x686868, 0xF07171, 0x86B300, 0xF2AE49, 0x399EE6, 0xA37ACC, 0x4CBF99, 0xD1D1D1,
            ],
        },
    },
    Skin {
        // GitHub Light（Primer）
        id: "github-light",
        name: "GitHub",
        dark: false,
        ui: UiColors {
            background: 0xFFFFFF,
            surface: 0xF6F8FA,
            subtle: 0xEFF2F5,
            border: 0xD8DEE4,
            text: 0x1F2328,
            text_muted: 0x59636E,
            accent: 0x0969DA,
            danger: 0xCF222E,
        },
        terminal: TermPalette {
            background: 0xFFFFFF,
            foreground: 0x1F2328,
            cursor: 0x0969DA,
            selection: 0xB6E3FF,
            ansi: [
                0x24292F, 0xCF222E, 0x116329, 0x4D2D00, 0x0969DA, 0x8250DF, 0x1B7C83, 0x6E7781,
                0x57606A, 0xA40E26, 0x1A7F37, 0x633C01, 0x218BFF, 0xA475F9, 0x3192AA, 0x8C959F,
            ],
        },
    },
    Skin {
        // Lavender：圆润 UI 的独立皮肤，保留原 GitHub 终端 ANSI 配色。
        id: "lavender",
        name: "Lavender",
        dark: false,
        ui: UiColors {
            background: 0xFFFFFF,
            surface: 0xF5F1FC,
            subtle: 0xEEE7FA,
            border: 0xDAD2E8,
            text: 0x252033,
            text_muted: 0x696177,
            accent: 0x7041D9,
            danger: 0xCF222E,
        },
        terminal: TermPalette {
            background: 0xFFFFFF,
            foreground: 0x1F2328,
            cursor: 0x0969DA,
            selection: 0xB6E3FF,
            ansi: [
                0x24292F, 0xCF222E, 0x116329, 0x4D2D00, 0x0969DA, 0x8250DF, 0x1B7C83, 0x6E7781,
                0x57606A, 0xA40E26, 0x1A7F37, 0x633C01, 0x218BFF, 0xA475F9, 0x3192AA, 0x8C959F,
            ],
        },
    },
    Skin {
        // Catppuccin Latte
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        dark: false,
        ui: UiColors {
            background: 0xEFF1F5,
            surface: 0xE6E9EF,
            subtle: 0xDCE0E8,
            border: 0xCCD0DA,
            text: 0x4C4F69,
            text_muted: 0x6C6F85,
            accent: 0x8839EF,
            danger: 0xD20F39,
        },
        terminal: TermPalette {
            background: 0xEFF1F5,
            foreground: 0x4C4F69,
            cursor: 0xDC8A78,
            selection: 0xCCD0DA,
            ansi: [
                0x5C5F77, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB, 0x179299, 0xACB0BE,
                0x6C6F85, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB, 0x179299, 0xBCC0CC,
            ],
        },
    },
    Skin {
        // Rosé Pine Dawn
        id: "rose-pine-dawn",
        name: "Rosé Pine Dawn",
        dark: false,
        ui: UiColors {
            background: 0xFAF4ED,
            surface: 0xFFFAF3,
            subtle: 0xF2E9E1,
            border: 0xDFDAD9,
            text: 0x575279,
            text_muted: 0x797593,
            accent: 0x907AA9,
            danger: 0xB4637A,
        },
        terminal: TermPalette {
            background: 0xFAF4ED,
            foreground: 0x575279,
            cursor: 0x575279,
            selection: 0xDFDAD9,
            ansi: [
                0xF2E9E1, 0xB4637A, 0x286983, 0xEA9D34, 0x56949F, 0x907AA9, 0xD7827E, 0x575279,
                0x9893A5, 0xB4637A, 0x286983, 0xEA9D34, 0x56949F, 0x907AA9, 0xD7827E, 0x575279,
            ],
        },
    },
    Skin {
        // Everforest Light（medium）
        id: "everforest-light",
        name: "Everforest",
        dark: false,
        ui: UiColors {
            background: 0xFDF6E3,
            surface: 0xF4F0D9,
            subtle: 0xEFEBD4,
            border: 0xE6E2CC,
            text: 0x5C6A72,
            text_muted: 0x7A8478,
            accent: 0x748A00,
            danger: 0xF85552,
        },
        terminal: TermPalette {
            background: 0xFDF6E3,
            foreground: 0x5C6A72,
            cursor: 0x5C6A72,
            selection: 0xE6E2CC,
            ansi: [
                0x5C6A72, 0xF85552, 0x8DA101, 0xDFA000, 0x3A94C5, 0xDF69BA, 0x35A77C, 0xDFDDC8,
                0x939F91, 0xF85552, 0x8DA101, 0xDFA000, 0x3A94C5, 0xDF69BA, 0x35A77C, 0xF3EAD3,
            ],
        },
    },
    Skin {
        // VS Code Dark Modern
        id: "graphite",
        name: "石墨", // i18n-allow: 显示名走 skin_name()
        dark: true,
        ui: UiColors {
            background: 0x1F1F1F,
            surface: 0x181818,
            subtle: 0x2A2D2E,
            border: 0x2B2B2B,
            text: 0xCCCCCC,
            text_muted: 0x9D9D9D,
            accent: 0x0078D4,
            danger: 0xF14C4C,
        },
        terminal: TermPalette {
            background: 0x1F1F1F,
            foreground: 0xCCCCCC,
            cursor: 0xAEAFAD,
            selection: 0x264F78,
            ansi: [
                0x000000, 0xCD3131, 0x0DBC79, 0xE5E510, 0x2472C8, 0xBC3FBC, 0x11A8CD, 0xE5E5E5,
                0x666666, 0xF14C4C, 0x23D18B, 0xF5F543, 0x3B8EEA, 0xD670D6, 0x29B8DB, 0xE5E5E5,
            ],
        },
    },
    Skin {
        // Tokyo Night
        id: "tokyo-night",
        name: "Tokyo Night",
        dark: true,
        ui: UiColors {
            background: 0x1A1B26,
            surface: 0x16161E,
            subtle: 0x292E42,
            border: 0x2A2D3E,
            text: 0xC0CAF5,
            text_muted: 0x8A92BC,
            accent: 0x7AA2F7,
            danger: 0xF7768E,
        },
        terminal: TermPalette {
            background: 0x1A1B26,
            foreground: 0xC0CAF5,
            cursor: 0xC0CAF5,
            selection: 0x283457,
            ansi: [
                0x15161E, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xA9B1D6,
                0x414868, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xC0CAF5,
            ],
        },
    },
    Skin {
        // Dracula
        id: "dracula",
        name: "Dracula",
        dark: true,
        ui: UiColors {
            background: 0x282A36,
            surface: 0x21222C,
            subtle: 0x343746,
            border: 0x3A3D4E,
            text: 0xF8F8F2,
            text_muted: 0xA9ABC2,
            accent: 0xBD93F9,
            danger: 0xFF5555,
        },
        terminal: TermPalette {
            background: 0x282A36,
            foreground: 0xF8F8F2,
            cursor: 0xF8F8F2,
            selection: 0x44475A,
            ansi: [
                0x21222C, 0xFF5555, 0x50FA7B, 0xF1FA8C, 0xBD93F9, 0xFF79C6, 0x8BE9FD, 0xF8F8F2,
                0x6272A4, 0xFF6E6E, 0x69FF94, 0xFFFFA5, 0xD6ACFF, 0xFF92DF, 0xA4FFFF, 0xFFFFFF,
            ],
        },
    },
    Skin {
        // Catppuccin Mocha
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        dark: true,
        ui: UiColors {
            background: 0x1E1E2E,
            surface: 0x181825,
            subtle: 0x313244,
            border: 0x313244,
            text: 0xCDD6F4,
            text_muted: 0xA6ADC8,
            accent: 0xCBA6F7,
            danger: 0xF38BA8,
        },
        terminal: TermPalette {
            background: 0x1E1E2E,
            foreground: 0xCDD6F4,
            cursor: 0xF5E0DC,
            selection: 0x45475A,
            ansi: [
                0x45475A, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xBAC2DE,
                0x585B70, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xA6ADC8,
            ],
        },
    },
    Skin {
        // Nord（Arctic Ice Studio）
        id: "nord",
        name: "Nord",
        dark: true,
        ui: UiColors {
            background: 0x2E3440,
            surface: 0x292E39,
            subtle: 0x3B4252,
            border: 0x3B4252,
            text: 0xE5E9F0,
            text_muted: 0xA3ABBA,
            accent: 0x88C0D0,
            danger: 0xBF616A,
        },
        terminal: TermPalette {
            background: 0x2E3440,
            foreground: 0xD8DEE9,
            cursor: 0xD8DEE9,
            selection: 0x434C5E,
            ansi: [
                0x3B4252, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x88C0D0, 0xE5E9F0,
                0x4C566A, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x8FBCBB, 0xECEFF4,
            ],
        },
    },
    Skin {
        // Gruvbox Dark（morhetz）
        id: "gruvbox-dark",
        name: "Gruvbox",
        dark: true,
        ui: UiColors {
            background: 0x282828,
            surface: 0x1D2021,
            subtle: 0x3C3836,
            border: 0x3C3836,
            text: 0xEBDBB2,
            text_muted: 0xA89984,
            accent: 0xFABD2F,
            danger: 0xFB4934,
        },
        terminal: TermPalette {
            background: 0x282828,
            foreground: 0xEBDBB2,
            cursor: 0xEBDBB2,
            selection: 0x504945,
            ansi: [
                0x282828, 0xCC241D, 0x98971A, 0xD79921, 0x458588, 0xB16286, 0x689D6A, 0xA89984,
                0x928374, 0xFB4934, 0xB8BB26, 0xFABD2F, 0x83A598, 0xD3869B, 0x8EC07C, 0xEBDBB2,
            ],
        },
    },
    Skin {
        // Rosé Pine
        id: "rose-pine",
        name: "Rosé Pine",
        dark: true,
        ui: UiColors {
            background: 0x191724,
            surface: 0x1F1D2E,
            subtle: 0x26233A,
            border: 0x2A2739,
            text: 0xE0DEF4,
            text_muted: 0x908CAA,
            accent: 0xC4A7E7,
            danger: 0xEB6F92,
        },
        terminal: TermPalette {
            background: 0x191724,
            foreground: 0xE0DEF4,
            cursor: 0xE0DEF4,
            selection: 0x403D52,
            ansi: [
                0x26233A, 0xEB6F92, 0x31748F, 0xF6C177, 0x9CCFD8, 0xC4A7E7, 0xEBBCBA, 0xE0DEF4,
                0x6E6A86, 0xEB6F92, 0x31748F, 0xF6C177, 0x9CCFD8, 0xC4A7E7, 0xEBBCBA, 0xE0DEF4,
            ],
        },
    },
    Skin {
        // Kanagawa Wave（rebelot）
        id: "kanagawa",
        name: "Kanagawa",
        dark: true,
        ui: UiColors {
            background: 0x1F1F28,
            surface: 0x16161D,
            subtle: 0x2A2A37,
            border: 0x2A2A37,
            text: 0xDCD7BA,
            text_muted: 0xA09E8E,
            accent: 0x7E9CD8,
            danger: 0xE46876,
        },
        terminal: TermPalette {
            background: 0x1F1F28,
            foreground: 0xDCD7BA,
            cursor: 0xC8C093,
            selection: 0x2D4F67,
            ansi: [
                0x090618, 0xC34043, 0x76946A, 0xC0A36E, 0x7E9CD8, 0x957FB8, 0x6A9589, 0xC8C093,
                0x727169, 0xE82424, 0x98BB6C, 0xE6C384, 0x7FB4CA, 0x938AA9, 0x7AA89F, 0xDCD7BA,
            ],
        },
    },
    Skin {
        // Everforest Dark（medium）
        id: "everforest-dark",
        name: "Everforest",
        dark: true,
        ui: UiColors {
            background: 0x2D353B,
            surface: 0x232A2E,
            subtle: 0x343F44,
            border: 0x3D484D,
            text: 0xD3C6AA,
            text_muted: 0x9DA9A0,
            accent: 0xA7C080,
            danger: 0xE67E80,
        },
        terminal: TermPalette {
            background: 0x2D353B,
            foreground: 0xD3C6AA,
            cursor: 0xD3C6AA,
            selection: 0x475258,
            ansi: [
                0x414B50, 0xE67E80, 0xA7C080, 0xDBBC7F, 0x7FBBB3, 0xD699B6, 0x83C092, 0xD3C6AA,
                0x859289, 0xE67E80, 0xA7C080, 0xDBBC7F, 0x7FBBB3, 0xD699B6, 0x83C092, 0xD3C6AA,
            ],
        },
    },
];

/// 连接状态点（与 0.2.14 标签上的点同色）：已连接 / 连接中 / 已断开 / 空标签。
const DOT_CONNECTED: u32 = 0x2FA16B;
const DOT_CONNECTING: u32 = 0xFBBF24;
const DOT_DISCONNECTED: u32 = 0xF43F5E;
const DOT_IDLE: u32 = 0x475569;
/// 浅色底上的警示琥珀色。
const CAUTION_ON_LIGHT: u32 = 0xB45309;

pub fn skin(id: &str) -> Option<&'static Skin> {
    SKINS.iter().find(|skin| skin.id == id)
}

/// 某个明暗下的皮肤；id 不认识（或明暗对不上）就用这个明暗的第一个（同 `skinFor`）。
pub fn skin_for(dark: bool, id: &str) -> &'static Skin {
    SKINS
        .iter()
        .find(|skin| skin.dark == dark && skin.id == id)
        .or_else(|| SKINS.iter().find(|skin| skin.dark == dark))
        .unwrap_or(&SKINS[0])
}

/// 某个明暗下的所有皮肤（设置页 / 主题菜单的列表）。
pub fn skins_of(dark: bool) -> impl Iterator<Item = &'static Skin> {
    SKINS.iter().filter(move |skin| skin.dark == dark)
}

/// 终端配色设置的值：跟随皮肤，或某个皮肤的配色。认不得的值（含旧版本的配色 id）一律当成
/// 跟随皮肤——旧版本里终端背景本来就跟着界面走，升级后固定成某一套会出现「界面浅色、终端深色」
/// （同 `normalizeTerminalTheme`）。
pub fn normalize_terminal_theme(id: &str) -> &str {
    if id == TERMINAL_FOLLOW_SKIN || skin(id).is_some() {
        id
    } else {
        TERMINAL_FOLLOW_SKIN
    }
}

/// 现在用的外观选择：浅色 / 深色各用哪个皮肤，终端配色用哪套。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub skin_light: String,
    pub skin_dark: String,
    pub terminal_theme: String,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            skin_light: DEFAULT_LIGHT_SKIN.to_owned(),
            skin_dark: DEFAULT_DARK_SKIN.to_owned(),
            terminal_theme: DEFAULT_TERMINAL_THEME.to_owned(),
        }
    }
}

impl Look {
    /// 明暗确定后实际用的皮肤。
    pub fn skin(&self, dark: bool) -> &'static Skin {
        skin_for(
            dark,
            if dark {
                &self.skin_dark
            } else {
                &self.skin_light
            },
        )
    }

    /// 终端实际用的色板：跟随皮肤就用皮肤配套的，单独选了就用选的那套。
    pub fn terminal_palette(&self, dark: bool) -> &'static TermPalette {
        let chosen = skin(normalize_terminal_theme(&self.terminal_theme));
        &chosen.unwrap_or_else(|| self.skin(dark)).terminal
    }
}

impl Global for Look {}

/// 全局外观选择；还没设过（测试、别的窗口先于主窗口）就是默认值。
pub fn look(cx: &App) -> Look {
    cx.try_global::<Look>().cloned().unwrap_or_default()
}

/// 现在界面用的皮肤（按全局主题的明暗取）。
pub fn active_skin(cx: &App) -> &'static Skin {
    look(cx).skin(is_dark(cx))
}

/// 终端现在用的色板。
pub fn terminal_palette(cx: &App) -> &'static TermPalette {
    look(cx).terminal_palette(is_dark(cx))
}

/// 填在强调色上的字：白色和皮肤里的深色哪个更清楚用哪个（同 `onAccent`）。
pub fn on_accent(skin: &Skin) -> u32 {
    let p = skin.ui;
    let ink = if skin.dark { p.surface } else { p.text };
    if contrast(0xFFFFFF, p.accent) >= contrast(ink, p.accent) {
        0xFFFFFF
    } else {
        ink
    }
}

/// 填在危险色上的图形 / 字：白色和皮肤里的深色哪个更清楚用哪个（同 [`on_accent`]；Tokyo Night、Dracula 的危险色是浅粉，
/// 白色放上去只有 2.5 : 1）。
pub fn on_danger(skin: &Skin) -> u32 {
    let p = skin.ui;
    let ink = if skin.dark { p.surface } else { p.text };
    if contrast(0xFFFFFF, p.danger) >= contrast(ink, p.danger) {
        0xFFFFFF
    } else {
        ink
    }
}

/// 主文字色比皮肤表里的正文色更靠近纯黑（浅色皮肤）/ 纯白（深色皮肤）的比例：标题、名称要「黑」，
/// 但保留皮肤自己的色调（Solarized 偏青、Rosé Pine 偏紫），不一刀切成 #000 / #FFF。
pub const INK_PUSH_LIGHT: f32 = 0.55;
pub const INK_PUSH_DARK: f32 = 0.55;
/// 次要色（说明文字）对界面底色 / 卡片底色的对比度下限：比 WCAG AA 正文 4.5 : 1 也不放宽。
pub const MUTED_MIN_CONTRAST: f32 = 4.5;
/// 第三级色（导航分组名、占位提示）的对比度下限：只给不承载关键信息的小标签用。
pub const FAINT_MIN_CONTRAST: f32 = 3.0;

/// 界面的三级文字色：主（标题、名称）/ 次（说明）/ 淡（分组名、提示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ink {
    pub primary: u32,
    pub secondary: u32,
    pub faint: u32,
    /// 未选中的导航项：介于主色和次色之间（比说明文字深，比选中的浅）。
    pub nav: u32,
}

/// 由皮肤的界面色板算出三级文字色。主色把皮肤正文色朝黑 / 白推一段；次色取皮肤自己的说明色，
/// 但在底色或卡片色上低于 [`MUTED_MIN_CONTRAST`] 时朝主色拉近，直到够；淡色从次色朝底色淡出，
/// 淡到对比度刚好守住 [`FAINT_MIN_CONTRAST`] 为止。三个值都是不透明的 RGB。
pub fn ink_for(skin: &Skin) -> Ink {
    let p = skin.ui;
    let (extreme, push) = if skin.dark {
        (0xFFFFFF, INK_PUSH_DARK)
    } else {
        (0x000000, INK_PUSH_LIGHT)
    };
    let primary = mix(extreme, p.text, push);
    let readable = |hex: u32, floor: f32| {
        contrast(hex, p.background) >= floor && contrast(hex, p.surface) >= floor
    };
    let secondary = (0..=20)
        .map(|step| mix(primary, p.text_muted, step as f32 * 0.05))
        .find(|hex| readable(*hex, MUTED_MIN_CONTRAST))
        .unwrap_or(primary);
    // 淡出的比例从 0.35 起往回收，直到对比度守住下限；收到 0 就是次色本身，一定满足。
    let faint = (0..=7)
        .map(|step| mix(p.background, secondary, 0.35 - step as f32 * 0.05))
        .find(|hex| readable(*hex, FAINT_MIN_CONTRAST))
        .unwrap_or(secondary);
    Ink {
        primary,
        secondary,
        faint,
        nav: mix(secondary, primary, 0.35),
    }
}

/// 一套解析好的颜色，主窗口各部件直接取用。
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    pub dark: bool,
    pub background: Hsla,
    pub surface: Hsla,
    pub subtle: Hsla,
    pub border: Hsla,
    /// 输入框 / 默认按钮的边框：和 `border` 一样，只是深色皮肤上会拉到对字段底色够清楚，见 [`field_border_hex`]。
    pub field_border: Hsla,
    /// 菜单、下拉、提示这类弹出层的底色，见 [`popover_hex`]。
    pub popover: Hsla,
    /// 开关的圆点：强调色轨道和灰色关闭轨道上都看得清的浅（或皮肤深色）色，见 [`switch_thumb_hex`]。
    pub switch_thumb: Hsla,
    /// 区域之间的分隔线（侧栏右边、标题栏下边、状态栏上边）：1px，取边框色和底色的中间，只起分界作用，
    /// 不抢眼；卡片、输入框、菜单仍用 `border`。
    pub divider: Hsla,
    /// 主文字色（标题、名称）：比皮肤表里的正文色更黑 / 更白，见 [`ink_for`]。
    pub text: Hsla,
    /// 次要文字色（说明）。
    pub text_muted: Hsla,
    /// 第三级文字色（导航分组名、占位提示）。
    pub text_faint: Hsla,
    /// 未选中的导航项的文字色。
    pub text_nav: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    /// 填在危险色上的图形 / 字，见 [`on_danger`]。
    pub on_danger: Hsla,
    /// 选中项 / 悬停的浅底色：强调色淡淡地叠在背景上。
    pub accent_soft: Hsla,
    /// 悬停底色：文字色以很低的透明度叠上去。
    pub hover: Hsla,
    pub danger: Hsla,
    pub selection: Hsla,
    pub dot_connected: Hsla,
    pub dot_connecting: Hsla,
    /// 警示文字色（「全部放行」「广播中」）：深色底上用琥珀黄，浅色底上用深一点的琥珀色保证读得清。
    pub caution: Hsla,
    pub dot_disconnected: Hsla,
    pub dot_idle: Hsla,
}

impl Tokens {
    /// 某个明暗的默认皮肤（测试里用；运行时一律按选中的皮肤取，见 [`Tokens::current`]）。
    #[cfg(test)]
    pub fn for_mode(dark: bool) -> Self {
        Self::for_skin(skin_for(
            dark,
            if dark {
                DEFAULT_DARK_SKIN
            } else {
                DEFAULT_LIGHT_SKIN
            },
        ))
    }

    pub fn for_skin(skin: &Skin) -> Self {
        let p = skin.ui;
        let dark = skin.dark;
        let soft = mix(p.accent, p.background, if dark { 0.22 } else { 0.12 });
        let mut hover = rgb(p.text);
        hover.a = if dark { 0.08 } else { 0.06 };
        let ink = ink_for(skin);
        Self {
            dark,
            background: rgb(p.background),
            surface: rgb(p.surface),
            subtle: rgb(p.subtle),
            border: rgb(p.border),
            field_border: rgb(field_border_hex(skin)),
            popover: rgb(popover_hex(skin)),
            switch_thumb: rgb(switch_thumb_hex(skin)),
            divider: rgb(divider_hex(skin)),
            text: rgb(ink.primary),
            text_muted: rgb(ink.secondary),
            text_faint: rgb(ink.faint),
            text_nav: rgb(ink.nav),
            accent: rgb(p.accent),
            on_accent: rgb(on_accent(skin)),
            on_danger: rgb(on_danger(skin)),
            accent_soft: rgb(soft),
            hover,
            danger: rgb(p.danger),
            selection: rgb(skin.terminal.selection),
            dot_connected: rgb(DOT_CONNECTED),
            dot_connecting: rgb(DOT_CONNECTING),
            caution: rgb(if dark {
                DOT_CONNECTING
            } else {
                CAUTION_ON_LIGHT
            }),
            dot_disconnected: rgb(DOT_DISCONNECTED),
            dot_idle: rgb(DOT_IDLE),
        }
    }

    /// 当前全局主题对应的 token（按明暗和选的皮肤取）。
    pub fn current(cx: &App) -> Self {
        Self::for_skin(active_skin(cx))
    }
}

pub fn is_dark(cx: &App) -> bool {
    use gpui_kit::component::ActiveTheme as _;
    cx.theme().mode == ThemeMode::Dark
}

/// 切换明暗，并把 token 写进 gpui-component 的全局主题。
/// 所有切换主题的地方都走这里：`Theme::change` 会把颜色重置成组件库默认值，
/// 单独调用它会让主窗口丢掉自己的配色。
pub fn set_mode(dark: bool, cx: &mut App) {
    let mode = if dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);
    apply(dark, cx);
}

/// 换皮肤 / 终端配色：记下选择，再按当前明暗重新写一遍主题。
pub fn set_look(new: Look, cx: &mut App) {
    cx.set_global(new);
    let dark = is_dark(cx);
    apply(dark, cx);
}

/// 启动时：先记下外观选择，再定明暗并写入 token。
pub fn init_mode(dark: bool, cx: &mut App) {
    set_mode(dark, cx);
}

fn rgb(hex: u32) -> Hsla {
    Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// 区域分隔线的颜色：边框色和底色各一半（见 [`Tokens::divider`]）。
pub fn divider_hex(skin: &Skin) -> u32 {
    mix(skin.ui.border, skin.ui.background, 0.5)
}

/// 组件库在深色模式下把输入框的底色算成 `input` 色以 30% 的不透明度叠在父级背景上（`Theme::input_background`）；
/// 浅色模式下输入框底色就是 `background`。下面算对比度时按这个公式还原字段底色。
const DARK_FIELD_FILL_ALPHA: f32 = 0.3;
/// 组件库给弹出层画的 1px 外圈（`popover_style`）是前景色的 10% 不透明度。
#[cfg(test)]
const POPOVER_RING_INK: f32 = 0.1;

/// 输入框边框对字段底色的对比度下限。浅色皮肤沿用现有最低值（everforest 约 1.21），保证浅色观感不变；
/// 深色皮肤原来只有 1.1–1.2 : 1，框和底糊在一起，定 1.5 : 1：WCAG 对非文字控件建议 3 : 1，
/// 深色界面里 3 : 1 的边框会很刺眼，1.5 : 1 在石墨这类很暗的底上已经是清楚的一圈。
pub const FIELD_BORDER_MIN_LIGHT: f32 = 1.2;
pub const FIELD_BORDER_MIN_DARK: f32 = 1.5;
/// 弹出层（菜单）底色对主背景的对比度下限。组件库给菜单画的只有一圈 1px、前景色 10% 的外圈（约 1.2–1.3 : 1，
/// 细得几乎看不见），真正把菜单和背景分开的是底色这一层。浅色皮肤的卡片色对白底本来就有 1.05–1.14，
/// 观感一直可以，下限取 1.05 只防退化。深色皮肤的卡片色比主背景还暗、只有 1.05–1.17：想抬到 1.3
/// （[`POPUP_FILL_TARGET_DARK`]），但菜单里的说明文字要守住 4.5 : 1，Nord / Gruvbox / Everforest 这类说明色偏暗的
/// 皮肤只能抬到约 1.17，所以硬下限取 1.15，目标 1.3 尽量达到。
pub const POPUP_FILL_MIN_LIGHT: f32 = 1.05;
#[cfg(test)]
pub const POPUP_FILL_MIN_DARK: f32 = 1.15;
/// 深色皮肤菜单底色想达到的对比度（受说明文字可读性限制，见 [`popover_hex`]）。
pub const POPUP_FILL_TARGET_DARK: f32 = 1.3;
/// 开关圆点对强调色轨道的对比度下限（取 WCAG 对非文字控件的 3 : 1）。
#[cfg(test)]
pub const SWITCH_THUMB_ON_MIN: f32 = 3.0;
/// 开关圆点对关闭轨道（灰色）的对比度下限：浅色皮肤原来最低约 1.9（白点放在浅灰轨道上），
/// 取 1.9 保证没有变差；深色皮肤的关闭轨道更深，实际都在 2.2 以上。
#[cfg(test)]
pub const SWITCH_THUMB_OFF_MIN: f32 = 1.9;

fn field_floor(skin: &Skin) -> f32 {
    if skin.dark {
        FIELD_BORDER_MIN_DARK
    } else {
        FIELD_BORDER_MIN_LIGHT
    }
}

/// 输入框在某个父级背景上的底色（`border` 是当前的输入框边框色）。
pub fn field_fill_hex(skin: &Skin, border: u32, parent: u32) -> u32 {
    if skin.dark {
        mix(border, parent, DARK_FIELD_FILL_ALPHA)
    } else {
        skin.ui.background
    }
}

/// 输入框边框对字段底色的对比度；父级背景取主背景和卡片 / 对话框底色里更差的那个。
pub fn field_border_contrast(skin: &Skin, border: u32) -> f32 {
    [skin.ui.background, skin.ui.surface]
        .into_iter()
        .map(|parent| contrast(border, field_fill_hex(skin, border, parent)))
        .fold(f32::MAX, f32::min)
}

/// 输入框 / 默认按钮的边框色：皮肤自己的边框色已经够清楚就原样用，不够就往主文字色混，
/// 混到 [`field_border_contrast`] 刚好过下限为止。不改 `SKINS` 里的值（那是脚本从 skins.ts 抽出来的）。
pub fn field_border_hex(skin: &Skin) -> u32 {
    let own = skin.ui.border;
    let floor = field_floor(skin);
    if field_border_contrast(skin, own) >= floor {
        return own;
    }
    (1..=50)
        .map(|step| mix(skin.ui.text, own, step as f32 * 0.02))
        .find(|hex| field_border_contrast(skin, *hex) >= floor)
        .unwrap_or(skin.ui.text)
}

/// 弹出层底色对主背景的对比度。
pub fn popover_contrast(skin: &Skin, popover: u32) -> f32 {
    contrast(popover, skin.ui.background)
}

/// 组件库画在弹出层外面的 1px 外圈（前景色 10% 叠在主背景上）对主背景的对比度；只在报告里列出，不当下限。
#[cfg(test)]
pub fn popover_ring_contrast(skin: &Skin) -> f32 {
    let bg = skin.ui.background;
    contrast(mix(ink_for(skin).primary, bg, POPOVER_RING_INK), bg)
}

#[cfg(test)]
fn popover_floor(skin: &Skin) -> f32 {
    if skin.dark {
        POPUP_FILL_MIN_DARK
    } else {
        POPUP_FILL_MIN_LIGHT
    }
}

/// 菜单 / 下拉 / 提示的底色：皮肤的卡片色（`surface`）够和主背景分开就原样用（浅色皮肤都是）；
/// 不够（深色皮肤的卡片色比主背景还暗，菜单糊进去）就从主背景往主文字色抬，和常见的「弹出层比页面亮一级」一致。
/// 抬到对主背景 [`POPUP_FILL_TARGET_DARK`] 为止，但不能把菜单里的说明文字（右侧快捷键）压到
/// [`MUTED_MIN_CONTRAST`] 以下：说明色是按主背景 / 卡片色算的，底色抬亮后会变难读，某些皮肤（Nord、Gruvbox 这类
/// 说明色本来就偏暗的）抬不到目标，就停在说明文字刚好守住下限的那一档。
pub fn popover_hex(skin: &Skin) -> u32 {
    let own = skin.ui.surface;
    let target = if skin.dark {
        POPUP_FILL_TARGET_DARK
    } else {
        POPUP_FILL_MIN_LIGHT
    };
    if popover_contrast(skin, own) >= target {
        return own;
    }
    let muted = ink_for(skin).secondary;
    let mut best = own;
    for step in 1..=50 {
        let hex = mix(skin.ui.text, skin.ui.background, step as f32 * 0.01);
        if contrast(muted, hex) < MUTED_MIN_CONTRAST {
            break;
        }
        best = hex;
        if popover_contrast(skin, hex) >= target {
            break;
        }
    }
    // 卡片色本来就是更暗的一档（Everforest 深色）而且分得更开时，保留它，不为了「抬亮」反而变差。
    if popover_contrast(skin, best) < popover_contrast(skin, own) {
        own
    } else {
        best
    }
}

/// 开关关着时的轨道色：边框色和说明色之间，浅深皮肤上都看得出来。
pub fn switch_off_hex(skin: &Skin) -> u32 {
    mix(skin.ui.text_muted, skin.ui.border, 0.45)
}

/// 开关圆点：和强调色上的字（`on_accent`）同一个颜色。组件库默认取 `background`，石墨上是近黑色的圆点，
/// 和别的浅色控件不一致；`on_accent` 本来就是按「放在强调色上最清楚」挑的，关闭轨道上也比原来更清楚。
pub fn switch_thumb_hex(skin: &Skin) -> u32 {
    on_accent(skin)
}

fn channels(hex: u32) -> [f32; 3] {
    [
        ((hex >> 16) & 0xFF) as f32,
        ((hex >> 8) & 0xFF) as f32,
        (hex & 0xFF) as f32,
    ]
}

/// `top` 占 `amount` 的比例混合到 `bottom` 上（对应 skins.ts 的 `mixHex`）。
pub fn mix(top: u32, bottom: u32, amount: f32) -> u32 {
    let (a, b) = (channels(top), channels(bottom));
    let mut out = 0u32;
    for i in 0..3 {
        let value = (a[i] * amount + b[i] * (1.0 - amount))
            .round()
            .clamp(0.0, 255.0) as u32;
        out = (out << 8) | value;
    }
    out
}

fn luminance(hex: u32) -> f32 {
    let [r, g, b] = channels(hex).map(|c| {
        let v = c / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// 终端右上角延迟徽标的颜色：字色 `fg`、底色 `fill`（都是不透明色，画的时候由界面加半透明）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyBadge {
    pub fg: u32,
    pub fill: u32,
}

/// 延迟徽标在 `terminal_bg`（终端底色，可能和界面皮肤不是一套）上的颜色。
///
/// 绿取状态点的已连接色，琥珀取警示色（深底用亮琥珀、浅底用深琥珀），红取皮肤的危险色；底色是
/// 这个颜色淡淡地叠在终端底色上。字色在此基础上向白 / 黑推到对底色的对比度不低于
/// [`MUTED_MIN_CONTRAST`]，深浅终端配色上都读得清。
pub fn latency_badge(
    level: zssh_core::ssh::latency::Level,
    skin: &Skin,
    terminal_bg: u32,
) -> LatencyBadge {
    use zssh_core::ssh::latency::Level;
    let dark_bg = luminance(terminal_bg) < 0.4;
    let base = match level {
        Level::Good => DOT_CONNECTED,
        Level::Warn if dark_bg => DOT_CONNECTING,
        Level::Warn => CAUTION_ON_LIGHT,
        Level::Bad => skin.ui.danger,
    };
    let fill = mix(base, terminal_bg, 0.16);
    let toward = if dark_bg { 0xFFFFFF } else { 0x000000 };
    let mut fg = base;
    for step in 0..=10 {
        fg = mix(toward, base, step as f32 / 10.0);
        if contrast(fg, fill) >= MUTED_MIN_CONTRAST {
            break;
        }
    }
    LatencyBadge { fg, fill }
}

/// WCAG 对比度。
pub fn contrast(a: u32, b: u32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn apply(dark: bool, cx: &mut App) {
    let skin = look(cx).skin(dark);
    let ui_font = crate::fonts::ui_font(cx);
    Theme::update(cx, |theme| {
        // 界面字体：显式的字体栈代替 `.SystemUIFont`（见 `fonts::resolve_ui_font`）。中文后备在各窗口的
        // 顶层元素上设（`typography::UiFontStyled`），主题只有主字体一个槽。
        theme.font_family = ui_font.family.clone().into();
        // 输入框聚焦只把 1px 边框换成强调色（`ring` = 强调色）：组件库默认还会在外面再画一圈半透明光晕，
        // 和边框叠成「粗蓝双框」（V2-U17）。关掉这个开关，组件库就退回到只染边框的样式，按钮的键盘焦点也是这样。
        theme.focus_ring = FOCUS_HALO;
        // PopupMenu 的外框也读取 radius；不重写菜单以保留键盘导航和辅助功能。
        theme.radius = px(CONTROL_RADIUS);
        theme.radius_lg = px(PANEL_RADIUS);
        write_colors(&mut theme.colors, skin);
    });
}

/// 把一个皮肤的颜色写进组件库的主题色表（`apply` 调用；单独成函数是为了测试能直接核对「哪个组件颜色读哪个 token」）。
fn write_colors(c: &mut ThemeColor, skin: &Skin) {
    let t = Tokens::for_skin(skin);
    let p = skin.ui;
    let dark = skin.dark;
    let hover_amount = if dark { 0.10 } else { 0.07 };
    let subtle_hover = rgb(mix(p.text, p.subtle, hover_amount));
    let subtle_active = rgb(mix(p.text, p.subtle, hover_amount * 2.0));
    let accent_hover = rgb(mix(p.text, p.accent, hover_amount));
    let accent_active = rgb(mix(p.text, p.accent, hover_amount * 2.0));
    let danger_hover = rgb(mix(p.text, p.danger, hover_amount));
    let danger_active = rgb(mix(p.text, p.danger, hover_amount * 2.0));
    // 状态色（成功 / 提示 / 警告）的悬停、按下：和上面按钮同一个公式，免得落到组件库自带的蓝绿色。
    let tint = |base: u32, k: f32| rgb(mix(p.text, base, hover_amount * k));
    // 开关关着时的轨道：边框色和说明色之间，浅深皮肤上都看得出来。
    let switch_off = rgb(switch_off_hex(skin));
    c.background = t.background;
    c.foreground = t.text;
    c.border = t.border;
    // 输入框（和默认按钮）的边框：深色皮肤的原边框对底色只有 1.1 : 1，糊在一起，见 [`field_border_hex`]。
    c.input = t.field_border;
    c.ring = t.accent;
    c.caret = t.text;
    c.selection = t.selection;
    c.muted = t.subtle;
    c.muted_foreground = t.text_muted;
    c.accent = t.accent_soft;
    c.accent_foreground = t.text;
    // 菜单 / 弹出层 / 下拉：深色皮肤上比主背景亮一点，见 [`popover_hex`]。
    c.popover = t.popover;
    c.popover_foreground = t.text;
    c.group_box = t.surface;
    c.group_box_foreground = t.text;
    c.primary = t.accent;
    c.primary_foreground = t.on_accent;
    c.primary_hover = accent_hover;
    c.primary_active = accent_active;
    c.secondary = t.subtle;
    c.secondary_foreground = t.text;
    c.secondary_hover = subtle_hover;
    c.secondary_active = subtle_active;
    c.danger = t.danger;
    c.danger_foreground = rgb(0xFFFFFF);
    c.danger_hover = danger_hover;
    c.danger_active = danger_active;
    c.success = t.dot_connected;
    c.warning = t.dot_connecting;
    // 下面这些以前没写，会露出组件库默认的蓝 / 绿 / 灰：滑块、开关、进度条、链接、拖放提示、骨架屏、状态色变体。
    c.success_hover = tint(DOT_CONNECTED, 1.0);
    c.success_active = tint(DOT_CONNECTED, 2.0);
    c.success_foreground = rgb(0xFFFFFF);
    c.info = t.accent;
    c.info_hover = accent_hover;
    c.info_active = accent_active;
    c.info_foreground = t.on_accent;
    c.button_success = t.dot_connected;
    c.button_success_hover = tint(DOT_CONNECTED, 1.0);
    c.button_success_active = tint(DOT_CONNECTED, 2.0);
    c.button_success_foreground = rgb(0xFFFFFF);
    c.button_warning = t.dot_connecting;
    c.button_warning_hover = tint(DOT_CONNECTING, 1.0);
    c.button_warning_active = tint(DOT_CONNECTING, 2.0);
    c.button_warning_foreground = rgb(0x1F2937);
    c.button_info = t.accent;
    c.button_info_hover = accent_hover;
    c.button_info_active = accent_active;
    c.button_info_foreground = t.on_accent;
    c.switch = switch_off;
    // 开关圆点和强调色上的字同一个颜色：库默认取 `background`，深色皮肤上是近黑色的圆点。
    c.switch_thumb = t.switch_thumb;
    c.slider_bar = t.accent;
    c.slider_thumb = t.on_accent;
    c.progress_bar = t.accent;
    c.link = t.accent;
    c.link_hover = accent_hover;
    c.link_active = accent_active;
    c.drop_target = t.accent_soft;
    c.drag_border = t.accent;
    c.skeleton = t.subtle;
    c.accordion = t.background;
    c.description_list_label = t.subtle;
    c.description_list_label_foreground = t.text_muted;
    c.button = t.subtle;
    c.button_hover = subtle_hover;
    c.button_active = subtle_active;
    c.button_foreground = t.text;
    c.button_primary = t.accent;
    c.button_primary_hover = accent_hover;
    c.button_primary_active = accent_active;
    c.button_primary_foreground = t.on_accent;
    c.button_danger = t.danger;
    c.button_danger_hover = danger_hover;
    c.button_danger_active = danger_active;
    c.button_danger_foreground = rgb(0xFFFFFF);
    c.button_secondary = t.subtle;
    c.button_secondary_hover = subtle_hover;
    c.button_secondary_active = subtle_active;
    c.button_secondary_foreground = t.text;
    c.list = t.background;
    c.list_even = t.background;
    c.list_hover = t.hover;
    c.list_active = t.accent_soft;
    c.list_active_border = t.accent;
    c.list_head = t.surface;
    c.sidebar = t.background;
    c.sidebar_foreground = t.text;
    c.sidebar_border = t.divider;
    c.sidebar_accent = t.accent_soft;
    c.sidebar_accent_foreground = t.text;
    c.sidebar_primary = t.accent;
    c.sidebar_primary_foreground = t.on_accent;
    c.tab_bar = t.background;
    c.tab = t.background;
    c.tab_active = t.background;
    c.tab_foreground = t.text_muted;
    c.tab_active_foreground = t.text;
    c.title_bar = t.background;
    c.title_bar_border = t.divider;
    c.status_bar = t.background;
    c.status_bar_border = t.divider;
    c.scrollbar = t.background;
    c.scrollbar_thumb = t.border;
    c.scrollbar_thumb_hover = t.text_muted;
    c.table = t.background;
    c.table_even = t.background;
    c.table_head = t.surface;
    c.table_hover = t.hover;
    c.table_row_border = t.border;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light() -> Skin {
        *skin_for(false, "paper")
    }

    fn dark() -> Skin {
        *skin_for(true, "dracula")
    }

    #[test]
    fn every_chinese_skin_name_has_an_english_display_name() {
        use crate::i18n::Msg as _;
        // 中文名的皮肤（纸白、石墨）在英文界面里必须显示英文名，不能把中文原样漏出来。
        assert_eq!(ThemeMsg::Graphite.en(), "Graphite");
        assert_eq!(ThemeMsg::Graphite.zh(), "石墨");
        for skin in SKINS {
            let shown = skin_name(skin);
            if !skin.name.is_ascii() && !skin.name.contains('é') {
                assert!(
                    shown == ThemeMsg::PaperWhite.zh()
                        || shown == ThemeMsg::PaperWhite.en()
                        || shown == ThemeMsg::Graphite.zh()
                        || shown == ThemeMsg::Graphite.en(),
                    "{} 的显示名没有走 i18n 表",
                    skin.id
                );
            }
        }
        // 英文表里不留汉字。
        for text in [ThemeMsg::PaperWhite.en(), ThemeMsg::Graphite.en()] {
            assert!(text.is_ascii(), "{text}");
        }
    }

    #[test]
    fn the_appearance_page_offers_every_skin_the_old_title_bar_menu_did() {
        // 标题栏的主题菜单（V2-U6 删掉）列的是 `skins_of(dark)`；「设置 → 外观」里浅色 / 深色两排皮肤
        // 卡片用的是同一个函数，所以每一套皮肤在外观页都能选，不会因为菜单没了就少一个。
        let mut offered: Vec<&str> = skins_of(false)
            .chain(skins_of(true))
            .map(|s| s.id)
            .collect();
        let mut all: Vec<&str> = SKINS.iter().map(|s| s.id).collect();
        offered.sort_unstable();
        all.sort_unstable();
        assert_eq!(offered, all);
        assert!(skins_of(false).all(|s| !s.dark));
        assert!(skins_of(true).all(|s| s.dark));
    }
    #[test]
    fn mixing_matches_the_frontend_formula() {
        // 全部取一端、取另一端，和 skins.ts 的 mixHex 一致。
        assert_eq!(mix(0x102030, 0xFFFFFF, 1.0), 0x102030);
        assert_eq!(mix(0x102030, 0xFFFFFF, 0.0), 0xFFFFFF);
        assert_eq!(mix(0x000000, 0xFFFFFF, 0.5), 0x808080);
    }

    #[test]
    fn focus_is_a_thin_accent_border_without_the_outer_halo() {
        // 聚焦输入框 = 1px 强调色边框；外圈光晕关着（组件库在光晕关闭时只染边框）。
        const { assert!(!FOCUS_HALO) };
        let source = include_str!("app_theme.rs");
        assert!(
            source.contains("theme.focus_ring = FOCUS_HALO;"),
            "apply 要把它写进主题"
        );
        assert!(source.contains("c.ring = t.accent;"), "边框色取强调色");
    }

    #[test]
    fn skin_ids_are_unique_and_grouped_like_0_2_14() {
        let mut ids: Vec<&str> = SKINS.iter().map(|skin| skin.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SKINS.len());
        assert_eq!(skins_of(false).next().map(|skin| skin.id), Some("paper"));
        assert_eq!(skins_of(true).next().map(|skin| skin.id), Some("graphite"));
        assert_eq!(skins_of(false).count(), 8);
        assert_eq!(skins_of(true).count(), 9);
    }

    #[test]
    fn skin_ids_match_the_sync_whitelist() {
        // 同步校验（core 的白名单）和这里的皮肤表必须一个不多一个不少，否则同步过来的皮肤认不得。
        let light: Vec<&str> = skins_of(false).map(|skin| skin.id).collect();
        let dark: Vec<&str> = skins_of(true).map(|skin| skin.id).collect();
        assert_eq!(light, zssh_core::sync::settings::LIGHT_SKIN_IDS);
        assert_eq!(dark, zssh_core::sync::settings::DARK_SKIN_IDS);
        for id in zssh_core::sync::settings::TERMINAL_THEME_IDS {
            if *id != TERMINAL_FOLLOW_SKIN && skin(id).is_none() {
                // 剩下的只能是 0.1.4 及以前的旧配色 id，界面一律当成跟随皮肤。
                assert!(id.starts_with("system-"), "{id} 既不是皮肤也不是旧配色");
                assert_eq!(normalize_terminal_theme(id), TERMINAL_FOLLOW_SKIN);
            }
        }
    }

    #[test]
    fn the_defaults_are_github_graphite_and_a_terminal_that_follows_the_skin() {
        // 新用户默认（V2-U17）：浅色 GitHub、深色石墨、终端配色跟随皮肤。
        let look = Look::default();
        assert_eq!(look.skin(true).id, "graphite");
        assert_eq!(look.skin(false).id, "github-light");
        assert_eq!(look.terminal_theme, TERMINAL_FOLLOW_SKIN);
        // 跟随皮肤：终端底色就是皮肤自己的终端底色，不会出现「界面浅色、终端深色」。
        for dark in [false, true] {
            assert_eq!(
                look.terminal_palette(dark).background,
                look.skin(dark).terminal.background
            );
        }
        // 这三个默认值都得是真实存在的皮肤，且明暗对得上。
        assert!(skin(DEFAULT_LIGHT_SKIN).is_some_and(|s| !s.dark));
        assert!(skin(DEFAULT_DARK_SKIN).is_some_and(|s| s.dark));
        assert_eq!(
            normalize_terminal_theme(DEFAULT_TERMINAL_THEME),
            TERMINAL_FOLLOW_SKIN
        );
    }

    #[test]
    fn unknown_or_wrong_mode_skin_ids_fall_back_to_the_first_of_that_mode() {
        assert_eq!(skin_for(true, "nonsense").id, "graphite");
        assert_eq!(skin_for(false, "nonsense").id, "paper");
        // 浅色皮肤 id 用在深色上也退回。
        assert_eq!(skin_for(true, "paper").id, "graphite");
        assert_eq!(skin_for(false, "nord").id, "paper");
        assert_eq!(skin_for(true, "nord").id, "nord");
    }

    #[test]
    fn the_terminal_palette_follows_the_skin_or_a_fixed_choice() {
        let mut look = Look {
            skin_light: "solarized-light".to_owned(),
            skin_dark: "nord".to_owned(),
            terminal_theme: TERMINAL_FOLLOW_SKIN.to_owned(),
        };
        assert_eq!(look.terminal_palette(true).background, 0x2E3440);
        assert_eq!(look.terminal_palette(false).background, 0xFDF6E3);
        look.terminal_theme = "dracula".to_owned();
        assert_eq!(look.terminal_palette(false).background, 0x282A36);
        // 旧版本的配色 id 一律跟随皮肤。
        look.terminal_theme = "system-monokai-pro-spectrum-dark".to_owned();
        assert_eq!(look.terminal_palette(false).background, 0xFDF6E3);
        look.terminal_theme = "nonsense".to_owned();
        assert_eq!(look.terminal_palette(true).background, 0x2E3440);
        assert_eq!(normalize_terminal_theme("gruvbox-dark"), "gruvbox-dark");
        assert_eq!(normalize_terminal_theme("skin"), "skin");
    }

    #[test]
    fn the_region_divider_sits_between_the_background_and_the_border_on_every_skin() {
        // 侧栏、标题栏、状态栏和主区同一个底色，只靠一根很淡的线分界：线比卡片边框更接近底色，但仍看得出来。
        for skin in SKINS {
            let ui = skin.ui;
            let divider = divider_hex(skin);
            let to_bg = contrast(divider, ui.background);
            let border_to_bg = contrast(ui.border, ui.background);
            assert!(
                to_bg < border_to_bg,
                "{} 分隔线({to_bg:.3})要比边框({border_to_bg:.3})更接近底色",
                skin.id
            );
            assert!(to_bg > 1.0, "{} 分隔线不能和底色一样", skin.id);
            assert_eq!(
                Tokens::for_skin(skin).divider,
                rgb(divider),
                "{} token 取的就是这个值",
                skin.id
            );
        }
    }
    #[test]
    fn every_skin_is_readable() {
        // 对应 skins.test.ts 的「文字都看得清」。
        for skin in SKINS {
            let (ui, term) = (skin.ui, skin.terminal);
            assert!(contrast(ui.text, ui.background) >= 4.5, "{} 正文", skin.id);
            assert!(
                contrast(ui.text, ui.surface) >= 4.5,
                "{} 卡片上的正文",
                skin.id
            );
            assert!(
                contrast(ui.text_muted, ui.background) >= 3.0,
                "{} 次要文字",
                skin.id
            );
            assert!(
                contrast(on_accent(skin), ui.accent) >= 3.0,
                "{} 按钮上的字",
                skin.id
            );
            assert!(
                contrast(term.foreground, term.background) >= 4.0,
                "{} 终端文字",
                skin.id
            );
            // 明暗要名副其实：深色皮肤背景比黑更接近黑。
            let dark_bg = contrast(ui.background, 0x000000) < contrast(ui.background, 0xFFFFFF);
            assert_eq!(dark_bg, skin.dark, "{} 明暗", skin.id);
        }
    }

    /// 每个皮肤的三级文字色（V2-U4）。
    fn inks() -> Vec<(&'static Skin, Ink)> {
        SKINS.iter().map(|skin| (skin, ink_for(skin))).collect()
    }

    #[test]
    fn the_primary_ink_is_much_darker_or_lighter_than_the_skin_text() {
        // 浅色接近 #1a1a1a、深色接近 #f2f2f2 的方向：比皮肤表里的正文色更靠近纯黑 / 纯白，
        // 对比度至少到 9.5 : 1（WCAG AAA 是 7 : 1；标题、名称要「黑」）。
        for (skin, ink) in inks() {
            let extreme = if skin.dark { 0xFFFFFF } else { 0x000000 };
            assert!(
                contrast(ink.primary, extreme) < contrast(skin.ui.text, extreme),
                "{} 主文字色没有比正文色更极端",
                skin.id
            );
            assert!(
                contrast(ink.primary, skin.ui.background) >= 9.5,
                "{} 主文字色对底色 {:.1}",
                skin.id,
                contrast(ink.primary, skin.ui.background)
            );
            assert!(contrast(ink.primary, skin.ui.surface) >= 9.5, "{}", skin.id);
        }
        // 纸白 #1b1b1b、Dracula #fcfcf9 的具体取值。
        assert_eq!(ink_for(&light()).primary, 0x1B1B1B);
        assert_eq!(ink_for(&dark()).primary, 0xFCFCF9);
    }

    #[test]
    fn secondary_and_faint_inks_stay_readable_on_every_skin() {
        for (skin, ink) in inks() {
            let ui = skin.ui;
            for (name, bg) in [("底色", ui.background), ("卡片色", ui.surface)] {
                assert!(
                    contrast(ink.secondary, bg) >= MUTED_MIN_CONTRAST,
                    "{} 次要色在{name}上 {:.2}",
                    skin.id,
                    contrast(ink.secondary, bg)
                );
                assert!(
                    contrast(ink.faint, bg) >= FAINT_MIN_CONTRAST,
                    "{} 第三级色在{name}上 {:.2}",
                    skin.id,
                    contrast(ink.faint, bg)
                );
            }
        }
    }

    #[test]
    fn the_three_inks_step_down_clearly_on_every_skin() {
        for (skin, ink) in inks() {
            let bg = skin.ui.background;
            let (p, s, f) = (
                contrast(ink.primary, bg),
                contrast(ink.secondary, bg),
                contrast(ink.faint, bg),
            );
            // 主 > 次 > 淡，而且每一级之间至少拉开 1 : 1 的对比度，说明文字才「明显更淡」。
            assert!(p - s >= 1.0, "{} 主 {p:.1} 与次 {s:.1} 太近", skin.id);
            assert!(s > f, "{} 次 {s:.1} 不比淡 {f:.1} 更清楚", skin.id);
            // 未选中的导航项落在主色和次色之间。
            let n = contrast(ink.nav, bg);
            assert!(
                n < p && n > s,
                "{} 导航色 {n:.1} 不在 {s:.1}～{p:.1} 之间",
                skin.id
            );
        }
    }

    #[test]
    fn a_skin_whose_own_muted_text_is_already_readable_keeps_it() {
        // 皮肤表里的说明色够清楚（纸白、GitHub）就原样用，不被改动。
        for id in ["paper", "github-light", "graphite", "dracula"] {
            let skin = skin(id).expect("内置皮肤");
            assert_eq!(ink_for(skin).secondary, skin.ui.text_muted, "{id}");
        }
        // 说明色在卡片色上不够 4.5 的皮肤（Solarized 浅色）会被拉深到够。
        let solarized = skin("solarized-light").expect("内置皮肤");
        assert!(contrast(solarized.ui.text_muted, solarized.ui.surface) < MUTED_MIN_CONTRAST);
        assert_ne!(ink_for(solarized).secondary, solarized.ui.text_muted);
    }

    #[test]
    fn tokens_carry_the_new_inks() {
        for skin in SKINS {
            let t = Tokens::for_skin(skin);
            let ink = ink_for(skin);
            assert_eq!(t.text, rgb(ink.primary));
            assert_eq!(t.text_muted, rgb(ink.secondary));
            assert_eq!(t.text_faint, rgb(ink.faint));
            assert_eq!(t.text_nav, rgb(ink.nav));
        }
    }

    /// 把全部皮肤的三级文字色和对比度打印出来（写报告用）：
    /// `cargo test -p zssh-gpui print_ink_table -- --ignored --nocapture`
    #[test]
    #[ignore = "只用来打印对比度表"]
    fn print_ink_table() {
        for (skin, ink) in inks() {
            let ui = skin.ui;
            println!(
                "{:<18} 主 {:06X} {:>5.1} | 次 {:06X} {:>4.1}（原 {:06X} {:>4.1}） | 淡 {:06X} {:>4.1} | 导航 {:06X} {:>4.1} | 卡片上 次 {:>4.1} 淡 {:>4.1}",
                skin.id,
                ink.primary,
                contrast(ink.primary, ui.background),
                ink.secondary,
                contrast(ink.secondary, ui.background),
                ui.text_muted,
                contrast(ui.text_muted, ui.background),
                ink.faint,
                contrast(ink.faint, ui.background),
                ink.nav,
                contrast(ink.nav, ui.background),
                contrast(ink.secondary, ui.surface),
                contrast(ink.faint, ui.surface),
            );
        }
    }

    #[test]
    fn the_two_modes_are_actually_different() {
        let (light_t, dark_t) = (Tokens::for_mode(false), Tokens::for_mode(true));
        assert_ne!(light_t.background, dark_t.background);
        assert_ne!(light_t.text, dark_t.text);
        assert!(!light_t.dark && dark_t.dark);
        // 深色底更暗：亮度差足够大。
        assert!(luminance(light().ui.background) > luminance(dark().ui.background) + 0.5);
    }

    #[test]
    fn terminal_colours_stay_readable_on_their_own_background() {
        // 16 色里除了和背景本来就该接近的那几个（黑/白/亮灰），都要能看清。
        // 阈值沿用原来的 2.0；个别皮肤（比如 Ayu 的亮色、Solarized 的灰阶）是原版色板的配色。
        for skin in [light(), dark()] {
            let term = skin.terminal;
            for (index, colour) in term.ansi.iter().enumerate() {
                if matches!(index, 0 | 7 | 8 | 15) {
                    continue;
                }
                assert!(
                    contrast(*colour, term.background) >= 2.0,
                    "{} 的 ansi {index} 在终端背景上看不清",
                    skin.id
                );
            }
        }
    }

    #[test]
    fn status_dots_are_distinct() {
        let t = Tokens::for_mode(true);
        let dots = [
            t.dot_connected,
            t.dot_connecting,
            t.dot_disconnected,
            t.dot_idle,
        ];
        for (i, a) in dots.iter().enumerate() {
            for b in &dots[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn every_token_is_opaque_except_the_hover_overlay() {
        for skin in SKINS {
            let t = Tokens::for_skin(skin);
            for colour in [
                t.background,
                t.surface,
                t.subtle,
                t.border,
                t.text,
                t.text_muted,
                t.accent,
                t.on_accent,
                t.accent_soft,
                t.danger,
                t.selection,
            ] {
                assert!((colour.a - 1.0).abs() < f32::EPSILON);
            }
            assert!(t.hover.a > 0.0 && t.hover.a < 0.2);
        }
    }

    #[test]
    fn a_picked_skin_changes_the_tokens() {
        let paper = Tokens::for_skin(skin_for(false, "paper"));
        let latte = Tokens::for_skin(skin_for(false, "catppuccin-latte"));
        assert_ne!(paper.background, latte.background);
        assert_ne!(paper.accent, latte.accent);
    }

    #[test]
    fn the_latency_badge_is_readable_on_every_terminal_background_and_tier() {
        use zssh_core::ssh::latency::Level;
        for ui in SKINS {
            for term in SKINS {
                for level in [Level::Good, Level::Warn, Level::Bad] {
                    let badge = latency_badge(level, ui, term.terminal.background);
                    assert!(
                        contrast(badge.fg, badge.fill) >= MUTED_MIN_CONTRAST,
                        "{} 的界面色 + {} 的终端底 + {level:?}：字 {:06X} 底 {:06X}",
                        ui.name,
                        term.name,
                        badge.fg,
                        badge.fill
                    );
                }
            }
        }
        // 三档在同一个底上互相分得开。
        let skin = skin_for(true, DEFAULT_DARK_SKIN);
        let bg = skin.terminal.background;
        let colours: Vec<u32> = [Level::Good, Level::Warn, Level::Bad]
            .map(|level| latency_badge(level, skin, bg).fg)
            .to_vec();
        assert!(colours[0] != colours[1] && colours[1] != colours[2] && colours[0] != colours[2]);
    }

    // ---- UI-UNIFY：控件边界、菜单层次、开关圆点 ----

    #[test]
    fn every_skin_has_a_visible_input_border_against_the_field_fill() {
        for skin in SKINS {
            let border = field_border_hex(skin);
            let floor = field_floor(skin);
            let got = field_border_contrast(skin, border);
            assert!(
                got >= floor,
                "{}: 输入框边框 {border:06X} 对字段底色只有 {got:.2} : 1，低于 {floor}",
                skin.id
            );
        }
    }

    #[test]
    fn a_skin_whose_input_border_is_already_clear_keeps_its_own() {
        // 只调真的不够的：浅色皮肤的边框（1.2 以上）原样保留，深色皮肤里不够的才被拉开。
        for skin in SKINS.iter().filter(|s| !s.dark) {
            assert_eq!(field_border_hex(skin), skin.ui.border, "{}", skin.id);
        }
        for skin in SKINS.iter().filter(|s| s.dark) {
            if field_border_contrast(skin, skin.ui.border) < FIELD_BORDER_MIN_DARK {
                assert_ne!(field_border_hex(skin), skin.ui.border, "{}", skin.id);
            }
        }
        // 石墨：原来 1.1 : 1。
        let graphite = skin("graphite").expect("石墨在表里");
        assert!(field_border_contrast(graphite, graphite.ui.border) < 1.2);
        assert!(field_border_contrast(graphite, field_border_hex(graphite)) >= 1.5);
    }

    #[test]
    fn every_skin_menu_stands_out_from_the_main_background() {
        for skin in SKINS {
            let popover = popover_hex(skin);
            let floor = popover_floor(skin);
            let got = popover_contrast(skin, popover);
            assert!(
                got >= floor,
                "{}: 菜单底色对主背景只有 {got:.2} : 1，低于 {floor}",
                skin.id
            );
            // 菜单里的说明文字（右侧快捷键）仍要读得清。
            let muted = ink_for(skin).secondary;
            assert!(
                contrast(muted, popover) >= MUTED_MIN_CONTRAST,
                "{}: 说明文字在菜单底色上不够清楚",
                skin.id
            );
            assert!(
                contrast(ink_for(skin).primary, popover) >= 7.0,
                "{}: 正文在菜单底色上不够清楚",
                skin.id
            );
        }
    }

    #[test]
    fn light_skin_menus_keep_the_card_colour() {
        for skin in SKINS.iter().filter(|s| !s.dark) {
            assert_eq!(popover_hex(skin), skin.ui.surface, "{}", skin.id);
        }
    }

    #[test]
    fn dark_menus_never_end_up_less_separated_than_the_skin_card_colour() {
        for skin in SKINS.iter().filter(|s| s.dark) {
            assert!(
                popover_contrast(skin, popover_hex(skin))
                    >= popover_contrast(skin, skin.ui.surface),
                "{}",
                skin.id
            );
        }
        // 石墨：菜单抬到比主背景亮一级（原来 #181818 比 #1F1F1F 还暗，只差 1.08 : 1）。
        let graphite = skin("graphite").expect("石墨在表里");
        assert!(luminance(popover_hex(graphite)) > luminance(graphite.ui.background));
        assert!(popover_contrast(graphite, popover_hex(graphite)) >= POPUP_FILL_TARGET_DARK);
    }

    #[test]
    fn the_switch_thumb_is_the_on_accent_colour_and_shows_on_both_tracks() {
        for skin in SKINS {
            let thumb = switch_thumb_hex(skin);
            assert_eq!(thumb, on_accent(skin), "{}", skin.id);
            let on = contrast(thumb, skin.ui.accent);
            let off = contrast(thumb, switch_off_hex(skin));
            assert!(
                on >= SWITCH_THUMB_ON_MIN,
                "{}: 圆点对强调色轨道 {on:.2}",
                skin.id
            );
            assert!(
                off >= SWITCH_THUMB_OFF_MIN,
                "{}: 圆点对关闭轨道 {off:.2}",
                skin.id
            );
        }
        // 石墨：不再是近黑色的圆点。
        let graphite = skin("graphite").expect("石墨在表里");
        assert_eq!(switch_thumb_hex(graphite), 0xFFFFFF);
    }

    #[test]
    fn the_theme_colour_table_reads_the_derived_tokens() {
        // 组件库的输入框边框读 `input`、菜单底色读 `popover`、开关圆点读 `switch_thumb`：
        // 把映射钉住，免得以后有人把它们改回 `border` / `surface` / 组件库默认值。
        for skin in SKINS {
            let mut colors = ThemeColor::default();
            write_colors(&mut colors, skin);
            let t = Tokens::for_skin(skin);
            assert_eq!(colors.input, t.field_border, "{}", skin.id);
            assert_eq!(colors.input, rgb(field_border_hex(skin)), "{}", skin.id);
            assert_eq!(colors.popover, rgb(popover_hex(skin)), "{}", skin.id);
            assert_eq!(colors.switch_thumb, rgb(on_accent(skin)), "{}", skin.id);
            assert_eq!(colors.switch, rgb(switch_off_hex(skin)), "{}", skin.id);
            assert_eq!(colors.primary, rgb(skin.ui.accent), "{}", skin.id);
            // 通用边框（卡片、分隔）不受影响。
            assert_eq!(colors.border, rgb(skin.ui.border), "{}", skin.id);
        }
    }

    #[test]
    #[ignore = "打印 UI-UNIFY 报告里的对比度表：cargo test -p zssh-gpui print_unify_table -- --ignored --nocapture"]
    fn print_unify_table() {
        for skin in SKINS {
            let border = field_border_hex(skin);
            let popover = popover_hex(skin);
            let thumb = switch_thumb_hex(skin);
            println!(
                "{:<18} {} | 边框 {:06X}->{:06X} {:.2}->{:.2} | 菜单 {:06X}->{:06X} {:.2}->{:.2} 外圈 {:.2} 说明字 {:.2} | 圆点 {:06X} 轨道 {:.2} / 关 {:.2}",
                skin.id,
                if skin.dark { "dark " } else { "light" },
                skin.ui.border,
                border,
                field_border_contrast(skin, skin.ui.border),
                field_border_contrast(skin, border),
                skin.ui.surface,
                popover,
                popover_contrast(skin, skin.ui.surface),
                popover_contrast(skin, popover),
                popover_ring_contrast(skin),
                contrast(ink_for(skin).secondary, popover),
                thumb,
                contrast(thumb, skin.ui.accent),
                contrast(thumb, switch_off_hex(skin)),
            );
        }
    }
}
