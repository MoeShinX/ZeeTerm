use std::time::{Duration, Instant};

use super::super::cache::Lookup;
use super::super::{FileKind, IconSource, choose_icon, classify};
use super::*;

#[test]
fn the_bucket_follows_the_window_scale_and_never_asks_for_a_blurry_upscale() {
    assert_eq!(bucket_for_scale(1.0), 16);
    assert_eq!(bucket_for_scale(0.5), 16, "缩放小于 1 也不取更小的一档");
    // 125% = 20 像素 → 32 档缩小到 20，比 16 放大到 20 清楚。
    assert_eq!(bucket_for_scale(1.25), 32);
    assert_eq!(bucket_for_scale(1.5), 32);
    assert_eq!(bucket_for_scale(2.0), 32);
    assert_eq!(bucket_for_scale(2.5), 48);
    assert_eq!(bucket_for_scale(3.0), 48);
    assert_eq!(bucket_for_scale(4.0), 256);
}

#[test]
fn a_real_shell_query_for_txt_returns_a_non_empty_bitmap_of_the_requested_size() {
    let _com = ComGuard::new();
    for bucket in [16u32, 32, 48] {
        let bitmap = icon_bitmap("txt", bucket).expect("txt 一定能取到系统图标");
        assert_eq!(
            (bitmap.width, bitmap.height),
            (bucket, bucket),
            "尺寸要符合请求的档位"
        );
        assert_eq!(bitmap.bgra.len(), (bucket * bucket * 4) as usize);
        // 非空：有不透明的像素，且不是纯黑一片。
        let (quads, _) = bitmap.bgra.as_chunks::<4>();
        assert!(quads.iter().any(|px| px[3] > 0), "全透明");
        assert!(
            quads
                .iter()
                .any(|px| px[3] > 0 && (px[0] | px[1] | px[2]) != 0),
            "全黑"
        );
    }
}

#[test]
fn an_extension_nobody_claims_comes_back_as_the_generic_icon() {
    let _com = ComGuard::new();
    for bucket in [16u32, 32] {
        // 两个互不相同、肯定没有关联的假扩展名：取到的图标一样，且都被认成通用图标。
        let a = icon_bitmap("zeeterminoassoc", bucket).expect("通用图标");
        let b = icon_bitmap("qqzzxxnone9", bucket).expect("通用图标");
        assert!(a == b, "没有关联的扩展名应当得到同一张通用图标");
        assert!(is_generic(&a, bucket));
        assert!(is_generic(&b, bucket));
        // query 把通用图标收成 None（不当作系统图标）。
        assert!(query("qqzzxxnone9", bucket).is_none());
    }
}

#[test]
fn a_real_association_is_not_mistaken_for_the_generic_icon() {
    let _com = ComGuard::new();
    let txt = icon_bitmap("txt", 32).expect("txt 图标");
    assert!(!is_generic(&txt, 32), "txt 有关联，不能被当成通用图标");
    assert!(query("txt", 32).is_some());
}

#[test]
fn a_known_category_without_an_association_ends_up_with_the_builtin_icon() {
    // 走完整条路径（后台线程 + 缓存 + 选择）：假扩展名没有关联 → Missing → 内置图标。
    // 分类认识的用分类图标，不认识的是普通文件图标。
    let deadline = Instant::now() + Duration::from_secs(20);
    let settle = |name: &str| loop {
        match lookup(name, 1.0) {
            Lookup::Pending if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => break other,
        }
    };
    // `.qqzzxxnone9` 不会有关联；分类不认识它 → 普通文件。
    let missing = settle("a.qqzzxxnone9");
    assert!(
        matches!(missing, Lookup::Missing),
        "没有关联的扩展名要收成 Missing"
    );
    assert_eq!(
        choose_icon(classify("a.qqzzxxnone9"), missing),
        IconSource::Builtin(FileKind::Plain)
    );
    // 分类认识的类别：把「没有系统图标」套到 .log / .pem 上，选的是内置分类图标。
    assert_eq!(
        choose_icon(classify("x.log"), Lookup::<u8>::Missing),
        IconSource::Builtin(FileKind::Text)
    );
    assert_eq!(
        choose_icon(classify("x.pem"), Lookup::<u8>::Missing),
        IconSource::Builtin(FileKind::Certificate)
    );
}

#[test]
fn names_without_a_usable_extension_never_reach_the_shell() {
    // 这些名字根本不会登记查询：直接 Missing（不是 Pending）。
    for name in [
        "noext",
        ".bashrc",
        "x.a b",
        "x.a/b",
        "x.0123456789abcdefg",
        "x.文档",
    ] {
        assert!(
            matches!(lookup(name, 1.0), Lookup::Missing),
            "{name:?} 不应该去问系统"
        );
    }
}

#[test]
fn the_bitmap_converts_to_a_gpui_image_of_the_same_size() {
    let bitmap = Bitmap {
        width: 2,
        height: 3,
        bgra: vec![255; 2 * 3 * 4],
    };
    let image = to_render_image(bitmap).expect("转换");
    let size = image.size(0);
    assert_eq!((size.width.0, size.height.0), (2, 3));
    // 尺寸和数据对不上的位图不能变成图像（也不 panic）。
    assert!(
        to_render_image(Bitmap {
            width: 4,
            height: 4,
            bgra: vec![0; 3],
        })
        .is_none()
    );
}
