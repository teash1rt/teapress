use crate::{codec, png_strategy, quality};

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Serialize;
use std::{io::Cursor, path::Path, time::Instant};

pub const MAX_BYTES: usize = 25 * 1024 * 1024;
const MAX_PIXELS: u64 = 20_000_000;

const MAX_DSSIM: f64 = 0.002;

#[derive(Serialize)]
pub struct Output {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub before: usize,
    pub after: usize,
    #[serde(skip)]
    pub retained: bool,
    pub dssim: f64,
    pub dssim_peak: f64,
    pub dssim_mode: &'static str,
    pub detail: String,
    pub ms: u128,
}

pub fn compress(data: &[u8], name: &str, lossless: bool) -> Result<Output> {
    let started = Instant::now();
    ensure!(
        !name.is_empty() && !name.contains(['/', '\\', '\0', '\r', '\n']),
        "Filename must be non-empty and contain no path separators or control characters"
    );
    ensure!(!data.is_empty(), "Image file is empty");
    ensure!(data.len() <= MAX_BYTES, "File exceeds 25 MiB");
    let expected = match Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::WebP,
        _ => bail!("Unsupported extension; expected .png, .jpg, .jpeg or .webp (case-insensitive)"),
    };
    let format = image::guess_format(data).context("Unrecognized image format")?;
    ensure!(
        format == expected,
        "File extension does not match image format"
    );
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PIXELS as u32);
    limits.max_image_height = Some(MAX_PIXELS as u32);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("Failed to parse image")?;
    let (width, height) = decoder.dimensions();
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Image exceeds 20 million pixels"
    );
    let icc = decoder.icc_profile()?;
    // 在 image 转成 RGB 之前检查 JPEG 的原始色彩空间
    let cmyk = if format == ImageFormat::Jpeg {
        let mut jpeg = zune_jpeg::JpegDecoder::new_with_options(
            zune_core::bytestream::ZCursor::new(data),
            zune_core::options::DecoderOptions::default()
                .set_strict_mode(false)
                .set_max_width(MAX_PIXELS as usize)
                .set_max_height(MAX_PIXELS as usize),
        );
        jpeg.decode_headers()
            .map_err(|e| anyhow::anyhow!("Failed to parse JPEG headers: {e}"))?;
        matches!(
            jpeg.input_colorspace(),
            Some(zune_core::colorspace::ColorSpace::CMYK | zune_core::colorspace::ColorSpace::YCCK)
        )
    } else {
        false
    };
    let transform = icc.as_deref().map(quality::color_transform).transpose();
    let mut protected_color = cmyk || transform.is_err();
    let transform = transform.ok().flatten();
    if format == ImageFormat::Png {
        let png = png::Decoder::new(Cursor::new(data)).read_info()?;
        let info = png.info();
        ensure!(
            info.animation_control.is_none(),
            "Animated PNG/APNG is unsupported; use a static image"
        );
        // 特殊颜色信息走无损 PNG 路径 避免改变颜色含义
        protected_color |= info.coding_independent_code_points.is_some()
            || info.mastering_display_color_volume.is_some()
            || info.content_light_level.is_some();
        protected_color |= icc.is_none()
            && info.srgb.is_none()
            && (info.source_chromaticities.is_some()
                || info.source_gamma.is_some_and(|g| g.into_scaled() != 45455));
    } else if format == ImageFormat::WebP {
        ensure!(
            !image::codecs::webp::WebPDecoder::new(Cursor::new(data))?.has_animation(),
            "Animated WebP is unsupported; use a static image"
        );
    }
    let orientation = decoder.orientation()?;
    let decoded = DynamicImage::from_decoder(decoder).context("Failed to decode image")?;
    let wide = decoded.color().bits_per_pixel() / u16::from(decoded.color().channel_count()) > 8;
    let mut best = data.to_vec();
    let mut detail = "unchanged: no smaller result".to_owned();
    let mut score = quality::Difference::default();
    let mut lossy_accepted = false;
    let mut composited = false;
    if lossless || protected_color || wide {
        if cmyk {
            detail = "CMYK/YCCK preserved".into();
        } else if protected_color {
            detail = "unsupported color profile; lossless only".into();
        } else if wide {
            detail = "16-bit PNG; lossless only".into();
        } else if format == ImageFormat::Jpeg {
            detail = "JPEG preserved in lossless mode".into();
        } else if format == ImageFormat::WebP
            && orientation != image::metadata::Orientation::NoTransforms
        {
            detail = "WebP orientation preserved".into();
        } else if format == ImageFormat::WebP {
            let candidate = codec::attach_icc(
                codec::webp(&decoded.to_rgba8(), 100.0, true)?,
                icc.as_deref(),
            )?;
            codec::verify_pixels(&decoded, &candidate)?;
            if candidate.len() < best.len() {
                best = candidate;
                detail = "WebP lossless".into();
            }
        }
    } else {
        // 直接转移解码缓冲区 避免再复制一份全图
        let mut reference = decoded;
        reference.apply_orientation(orientation);
        let rgba = reference.into_rgba8();
        let limit = MAX_DSSIM;
        if format == ImageFormat::Png {
            let transparent = rgba.pixels().any(|p| p[3] != 255);
            let opaque = if transparent {
                None
            } else {
                png_strategy::compress(&rgba, icc.as_deref(), transform.as_ref(), best.len())?
            };
            if let Some((bytes, difference, candidate_detail)) = opaque {
                best = bytes;
                score = difference;
                lossy_accepted = true;
                detail = candidate_detail;
            } else if transparent {
                let targets: &[u8] = &[80, 100];
                let mut seen = Vec::new();
                let mut candidates = Vec::new();
                for &target in targets {
                    for dithering in [0.0, 0.5, 1.0] {
                        let candidate = codec::palette_png(&rgba, target, dithering)?;
                        // 最多六个候选 直接比较字节去重 避免哈希冲突
                        if seen.contains(&candidate) {
                            continue;
                        }
                        seen.push(candidate);
                        let candidate = seen.last().expect("candidate just inserted");
                        let pixels = image::load_from_memory(candidate)?.into_rgba8();
                        let difference =
                            quality::compare_visible(&rgba, &pixels, transform.as_ref())?;
                        if !difference.accepts_visual(limit) {
                            continue;
                        }
                        candidates.push((
                            candidate.clone(),
                            difference,
                            format!("PNG palette q={target}, dither={dithering}"),
                        ));
                    }
                }
                // 只在接近最佳实测可见画质的候选里挑最小的
                let best_error = candidates
                    .iter()
                    .map(|(_, score, _)| score.visual_error())
                    .fold(f64::INFINITY, f64::min);
                let tolerance = 1.25;
                for (candidate, difference, candidate_detail) in candidates {
                    if difference.visual_error() > best_error * tolerance {
                        continue;
                    }
                    let candidate = codec::attach_icc(candidate, icc.as_deref())?;
                    let candidate = codec::lossless_png(&candidate, false)?;
                    if candidate.len() < best.len() {
                        best = candidate;
                        score = difference;
                        lossy_accepted = true;
                        composited = true;
                        detail = candidate_detail;
                    }
                }
            }
        } else {
            let qualities: &[u8] = &[70, 80, 85, 90, 95];
            // 两种色度采样共用质量门槛 不用先判断是照片还是文字
            // 彩色边缘需要时可以选 4:4:4
            let sampling: &[bool] = if format == ImageFormat::Jpeg {
                &[true, false]
            } else {
                &[false]
            };
            let rgb = (format == ImageFormat::Jpeg)
                .then(|| DynamicImage::ImageRgba8(rgba.clone()).into_rgb8());
            for &subsample in sampling {
                for &q in qualities {
                    let candidate = if let Some(rgb) = &rgb {
                        codec::jpeg(rgb, q, subsample)?
                    } else {
                        codec::webp(&rgba, f32::from(q), false)?
                    };
                    let candidate = codec::attach_icc(candidate, icc.as_deref())?;
                    if candidate.len() >= best.len() {
                        break;
                    }
                    let difference = quality::compare(
                        &rgba,
                        &image::load_from_memory(&candidate)?.into_rgba8(),
                        transform.as_ref(),
                    )?;
                    if difference.accepts(limit) {
                        best = candidate;
                        score = difference;
                        lossy_accepted = true;
                        detail = format!(
                            "{} q={q}{}",
                            if format == ImageFormat::Jpeg {
                                "mozjpeg"
                            } else {
                                "libwebp"
                            },
                            if format == ImageFormat::Jpeg {
                                if subsample { " 4:2:0" } else { " 4:4:4" }
                            } else {
                                ""
                            }
                        );
                        break;
                    }
                }
            }
        }
        if !lossy_accepted {
            detail = "no acceptable smaller lossy candidate".into();
        }
    }
    // 原始灰度或真彩色的无损结果可能更小 也要和调色板候选一起比较
    if format == ImageFormat::Png {
        let mut lossless = if lossy_accepted {
            codec::lossless_png_fast(data, wide)?
        } else {
            codec::lossless_png(data, wide)?
        };
        if lossy_accepted && lossless.len() <= best.len() {
            let refined = codec::lossless_png(data, wide)?;
            if refined.len() < lossless.len() {
                lossless = refined;
            }
        }
        if lossless.len() < best.len() || (lossy_accepted && lossless.len() == best.len()) {
            // 只有准备返回的结果才需要解码校验
            codec::verify_pixels(&image::load_from_memory(data)?, &lossless)?;
            best = lossless;
            if lossy_accepted {
                detail = "PNG lossless; no larger than lossy candidate".into();
            } else if detail == "unchanged: no smaller result" {
                detail = "PNG lossless".into();
            } else {
                detail.push_str("; PNG lossless");
            }
            score = quality::Difference::default();
            lossy_accepted = false;
        }
    }
    let retained = best == data;
    if retained {
        score = quality::Difference::default();
    }
    Ok(Output {
        before: data.len(),
        after: best.len(),
        bytes: best,
        retained,
        dssim: score.mean,
        dssim_peak: score.peak,
        dssim_mode: if lossy_accepted && composited {
            "composited-visible"
        } else if lossy_accepted {
            quality::mode(width, height)
        } else {
            "exact"
        },
        detail,
        ms: started.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_mode_enforces_reported_difference_limit() {
        let original = image::RgbaImage::from_fn(96, 96, |x, y| {
            image::Rgba([(x * 2) as u8, (y * 2) as u8, ((x + y) % 256) as u8, 255])
        });
        let mut bytes = std::io::Cursor::new(Vec::new());
        original
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let result = compress(bytes.get_ref(), "gradient.png", false).unwrap();
        // JSON 用 DSSIM 字段 避免和旧版 SSIM 混淆
        let json = serde_json::to_value(&result).unwrap();
        assert!(json["dssim"].as_f64().is_some_and(|value| value <= 0.002));
        assert!(json.get("ssim").is_none());
    }

    #[test]
    fn large_images_are_not_unconditionally_bypassed() {
        let original = image::RgbImage::from_pixel(2001, 2000, image::Rgb([12, 45, 99]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        original
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let result = compress(bytes.get_ref(), "large.png", false).unwrap();
        assert_eq!(result.dssim, 0.0);
        assert!(image::load_from_memory(&result.bytes).unwrap().to_rgb8() == original);
    }

    #[test]
    fn tiny_images_do_not_fail_quality_comparison() {
        for (width, height) in [(1, 1), (1, 31), (31, 1), (3, 3)] {
            let im = image::RgbImage::from_fn(width, height, |x, y| {
                image::Rgb([(x * 13) as u8, (y * 7) as u8, 77])
            });
            let mut encoded = std::io::Cursor::new(Vec::new());
            im.write_to(&mut encoded, image::ImageFormat::Png).unwrap();
            let result = compress(encoded.get_ref(), "tiny.png", false).unwrap();
            assert!(result.bytes.len() <= encoded.get_ref().len());
        }
    }

    #[test]
    fn grayscale_lossless_candidate_competes_with_palette_output() {
        let original = image::GrayImage::from_fn(512, 512, |x, _| image::Luma([x as u8]));
        let mut data = std::io::Cursor::new(Vec::new());
        original
            .write_to(&mut data, image::ImageFormat::Png)
            .unwrap();
        let result = compress(data.get_ref(), "gray.png", false).unwrap();
        assert!(
            result.after <= 1500,
            "missed smaller grayscale lossless output: {} B",
            result.after
        );
        assert_eq!(
            image::load_from_memory(&result.bytes).unwrap().into_luma8(),
            original
        );
        assert_eq!(result.dssim_mode, "exact");
        assert_eq!(result.dssim, 0.0);
    }
}
