//! 品牌 Logo：界面里（标题栏左上角）用的位图。
//!
//! 用 64px 的透明底 PNG，标题栏的 26 逻辑像素标志在常见 DPI 下仍然清晰；深浅主题都是
//! 同一张图（紫色卡片 + 深色终端窗口自带轮廓，不靠主题色）。

use std::sync::{Arc, OnceLock};

use ::gpui::{Image, ImageFormat};

const LOGO_PNG: &[u8] = include_bytes!("../assets/brand/zeeterm-logo-64.png");

/// 解码缓存按图的内容哈希，所以每次渲染都拿同一个 `Arc` 就不会重复解码。
pub fn logo() -> Arc<Image> {
    static LOGO: OnceLock<Arc<Image>> = OnceLock::new();
    LOGO.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, LOGO_PNG.to_vec())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_is_a_png_and_is_shared() {
        let logo = logo();
        assert_eq!(logo.format, ImageFormat::Png);
        assert!(logo.bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        assert!(Arc::ptr_eq(&logo, &self::logo()));
    }
}
