//! ZeeTerm 图标出图工具：把 SVG 渲染成各尺寸的 PNG，打包成 `zeeterm.ico`。
//!
//! 为什么是一个 Rust 小工具、再由 `scripts/build-icons.ps1` 包一层：PowerShell / .NET 自带的 System.Drawing
//! 画不了 SVG，而 resvg / tiny-skia 本来就在依赖树里（gpui 用它画 SVG），不引入任何新的第三方代码。
//! 渲染结果只取决于输入的 SVG 和 resvg 的版本（不读系统字体、不带时间戳），同样的输入产出逐字节相同的文件。
//!
//! 尺寸分两档：
//! - 16 / 20 / 24 / 32：用 `assets/brand/small/zeeterm-icon-<N>.svg`。每个尺寸一张按像素网格画的 SVG（viewBox 就是
//!   N×N，坐标落在像素边界上，线宽不小于 2 像素），小尺寸直接缩矢量图会糊成一团。
//! - 48 / 64 / 128 / 256：用 `assets/brand/zeeterm-logo.svg` 直接渲染。
//!
//! 用法：`zssh-icons <brand 目录> <安装包图标路径> [--check]`。`--check` 只比对不写入，有差异就以非 0 退出。
//!
//! Linux 打包另有一个模式：`zssh-icons --hicolor <brand 目录> <输出目录>`，按 freedesktop 的 hicolor 图标主题布局
//! 写出 `<输出目录>/<N>x<N>/apps/app.zeeterm.ZeeTerm.png`（N 见 [`HICOLOR_SIZES`]），同一套渲染、同样逐字节可复现；
//! 由 `scripts/package-linux.sh` 调用，不碰 brand 目录和安装包图标。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use resvg::{tiny_skia, usvg};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// ico 里的尺寸，顺序也是写入顺序。
const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 48, 64, 128, 256];
/// 这些尺寸及以下用像素对齐的小图 SVG。
const SMALL_MAX: u32 = 32;
/// Linux hicolor 图标主题里提供的尺寸（`.deb` 和 `.tar.gz` 都装这一套）。
const HICOLOR_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];
/// Linux 上的图标名 = 应用标识（`.desktop` 的 `Icon=`、窗口的 app_id 都是它；打包脚本和测试核对）。
const LINUX_ICON_NAME: &str = "app.zeeterm.ZeeTerm";

/// 一个待写出的文件：目标路径 + 内容。
struct Output {
    path: PathBuf,
    bytes: Vec<u8>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--hicolor") {
        let (Some(brand), Some(out_dir), 3) = (args.get(1), args.get(2), args.len()) else {
            eprintln!("用法：zssh-icons --hicolor <brand 目录> <输出目录>");
            return ExitCode::from(2);
        };
        return match write_hicolor(Path::new(brand), Path::new(out_dir)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("错误：{err}");
                ExitCode::from(2)
            }
        };
    }
    let check = args.iter().any(|a| a == "--check");
    let paths: Vec<&String> = args.iter().filter(|a| *a != "--check").collect();
    let (Some(brand), Some(installer_icon), 2) = (paths.first(), paths.get(1), paths.len()) else {
        eprintln!("用法：zssh-icons <brand 目录> <安装包图标路径> [--check]");
        return ExitCode::from(2);
    };
    match run(Path::new(brand), Path::new(installer_icon), check) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("错误：{err}");
            ExitCode::from(2)
        }
    }
}

/// 返回 `Ok(true)` 表示成功（`--check` 时表示和磁盘上的文件完全一致）。
fn run(brand: &Path, installer_icon: &Path, check: bool) -> Result<bool> {
    let outputs = build_outputs(brand, installer_icon)?;
    let mut same = true;
    for out in &outputs {
        let current = std::fs::read(&out.path).ok();
        let unchanged = current.as_deref() == Some(out.bytes.as_slice());
        if check {
            if !unchanged {
                same = false;
                eprintln!("与源 SVG 渲染结果不一致：{}", out.path.display());
            }
        } else if unchanged {
            println!("未变化 {}", out.path.display());
        } else {
            std::fs::write(&out.path, &out.bytes)?;
            println!("已写入 {}（{} 字节）", out.path.display(), out.bytes.len());
        }
    }
    Ok(same)
}

/// 渲染 hicolor 主题用的全部尺寸，返回（相对输出目录的路径，PNG 字节）。
fn hicolor_outputs(brand: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let big_svg = std::fs::read_to_string(brand.join("zeeterm-logo.svg"))?;
    HICOLOR_SIZES
        .iter()
        .map(|&size| {
            let svg = svg_for(brand, &big_svg, size)?;
            let path = PathBuf::from(format!("{size}x{size}"))
                .join("apps")
                .join(format!("{LINUX_ICON_NAME}.png"));
            Ok((path, render_png(&svg, size)?))
        })
        .collect()
}

fn write_hicolor(brand: &Path, out_dir: &Path) -> Result<()> {
    for (relative, bytes) in hicolor_outputs(brand)? {
        let path = out_dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
        println!("已写入 {}", path.display());
    }
    Ok(())
}

fn build_outputs(brand: &Path, installer_icon: &Path) -> Result<Vec<Output>> {
    let big_svg = std::fs::read_to_string(brand.join("zeeterm-logo.svg"))?;
    let mut entries = Vec::new();
    for size in ICO_SIZES {
        let svg = svg_for(brand, &big_svg, size)?;
        entries.push((size, render_png(&svg, size)?));
    }
    let ico = pack_ico(&entries)?;
    // 32 位 PNG 给托盘资源加载失败时的后备图标用（小图），64 位给窗口里的 Logo 用（矢量图直接渲染）。
    let png_32 = png_of(&entries, 32)?;
    let png_64 = png_of(&entries, 64)?;
    Ok(vec![
        Output {
            path: brand.join("zeeterm.ico"),
            bytes: ico.clone(),
        },
        Output {
            path: installer_icon.to_path_buf(),
            bytes: ico,
        },
        Output {
            path: brand.join("zeeterm-logo-32.png"),
            bytes: png_32,
        },
        Output {
            path: brand.join("zeeterm-logo-64.png"),
            bytes: png_64,
        },
    ])
}

fn png_of(entries: &[(u32, Vec<u8>)], size: u32) -> Result<Vec<u8>> {
    entries
        .iter()
        .find(|(s, _)| *s == size)
        .map(|(_, png)| png.clone())
        .ok_or_else(|| format!("缺少 {size} 像素的渲染结果").into())
}

fn svg_for(brand: &Path, big_svg: &str, size: u32) -> Result<String> {
    if size <= SMALL_MAX {
        let path = brand.join("small").join(format!("zeeterm-icon-{size}.svg"));
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(big_svg.to_owned())
    }
}

/// 把 SVG 渲染成 `px`×`px` 的 PNG（透明底，非预乘 RGBA）。SVG 的 viewBox 必须是正方形。
fn render_png(svg: &str, px: u32) -> Result<Vec<u8>> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default())?;
    let size = tree.size();
    if (size.width() - size.height()).abs() > f32::EPSILON {
        return Err(format!("SVG 不是正方形：{}×{}", size.width(), size.height()).into());
    }
    let mut pixmap = tiny_skia::Pixmap::new(px, px).ok_or("无法创建画布")?;
    let scale = px as f32 / size.width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

/// 打包 ICO：每个尺寸一张 PNG（Windows Vista 起都认），256 的宽高字节按规范写 0。
fn pack_ico(entries: &[(u32, Vec<u8>)]) -> Result<Vec<u8>> {
    let count = u16::try_from(entries.len())?;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    out.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6 + 16 * u32::from(count);
    for (size, png) in entries {
        let dim = if *size >= 256 {
            0
        } else {
            u8::try_from(*size)?
        };
        out.push(dim);
        out.push(dim);
        out.push(0); // 调色板颜色数
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&u32::try_from(png.len())?.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += u32::try_from(png.len())?;
    }
    for (_, png) in entries {
        out.extend_from_slice(png);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#fff"/></svg>"##;

    #[test]
    fn render_is_deterministic_and_has_requested_size() {
        let a = render_png(SQUARE, 16).unwrap();
        let b = render_png(SQUARE, 16).unwrap();
        assert_eq!(a, b);
        let img = tiny_skia::Pixmap::decode_png(&a).unwrap();
        assert_eq!((img.width(), img.height()), (16, 16));
    }

    #[test]
    fn hicolor_layout_covers_every_size_from_the_brand_sources() {
        let brand = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../zssh-gpui/assets/brand");
        let outputs = hicolor_outputs(&brand).unwrap();
        assert_eq!(outputs.len(), HICOLOR_SIZES.len());
        for ((path, bytes), size) in outputs.iter().zip(HICOLOR_SIZES) {
            let expected = format!("{size}x{size}/apps/app.zeeterm.ZeeTerm.png");
            assert_eq!(path.to_string_lossy().replace('\\', "/"), expected);
            let img = tiny_skia::Pixmap::decode_png(bytes).unwrap();
            assert_eq!((img.width(), img.height()), (size, size));
            assert!(
                img.pixels().iter().any(|p| p.alpha() > 0),
                "{size} 像素的图标是空的"
            );
        }
        // 可复现：再渲染一遍逐字节相同。
        assert_eq!(outputs, hicolor_outputs(&brand).unwrap());
    }

    #[test]
    fn non_square_svg_is_rejected() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 20"></svg>"#;
        assert!(render_png(svg, 16).is_err());
    }

    #[test]
    fn ico_directory_points_at_each_png() {
        let entries = vec![(16, vec![1u8; 5]), (256, vec![2u8; 7])];
        let ico = pack_ico(&entries).unwrap();
        assert_eq!(&ico[0..6], &[0, 0, 1, 0, 2, 0]);
        // 第一项：16×16，长度 5，偏移 6 + 2*16。
        assert_eq!(&ico[6..8], &[16, 16]);
        assert_eq!(u32::from_le_bytes(ico[14..18].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(ico[18..22].try_into().unwrap()), 38);
        // 第二项：256 写成 0。
        assert_eq!(&ico[22..24], &[0, 0]);
        assert_eq!(ico.len(), 38 + 5 + 7);
        assert_eq!(&ico[43..], &[2u8; 7]);
    }
}
