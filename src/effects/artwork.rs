use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageEncoder, RgbaImage, imageops::FilterType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::image_skin::{CARD_HEIGHT, CARD_WIDTH};

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DIMENSION: u32 = 8192;
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DynamicConfiguration {
    pub version: u32,
    pub parallax_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neutral_emitter: Option<EmitterConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_dissolve: Option<CrossDissolveConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_effect: Option<TransactionConfiguration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionConfiguration {
    pub style: TransactionStyle,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionStyle {
    #[default]
    ZoomedShape,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossDissolveConfiguration {
    pub intensity: CrossDissolveIntensity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CrossDissolveIntensity {
    Default,
}

impl Default for CrossDissolveConfiguration {
    fn default() -> Self {
        Self {
            intensity: CrossDissolveIntensity::Default,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EmitterStyle {
    Twinkling,
    Falling,
    Blooming,
    Rising,
    Shooting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EmitterDensity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EmitterPhysics {
    Off,
    Turbulence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EmitterConfiguration {
    pub style: EmitterStyle,
    pub density: EmitterDensity,
    pub physics_effect: EmitterPhysics,
}

impl DynamicConfiguration {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2,
            "Only dynamic configuration version 2 is supported"
        );
        ensure!(
            self.parallax_enabled,
            "This checkpoint supports two-layer parallax only"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            width: CARD_WIDTH,
            height: CARD_HEIGHT,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alignment {
    pub focus_x: f64,
    pub focus_y: f64,
}

impl Default for Alignment {
    fn default() -> Self {
        Self {
            focus_x: 0.5,
            focus_y: 0.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewConfig {
    pub dynamic_layer_configuration: DynamicConfiguration,
    pub background: PathBuf,
    pub foreground: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_sprite: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternate_background: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_sprite: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_shape: Option<PathBuf>,
    #[serde(default)]
    pub canvas: Canvas,
    #[serde(default)]
    pub alignment: Alignment,
}

impl PreviewConfig {
    fn validate(&self) -> Result<()> {
        self.dynamic_layer_configuration.validate()?;
        let transaction = self
            .dynamic_layer_configuration
            .transaction_effect
            .is_some();
        ensure!(
            transaction == self.transaction_sprite.is_some()
                && transaction == self.transaction_shape.is_some(),
            "Choose a transaction particle PNG and shape SVG"
        );
        ensure!(
            self.alternate_background.is_some()
                == self.dynamic_layer_configuration.cross_dissolve.is_some(),
            "Choose an alternate background image for cross-dissolve"
        );
        ensure!(
            self.emitter_sprite.is_some()
                == self.dynamic_layer_configuration.neutral_emitter.is_some(),
            "neutralEmitter configuration and emitter_sprite must be supplied together"
        );
        check_dimensions(self.canvas.width, self.canvas.height)?;
        let (width, height, _, _) = bleed_canvas(&self.canvas);
        check_dimensions(width, height)?;
        for focus in [self.alignment.focus_x, self.alignment.focus_y] {
            ensure!(
                focus.is_finite() && (0.0..=1.0).contains(&focus),
                "Focus must be finite and between 0 and 1"
            );
        }
        Ok(())
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn check_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "Image dimensions must be 1..=8192"
    );
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Image exceeds 16 megapixels"
    );
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("Inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Expected regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= limit as u64,
        "File exceeds limit: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "File grew beyond limit: {}",
        path.display()
    );
    Ok(bytes)
}

fn decode_png(bytes: &[u8]) -> Result<RgbaImage> {
    ensure!(bytes.len() <= MAX_IMAGE_BYTES, "PNG exceeds 32 MiB");
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    let mut decoder =
        image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits.clone())?;
    ensure!(
        !decoder.is_apng()?,
        "Animated PNG inputs are outside this implementation"
    );
    let (width, height) = decoder.dimensions();
    check_dimensions(width, height)?;
    decoder.set_limits(limits)?;
    Ok(DynamicImage::from_decoder(decoder)?.to_rgba8())
}

fn decode_background(bytes: &[u8]) -> Result<RgbaImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format().context("Unrecognized background image")?;
    if format == image::ImageFormat::Png {
        return decode_png(bytes);
    }
    ensure!(
        matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::WebP),
        "Choose a PNG, JPG, or WebP background image"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    check_dimensions(width, height)?;
    Ok(DynamicImage::from_decoder(decoder)?.to_rgba8())
}

struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("Encoded PNG exceeds 32 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut output = BoundedOutput(Vec::new());
    image::codecs::png::PngEncoder::new(&mut output).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(output.0)
}

// Filter premultiplied colors so invisible RGB cannot bleed into visible edges
fn resize_rgba(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    if image.dimensions() == (width, height) {
        return image.clone();
    }
    let premultiplied = image::ImageBuffer::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y);
        let alpha = f32::from(p[3]) / 255.0;
        image::Rgba([
            f32::from(p[0]) / 255.0 * alpha,
            f32::from(p[1]) / 255.0 * alpha,
            f32::from(p[2]) / 255.0 * alpha,
            alpha,
        ])
    });
    let resized = image::imageops::resize(&premultiplied, width, height, FilterType::Lanczos3);
    image::ImageBuffer::from_fn(width, height, |x, y| {
        let p = resized.get_pixel(x, y);
        let alpha = p[3].clamp(0.0, 1.0);
        let a = (alpha * 255.0).round() as u8;
        if a == 0 {
            return image::Rgba([0, 0, 0, 0]);
        }
        image::Rgba([
            (p[0] / alpha * 255.0).clamp(0.0, 255.0).round() as u8,
            (p[1] / alpha * 255.0).clamp(0.0, 255.0).round() as u8,
            (p[2] / alpha * 255.0).clamp(0.0, 255.0).round() as u8,
            a,
        ])
    })
}

fn prepare_sprite(sprite: RgbaImage) -> Result<RgbaImage> {
    check_dimensions(sprite.width(), sprite.height())?;
    ensure!(
        sprite.pixels().any(|p| p[3] < 255),
        "Emitter sprite must contain transparency"
    );
    ensure!(
        sprite.pixels().any(|p| p[3] > 0),
        "Emitter sprite must contain visible pixels"
    );
    // Preserve the authored canvas for native particle sizing
    Ok(sprite)
}

fn aligned_layers(
    background: RgbaImage,
    foreground: RgbaImage,
    config: &PreviewConfig,
    allow_invalid_overlay: bool,
) -> Result<(RgbaImage, RgbaImage, RgbaImage)> {
    config.validate()?;
    ensure!(
        background.pixels().all(|p| p[3] == 255),
        "Background must be opaque"
    );
    if !allow_invalid_overlay && let Some(issue) = overlay_issue(&foreground, false) {
        anyhow::bail!(issue);
    }
    let background = crop_layer(&background, &config.canvas, &config.alignment)?;
    let foreground = crop_layer(&foreground, &config.canvas, &config.alignment)?;
    if !allow_invalid_overlay && let Some(issue) = overlay_issue(&foreground, true) {
        anyhow::bail!(issue);
    }
    let fallback = composite(&background, &foreground);
    Ok((background, foreground, fallback))
}

fn crop_layer(layer: &RgbaImage, canvas: &Canvas, alignment: &Alignment) -> Result<RgbaImage> {
    let (width, height) = layer.dimensions();
    check_dimensions(width, height)?;
    let target_ratio = canvas.width as f64 / canvas.height as f64;
    let (crop_width, crop_height) = if width as f64 / height as f64 > target_ratio {
        (
            ((height as f64 * target_ratio).round() as u32).clamp(1, width),
            height,
        )
    } else {
        (
            width,
            ((width as f64 / target_ratio).round() as u32).clamp(1, height),
        )
    };
    let x = (alignment.focus_x * (width - crop_width) as f64).round() as u32;
    let y = (alignment.focus_y * (height - crop_height) as f64).round() as u32;
    let cropped = image::imageops::crop_imm(layer, x, y, crop_width, crop_height).to_image();
    Ok(resize_rgba(&cropped, canvas.width, canvas.height))
}

/// slide magnitude is (round(size * 1.1) - size) / 2,
fn bleed_canvas(canvas: &Canvas) -> (u32, u32, u32, u32) {
    let x = (f64::from(canvas.width) * 0.05).round() as u32;
    let y = (f64::from(canvas.height) * 0.05).round() as u32;
    (canvas.width + 2 * x, canvas.height + 2 * y, x, y)
}

fn reflected_index(index: i64, length: u32) -> u32 {
    let period = i64::from(length) * 2;
    let index = index.rem_euclid(period);
    if index < i64::from(length) {
        index as u32
    } else {
        (period - 1 - index) as u32
    }
}

fn padded_layers(
    source: &RgbaImage,
    background: &RgbaImage,
    foreground: &RgbaImage,
    config: &PreviewConfig,
) -> (RgbaImage, RgbaImage) {
    let (width, height, pad_x, pad_y) = bleed_canvas(&config.canvas);
    let (source_width, source_height) = source.dimensions();
    let ratio = config.canvas.width as f64 / config.canvas.height as f64;
    // Match aligned_layers' rounded crop exactly, including off-center focus.
    let (crop_width, crop_height) = if source_width as f64 / source_height as f64 > ratio {
        (
            ((source_height as f64 * ratio).round() as u32).clamp(1, source_width),
            source_height,
        )
    } else {
        (
            source_width,
            ((source_width as f64 / ratio).round() as u32).clamp(1, source_height),
        )
    };
    let origin_x = (config.alignment.focus_x * (source_width - crop_width) as f64).round();
    let origin_y = (config.alignment.focus_y * (source_height - crop_height) as f64).round();
    let back = RgbaImage::from_fn(width, height, |x, y| {
        if x >= pad_x
            && x < pad_x + background.width()
            && y >= pad_y
            && y < pad_y + background.height()
        {
            return *background.get_pixel(x - pad_x, y - pad_y);
        }

        let sx = origin_x
            + (x as f64 - pad_x as f64 + 0.5) * crop_width as f64 / background.width() as f64
            - 0.5;
        let sy = origin_y
            + (y as f64 - pad_y as f64 + 0.5) * crop_height as f64 / background.height() as f64
            - 0.5;
        let ix = sx.floor() as i64;
        let iy = sy.floor() as i64;
        let fx = sx - sx.floor();
        let fy = sy - sy.floor();
        let mut pixel = image::Rgba([0, 0, 0, 255]);
        for channel in 0..3 {
            let sample = |dx, dy| {
                f64::from(
                    source.get_pixel(
                        reflected_index(ix + dx, source_width),
                        reflected_index(iy + dy, source_height),
                    )[channel],
                )
            };
            pixel[channel] = ((sample(0, 0) * (1.0 - fx) + sample(1, 0) * fx) * (1.0 - fy)
                + (sample(0, 1) * (1.0 - fx) + sample(1, 1) * fx) * fy)
                .round() as u8;
        }
        pixel
    });
    let mut front = RgbaImage::new(width, height);
    image::imageops::replace(&mut front, foreground, i64::from(pad_x), i64::from(pad_y));
    (back, front)
}

fn composite(background: &RgbaImage, foreground: &RgbaImage) -> RgbaImage {
    let mut fallback = background.clone();
    for (out, front) in fallback.pixels_mut().zip(foreground.pixels()) {
        let alpha = u32::from(front[3]);
        for channel in 0..3 {
            out[channel] = ((u32::from(front[channel]) * alpha
                + u32::from(out[channel]) * (255 - alpha)
                + 127)
                / 255) as u8;
        }
        out[3] = 255;
    }
    fallback
}

pub struct EffectsPreview {
    pub background: RgbaImage,
    pub foreground: RgbaImage,
    pub fallback: RgbaImage,
    pub sprite: Option<RgbaImage>,
    pub alternate_background: Option<RgbaImage>,
    pub transaction_sprite: Option<RgbaImage>,
    pub transaction_shape: Option<Vec<u8>>,
}


fn overlay_issue(image: &RgbaImage, cropped: bool) -> Option<&'static str> {
    if image.pixels().all(|p| p[3] == 255) {
        Some(if cropped {
            "The crop removes all overlay transparency. Adjust the crop or choose another overlay."
        } else {
            "This overlay has no transparency. Choose a PNG with a transparent background."
        })
    } else if image.pixels().all(|p| p[3] == 0) {
        Some(if cropped {
            "The visible overlay is outside the crop. Adjust the crop or choose another overlay."
        } else {
            "This overlay is fully transparent and has no visible artwork. Choose another PNG."
        })
    } else {
        None
    }
}

pub struct EditorPreview {
    pub artwork: EffectsPreview,
    pub issue: Option<&'static str>,
}

pub fn preview(config: &PreviewConfig, base: &Path) -> Result<EffectsPreview> {
    let preview = preview_internal(config, base, false)?;
    if let Some(issue) = preview.issue {
        anyhow::bail!(issue);
    }
    Ok(preview.artwork)
}

pub fn preview_for_editor(config: &PreviewConfig, base: &Path) -> Result<EditorPreview> {
    preview_internal(config, base, true)
}

pub(super) fn preview_static_draft(
    desired: &super::configuration::DesiredEffects,
    alignment: &Alignment,
) -> Result<EditorPreview> {
    ensure!(
        !desired.selection.parallax,
        "Use the native parallax preview path"
    );
    desired.validate_selected_inputs()?;
    for focus in [alignment.focus_x, alignment.focus_y] {
        ensure!(
            focus.is_finite() && (0.0..=1.0).contains(&focus),
            "Invalid crop focus"
        );
    }
    let canvas = Canvas::default();
    let read_background = |path: &Path| -> Result<RgbaImage> {
        let image = decode_background(&read_bounded(path, MAX_IMAGE_BYTES)?)?;
        ensure!(
            image.pixels().all(|pixel| pixel[3] == 255),
            "Background must be opaque"
        );
        crop_layer(&image, &canvas, alignment)
    };
    let background = read_background(
        desired
            .inputs
            .background
            .as_deref()
            .context("Choose a background image")?,
    )?;
    let read_sprite =
        |path: &Path| prepare_sprite(decode_png(&read_bounded(path, MAX_IMAGE_BYTES)?)?);
    let sprite = if desired.selection.particles {
        Some(read_sprite(
            desired.inputs.particle_sprite.as_deref().unwrap(),
        )?)
    } else {
        None
    };
    let alternate_background = if desired.selection.cross_dissolve {
        Some(read_background(
            desired.inputs.alternate_background.as_deref().unwrap(),
        )?)
    } else {
        None
    };
    let transaction_sprite = if desired.selection.transaction {
        Some(read_sprite(
            desired.inputs.transaction_sprite.as_deref().unwrap(),
        )?)
    } else {
        None
    };
    let transaction_shape = if desired.selection.transaction {
        let bytes = read_bounded(
            desired.inputs.transaction_shape.as_deref().unwrap(),
            super::transaction::MAX_SVG_BYTES,
        )?;
        super::transaction::validate(&bytes)?;
        Some(bytes)
    } else {
        None
    };
    Ok(EditorPreview {
        artwork: EffectsPreview {
            fallback: background.clone(),
            background,
            foreground: RgbaImage::new(canvas.width, canvas.height),
            sprite,
            alternate_background,
            transaction_sprite,
            transaction_shape,
        },
        issue: None,
    })
}

/// Captured parallax backgrounds contain bleed, but never the foreground.
pub(super) fn static_from_captured_background(
    bytes: &[u8],
) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let image = decode_png(bytes)?;
    ensure!(
        image.pixels().all(|p| p[3] == 255),
        "Captured background must be opaque"
    );
    let canvas = Canvas::default();
    let (width, height, x, y) = bleed_canvas(&canvas);
    let image = if image.dimensions() == (canvas.width, canvas.height) {
        image
    } else {
        ensure!(
            image.dimensions() == (width, height),
            "Captured background dimensions are unsupported; choose a background image"
        );
        image::imageops::crop_imm(&image, x, y, canvas.width, canvas.height).to_image()
    };
    ordinary_artwork(&image)
}

fn ordinary_artwork(image: &RgbaImage) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let png = encode_png(image)?;
    let pdf = crate::image_skin::png_to_pdf(&png)?;
    Ok(crate::wallet_backup::CARD_ARTWORK_ASSETS
        .iter()
        .map(|leaf| {
            (
                (*leaf).to_owned(),
                if leaf.ends_with(".pdf") {
                    pdf.clone()
                } else {
                    png.clone()
                },
            )
        })
        .collect())
}

pub(super) fn managed_artwork(
    desired: &super::configuration::DesiredEffects,
    alignment: &Alignment,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let selected = desired.selection;
    desired.validate_selected_inputs()?;
    let preview = if selected.parallax {
        let config = PreviewConfig {
            dynamic_layer_configuration: selected.native_configuration().unwrap(),
            background: desired
                .inputs
                .background
                .clone()
                .context("Choose a background image")?,
            foreground: desired
                .inputs
                .foreground
                .clone()
                .context("Choose a transparent overlay PNG")?,
            emitter_sprite: selected
                .particles
                .then(|| desired.inputs.particle_sprite.clone().unwrap()),
            alternate_background: selected
                .cross_dissolve
                .then(|| desired.inputs.alternate_background.clone().unwrap()),
            transaction_sprite: selected
                .transaction
                .then(|| desired.inputs.transaction_sprite.clone().unwrap()),
            transaction_shape: selected
                .transaction
                .then(|| desired.inputs.transaction_shape.clone().unwrap()),

            canvas: Canvas::default(),
            alignment: alignment.clone(),
        };
        preview(&config, Path::new("."))?
    } else {
        preview_static_draft(desired, alignment)?.artwork
    };
    let mut resources = std::collections::BTreeMap::new();
    if selected.is_empty() {
        return ordinary_artwork(&preview.fallback);
    }
    use super::configuration::Resource as Role;
    for role in selected.requirements(false).resources {
        let bytes = match role {
            Role::Background => encode_png(&preview.background)?,
            Role::Foreground => encode_png(&preview.foreground)?,
            Role::Fallback => encode_png(&preview.fallback)?,
            Role::AlternateBackground => encode_png(
                preview
                    .alternate_background
                    .as_ref()
                    .context("Missing alternate background")?,
            )?,
            Role::NeutralEmitter => {
                encode_png(preview.sprite.as_ref().context("Missing neutral sprite")?)?
            }
            Role::TransactionEmitter => encode_png(
                preview
                    .transaction_sprite
                    .as_ref()
                    .context("Missing transaction sprite")?,
            )?,
            Role::TransactionShape => preview
                .transaction_shape
                .clone()
                .context("Missing transaction shape")?,
        };
        for leaf in role.candidate_filenames() {
            resources.insert(leaf, bytes.clone());
        }
    }
    Ok(resources)
}

fn preview_internal(
    config: &PreviewConfig,
    base: &Path,
    allow_invalid_overlay: bool,
) -> Result<EditorPreview> {
    config.validate()?;
    let source_background = decode_background(&read_bounded(
        &base.join(&config.background),
        MAX_IMAGE_BYTES,
    )?)?;
    let foreground = decode_png(&read_bounded(
        &base.join(&config.foreground),
        MAX_IMAGE_BYTES,
    )?)?;
    let source_issue = overlay_issue(&foreground, false);
    let (background, foreground, fallback) = aligned_layers(
        source_background.clone(),
        foreground,
        config,
        allow_invalid_overlay,
    )?;
    let issue = source_issue.or_else(|| overlay_issue(&foreground, true));
    let (background, foreground) =
        padded_layers(&source_background, &background, &foreground, config);
    let sprite = config
        .emitter_sprite
        .as_ref()
        .map(|path| {
            prepare_sprite(decode_png(&read_bounded(
                &base.join(path),
                MAX_IMAGE_BYTES,
            )?)?)
        })
        .transpose()?;
    let alternate_background = config
        .alternate_background
        .as_ref()
        .map(|path| -> Result<_> {
            let source = decode_background(&read_bounded(&base.join(path), MAX_IMAGE_BYTES)?)?;
            // Use precisely the same crop focus and bleed as the primary background.
            let (back, front, _) = aligned_layers(
                source.clone(),
                RgbaImage::new(config.canvas.width, config.canvas.height),
                config,
                true,
            )?;
            Ok(padded_layers(&source, &back, &front, config).0)
        })
        .transpose()?;
    Ok(EditorPreview {
        artwork: EffectsPreview {
            background,
            foreground,
            fallback,
            sprite,
            alternate_background,
            transaction_sprite: config
                .transaction_sprite
                .as_ref()
                .map(|path| {
                    prepare_sprite(decode_png(&read_bounded(
                        &base.join(path),
                        MAX_IMAGE_BYTES,
                    )?)?)
                })
                .transpose()?,
            transaction_shape: config
                .transaction_shape
                .as_ref()
                .map(|path| -> Result<_> {
                    let bytes = read_bounded(&base.join(path), super::transaction::MAX_SVG_BYTES)?;
                    super::transaction::validate(&bytes)?;
                    Ok(bytes)
                })
                .transpose()?,
        },
        issue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn removal_background_strips_exact_bleed_without_compositing_foreground() {
        let canvas = Canvas::default();
        let (width, height, x, y) = bleed_canvas(&canvas);
        let mut background = RgbaImage::from_pixel(width, height, Rgba([0, 0, 255, 255]));
        for py in y..y + canvas.height {
            for px in x..x + canvas.width {
                background.put_pixel(px, py, Rgba([240, 20, 10, 255]));
            }
        }
        for image in [
            background,
            RgbaImage::from_pixel(canvas.width, canvas.height, Rgba([240, 20, 10, 255])),
        ] {
            let assets = static_from_captured_background(&encode_png(&image).unwrap()).unwrap();
            let png = decode_png(&assets["cardBackgroundCombined@3x.png"]).unwrap();
            assert_eq!(png.dimensions(), (canvas.width, canvas.height));
            assert!(png.pixels().all(|p| p.0 == [240, 20, 10, 255]));
            assert_eq!(
                assets["cardBackgroundCombined@2x.png"],
                assets["cardBackgroundCombined@3x.png"]
            );
            assert!(assets["cardBackgroundCombined.pdf"].starts_with(b"%PDF"));
        }
        let wrong_size = RgbaImage::from_pixel(100, 100, Rgba([0, 0, 0, 255]));
        assert!(static_from_captured_background(&encode_png(&wrong_size).unwrap()).is_err());
    }

    #[test]
    fn managed_artwork_has_exact_subset_inventory_alpha_and_static_all_off() {
        use super::super::configuration::{DesiredEffects, Inputs};
        let fixture = Fixture::new();
        fixture.prepare();
        let shape = fixture.0.join("shape.svg");
        let svg = br#"<svg width="100" height="100"><path d="M50 5L95 95L5 95Z"/></svg>"#;
        fs::write(&shape, svg).unwrap();
        for bits in 0..16 {
            let selected = super::super::transition::from_bits(bits);
            let desired = DesiredEffects {
                selection: selected,
                static_foreground: false,
                inputs: Inputs {
                    background: Some(fixture.0.join("background.png")),
                    foreground: Some(if selected.parallax {
                        fixture.0.join("foreground.png")
                    } else {
                        fixture.0.join("missing.png")
                    }),
                    alternate_background: selected
                        .cross_dissolve
                        .then(|| fixture.0.join("background.png")),
                    particle_sprite: selected.particles.then(|| fixture.0.join("foreground.png")),
                    transaction_sprite: selected
                        .transaction
                        .then(|| fixture.0.join("foreground.png")),
                    transaction_shape: selected.transaction.then(|| shape.clone()),
                },
            };
            let resources = managed_artwork(&desired, &Alignment::default()).unwrap();
            let expected: std::collections::BTreeSet<_> = if bits == 0 {
                crate::wallet_backup::CARD_ARTWORK_ASSETS
                    .iter()
                    .map(|leaf| (*leaf).to_owned())
                    .collect()
            } else {
                super::super::transition::resource_names(selected)
            };
            assert_eq!(
                resources
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                expected
            );
            if bits == 0 {
                assert!(resources["cardBackgroundCombined.pdf"].starts_with(b"%PDF-"));
                assert!(
                    decode_png(&resources["cardBackgroundCombined@3x.png"])
                        .unwrap()
                        .pixels()
                        .all(|pixel| pixel[3] == 255)
                );
            } else {
                assert_eq!(
                    decode_png(&resources["dynamicLayerStaticFallback.png"])
                        .unwrap()
                        .dimensions(),
                    (CARD_WIDTH, CARD_HEIGHT)
                );
                if selected.parallax {
                    let foreground = decode_png(&resources["foregroundParallax.png"]).unwrap();
                    assert!(foreground.pixels().any(|pixel| pixel[3] == 0));
                    assert!(foreground.width() > CARD_WIDTH);
                }
                if selected.particles {
                    assert_eq!(
                        decode_png(&resources["neutralEmitter.png"])
                            .unwrap()
                            .dimensions(),
                        (4, 2)
                    );
                }
                if selected.transaction {
                    assert_eq!(resources["transactionEffectShape.svg"], svg);
                }
            }
        }
    }

    #[test]
    fn standalone_static_preview_reuses_crop_and_ignores_disabled_missing_assets() {
        use super::super::configuration::{DesiredEffects, Inputs, Selection};
        let fixture = Fixture::new();
        let background = fixture.0.join("static-background.png");
        image::RgbaImage::from_fn(48, 16, |x, _| Rgba([x as u8, 40, 80, 255]))
            .save(&background)
            .unwrap();
        let mut desired = DesiredEffects {
            selection: Selection::default(),
            inputs: Inputs {
                background: Some(background),
                foreground: Some("missing.png".into()),
                particle_sprite: Some("missing-sprite.png".into()),
                transaction_shape: Some("missing.svg".into()),
                ..Default::default()
            },
            static_foreground: false,
        };
        let left = preview_static_draft(
            &desired,
            &Alignment {
                focus_x: 0.0,
                focus_y: 0.5,
            },
        )
        .unwrap();
        let right = preview_static_draft(
            &desired,
            &Alignment {
                focus_x: 1.0,
                focus_y: 0.5,
            },
        )
        .unwrap();
        assert_eq!(
            left.artwork.fallback.dimensions(),
            (CARD_WIDTH, CARD_HEIGHT)
        );
        assert_eq!(left.artwork.background, left.artwork.fallback);
        assert!(left.artwork.foreground.pixels().all(|pixel| pixel[3] == 0));
        assert!(left.artwork.fallback.pixels().all(|pixel| pixel[3] == 255));
        assert!(
            left.artwork.fallback.get_pixel(0, 0)[0] < right.artwork.fallback.get_pixel(0, 0)[0]
        );
        assert!(left.artwork.sprite.is_none() && left.artwork.transaction_shape.is_none());
        desired.selection.particles = true;
        assert!(preview_static_draft(&desired, &Alignment::default()).is_err());
        desired.selection.particles = false;
        desired.selection.transaction = true;
        assert!(preview_static_draft(&desired, &Alignment::default()).is_err());
    }

    #[test]
    fn cross_dissolve_requires_matching_inputs_and_only_default_intensity() {
        let fixture = Fixture::new();
        fixture.prepare();
        let mut config = config();
        let serialized = serde_json::to_value(&config).unwrap();
        assert!(serialized.get("alternate_background").is_none());
        assert!(
            serialized["dynamic_layer_configuration"]
                .get("crossDissolve")
                .is_none()
        );
        config.dynamic_layer_configuration.cross_dissolve = Some(Default::default());
        assert!(preview(&config, &fixture.0).is_err());
        config.alternate_background = Some("missing.png".into());
        assert!(preview(&config, &fixture.0).is_err());
        config.alternate_background = Some("foreground.png".into());
        assert!(
            preview(&config, &fixture.0).is_err(),
            "Transparent foreground is not an opaque alternate background"
        );
        let mut value = serde_json::to_value(&config).unwrap();
        value["dynamic_layer_configuration"]["crossDissolve"]["intensity"] = "high".into();
        assert!(serde_json::from_value::<PreviewConfig>(value).is_err());
        config.dynamic_layer_configuration.cross_dissolve = None;
        assert!(config.validate().is_err());
    }

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let name = format!(
                "aircard-effects-test-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            );
            let directory = std::env::temp_dir().join(name);
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }

        fn prepare(&self) {
            let background = RgbaImage::from_pixel(4, 2, Rgba([10, 20, 30, 255]));
            let mut foreground = RgbaImage::from_pixel(4, 2, Rgba([200, 100, 0, 0]));
            foreground.put_pixel(2, 1, Rgba([200, 100, 0, 128]));
            fs::write(
                self.0.join("background.png"),
                encode_png(&background).unwrap(),
            )
            .unwrap();
            fs::write(
                self.0.join("foreground.png"),
                encode_png(&foreground).unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn config() -> PreviewConfig {
        PreviewConfig {
            dynamic_layer_configuration: DynamicConfiguration {
                version: 2,
                parallax_enabled: true,
                neutral_emitter: None,
                cross_dissolve: None,
                transaction_effect: None,
            },
            background: "background.png".into(),
            foreground: "foreground.png".into(),
            emitter_sprite: None,
            alternate_background: None,
            transaction_sprite: None,
            transaction_shape: None,

            canvas: Canvas {
                width: 2,
                height: 2,
            },
            alignment: Alignment::default(),
        }
    }

    #[test]
    fn jpeg_and_webp_backgrounds_keep_the_same_crop_alpha_and_composite() {
        let fixture = Fixture::new();
        fixture.prepare();
        for (name, format) in [
            ("background.jpg", image::ImageFormat::Jpeg),
            ("background.webp", image::ImageFormat::WebP),
        ] {
            image::RgbImage::from_pixel(12, 8, image::Rgb([24, 70, 110]))
                .save_with_format(fixture.0.join(name), format)
                .unwrap();
            let mut config = config();
            config.background = name.into();
            let inspected = preview_for_editor(&config, &fixture.0).unwrap();
            let prepared = preview(&config, &fixture.0).unwrap();
            assert!(inspected.issue.is_none());
            assert_eq!(prepared.background, inspected.artwork.background);
            assert_eq!(prepared.foreground, inspected.artwork.foreground);
            assert_eq!(
                prepared.fallback,
                composite(&prepared.background, &prepared.foreground)
            );
            assert!(prepared.foreground.pixels().any(|pixel| pixel[3] < 255));
        }
    }

    #[test]
    fn bleed_preserves_center_uses_source_then_reflection_and_keeps_overlay_clear() {
        let mut config = config();
        config.canvas = Canvas {
            width: 40,
            height: 40,
        };
        let source = RgbaImage::from_fn(44, 40, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let front = RgbaImage::from_pixel(40, 40, Rgba([200, 0, 0, 128]));
        for focus in [0.0, 0.5, 1.0] {
            config.alignment.focus_x = focus;
            let (back, front, fallback) =
                aligned_layers(source.clone(), front.clone(), &config, false).unwrap();
            let (padded_back, padded_front) = padded_layers(&source, &back, &front, &config);
            assert_eq!(padded_back.dimensions(), (44, 44));
            assert!(padded_back.pixels().all(|p| p[3] == 255));
            assert_eq!(
                image::imageops::crop_imm(&padded_back, 2, 2, 40, 40).to_image(),
                back
            );
            assert_eq!(
                image::imageops::crop_imm(&padded_front, 2, 2, 40, 40).to_image(),
                front
            );
            assert_eq!(composite(&back, &front), fallback);
            for (x, y, pixel) in padded_front.enumerate_pixels() {
                if x < 2 || y < 2 || x >= 42 || y >= 42 {
                    assert_eq!(pixel.0, [0, 0, 0, 0]);
                }
            }
            // Center focus uses actual source columns outside the crop. At the
            // source boundary, reflection starts with the edge then turns inward.
            let expected = if focus == 0.0 {
                [1, 0]
            } else if focus == 0.5 {
                [0, 1]
            } else {
                [2, 3]
            };
            assert_eq!(padded_back.get_pixel(0, 2)[0], expected[0]);
            assert_eq!(padded_back.get_pixel(1, 2)[0], expected[1]);
            assert_eq!(padded_back.get_pixel(2, 0)[1], 1);
        }
        for index in -20..20 {
            assert_eq!(reflected_index(index, 1), 0);
        }
        assert_eq!(bleed_canvas(&Canvas::default()), (1690, 1065, 77, 48));
    }

    #[test]
    fn preserves_alpha_and_uses_identical_crop_for_both_layers() {
        let mut background = RgbaImage::new(4, 2);
        let mut foreground = RgbaImage::new(4, 2);
        for x in 0..4 {
            for y in 0..2 {
                background.put_pixel(x, y, Rgba([x as u8 * 40, 0, 0, 255]));
                foreground.put_pixel(x, y, Rgba([0, x as u8 * 40, 0, 128]));
            }
        }
        for (focus, first_column) in [(0.0, 0), (0.5, 1), (1.0, 2)] {
            let mut config = config();
            config.alignment.focus_x = focus;
            let (back, front, fallback) =
                aligned_layers(background.clone(), foreground.clone(), &config, false).unwrap();
            assert_eq!(back.get_pixel(0, 0)[0], first_column * 40);
            assert_eq!(front.get_pixel(0, 0)[1], first_column * 40);
            assert_eq!(front.get_pixel(0, 0)[3], 128);
            assert!(fallback.pixels().all(|p| p[3] == 255));
            assert_eq!(decode_png(&encode_png(&front).unwrap()).unwrap(), front);
        }
        let back = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 255, 255]));
        let front = RgbaImage::from_pixel(1, 1, Rgba([255, 0, 0, 128]));
        assert_eq!(
            composite(&back, &front).get_pixel(0, 0).0,
            [128, 0, 127, 255]
        );
    }

    #[test]
    fn rejects_invalid_dimensions_alignment_versions_and_layers() {
        for (width, height) in [(0, 1), (1, 0), (8193, 1), (8192, 8192)] {
            assert!(check_dimensions(width, height).is_err());
        }
        for focus in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            let mut config = config();
            config.alignment.focus_x = focus;
            assert!(config.validate().is_err());
        }
        let mut unsupported = config();
        unsupported.dynamic_layer_configuration.version = 1;
        assert!(unsupported.validate().is_err());
        let opaque = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 0, 255]));
        let clear = RgbaImage::new(2, 2);
        assert!(aligned_layers(opaque.clone(), opaque.clone(), &config(), false).is_err());
        assert!(aligned_layers(clear.clone(), clear.clone(), &config(), false).is_err());
        assert!(aligned_layers(opaque, RgbaImage::new(3, 2), &config(), false).is_err());
        assert!(decode_png(b"not a PNG").is_err());
        let mut value = serde_json::to_value(config()).unwrap();
        value["dynamic_layer_configuration"]["neutralEmitter"] =
            serde_json::json!({"style":"unknown"});
        assert!(serde_json::from_value::<PreviewConfig>(value).is_err());
    }

    #[test]
    fn indexed_png_preview_shows_opaque_artwork_but_only_exports_real_transparency() {
        fn indexed_png(transparent: bool) -> Vec<u8> {
            fn chunk(output: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
                output.extend_from_slice(&(data.len() as u32).to_be_bytes());
                let start = output.len();
                output.extend_from_slice(name);
                output.extend_from_slice(data);
                let mut crc = u32::MAX;
                for byte in &output[start..] {
                    crc ^= u32::from(*byte);
                    for _ in 0..8 {
                        crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb88320 } else { 0 };
                    }
                }
                output.extend_from_slice(&(!crc).to_be_bytes());
            }
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            chunk(
                &mut bytes,
                b"IHDR",
                &[0, 0, 0, 2, 0, 0, 0, 2, 4, 3, 0, 0, 0],
            );
            chunk(&mut bytes, b"PLTE", &[120, 120, 120, 180, 180, 180]);
            if transparent {
                chunk(&mut bytes, b"tRNS", &[0, 128]);
            }
            chunk(
                &mut bytes,
                b"IDAT",
                &miniz_oxide::deflate::compress_to_vec_zlib(&[0, 0x01, 0, 0x10], 6),
            );
            chunk(&mut bytes, b"IEND", &[]);
            bytes
        }
        let fixture = Fixture::new();
        fixture.prepare();
        for transparent in [false, true] {
            fs::write(fixture.0.join("foreground.png"), indexed_png(transparent)).unwrap();
            let inspected = preview_for_editor(&config(), &fixture.0).unwrap();
            assert_eq!(inspected.issue.is_none(), transparent);
            assert_eq!(
                inspected.artwork.foreground.get_pixel(1, 0).0,
                [180, 180, 180, if transparent { 128 } else { 255 }]
            );
            assert_eq!(preview(&config(), &fixture.0).is_ok(), transparent);
        }
    }

    #[test]
    fn empty_and_cropped_out_overlays_explain_why_apply_is_blocked() {
        let fixture = Fixture::new();
        fixture.prepare();
        let mut front = RgbaImage::new(4, 2);
        fs::write(
            fixture.0.join("foreground.png"),
            encode_png(&front).unwrap(),
        )
        .unwrap();
        let inspected = preview_for_editor(&config(), &fixture.0).unwrap();
        assert!(inspected.issue.unwrap().contains("fully transparent"));
        assert!(preview(&config(), &fixture.0).is_err());
        front.put_pixel(3, 1, Rgba([255, 0, 0, 128]));
        fs::write(
            fixture.0.join("foreground.png"),
            encode_png(&front).unwrap(),
        )
        .unwrap();
        let mut left = config();
        left.alignment.focus_x = 0.0;
        let inspected = preview_for_editor(&left, &fixture.0).unwrap();
        assert!(inspected.issue.unwrap().contains("outside the crop"));
        assert!(preview(&left, &fixture.0).is_err());
        left.alignment.focus_x = 1.0;
        assert!(preview(&left, &fixture.0).is_ok());
    }

    #[test]
    fn png_output_is_bounded_and_animation_is_rejected() {
        let mut output = BoundedOutput(vec![0; MAX_IMAGE_BYTES]);
        assert!(output.write(b"x").is_err());
        assert_eq!(output.0.len(), MAX_IMAGE_BYTES);
        drop(output);

        // Add a valid APNG control chunk; the decoder must reject animated input
        let plain = encode_png(&RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]))).unwrap();
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&8u32.to_be_bytes());
        chunk.extend_from_slice(b"acTL");
        chunk.extend_from_slice(&1u32.to_be_bytes());
        chunk.extend_from_slice(&0u32.to_be_bytes());
        let mut crc = u32::MAX;
        for byte in &chunk[4..] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb88320 } else { 0 };
            }
        }
        chunk.extend_from_slice(&(!crc).to_be_bytes());
        let mut animated = plain[..33].to_vec();
        animated.extend_from_slice(&chunk);
        animated.extend_from_slice(&plain[33..]);
        let error = decode_png(&animated).unwrap_err();
        assert!(error.to_string().contains("Animated PNG"), "{error}");
    }

    #[test]
    fn emitter_enums_are_finite_explicit_and_bound_to_sprite() {
        for style in ["twinkling", "falling", "blooming", "rising", "shooting"] {
            for density in ["low", "medium", "high"] {
                for physics in ["off", "turbulence"] {
                    let value = serde_json::json!({"style":style,"density":density,"physicsEffect":physics});
                    let parsed: EmitterConfiguration =
                        serde_json::from_value(value.clone()).unwrap();
                    assert_eq!(serde_json::to_value(parsed).unwrap(), value);
                }
            }
        }
        let valid = serde_json::json!({"style":"twinkling","density":"low","physicsEffect":"off"});
        for key in ["style", "density", "physicsEffect"] {
            let mut value = valid.clone();
            value[key] = "unknown".into();
            assert!(serde_json::from_value::<EmitterConfiguration>(value).is_err());
            let mut value = valid.clone();
            value.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<EmitterConfiguration>(value).is_err());
        }
        let mut value = valid.clone();
        value["extra"] = true.into();
        assert!(serde_json::from_value::<EmitterConfiguration>(value).is_err());
        let mut cfg = config();
        cfg.dynamic_layer_configuration.neutral_emitter =
            Some(serde_json::from_value(valid).unwrap());
        assert!(cfg.validate().is_err());
        cfg.emitter_sprite = Some("sprite.png".into());
        assert!(cfg.validate().is_ok());
        cfg.dynamic_layer_configuration.neutral_emitter = None;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn interpolation_ignores_invisible_rgb_at_transparent_edges() {
        let mut red = RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 0]));
        red.put_pixel(1, 0, Rgba([0, 0, 255, 255]));
        let mut green = red.clone();
        green.put_pixel(0, 0, Rgba([0, 255, 0, 0]));
        let resized = resize_rgba(&red, 8, 4);
        assert_eq!(resized, resize_rgba(&green, 8, 4));
        for p in resized.pixels().filter(|p| p[3] > 0) {
            assert_eq!(p[0], 0);
            assert_eq!(p[1], 0);
            assert_eq!(p[2], 255);
        }
        let constant = RgbaImage::from_pixel(2, 2, Rgba([45, 127, 230, 128]));
        assert!(
            resize_rgba(&constant, 8, 4)
                .pixels()
                .all(|p| *p == Rgba([45, 127, 230, 128]))
        );
    }
}
