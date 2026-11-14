use anyhow::{Result, ensure};
use image::{RgbaImage, imageops};
use moxcms::{ColorProfile, DataColorSpace, Layout, Transform8BitExecutor, TransformOptions};
use rgb::FromSlice;
use std::sync::Arc;

pub const MAX_ALPHA_ERROR: u8 = 4;

pub type ColorTransform = Arc<Transform8BitExecutor>;

pub fn color_transform(icc: &[u8]) -> Result<ColorTransform> {
    let profile = ColorProfile::new_from_slice(icc)?;
    ensure!(
        profile.color_space == DataColorSpace::Rgb,
        "Lossy compression requires an RGB ICC profile"
    );
    Ok(profile.create_transform_8bit(
        Layout::Rgba,
        &ColorProfile::new_srgb(),
        Layout::Rgba,
        TransformOptions::default(),
    )?)
}

#[derive(Clone, Copy, Default)]
pub struct Difference {
    pub mean: f64,
    pub peak: f64,
    pub alpha_error: u8,
}
impl Difference {
    pub fn accepts_visual(self, limit: f64) -> bool {
        self.mean <= limit && self.peak <= limit * 4.0
    }

    pub fn visual_error(self) -> f64 {
        self.mean.max(self.peak / 4.0)
    }

    pub fn accepts(self, limit: f64) -> bool {
        self.accepts_visual(limit) && self.alpha_error <= MAX_ALPHA_ERROR
    }
}

pub fn mode(width: u32, height: u32) -> &'static str {
    if u64::from(width) * u64::from(height) > 1_000_000 {
        "tiled-512"
    } else {
        "full"
    }
}

// 按原始分辨率分块 控制 DSSIM 内存占用 所有像素都参与评分
// 同时检查峰值 避免平均值掩盖局部损伤
// 分块分数不能当成整图多尺度 DSSIM
pub fn compare(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
) -> Result<Difference> {
    Ok(
        compare_inner(a, b, transform, false, f64::INFINITY, f64::INFINITY)?
            .expect("unbounded comparison cannot reject"),
    )
}

// 分块误差非负 累计均值和峰值只会增加 确定不合格就提前结束
// 合格候选的分数仍与 compare() 一致
pub fn compare_bounded(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
    mean_limit: f64,
    peak_limit: f64,
) -> Result<Option<Difference>> {
    compare_inner(a, b, transform, false, mean_limit, peak_limit)
}

// 透明图同时检查黑白背景
// 只裁掉两图都不可见的边缘 新出现的可见像素不能漏掉
pub fn compare_visible(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
) -> Result<Difference> {
    Ok(
        compare_inner(a, b, transform, true, f64::INFINITY, f64::INFINITY)?
            .expect("unbounded comparison cannot reject"),
    )
}

fn compare_inner(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
    composite: bool,
    mean_limit: f64,
    peak_limit: f64,
) -> Result<Option<Difference>> {
    ensure!(
        a.dimensions() == b.dimensions(),
        "Image dimensions differ before and after compression"
    );
    if a == b {
        return Ok(Some(Difference::default()));
    }
    let mut bounds = (a.width(), a.height(), 0, 0);
    let mut difference = Difference::default();
    for (x, y, p) in a.enumerate_pixels() {
        let q = b.get_pixel(x, y);
        difference.alpha_error = difference.alpha_error.max(p[3].abs_diff(q[3]));
        if !composite || p[3] != 0 || q[3] != 0 {
            bounds.0 = bounds.0.min(x);
            bounds.1 = bounds.1.min(y);
            bounds.2 = bounds.2.max(x + 1);
            bounds.3 = bounds.3.max(y + 1);
        }
    }
    if bounds.2 == 0 {
        return Ok(Some(difference));
    }
    let (left, top, right, bottom) = bounds;
    let (width, height) = (right - left, bottom - top);
    if left == 0
        && top == 0
        && right == a.width()
        && bottom == a.height()
        && mode(width, height) == "full"
    {
        let score = compare_region(a, b, transform, composite)?;
        difference.mean = score;
        difference.peak = score;
        return Ok((score <= mean_limit && score <= peak_limit).then_some(difference));
    }
    let step = if mode(width, height) == "full" {
        width.max(height) as usize
    } else {
        512
    };
    for y in (top..bottom).step_by(step) {
        for x in (left..right).step_by(step) {
            let w = (step as u32).min(right - x);
            let h = (step as u32).min(bottom - y);
            let a = imageops::crop_imm(a, x, y, w, h).to_image();
            let b = imageops::crop_imm(b, x, y, w, h).to_image();
            let score = compare_region(&a, &b, transform, composite)?;
            difference.mean += score * f64::from(w) * f64::from(h);
            difference.peak = difference.peak.max(score);
            if difference.peak > peak_limit
                || difference.mean / (f64::from(width) * f64::from(height)) > mean_limit
            {
                return Ok(None);
            }
        }
    }
    difference.mean /= f64::from(width) * f64::from(height);
    Ok(Some(difference))
}

fn compare_region(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
    composite: bool,
) -> Result<f64> {
    let compare = |a: &RgbaImage, b: &RgbaImage| -> Result<f64> {
        if !composite {
            return dssim(a, b);
        }
        let mut worst: f64 = 0.0;
        for background in [0u32, 255] {
            let flatten = |im: &RgbaImage| {
                RgbaImage::from_fn(im.width(), im.height(), |x, y| {
                    let p = im.get_pixel(x, y);
                    let alpha = u32::from(p[3]);
                    let mut out = image::Rgba([0, 0, 0, 255]);
                    for c in 0..3 {
                        out[c] = ((u32::from(p[c]) * alpha + background * (255 - alpha) + 127)
                            / 255) as u8;
                    }
                    out
                })
            };
            worst = worst.max(dssim(&flatten(a), &flatten(b))?);
        }
        Ok(worst)
    };
    let raw = compare(a, b)?;
    let Some(transform) = transform else {
        return Ok(raw);
    };
    let convert = |im: &RgbaImage| -> Result<RgbaImage> {
        let mut converted = RgbaImage::new(im.width(), im.height());
        transform.transform(im.as_raw(), converted.as_mut())?;
        Ok(converted)
    };
    // 先转前景颜色 再合成背景 同时检查原始编码值
    // 避免宽色域转 sRGB 时的裁剪掩盖损伤
    Ok(raw.max(compare(&convert(a)?, &convert(b)?)?))
}

// 小网格只补充局部检查 不替代全分辨率的 512 像素分块评分
pub fn region_errors(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
) -> Result<Vec<f64>> {
    let mut errors = Vec::new();
    check_regions(a, b, transform, |_, score| {
        errors.push(score);
        true
    })?;
    Ok(errors)
}

// 按原顺序检查 遇到不合格区域就停止
pub fn check_regions(
    a: &RgbaImage,
    b: &RgbaImage,
    transform: Option<&ColorTransform>,
    mut accepts: impl FnMut(usize, f64) -> bool,
) -> Result<bool> {
    ensure!(
        a.dimensions() == b.dimensions(),
        "Image dimensions differ before and after compression"
    );
    let mut index = 0;
    for y in (0..a.height()).step_by(256) {
        for x in (0..a.width()).step_by(256) {
            let w = 256.min(a.width() - x);
            let h = 256.min(a.height() - y);
            let score = compare_region(
                &imageops::crop_imm(a, x, y, w, h).to_image(),
                &imageops::crop_imm(b, x, y, w, h).to_image(),
                transform,
                false,
            )?;
            if !accepts(index, score) {
                return Ok(false);
            }
            index += 1;
        }
    }
    Ok(true)
}

fn dssim(a: &RgbaImage, b: &RgbaImage) -> Result<f64> {
    if a == b {
        return Ok(0.0);
    }
    // DSSIM 金字塔要求最小尺寸 边缘小块也要补齐
    if a.width() < 8 || a.height() < 8 {
        let pad = |im: &RgbaImage| {
            RgbaImage::from_fn(im.width().max(8), im.height().max(8), |x, y| {
                *im.get_pixel(x.min(im.width() - 1), y.min(im.height() - 1))
            })
        };
        return dssim(&pad(a), &pad(b));
    }
    let context = dssim_core::Dssim::new();
    let create = |im: &RgbaImage| {
        context
            .create_image_rgba(
                im.as_raw().as_rgba(),
                im.width() as usize,
                im.height() as usize,
            )
            .ok_or_else(|| anyhow::anyhow!("Failed to create DSSIM image"))
    };
    let original = create(a)?;
    let modified = create(b)?;
    let score: f64 = context.compare(&original, modified).0.into();
    ensure!(
        score.is_finite() && score >= 0.0,
        "Quality metric is not a finite nonnegative value"
    );
    Ok(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_comparison_matches_complete_scores_and_decisions() {
        let p3 = color_transform(&ColorProfile::new_display_p3().encode().unwrap()).unwrap();
        for (width, height) in [(7, 9), (96, 64), (513, 2049)] {
            let original = RgbaImage::from_fn(width, height, |x, y| {
                image::Rgba([(x % 256) as u8, (y % 256) as u8, 80, 255])
            });
            let mut changed = original.clone();
            // 覆盖末尾的小块 包括只有一个像素宽的边缘
            for y in height.saturating_sub(32)..height {
                for x in width.saturating_sub(32)..width {
                    changed.put_pixel(x, y, image::Rgba([220, 40, 20, 255]));
                }
            }
            for transform in [None, Some(&p3)] {
                let full = compare(&original, &changed, transform).unwrap();
                assert!(full.mean > 0.0);
                for (mean, peak) in [
                    // DSSIM 并行归约可能有机器精度量级的浮点差异
                    (full.mean + 1e-12, full.peak + 1e-12),
                    (full.mean * 0.5, f64::INFINITY),
                    (f64::INFINITY, full.peak * 0.5),
                ] {
                    let bounded =
                        compare_bounded(&original, &changed, transform, mean, peak).unwrap();
                    assert_eq!(bounded.is_some(), full.mean <= mean && full.peak <= peak);
                    if let Some(score) = bounded {
                        assert!((score.mean - full.mean).abs() < 1e-12);
                        assert!((score.peak - full.peak).abs() < 1e-12);
                        assert_eq!(score.alpha_error, full.alpha_error);
                    }
                }
                let exact = compare_bounded(&original, &original, transform, 0.0, 0.0)
                    .unwrap()
                    .unwrap();
                assert_eq!(exact.mean, 0.0);
                assert_eq!(exact.peak, 0.0);
            }
        }
    }

    #[test]
    fn local_check_stops_at_rejection_and_keeps_region_order() {
        let a = RgbaImage::from_pixel(513, 257, image::Rgba([80, 80, 80, 255]));
        let mut b = a.clone();
        b.put_pixel(512, 256, image::Rgba([250, 0, 0, 255]));
        let scores = region_errors(&a, &b, None).unwrap();
        assert_eq!(scores.len(), 6);
        assert!(scores[5] > 0.0);
        let mut visits = 0;
        assert!(
            !check_regions(&a, &b, None, |i, score| {
                assert!((score - scores[i]).abs() < 1e-12);
                visits += 1;
                i < 2
            })
            .unwrap()
        );
        assert_eq!(visits, 3);
        assert!(!check_regions(&a, &b, None, |_, score| score == 0.0).unwrap());
    }

    #[test]
    fn visible_comparison_ignores_padding_but_catches_new_content() {
        let a = RgbaImage::from_pixel(16, 16, image::Rgba([200, 100, 40, 100]));
        let b = RgbaImage::from_pixel(16, 16, image::Rgba([180, 100, 40, 110]));
        let score = compare_visible(&a, &b, None).unwrap();
        let mut padded_a = RgbaImage::new(128, 128);
        let mut padded_b = padded_a.clone();
        imageops::replace(&mut padded_a, &a, 48, 48);
        imageops::replace(&mut padded_b, &b, 48, 48);
        let padded = compare_visible(&padded_a, &padded_b, None).unwrap();
        assert!((score.mean - padded.mean).abs() < 1e-10);
        assert_eq!(score.alpha_error, padded.alpha_error);
        padded_b.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));
        assert_eq!(
            compare_visible(&padded_a, &padded_b, None)
                .unwrap()
                .alpha_error,
            255
        );
        let empty = RgbaImage::new(16, 16);
        let black = RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 0, 255]));
        // 只看黑色背景会漏掉这里的不透明度变化
        assert!(
            !compare_visible(&empty, &black, None)
                .unwrap()
                .accepts_visual(0.002)
        );
    }
    #[test]
    fn color_management_preserves_alpha() {
        let transform = color_transform(&ColorProfile::new_display_p3().encode().unwrap()).unwrap();
        let input = [255, 0, 0, 0, 0, 255, 0, 128, 0, 0, 255, 255];
        let mut output = [0; 12];
        transform.transform(&input, &mut output).unwrap();
        assert_eq!([output[3], output[7], output[11]], [0, 128, 255]);
    }
    #[test]
    fn tiled_check_catches_local_damage_and_last_edge() {
        let a = RgbaImage::from_pixel(1025, 1025, image::Rgba([120, 120, 120, 255]));
        let mut b = a.clone();
        for y in 512..1025 {
            for x in 512..1025 {
                b.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
        }
        let score = compare(&a, &b, None).unwrap();
        assert!(score.peak > 0.01);
        assert!(!score.accepts(0.0015));
    }
}
