use anyhow::{Result, ensure};
use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage, imageops};
use img_parts::ImageICC;
use rgb::FromSlice;

use std::time::Duration;

pub fn lossless_png(data: &[u8], wide: bool) -> Result<Vec<u8>> {
    optimize_png(data, wide, 2)
}

pub fn lossless_png_fast(data: &[u8], wide: bool) -> Result<Vec<u8>> {
    optimize_png(data, wide, 0)
}

fn optimize_png(data: &[u8], wide: bool, preset: u8) -> Result<Vec<u8>> {
    let mut options = oxipng::Options::from_preset(preset);
    options.timeout = Some(Duration::from_secs(10));
    options.optimize_alpha = false;
    if wide {
        options.bit_depth_reduction = false;
    }
    Ok(oxipng::optimize_from_memory(data, &options)?)
}

pub fn verify_pixels(original: &DynamicImage, encoded: &[u8]) -> Result<()> {
    let decoded = image::load_from_memory(encoded)?;
    ensure!(
        original.dimensions() == decoded.dimensions(),
        "Lossless verification failed: dimensions differ"
    );
    if original.color() == decoded.color() {
        ensure!(
            original.as_bytes() == decoded.as_bytes(),
            "Lossless verification failed: pixels differ"
        );
    } else {
        // 分条带按 16 位精度比较 避免颜色类型转换掩盖低位精度损失
        // 也能省下两份完整 RGBA16 图像的内存
        for y in (0..original.height()).step_by(32) {
            let height = 32.min(original.height() - y);
            ensure!(
                original
                    .crop_imm(0, y, original.width(), height)
                    .to_rgba16()
                    == decoded.crop_imm(0, y, decoded.width(), height).to_rgba16(),
                "Lossless verification failed: pixels differ"
            );
        }
    }
    Ok(())
}

pub fn palette_png(image: &RgbaImage, target: u8, dithering: f32) -> Result<Vec<u8>> {
    let mut attr = imagequant::new();
    attr.set_speed(4)?;
    attr.set_quality(0, target)?;
    let large = u64::from(image.width()) * u64::from(image.height()) > 1_000_000;
    let mut result = if large {
        let scale = (1_000_000.0 / (f64::from(image.width()) * f64::from(image.height()))).sqrt();
        let sample = imageops::resize(
            image,
            ((f64::from(image.width()) * scale) as u32).max(1),
            ((f64::from(image.height()) * scale) as u32).max(1),
            imageops::FilterType::Nearest,
        );
        let mut sample_input = attr.new_image_borrowed(
            sample.as_raw().as_rgba(),
            sample.width() as usize,
            sample.height() as usize,
            0.0,
        )?;
        attr.quantize(&mut sample_input)?
    } else {
        let mut input = attr.new_image_borrowed(
            image.as_raw().as_rgba(),
            image.width() as usize,
            image.height() as usize,
            0.0,
        )?;
        attr.quantize(&mut input)?
    };
    result.set_dithering_level(dithering)?;
    // 训练只用采样图 重映射仍覆盖每个原始像素
    if large {
        attr.set_speed(8)?;
    }
    let mut input = attr.new_image_borrowed(
        image.as_raw().as_rgba(),
        image.width() as usize,
        image.height() as usize,
        0.0,
    )?;
    let (palette, indices) = result.remapped(&mut input)?;
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width(), image.height());
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(
            palette
                .iter()
                .flat_map(|p| [p.r, p.g, p.b])
                .collect::<Vec<_>>(),
        );
        encoder.set_trns(palette.iter().map(|p| p.a).collect::<Vec<_>>());
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.write_header()?.write_image_data(&indices)?;
    }
    Ok(bytes)
}

// 候选通过质量检查后再编码 避免白做 PNG 编解码
// 不同抖动强度共用同一份调色板
pub struct IndexedPng {
    palette: Vec<imagequant::RGBA>,
    indices: Vec<u8>,
    width: u32,
    height: u32,
    pub dither: f32,
}
impl IndexedPng {
    pub fn pixels(&self) -> RgbaImage {
        RgbaImage::from_fn(self.width, self.height, |x, y| {
            let p = self.palette[self.indices[(y * self.width + x) as usize] as usize];
            image::Rgba([p.r, p.g, p.b, p.a])
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, self.width, self.height);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(
                self.palette
                    .iter()
                    .flat_map(|p| [p.r, p.g, p.b])
                    .collect::<Vec<_>>(),
            );
            encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
            encoder.write_header()?.write_image_data(&self.indices)?;
        }
        Ok(bytes)
    }
}

pub struct OpaquePalette<'a> {
    input: imagequant::Image<'a>,
    trained: imagequant::QuantizationResult,
    width: u32,
    height: u32,
}
impl OpaquePalette<'_> {
    pub fn remap(&self, dither: f32) -> Result<IndexedPng> {
        let mut result = self.trained.clone();
        result.set_dithering_level(dither)?;
        let (palette, indices) = result.remapped(&mut self.input.clone())?;
        Ok(IndexedPng {
            palette,
            indices,
            width: self.width,
            height: self.height,
            dither,
        })
    }
}

// 仅处理不透明 PNG
// 限制训练内存 重映射仍跑全图
pub fn opaque_palette<'a>(
    image: &'a RgbaImage,
    colors: u32,
    importance: Option<&[u8]>,
    fixed: &[imagequant::RGBA],
) -> Result<OpaquePalette<'a>> {
    let mut attr = imagequant::new();
    attr.set_speed(1)?;
    attr.set_quality(0, 100)?;
    attr.set_max_colors(colors)?;
    let mut input = attr.new_image_borrowed(
        image.as_raw().as_rgba(),
        image.width() as usize,
        image.height() as usize,
        0.0,
    )?;
    for &color in fixed {
        input.add_fixed_color(color)?;
    }
    if let Some(map) = importance {
        input.set_importance_map(map.to_vec())?;
    }
    let count = u64::from(image.width()) * u64::from(image.height());
    let trained = if count > 4_000_000 {
        let scale = (4_000_000.0 / count as f64).sqrt();
        let width = ((f64::from(image.width()) * scale) as u32).max(1);
        let height = ((f64::from(image.height()) * scale) as u32).max(1);
        let sample = imageops::resize(image, width, height, imageops::FilterType::Nearest);
        let mut training = attr.new_image_borrowed(
            sample.as_raw().as_rgba(),
            width as usize,
            height as usize,
            0.0,
        )?;
        for &color in fixed {
            training.add_fixed_color(color)?;
        }
        if let Some(map) = importance {
            let map = image::GrayImage::from_raw(image.width(), image.height(), map.to_vec())
                .ok_or_else(|| anyhow::anyhow!("Palette importance map dimensions do not match"))?;
            training.set_importance_map(
                imageops::resize(&map, width, height, imageops::FilterType::Nearest).into_raw(),
            )?;
        }
        attr.quantize(&mut training)?
    } else {
        attr.quantize(&mut input)?
    };
    Ok(OpaquePalette {
        input,
        trained,
        width: image.width(),
        height: image.height(),
    })
}

pub fn jpeg(image: &RgbImage, quality: u8, subsample: bool) -> Result<Vec<u8>> {
    let mut encoder = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    encoder.set_size(image.width() as usize, image.height() as usize);
    encoder.set_quality(f32::from(quality));
    encoder.set_progressive_mode();
    let sampling = if subsample { (2, 2) } else { (1, 1) };
    encoder.set_chroma_sampling_pixel_sizes(sampling, sampling);
    let mut writer = encoder.start_compress(Vec::new())?;
    writer.write_scanlines(image.as_raw())?;
    Ok(writer.finish()?)
}

pub fn webp(image: &RgbaImage, quality: f32, lossless: bool) -> Result<Vec<u8>> {
    let mut config = webp::WebPConfig::new()
        .map_err(|_| anyhow::anyhow!("Failed to initialize WebP encoder"))?;
    config.quality = quality;
    config.method = 4;
    config.lossless = i32::from(lossless);
    config.exact = 1;
    let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
    let output = encoder
        .encode_advanced(&config)
        .map_err(|e| anyhow::anyhow!("WebP encoding failed: {e:?}"))?;
    Ok(output.to_vec())
}

// 交给容器库原样保留 ICC
// 不把 RGB 配置套到转换后的 CMYK 数据上 也不把 P3 当成无配置的 sRGB
pub fn attach_icc(bytes: Vec<u8>, icc: Option<&[u8]>) -> Result<Vec<u8>> {
    let Some(icc) = icc else {
        return Ok(bytes);
    };
    let mut container = img_parts::DynImage::from_bytes(bytes.into())?
        .ok_or_else(|| anyhow::anyhow!("Cannot embed ICC profile in this format"))?;
    container.set_icc_profile(Some(icc.to_vec().into()));
    let mut bytes = Vec::new();
    container.encoder().write_to(&mut bytes)?;
    Ok(bytes)
}
