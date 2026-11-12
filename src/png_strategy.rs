use crate::{codec, quality};
use anyhow::Result;
use image::RgbaImage;
use quality::{ColorTransform, Difference};

// 用原图生成高质量参考 再据此设定门槛 不给每张图套固定误差额度
// 参考本身也要通过绝对误差上限
fn accepts(candidate: Difference, reference: Difference) -> bool {
    let (mean, peak) = limits(reference);
    candidate.mean <= mean && candidate.peak <= peak
}

fn limits(reference: Difference) -> (f64, f64) {
    (
        (reference.mean * 1.08).min(0.003) + 1e-12,
        (reference.peak * 1.2).min(0.008) + 1e-12,
    )
}

fn accepts_region(candidate: f64, reference: f64, peak_limit: f64) -> bool {
    // 原本精确的区域必须保持精确
    // 调色板可以重新分配小误差 但最差局部误差不能超出参考范围
    let limit = if reference <= 1e-12 { 0.0 } else { peak_limit };
    candidate <= limit + 1e-12
}

pub fn compress(
    image: &RgbaImage,
    icc: Option<&[u8]>,
    transform: Option<&ColorTransform>,
    original_size: usize,
) -> Result<Option<(Vec<u8>, Difference, String)>> {
    let reference_family = codec::opaque_palette(image, 256, None, &[])?;
    let reference_candidate = reference_family.remap(1.0)?;
    let reference_pixels = reference_candidate.pixels();
    if reference_pixels == *image {
        let bytes = codec::attach_icc(reference_candidate.encode()?, icc)?;
        let bytes = codec::lossless_png(&bytes, false)?;
        return Ok((bytes.len() < original_size)
            .then(|| (bytes, Difference::default(), "PNG exact palette".into())));
    }
    let mut reference = quality::compare(image, &reference_pixels, transform)?;
    if reference.mean == 0.0 || !accepts(reference, reference) {
        return Ok(None);
    }
    let reference_regions = quality::region_errors(image, &reference_pixels, transform)?;
    // 让 imagequant 给难处理的区域分配更多颜色
    // 只改候选 不改指标和原始像素
    let mut importance = vec![0; (image.width() * image.height()) as usize];
    let columns = image.width().div_ceil(256) as usize;
    let mut fixed = std::collections::BTreeSet::new();
    for (i, &error) in reference_regions.iter().enumerate() {
        let weight = ((error / reference.mean).sqrt() * 96.0).clamp(24.0, 255.0) as u8;
        let x = (i % columns) as u32 * 256;
        let y = (i / columns) as u32 * 256;
        if error <= 1e-12
            && (y..(y + 256).min(image.height())).all(|row| {
                (x..(x + 256).min(image.width()))
                    .all(|col| image.get_pixel(col, row) == reference_pixels.get_pixel(col, row))
            })
        {
            // 调整权重时保住精确的纯色和背景区域 不放宽零误差要求
            for row in y..(y + 256).min(image.height()) {
                for col in x..(x + 256).min(image.width()) {
                    fixed.insert(image.get_pixel(col, row).0);
                }
            }
        }
        for row in y..(y + 256).min(image.height()) {
            let start = (row * image.width() + x) as usize;
            let end = start + 256.min(image.width() - x) as usize;
            importance[start..end].fill(weight);
        }
    }
    let fixed: Vec<_> = fixed
        .into_iter()
        .map(|[r, g, b, a]| imagequant::RGBA::new(r, g, b, a))
        .collect();
    let region_peak = reference_regions.iter().copied().fold(0.0, f64::max);
    let mut anchors = vec![(reference, region_peak)];
    // 只缓存分数 不给每组候选多留一份全图
    // 再次搜索 dither=1 时复用已有评分
    let mut families = vec![(
        256,
        false,
        reference_family,
        (reference, reference_regions.clone()),
    )];
    for colors in [224, 192, 256] {
        if fixed.len() > colors as usize {
            continue;
        }
        let family = codec::opaque_palette(image, colors, Some(&importance), &fixed)?;
        let pixels = family.remap(1.0)?.pixels();
        let score = quality::compare(image, &pixels, transform)?;
        // 先用实际能达到的均值和峰值分别收紧门槛 再挑更小的候选
        reference.mean = reference.mean.min(score.mean);
        reference.peak = reference.peak.min(score.peak);
        let regions = quality::region_errors(image, &pixels, transform)?;
        anchors.push((score, regions.iter().copied().fold(0.0, f64::max)));
        families.push((colors, true, family, (score, regions)));
    }
    let Some(region_limit) = local_limit(&anchors, reference) else {
        return Ok(None);
    };
    let (mean_limit, peak_limit) = limits(reference);
    let mut best: Option<(Vec<u8>, Difference, String)> = None;
    let mut consider = |family: codec::OpaquePalette<'_>,
                        colors,
                        refined,
                        anchor: (Difference, Vec<f64>)|
     -> Result<()> {
        let mut consider_candidate = |candidate: codec::IndexedPng| -> Result<bool> {
            let difference = if candidate.dither == 1.0 {
                if !accepts(anchor.0, reference)
                    || anchor
                        .1
                        .iter()
                        .zip(&reference_regions)
                        .any(|(a, b)| !accepts_region(*a, *b, region_limit))
                {
                    return Ok(false);
                }
                anchor.0
            } else {
                let pixels = candidate.pixels();
                let Some(difference) =
                    quality::compare_bounded(image, &pixels, transform, mean_limit, peak_limit)?
                else {
                    return Ok(false);
                };
                // 沿用原来的局部网格和精确区域要求
                if !quality::check_regions(image, &pixels, transform, |i, error| {
                    accepts_region(error, reference_regions[i], region_limit)
                })? {
                    return Ok(false);
                }
                difference
            };
            let bytes = codec::attach_icc(candidate.encode()?, icc)?;
            let bytes = codec::lossless_png(&bytes, false)?;
            let best_size = best.as_ref().map_or(original_size, |b| b.0.len());
            if bytes.len() < best_size {
                best = Some((
                    bytes,
                    difference,
                    format!(
                        "PNG {colors} colors, dither={}{}",
                        candidate.dither,
                        if refined { ", weighted" } else { "" },
                    ),
                ));
            }
            Ok(true)
        };
        // 先粗搜 再细搜不合格与合格候选之间的区间
        // 不假设整个码率失真曲线单调 每个输出都要校验
        let mut previous = None;
        let mut bracket = None;
        for dither in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let passed = consider_candidate(family.remap(dither)?)?;
            if passed
                && bracket.is_none()
                && let Some((low, false)) = previous
            {
                bracket = Some((low, dither));
            }
            previous = Some((dither, passed));
        }
        if let Some((mut low, mut high)) = bracket {
            for _ in 0..4 {
                let mid = (low + high) / 2.0;
                if consider_candidate(family.remap(mid)?)? {
                    high = mid;
                } else {
                    low = mid;
                }
            }
        }
        Ok(())
    };
    for (colors, refined, family, anchor) in families {
        consider(family, colors, refined, anchor)?;
    }
    Ok(best)
}

fn local_limit(anchors: &[(Difference, f64)], reference: Difference) -> Option<f64> {
    // 只有通过均值和 512 像素分块检查的候选才能设定 256 像素局部门槛
    // 避免设出合格候选也达不到的目标
    anchors
        .iter()
        .filter(|(score, _)| accepts(*score, reference))
        .map(|(_, peak)| *peak)
        .reduce(f64::min)
        .map(|peak| (peak * 1.2).min(0.012))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_guard_preserves_exact_regions_and_rejects_new_damage() {
        assert!(!accepts_region(0.00001, 0.0, 0.001));
        assert!(accepts_region(0.00008, 0.00004, 0.001));
        assert!(!accepts_region(0.002, 0.001, 0.0012));
    }

    #[test]
    fn rejected_palette_cannot_force_an_unreachable_local_target() {
        let reference = Difference {
            mean: 0.001,
            peak: 0.002,
            alpha_error: 0,
        };
        let rejected = Difference {
            mean: 0.0015,
            ..reference
        };
        let limit = local_limit(&[(reference, 0.004), (rejected, 0.001)], reference).unwrap();
        assert!(local_limit(&[(rejected, 0.001)], reference).is_none());
        assert!((limit - 0.0048).abs() < 1e-12);
        assert!(accepts_region(0.004, 0.003, limit));
        assert!(!accepts_region(0.005, 0.003, limit));
    }

    #[test]
    fn acceptance_is_relative_and_checks_mean_and_peak_separately() {
        let reference = Difference {
            mean: 0.001,
            peak: 0.004,
            alpha_error: 0,
        };
        assert!(accepts(reference, reference));
        // 固定 0.002 门槛会放过这两个候选 原图参考门槛会拒绝它们
        assert!(!accepts(
            Difference {
                mean: 0.0014,
                ..reference
            },
            reference
        ));
        assert!(!accepts(
            Difference {
                peak: 0.005,
                ..reference
            },
            reference
        ));
        let bad = Difference {
            mean: 0.004,
            peak: 0.009,
            alpha_error: 0,
        };
        assert!(!accepts(bad, bad));
        assert!(!accepts(
            Difference {
                mean: f64::NAN,
                ..reference
            },
            reference
        ));
    }
}
