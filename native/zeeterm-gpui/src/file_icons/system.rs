//! Windows 系统文件图标：像 Xftp 那样按扩展名向外壳要关联图标。
//!
//! 怎么取：`SHGetFileInfoW` 用 `SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX` 按**假文件名**查系统图标索引，
//! 再从 `SHGetImageList` 的对应尺寸的图标列表里取出图标（16 / 32 / 48 / 256 像素，按窗口缩放选够清晰的一档，
//! 高分屏上由 GPUI 缩小到 16 逻辑像素），转成 BGRA 位图交给 GPUI。图标句柄和 GDI 位图用完就释放。
//!
//! 隐私和安全：传给系统的**只是**清洗过的扩展名拼出来的假名（`zeeterm-icon.<ext>`，见 `fake_name`），
//! `USEFILEATTRIBUTES` 让外壳不去碰文件系统，所以远端或本地的真实路径、文件名都不会交给系统。
//!
//! 缓存和后台线程见 [`super::cache`]；第一次查询可能要加载外壳扩展，所以放在后台线程里（那条线程先初始化 COM），
//! 查好之前界面先画内置图标。
//!
//! 通用图标：扩展名没有关联时，外壳会给一个通用白纸。分类认识它的类型（`.log`、`.conf`、`.pem` 常常没有关联）
//! 应当用内置分类图标，所以要认出它：和一个肯定没有关联的假扩展名取到的图标**逐像素比较**，相同就当「没有系统图标」。
//! 分类也不认识的扩展名遇到通用图标同样回到内置的灰色文件图标，两条路径一致。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use ::gpui::{AnyElement, ImageSource, IntoElement as _, RenderImage, Styled as _, img, px};
use image::{Frame, RgbaImage};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HGDIOBJ, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, SHGetImageList,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::PCWSTR;

use super::cache::{Lookup, Resolver};
use super::system_extension;

/// 一个肯定没有任何关联的假扩展名：取到的就是系统的「通用文件」图标，拿来当比较基准。
const PROBE_EXTENSION: &str = "zeeterminoassoc";
/// 最多缓存多少个（扩展名 × 尺寸）：远端目录的文件名由对方说了算，别让缓存无限长。
const CACHE_CAPACITY: usize = 512;
/// 文件行图标的逻辑边长。
const LOGICAL_SIZE: f32 = 16.0;

/// 一张取回来的图标位图（BGRA，直通 alpha，行从上到下）。
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Bitmap {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bgra: Vec<u8>,
}

/// 线程上初始化 COM 的守卫：外壳要在 STA 线程里用。已经初始化过（别的模式）也不算错，只是不用配对反初始化。
pub(super) struct ComGuard(bool);

impl ComGuard {
    pub(super) fn new() -> Self {
        // SAFETY: 只是在当前线程上初始化 COM；成功才在 drop 里配对反初始化。
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Self(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 和 `new` 里成功的 CoInitializeEx 配对，同一线程。
            unsafe { CoUninitialize() };
        }
    }
}

/// 窗口缩放 → 要取的位图边长（像素）：外壳图标列表只有 16 / 32 / 48 / 256 这几档，取够用的最小一档，
/// 显示时 GPUI 缩小到 16 逻辑像素，所以 125% / 150% / 200% 屏上都不是放大的。
pub(super) fn bucket_for_scale(scale: f32) -> u32 {
    let wanted = (LOGICAL_SIZE * scale.max(1.0)).ceil() as u32;
    match wanted {
        0..=16 => 16,
        17..=32 => 32,
        33..=48 => 48,
        _ => 256,
    }
}

/// 图标列表的档位编号（`SHIL_*`）。
fn image_list_kind(bucket: u32) -> i32 {
    use windows::Win32::UI::Shell::{SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL};
    (match bucket {
        0..=16 => SHIL_SMALL,
        17..=32 => SHIL_LARGE,
        33..=48 => SHIL_EXTRALARGE,
        _ => SHIL_JUMBO,
    }) as i32
}

/// 交给系统的假文件名：只由已经清洗过的扩展名拼成。
fn fake_name(ext: &str) -> Vec<u16> {
    format!("zeeterm-icon.{ext}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// 取一个扩展名在 `bucket` 档位下的系统图标位图。**调用线程要先初始化 COM。** 任何一步失败都是 `None`。
pub(super) fn icon_bitmap(ext: &str, bucket: u32) -> Option<Bitmap> {
    // 外壳的系统图标列表第一次初始化时并发调用会偶发失败：正式路径只有一条后台线程，
    // 这把锁让测试线程等也和它一样一次一个。
    static SHELL: Mutex<()> = Mutex::new(());
    let _serial = SHELL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // 防御：再清洗一次，保证传给系统的永远是字母数字扩展名。
    let ext = super::sanitize_extension(ext)?;
    let name = fake_name(&ext);
    let mut info = SHFILEINFOW::default();
    // SAFETY: `name` 以 0 结尾且活过这次调用；`info` 是有效的输出缓冲，大小传对。
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(name.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: 只是按档位向外壳要系统图标列表。
    let list: IImageList = unsafe { SHGetImageList(image_list_kind(bucket)) }.ok()?;
    // SAFETY: 索引来自上面的查询；返回的图标句柄由 `IconGuard` 负责销毁。
    let icon = unsafe { list.GetIcon(info.iIcon, ILD_TRANSPARENT.0) }.ok()?;
    let icon = IconGuard(icon);
    icon_to_bitmap(icon.0)
}

/// 图标句柄：用完必须 `DestroyIcon`，不然每查一个扩展名漏一个 GDI 对象。
struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        // SAFETY: 句柄是 `GetIcon` 返回的、归我们所有的图标，只销毁这一次。
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

/// GDI 位图句柄：drop 时删除。
struct BitmapGuard(HGDIOBJ);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `GetIconInfo` 给的位图归调用者所有，必须自己删除。
            let _ = unsafe { DeleteObject(self.0) };
        }
    }
}

/// 把图标转成 BGRA 位图：优先用带 alpha 的彩色位图；老式无 alpha 的图标用掩码位图补透明。
fn icon_to_bitmap(icon: HICON) -> Option<Bitmap> {
    let mut info = ICONINFO::default();
    // SAFETY: `info` 是有效的输出结构。
    unsafe { GetIconInfo(icon, &mut info) }.ok()?;
    let colour = BitmapGuard(HGDIOBJ(info.hbmColor.0));
    let mask = BitmapGuard(HGDIOBJ(info.hbmMask.0));
    if info.hbmColor.is_invalid() {
        // 单色图标（没有彩色位图）：外壳图标列表里不会出现，不处理。
        return None;
    }
    let mut bm = BITMAP::default();
    // SAFETY: `bm` 大小传对，句柄有效。
    let got = unsafe {
        GetObjectW(
            colour.0,
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (bm.bmWidth as u32, bm.bmHeight as u32);
    // 屏幕 DC：只是给 GetDIBits 一个设备上下文。
    // SAFETY: 配对的 ReleaseDC 在下面。
    let dc = unsafe { GetDC(Some(HWND::default())) };
    if dc.is_invalid() {
        return None;
    }
    let read = |bitmap: HGDIOBJ| -> Option<Vec<u8>> {
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // 负数 = 自上而下。
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        // SAFETY: 缓冲大小 = 宽 × 高 × 4，和 header 声明一致；句柄有效。
        let lines = unsafe {
            GetDIBits(
                dc,
                windows::Win32::Graphics::Gdi::HBITMAP(bitmap.0),
                0,
                height,
                Some(pixels.as_mut_ptr().cast()),
                &mut header,
                DIB_RGB_COLORS,
            )
        };
        (lines != 0).then_some(pixels)
    };
    let mut bgra = read(colour.0);
    if let Some(pixels) = bgra.as_mut() {
        // 老式图标的彩色位图没有 alpha（全 0）：用掩码位图补（掩码里黑 = 不透明）。
        let (pixel_quads, _) = pixels.as_chunks_mut::<4>();
        if pixel_quads.iter().all(|px| px[3] == 0) {
            match read(mask.0) {
                Some(mask_pixels) => {
                    let (mask_quads, _) = mask_pixels.as_chunks::<4>();
                    for (px, m) in pixel_quads.iter_mut().zip(mask_quads) {
                        px[3] = if m[0] == 0 { 255 } else { 0 };
                    }
                }
                None => pixel_quads.iter_mut().for_each(|px| px[3] = 255),
            }
        }
    }
    // SAFETY: 和上面成功的 GetDC 配对。
    let _ = unsafe { ReleaseDC(Some(HWND::default()), dc) };
    Some(Bitmap {
        width,
        height,
        bgra: bgra?,
    })
}

/// 基准：假扩展名在各档位下的「通用文件」图标。
fn generic_icon(bucket: u32) -> Option<Bitmap> {
    static GENERIC: OnceLock<Mutex<HashMap<u32, Option<Bitmap>>>> = OnceLock::new();
    let cache = GENERIC.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().ok()?;
    cache
        .entry(bucket)
        .or_insert_with(|| icon_bitmap(PROBE_EXTENSION, bucket))
        .clone()
}

/// 这张图标是不是系统给没有关联的扩展名的通用图标（和基准逐像素相同）。
pub(super) fn is_generic(bitmap: &Bitmap, bucket: u32) -> bool {
    generic_icon(bucket).is_some_and(|generic| &generic == bitmap)
}

/// 查一个（扩展名，档位）：有可用的系统图标返回它，通用图标 / 失败返回 `None`。
fn query(ext: &str, bucket: u32) -> Option<Bitmap> {
    let bitmap = icon_bitmap(ext, bucket)?;
    (!is_generic(&bitmap, bucket)).then_some(bitmap)
}

fn to_render_image(bitmap: Bitmap) -> Option<Arc<RenderImage>> {
    // GPUI 的位图是 BGRA 排列，和 GetDIBits 的输出一致，直接装进去。
    let buffer = RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.bgra)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

type IconResolver = Resolver<(String, u32), Arc<RenderImage>>;

fn resolver() -> &'static IconResolver {
    static RESOLVER: OnceLock<IconResolver> = OnceLock::new();
    RESOLVER.get_or_init(|| {
        Resolver::new(
            CACHE_CAPACITY,
            // COM 守卫放进线程的生命周期里：线程不退出，守卫就一直在。
            || std::mem::forget(ComGuard::new()),
            |(ext, bucket): &(String, u32)| query(ext, *bucket).and_then(to_render_image),
        )
    })
}

/// 调试版的开关 `ZSSH_FILE_ICONS_BUILTIN=1`：不用系统图标，只画内置图标（截图对比用）。发行版里恒为 `false`。
fn system_icons_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::dev_hooks::var("ZSSH_FILE_ICONS_BUILTIN").is_none())
}

/// 这个文件名的系统图标：扩展名不合格 / 开关关着时直接是 `Missing`（根本不去问系统），
/// 还在查是 `Pending`，查到可用的是 `Ready`。
pub(super) fn lookup(name: &str, scale: f32) -> Lookup<Arc<RenderImage>> {
    if !system_icons_enabled() {
        return Lookup::Missing;
    }
    let Some(ext) = system_extension(name) else {
        return Lookup::Missing;
    };
    resolver().lookup(&(ext, bucket_for_scale(scale)))
}

/// 系统图标的显示元素：16 逻辑像素见方。
pub(super) fn image_element(image: Arc<RenderImage>) -> AnyElement {
    img(ImageSource::Render(image))
        .size(px(LOGICAL_SIZE))
        .flex_none()
        .into_any_element()
}

/// 后台查询完成的计数：变了就要重画列表。
pub(super) fn generation() -> u64 {
    resolver().generation()
}

#[cfg(test)]
mod tests;
