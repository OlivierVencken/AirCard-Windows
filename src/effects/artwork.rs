//! Bounded image preparation and checksum-verified parallax packs

use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageEncoder, RgbaImage, imageops::FilterType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::metadata::{MAX_JSON_BYTES, candidate, parse_unique};
use crate::image_skin::{CARD_HEIGHT, CARD_WIDTH};

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DIMENSION: u32 = 8192;
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const RESOURCE_NAMES: [(&str, &str); 3] = [
    ("background", "backgroundParallax"),
    ("foreground", "foregroundParallax"),
    ("fallback", "dynamicLayerStaticFallback"),
];
const METADATA_NAMES: [&str; 3] = [
    "original-pass.json",
    "candidate-pass.json",
    "metadata-diff.json",
];

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
pub struct PrepareConfig {
    pub schema_version: u32,
    /// Older packs used card-sized moving layers. Never reinterpret their bytes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallax_bleed: bool,
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
    pub original_pass_json: PathBuf,
    pub output_directory: PathBuf,
    #[serde(default)]
    pub canvas: Canvas,
    #[serde(default)]
    pub alignment: Alignment,
}

impl PrepareConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "Unsupported authoring schema version"
        );
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
        if self.parallax_bleed {
            let (width, height, _, _) = bleed_canvas(&self.canvas);
            check_dimensions(width, height)?;
        }
        for focus in [self.alignment.focus_x, self.alignment.focus_y] {
            ensure!(
                focus.is_finite() && (0.0..=1.0).contains(&focus),
                "Focus must be finite and between 0 and 1"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRecord {
    filename: String,
    byte_count: usize,
    sha256: String,
}

impl FileRecord {
    fn new(filename: &str, bytes: &[u8]) -> Self {
        Self {
            filename: filename.into(),
            byte_count: bytes.len(),
            sha256: sha256(bytes),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    role: String,
    recovered_basename: String,
    // Local candidate names; device scale and suffix lookup is unverified
    candidate_filenames: Vec<String>,
    width: u32,
    height: u32,
    file: FileRecord,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    configuration: PrepareConfig,
    resources: Vec<Resource>,
    metadata: Vec<FileRecord>,
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
    config: &PrepareConfig,
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
    // Use the same crop focus and card canvas for both layers
    let transform = |layer: &RgbaImage| -> Result<RgbaImage> {
        let (width, height) = layer.dimensions();
        check_dimensions(width, height)?;
        let target_ratio = config.canvas.width as f64 / config.canvas.height as f64;
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
        let x = (config.alignment.focus_x * (width - crop_width) as f64).round() as u32;
        let y = (config.alignment.focus_y * (height - crop_height) as f64).round() as u32;
        let cropped = image::imageops::crop_imm(layer, x, y, crop_width, crop_height).to_image();
        Ok(resize_rgba(
            &cropped,
            config.canvas.width,
            config.canvas.height,
        ))
    };
    let background = transform(&background)?;
    let foreground = transform(&foreground)?;
    if !allow_invalid_overlay && let Some(issue) = overlay_issue(&foreground, true) {
        anyhow::bail!(issue);
    }
    let fallback = composite(&background, &foreground);
    Ok((background, foreground, fallback))
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
    config: &PrepareConfig,
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

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
fn prepare(config_path: &Path) -> Result<PathBuf> {
    let config: PrepareConfig =
        serde_json::from_value(parse_unique(&read_bounded(config_path, MAX_JSON_BYTES)?)?)?;
    let base = config_path.parent().unwrap_or_else(|| Path::new("."));
    prepare_configuration(&config, base)
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

impl EffectsPreview {
    /// An artwork endpoint for inspection, not a simulation of Wallet's radial mask.
    pub fn alternate_composite(&self) -> Option<RgbaImage> {
        self.alternate_background.as_ref().map(|alternate| {
            let (width, height) = self.fallback.dimensions();
            let x = (alternate.width() - width) / 2;
            let y = (alternate.height() - height) / 2;
            let back = image::imageops::crop_imm(alternate, x, y, width, height).to_image();
            let front = image::imageops::crop_imm(&self.foreground, x, y, width, height).to_image();
            composite(&back, &front)
        })
    }
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

/// Preview with the same image processing as packs, without pass metadata
pub fn preview(config: &PrepareConfig, base: &Path) -> Result<EffectsPreview> {
    let preview = preview_internal(config, base, false)?;
    if let Some(issue) = preview.issue {
        anyhow::bail!(issue);
    }
    Ok(preview.artwork)
}

/// Show decodable artwork even when its alpha makes it unsuitable for Apply
/// Pack preparation continues to use the strict `preview` entry point.
pub fn preview_for_editor(config: &PrepareConfig, base: &Path) -> Result<EditorPreview> {
    preview_internal(config, base, true)
}

fn preview_internal(
    config: &PrepareConfig,
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
    let (background, foreground) = if config.parallax_bleed {
        padded_layers(&source_background, &background, &foreground, config)
    } else {
        (background, foreground)
    };
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
            Ok(if config.parallax_bleed {
                padded_layers(&source, &back, &front, config).0
            } else {
                back
            })
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
pub fn prepare_configuration(config: &PrepareConfig, base: &Path) -> Result<PathBuf> {
    prepare_configuration_from_preview(config, base, preview(config, base)?)
}

/// Use the exact frozen layers. Re-cropping prepared moving assets would crop
/// away their bleed and enlarge the composition again.
pub fn prepare_configuration_from_preview(
    config: &PrepareConfig,
    base: &Path,
    frozen: EffectsPreview,
) -> Result<PathBuf> {
    config.validate()?;
    let EffectsPreview {
        background,
        foreground,
        fallback,
        sprite,
        alternate_background,
        transaction_sprite,
        transaction_shape,
    } = frozen;
    let original = read_bounded(&base.join(&config.original_pass_json), MAX_JSON_BYTES)?;
    let (patched, diff) = candidate(&original, &config.dynamic_layer_configuration)?;
    ensure!(
        patched.len() <= MAX_JSON_BYTES && diff.len() <= MAX_JSON_BYTES,
        "Prepared metadata exceeds limit"
    );
    let mut dimensions = vec![
        background.dimensions(),
        foreground.dimensions(),
        fallback.dimensions(),
    ];
    let mut assets = vec![
        encode_png(&background)?,
        encode_png(&foreground)?,
        encode_png(&fallback)?,
    ];
    let mut names = RESOURCE_NAMES.to_vec();
    if let Some(sprite) = sprite {
        dimensions.push(sprite.dimensions());
        names.push(("emitter", "neutralEmitter"));
        assets.push(encode_png(&sprite)?);
    }
    if let Some(alternate) = alternate_background {
        dimensions.push(alternate.dimensions());
        names.push(("alternate_background", "backgroundParallaxCrossDissolve"));
        assets.push(encode_png(&alternate)?);
    }
    if let Some(sprite) = transaction_sprite {
        dimensions.push(sprite.dimensions());
        names.push(("transaction_emitter", "transactionEffectEmitter"));
        assets.push(encode_png(&sprite)?);
    }
    if let Some(bytes) = transaction_shape {
        let shape = super::transaction::validate(&bytes)?;
        dimensions.push((shape.width.ceil() as u32, shape.height.ceil() as u32));
        names.push(("transaction_shape", "transactionEffectShape"));
        assets.push(bytes);
    }
    let metadata_bytes = [original, patched, diff];
    let output = base.join(&config.output_directory);
    let resources = names
        .iter()
        .zip(&assets)
        .zip(dimensions)
        .map(|(((role, basename), bytes), (width, height))| Resource {
            role: (*role).into(),
            recovered_basename: (*basename).into(),
            candidate_filenames: if *role == "transaction_shape" {
                vec![format!("{basename}.svg")]
            } else {
                vec![
                    format!("{basename}.png"),
                    format!("{basename}@2x.png"),
                    format!("{basename}@3x.png"),
                ]
            },
            width,
            height,
            file: FileRecord::new(
                &format!(
                    "{basename}.{}",
                    if *role == "transaction_shape" {
                        "svg"
                    } else {
                        "png"
                    }
                ),
                bytes,
            ),
        })
        .collect();
    let metadata = METADATA_NAMES
        .iter()
        .zip(&metadata_bytes)
        .map(|(name, bytes)| FileRecord::new(name, bytes))
        .collect();
    let manifest = Manifest {
        schema_version: 1,
        configuration: config.clone(),
        resources,
        metadata,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    // No overwrite or reuse, even if an earlier preparation failed
    fs::create_dir(&output).with_context(|| format!("Create new pack {}", output.display()))?;
    for (resource, bytes) in manifest.resources.iter().zip(&assets) {
        write_new(&output.join(&resource.file.filename), bytes)?;
    }
    for (record, bytes) in manifest.metadata.iter().zip(&metadata_bytes) {
        write_new(&output.join(&record.filename), bytes)?;
    }
    write_new(&output.join("manifest.json"), &manifest_bytes)?;
    // Completion marker is written last; a partial pack will fail inspection
    write_new(
        &output.join("manifest.sha256"),
        sha256(&manifest_bytes).as_bytes(),
    )?;
    load_deployment(&output)?;
    Ok(output)
}

#[cfg(test)]
pub fn preview_pack(directory: &Path) -> Result<EffectsPreview> {
    let pack = load_deployment(directory)?;
    let find = |name: &str| -> Result<RgbaImage> {
        decode_png(
            &pack
                .resources
                .iter()
                .find(|resource| resource.filename == name)
                .with_context(|| format!("Missing {name}"))?
                .bytes,
        )
    };
    Ok(EffectsPreview {
        background: find("backgroundParallax.png")?,
        foreground: find("foregroundParallax.png")?,
        fallback: find("dynamicLayerStaticFallback.png")?,
        sprite: if pack.contains_emitter {
            Some(find("neutralEmitter.png")?)
        } else {
            None
        },
        alternate_background: if pack.contains_cross_dissolve {
            Some(find("backgroundParallaxCrossDissolve.png")?)
        } else {
            None
        },
        transaction_sprite: if pack.contains_transaction {
            Some(find("transactionEffectEmitter.png")?)
        } else {
            None
        },
        transaction_shape: pack
            .resources
            .iter()
            .find(|r| r.filename == "transactionEffectShape.svg")
            .map(|r| r.bytes.clone()),
    })
}

#[cfg(test)]
pub(super) fn trial_configuration(directory: &Path) -> Result<PrepareConfig> {
    let bytes = read_bounded(&directory.join("manifest.json"), MAX_JSON_BYTES)?;
    let deployment = load_deployment(directory)?;
    ensure!(
        sha256(&bytes) == deployment.manifest_sha256,
        "Trial manifest changed"
    );
    let manifest: Manifest = serde_json::from_value(parse_unique(&bytes)?)?;
    Ok(manifest.configuration)
}

/// Prepare artwork only, without reading or creating pass metadata
pub fn prepare_artwork(config: &PrepareConfig, base: &Path) -> Result<(PathBuf, EffectsPreview)> {
    let preview = preview(config, base)?;
    let mut images = vec![
        ("backgroundParallax.png", &preview.background),
        ("foregroundParallax.png", &preview.foreground),
        ("dynamicLayerStaticFallback.png", &preview.fallback),
    ];
    if let Some(sprite) = &preview.sprite {
        images.push(("neutralEmitter.png", sprite));
    }
    if let Some(alternate) = &preview.alternate_background {
        images.push(("backgroundParallaxCrossDissolve.png", alternate));
    }
    if let Some(sprite) = &preview.transaction_sprite {
        images.push(("transactionEffectEmitter.png", sprite));
    }
    let mut records = Vec::new();
    let mut encoded = Vec::new();
    for (name, image) in images {
        let bytes = encode_png(image)?;
        records.push(serde_json::json!({"filename":name, "width":image.width(),
            "height":image.height(), "byte_count":bytes.len(), "sha256":sha256(&bytes)}));
        encoded.push((name, bytes));
    }
    if let Some(bytes) = &preview.transaction_shape {
        let shape = super::transaction::validate(bytes)?;
        records.push(
            serde_json::json!({"filename":"transactionEffectShape.svg", "width":shape.width,
            "height":shape.height,"byte_count":bytes.len(),"sha256":sha256(bytes)}),
        );
        encoded.push(("transactionEffectShape.svg", bytes.clone()));
    }
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version":1, "kind":"artwork-only", "deployable":false,
        "dynamic_layer_configuration":config.dynamic_layer_configuration,
        "parallax_bleed":config.parallax_bleed,
        "alignment":config.alignment, "canvas":config.canvas, "resources":records,
    }))?;
    let output = base.join(&config.output_directory);
    fs::create_dir(&output).context("Artwork output directory must be new")?;
    for (name, bytes) in encoded {
        write_new(&output.join(name), &bytes)?;
        ensure!(
            read_bounded(&output.join(name), MAX_IMAGE_BYTES)? == bytes,
            "Artwork readback mismatch"
        );
    }
    write_new(&output.join("artwork-manifest.json"), &manifest)?;
    write_new(
        &output.join("artwork-manifest.sha256"),
        sha256(&manifest).as_bytes(),
    )?;
    Ok((output, preview))
}

fn verify_file(
    directory: &Path,
    record: &FileRecord,
    expected: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    ensure!(
        record.filename == expected,
        "Unexpected resource filename (traversal or unsupported resource)"
    );
    ensure!(
        record.byte_count <= limit,
        "Manifest byte count exceeds limit"
    );
    let bytes = read_bounded(&directory.join(expected), limit)?;
    ensure!(
        bytes.len() == record.byte_count && sha256(&bytes) == record.sha256,
        "Hash/size mismatch for {expected}"
    );
    Ok(bytes)
}

/// Validate and read each pack file once within size limits
/// Local unscaled filenames still require device lookup verification
pub struct PreparedResource {
    pub filename: String,
    pub bytes: Vec<u8>,
}

pub struct PreparedDeployment {
    pub manifest_sha256: String,
    pub original_metadata: Vec<u8>,
    pub candidate_metadata: Vec<u8>,
    pub resources: Vec<PreparedResource>,
    pub contains_emitter: bool,
    pub contains_cross_dissolve: bool,
    pub contains_transaction: bool,
}

pub fn load_deployment(directory: &Path) -> Result<PreparedDeployment> {
    let kind = fs::symlink_metadata(directory)?;
    ensure!(
        kind.is_dir() && !kind.file_type().is_symlink(),
        "Pack must be a directory, not a symlink"
    );
    let manifest_bytes = read_bounded(&directory.join("manifest.json"), MAX_JSON_BYTES)?;
    let checksum = read_bounded(&directory.join("manifest.sha256"), 64)?;
    ensure!(
        checksum == sha256(&manifest_bytes).as_bytes(),
        "Manifest hash mismatch"
    );
    let manifest: Manifest = serde_json::from_value(parse_unique(&manifest_bytes)?)?;
    ensure!(manifest.schema_version == 1, "Unsupported manifest version");
    manifest.configuration.validate()?;
    let contains_emitter = manifest
        .configuration
        .dynamic_layer_configuration
        .neutral_emitter
        .is_some();
    let mut names = RESOURCE_NAMES.to_vec();
    if contains_emitter {
        names.push(("emitter", "neutralEmitter"));
    }
    let contains_cross_dissolve = manifest
        .configuration
        .dynamic_layer_configuration
        .cross_dissolve
        .is_some();
    if contains_cross_dissolve {
        names.push(("alternate_background", "backgroundParallaxCrossDissolve"));
    }
    let contains_transaction = manifest
        .configuration
        .dynamic_layer_configuration
        .transaction_effect
        .is_some();
    if contains_transaction {
        names.push(("transaction_emitter", "transactionEffectEmitter"));
        names.push(("transaction_shape", "transactionEffectShape"));
    }
    ensure!(
        manifest.resources.len() == names.len() && manifest.metadata.len() == 3,
        "Incomplete resource/metadata inventory"
    );
    let mut allowed = vec!["manifest.json".to_string(), "manifest.sha256".to_string()];
    let mut layers = Vec::new();
    let mut resources = Vec::new();
    for (resource, (role, basename)) in manifest.resources.iter().zip(names) {
        ensure!(
            resource.role == role && resource.recovered_basename == basename,
            "Invalid resource role/basename"
        );
        let candidates = if role == "transaction_shape" {
            vec![format!("{basename}.svg")]
        } else {
            vec![
                format!("{basename}.png"),
                format!("{basename}@2x.png"),
                format!("{basename}@3x.png"),
            ]
        };
        ensure!(
            resource.candidate_filenames == candidates,
            "Unsupported filename candidates"
        );
        let filename = format!(
            "{basename}.{}",
            if role == "transaction_shape" {
                "svg"
            } else {
                "png"
            }
        );
        let bytes = verify_file(
            directory,
            &resource.file,
            &filename,
            if role == "transaction_shape" {
                super::transaction::MAX_SVG_BYTES
            } else {
                MAX_IMAGE_BYTES
            },
        )?;
        if role == "transaction_shape" {
            let shape = super::transaction::validate(&bytes)?;
            ensure!(
                (resource.width, resource.height)
                    == (shape.width.ceil() as u32, shape.height.ceil() as u32),
                "SVG canvas mismatch"
            );
            allowed.push(filename.clone());
            resources.push(PreparedResource { filename, bytes });
            continue;
        }
        let decoded = decode_png(&bytes)?;
        ensure!(
            decoded.dimensions() == (resource.width, resource.height),
            "Resource dimension mismatch"
        );
        if role == "emitter" || role == "transaction_emitter" {
            prepare_sprite(decoded)?;
        } else {
            let canvas = &manifest.configuration.canvas;
            let expected = if manifest.configuration.parallax_bleed && role != "fallback" {
                let (width, height, _, _) = bleed_canvas(canvas);
                (width, height)
            } else {
                (canvas.width, canvas.height)
            };
            ensure!(decoded.dimensions() == expected, "Resource canvas mismatch");
            if role == "alternate_background" {
                ensure!(
                    decoded.pixels().all(|p| p[3] == 255),
                    "Alternate background is not opaque"
                );
            }
            layers.push(decoded);
        }
        resources.push(PreparedResource {
            filename: filename.clone(),
            bytes,
        });
        allowed.push(filename);
    }
    ensure!(
        layers[0].pixels().all(|p| p[3] == 255),
        "Background is not opaque"
    );
    ensure!(
        layers[1].pixels().any(|p| p[3] < 255),
        "Foreground has no transparency"
    );
    let expected_fallback = if manifest.configuration.parallax_bleed {
        let canvas = &manifest.configuration.canvas;
        let (_, _, x, y) = bleed_canvas(canvas);
        let back =
            image::imageops::crop_imm(&layers[0], x, y, canvas.width, canvas.height).to_image();
        let front =
            image::imageops::crop_imm(&layers[1], x, y, canvas.width, canvas.height).to_image();
        ensure!(
            overlay_issue(&front, true).is_none(),
            "Invalid overlay inside bleed border"
        );
        composite(&back, &front)
    } else {
        composite(&layers[0], &layers[1])
    };
    ensure!(
        layers[2] == expected_fallback,
        "Fallback does not match aligned artwork"
    );
    let mut metadata = Vec::new();
    for (record, name) in manifest.metadata.iter().zip(METADATA_NAMES) {
        metadata.push(verify_file(directory, record, name, MAX_JSON_BYTES)?);
        allowed.push(name.into());
    }
    let (patched, diff) = candidate(
        &metadata[0],
        &manifest.configuration.dynamic_layer_configuration,
    )?;
    ensure!(
        metadata[1] == patched && metadata[2] == diff,
        "Metadata candidate/diff does not match original"
    );
    let mut entry_count = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Non-UTF-8 pack filename"))?;
        ensure!(allowed.contains(&name), "Unexpected pack entry: {name}");
        entry_count += 1;
    }
    ensure!(entry_count == allowed.len(), "Incomplete pack directory");
    Ok(PreparedDeployment {
        manifest_sha256: sha256(&manifest_bytes),
        original_metadata: metadata.remove(0),
        candidate_metadata: metadata.remove(0),
        resources,
        contains_emitter,
        contains_cross_dissolve,
        contains_transaction,
    })
}

#[cfg(test)]
fn inspect(directory: &Path) -> Result<String> {
    let deployment = load_deployment(directory)?;
    Ok(format!(
        "Verified offline effects pack: {} PNGs, exact original JSON, candidate and semantic diff. Device filename lookup and metadata reload remain unverified.{}",
        deployment.resources.len(),
        if deployment.contains_emitter {
            " Particle deployment requires recorded parallax acceptance."
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn particles_freeze_with_cross_dissolve_and_transaction_without_changing_fallback() {
        for cross in [false, true] {
            for transaction in [false, true] {
                let fixture = Fixture::new();
                fixture.prepare();
                let mut cfg = config();
                cfg.dynamic_layer_configuration.cross_dissolve = cross.then(Default::default);
                cfg.alternate_background = cross.then(|| "background.png".into());
                cfg.dynamic_layer_configuration.transaction_effect =
                    transaction.then(Default::default);
                if transaction {
                    fs::write(
                        fixture.0.join("shape.svg"),
                        br#"<svg width="100" height="100"><path d="M50 5L95 95L5 95Z"/></svg>"#,
                    )
                    .unwrap();
                    cfg.transaction_sprite = Some("foreground.png".into());
                    cfg.transaction_shape = Some("shape.svg".into());
                }
                let base = preview(&cfg, &fixture.0).unwrap();
                let mut original = serde_json::json!({"serialNumber":"retained", "dynamicLayerConfiguration":cfg.dynamic_layer_configuration});
                original["dynamicLayerConfiguration"]
                    .as_object_mut()
                    .unwrap()
                    .remove("neutralEmitter");
                fs::write(
                    fixture.0.join(&cfg.original_pass_json),
                    serde_json::to_vec(&original).unwrap(),
                )
                .unwrap();
                cfg.dynamic_layer_configuration.neutral_emitter = Some(EmitterConfiguration {
                    style: EmitterStyle::Twinkling,
                    density: EmitterDensity::Low,
                    physics_effect: EmitterPhysics::Off,
                });
                cfg.emitter_sprite = Some("foreground.png".into());
                cfg.output_directory = "frozen-combined".into();
                let (frozen_dir, frozen) = prepare_artwork(&cfg, &fixture.0).unwrap();
                assert_eq!(frozen.fallback, base.fallback);
                assert_eq!(frozen.alternate_background, base.alternate_background);
                assert!(frozen_dir.join("neutralEmitter.png").is_file());
                let sprite = frozen.sprite.clone();
                fs::write(fixture.0.join("foreground.png"), b"changed after freezing").unwrap();
                cfg.output_directory = "combined-pack".into();
                let pack = prepare_configuration_from_preview(&cfg, &fixture.0, frozen).unwrap();
                let deployment = load_deployment(&pack).unwrap();
                assert!(deployment.contains_emitter);
                assert_eq!(
                    deployment.resources.len(),
                    4 + usize::from(cross) + if transaction { 2 } else { 0 }
                );
                let loaded = preview_pack(&pack).unwrap();
                assert_eq!(loaded.sprite, sprite);
                assert_eq!(loaded.fallback, base.fallback);
                assert_eq!(loaded.transaction_shape, base.transaction_shape);
                assert_eq!(loaded.alternate_background, base.alternate_background);
            }
        }
    }

    #[test]
    fn transaction_freezes_svg_and_sprite_preserves_fallback_and_detects_tampering() {
        for cross in [false, true] {
            let fixture = Fixture::new();
            fixture.prepare();
            let mut config = config();
            let base = preview(&config, &fixture.0).unwrap().fallback;
            config.dynamic_layer_configuration.transaction_effect = Some(Default::default());
            assert!(preview(&config, &fixture.0).is_err());
            config.transaction_sprite = Some("foreground.png".into());
            config.transaction_shape = Some("shape.svg".into());
            let svg = br#"<svg width="100" height="100"><path d="M50 5L95 95L5 95Z"/></svg>"#;
            fs::write(fixture.0.join("shape.svg"), svg).unwrap();
            if cross {
                config.dynamic_layer_configuration.cross_dissolve = Some(Default::default());
                config.alternate_background = Some("background.png".into());
            }
            config.output_directory = "frozen".into();
            let (frozen_dir, frozen) = prepare_artwork(&config, &fixture.0).unwrap();
            assert_eq!(
                fs::read(frozen_dir.join("transactionEffectShape.svg")).unwrap(),
                svg
            );
            assert_eq!(frozen.fallback, base);
            let sprite = frozen.transaction_sprite.clone();
            fs::write(fixture.0.join("shape.svg"), b"changed after freeze").unwrap();
            config.output_directory = "transaction-pack".into();
            let pack = prepare_configuration_from_preview(&config, &fixture.0, frozen).unwrap();
            let deployment = load_deployment(&pack).unwrap();
            assert!(deployment.contains_transaction);
            assert_eq!(deployment.resources.len(), if cross { 6 } else { 5 });
            let loaded = preview_pack(&pack).unwrap();
            assert_eq!(loaded.transaction_sprite, sprite);
            assert_eq!(loaded.transaction_shape.as_deref(), Some(svg.as_slice()));
            assert_eq!(loaded.fallback, base);
            assert_eq!(
                parse_unique(&deployment.candidate_metadata).unwrap()["dynamicLayerConfiguration"]
                    ["transactionEffect"],
                serde_json::json!({"style":"zoomedShape"})
            );
            let shape_path = pack.join("transactionEffectShape.svg");
            fs::write(&shape_path, b"tampered").unwrap();
            assert!(load_deployment(&pack).is_err());
            // Rebinding hashes cannot make an invalid SVG acceptable.
            update_manifest(&pack, |m| {
                let record = m["resources"].as_array_mut().unwrap().last_mut().unwrap();
                record["file"]["byte_count"] = 8.into();
                record["file"]["sha256"] = sha256(b"tampered").into();
            });
            assert!(load_deployment(&pack).is_err());
            let mut value = serde_json::to_value(&config).unwrap();
            value["dynamic_layer_configuration"]["transactionEffect"]["style"] = "other".into();
            assert!(serde_json::from_value::<PrepareConfig>(value).is_err());
        }
    }

    #[test]
    fn cross_dissolve_freezes_aligned_alternate_and_preserves_base_fallback() {
        let fixture = Fixture::new();
        fixture.prepare();
        let mut config = config();
        config.canvas = Canvas {
            width: 20,
            height: 12,
        };
        config.parallax_bleed = true;
        config.alignment.focus_x = 0.8;
        let base = preview(&config, &fixture.0).unwrap();
        let alternate =
            RgbaImage::from_fn(48, 32, |x, y| Rgba([x as u8 * 4, y as u8 * 6, 180, 255]));
        alternate.save(fixture.0.join("alternate.png")).unwrap();
        config.dynamic_layer_configuration.cross_dissolve = Some(Default::default());
        config.alternate_background = Some("alternate.png".into());
        let mut reference = config.clone();
        reference.background = "alternate.png".into();
        reference.alternate_background = None;
        reference.dynamic_layer_configuration.cross_dissolve = None;
        let reference = preview(&reference, &fixture.0).unwrap();
        config.output_directory = "frozen-cross".into();
        let (_, frozen) = prepare_artwork(&config, &fixture.0).unwrap();
        assert_eq!(
            frozen.alternate_background.as_ref().unwrap(),
            &reference.background
        );
        assert_eq!(frozen.fallback, base.fallback);
        assert_eq!(frozen.alternate_composite().unwrap(), reference.fallback);
        // Source changes after freezing cannot change the installed pack.
        fs::write(fixture.0.join("alternate.png"), b"changed after freeze").unwrap();
        config.output_directory = "cross-pack".into();
        let pack = prepare_configuration_from_preview(&config, &fixture.0, frozen).unwrap();
        let deployment = load_deployment(&pack).unwrap();
        assert!(deployment.contains_cross_dissolve);
        assert_eq!(deployment.resources.len(), 4);
        let loaded = preview_pack(&pack).unwrap();
        assert_eq!(loaded.fallback, base.fallback);
        assert_eq!(loaded.alternate_background, Some(reference.background));
        let mut damaged = loaded.alternate_background.unwrap();
        damaged.put_pixel(0, 0, Rgba([1, 2, 3, 0]));
        let bytes = encode_png(&damaged).unwrap();
        fs::write(pack.join("backgroundParallaxCrossDissolve.png"), &bytes).unwrap();
        update_manifest(&pack, |manifest| {
            manifest["resources"][3]["file"]["byte_count"] = bytes.len().into();
            manifest["resources"][3]["file"]["sha256"] = sha256(&bytes).into();
        });
        assert!(
            load_deployment(&pack)
                .err()
                .unwrap()
                .to_string()
                .contains("Alternate background is not opaque")
        );
    }

    #[test]
    fn cross_dissolve_requires_matching_inputs_and_only_default_intensity() {
        let fixture = Fixture::new();
        fixture.prepare();
        let mut config = config();
        let legacy = serde_json::to_value(&config).unwrap();
        assert!(legacy.get("alternate_background").is_none());
        assert!(
            legacy["dynamic_layer_configuration"]
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
        assert!(serde_json::from_value::<PrepareConfig>(value).is_err());
        config.dynamic_layer_configuration.cross_dissolve = None;
        assert!(config.validate().is_err());
    }

    #[test]
    fn wallet_jpeg_and_webp_backgrounds_prepare_the_same_overlay_pixels() {
        let fixture = Fixture::new();
        fixture.prepare();
        for (name, format) in [
            ("background.jpg", image::ImageFormat::Jpeg),
            ("background.webp", image::ImageFormat::WebP),
        ] {
            let source = image::RgbImage::from_pixel(12, 8, image::Rgb([24, 70, 110]));
            source
                .save_with_format(fixture.0.join(name), format)
                .unwrap();
            let mut config = config();
            config.background = name.into();
            config.output_directory = format!("pack-{name}").into();
            let preview = preview(&config, &fixture.0).unwrap();
            let pack = prepare_configuration(&config, &fixture.0).unwrap();
            let output = preview_pack(&pack).unwrap();
            assert_eq!(output.background, preview.background);
            assert_eq!(output.foreground, preview.foreground);
            assert_eq!(output.fallback, preview.fallback);
        }
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

        fn prepare(&self) -> PathBuf {
            let config = config();
            fs::write(
                self.0.join("config.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .unwrap();
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
            fs::write(
                self.0.join("pass.json"),
                b" {\"serialNumber\":\"captured\",\"unknown\":42}\n",
            )
            .unwrap();
            prepare(&self.0.join("config.json")).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn config() -> PrepareConfig {
        PrepareConfig {
            schema_version: 1,
            parallax_bleed: false,
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
            original_pass_json: "pass.json".into(),
            output_directory: "pack".into(),
            canvas: Canvas {
                width: 2,
                height: 2,
            },
            alignment: Alignment::default(),
        }
    }

    fn update_manifest(pack: &Path, change: impl FnOnce(&mut serde_json::Value)) {
        let path = pack.join("manifest.json");
        let mut value = parse_unique(&fs::read(&path).unwrap()).unwrap();
        change(&mut value);
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(path, &bytes).unwrap();
        fs::write(pack.join("manifest.sha256"), sha256(&bytes)).unwrap();
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
    fn bleed_pack_roundtrips_without_changing_fallback_or_legacy_packs() {
        let fixture = Fixture::new();
        let legacy = fixture.prepare();
        let legacy_manifest = fs::read(legacy.join("manifest.json")).unwrap();
        assert!(
            !parse_unique(&legacy_manifest).unwrap()["configuration"]
                .as_object()
                .unwrap()
                .contains_key("parallax_bleed")
        );
        let mut config = config();
        config.canvas = Canvas {
            width: 40,
            height: 40,
        };
        let original = preview(&config, &fixture.0).unwrap();
        config.parallax_bleed = true;
        let padded = preview(&config, &fixture.0).unwrap();
        assert_eq!(padded.fallback, original.fallback);
        config.output_directory = "frozen-artwork".into();
        let (frozen_directory, frozen_layers) = prepare_artwork(&config, &fixture.0).unwrap();
        let mut apply_config = config.clone();
        apply_config.background = frozen_directory.join("backgroundParallax.png");
        apply_config.foreground = frozen_directory.join("foregroundParallax.png");
        apply_config.output_directory = "bleed-pack".into();
        let pack =
            prepare_configuration_from_preview(&apply_config, &fixture.0, frozen_layers).unwrap();
        for name in [
            "backgroundParallax.png",
            "foregroundParallax.png",
            "dynamicLayerStaticFallback.png",
        ] {
            assert_eq!(
                fs::read(pack.join(name)).unwrap(),
                fs::read(frozen_directory.join(name)).unwrap()
            );
        }
        let loaded = preview_pack(&pack).unwrap();
        assert_eq!(loaded.background, padded.background);
        assert_eq!(loaded.foreground, padded.foreground);
        assert_eq!(loaded.fallback, original.fallback);
        assert!(inspect(&legacy).is_ok());
        assert_eq!(
            fs::read(legacy.join("manifest.json")).unwrap(),
            legacy_manifest
        );
        // Hash-correct changes to the fallback still fail semantic validation.
        let mut fallback = loaded.fallback;
        fallback.get_pixel_mut(0, 0)[0] ^= 255;
        let bytes = encode_png(&fallback).unwrap();
        fs::write(pack.join("dynamicLayerStaticFallback.png"), &bytes).unwrap();
        update_manifest(&pack, |value| {
            value["resources"][2]["file"]["byte_count"] = bytes.len().into();
            value["resources"][2]["file"]["sha256"] = sha256(&bytes).into();
        });
        assert!(inspect(&pack).is_err());
        // Padding must not turn an opaque input into an acceptable overlay.
        RgbaImage::from_pixel(40, 40, Rgba([10, 20, 30, 255]))
            .save(fixture.0.join("foreground.png"))
            .unwrap();
        assert!(preview(&config, &fixture.0).is_err());
        assert!(
            preview_for_editor(&config, &fixture.0)
                .unwrap()
                .issue
                .unwrap()
                .contains("no transparency")
        );
        config.canvas = Canvas {
            width: 8192,
            height: 1,
        };
        assert!(config.validate().is_err());
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
        unsupported = config();
        unsupported.schema_version = 2;
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
        assert!(serde_json::from_value::<PrepareConfig>(value).is_err());
    }

    #[test]
    fn indexed_png_preview_shows_opaque_artwork_but_only_exports_real_transparency() {
        // A 4-bit indexed PNG like the supplied smoke image. Transparency in
        // indexed PNGs lives in tRNS, not in the palette's RGB entries.
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
            let mut export = config();
            export.output_directory = format!("indexed-{transparent}").into();
            assert_eq!(
                prepare_configuration(&export, &fixture.0).is_ok(),
                transparent
            );
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
    fn prepares_deterministically_and_never_overwrites_original_pack() {
        let fixture = Fixture::new();
        let pack = fixture.prepare();
        let original = fs::read(fixture.0.join("pass.json")).unwrap();
        assert_eq!(fs::read(pack.join("original-pass.json")).unwrap(), original);
        let manifest = fs::read(pack.join("manifest.json")).unwrap();
        assert!(prepare(&fixture.0.join("config.json")).is_err());
        assert_eq!(fs::read(pack.join("manifest.json")).unwrap(), manifest);
        let first = fixture.0.join("first");
        fs::rename(&pack, &first).unwrap();
        let pack = prepare(&fixture.0.join("config.json")).unwrap();
        assert_eq!(fs::read(pack.join("manifest.json")).unwrap(), manifest);
        for entry in fs::read_dir(&first).unwrap() {
            let entry = entry.unwrap();
            assert_eq!(
                fs::read(entry.path()).unwrap(),
                fs::read(pack.join(entry.file_name())).unwrap()
            );
        }
        assert!(inspect(&pack).is_ok());
    }

    #[test]
    fn inspection_rejects_tampering_missing_files_and_traversal() {
        let fixture = Fixture::new();
        let pack = fixture.prepare();
        let asset_path = pack.join("backgroundParallax.png");
        let asset = fs::read(&asset_path).unwrap();
        fs::write(&asset_path, b"tampered").unwrap();
        assert!(inspect(&pack).is_err());
        fs::write(&asset_path, &asset).unwrap();
        fs::remove_file(&asset_path).unwrap();
        assert!(inspect(&pack).is_err());
        fs::write(&asset_path, &asset).unwrap();
        fs::write(pack.join("extra.png"), b"unexpected").unwrap();
        assert!(inspect(&pack).is_err());
        fs::remove_file(pack.join("extra.png")).unwrap();
        update_manifest(&pack, |value| {
            value["resources"][0]["file"]["filename"] = "../pass.json".into()
        });
        assert!(inspect(&pack).is_err());
    }

    #[test]
    fn inspection_validates_semantics_even_when_hashes_are_recomputed() {
        let fixture = Fixture::new();
        let pack = fixture.prepare();
        let path = pack.join("candidate-pass.json");
        let bytes = br#"{"serialNumber":"changed","dynamicLayerConfiguration":{"version":2,"parallaxEnabled":true}}"#;
        fs::write(path, bytes).unwrap();
        update_manifest(&pack, |value| {
            value["metadata"][1]["byte_count"] = bytes.len().into();
            value["metadata"][1]["sha256"] = sha256(bytes).into();
        });
        assert!(inspect(&pack).is_err());
    }

    #[test]
    fn incomplete_and_oversized_packs_and_inputs_are_rejected() {
        let fixture = Fixture::new();
        let pack = fixture.prepare();
        let checksum = fs::read(pack.join("manifest.sha256")).unwrap();
        fs::write(pack.join("manifest.sha256"), b"wrong hash").unwrap();
        assert!(inspect(&pack).is_err());
        fs::write(pack.join("manifest.sha256"), checksum).unwrap();
        update_manifest(&pack, |value| {
            value["resources"][0]["file"]["byte_count"] = (MAX_IMAGE_BYTES + 1).into()
        });
        assert!(inspect(&pack).is_err());
        fs::remove_file(pack.join("manifest.sha256")).unwrap();
        assert!(inspect(&pack).is_err());
        let large = fixture.0.join("large");
        File::create(&large)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(read_bounded(&large, MAX_IMAGE_BYTES).is_err());
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
    fn transparent_sprite_preserves_own_canvas_and_is_inspected_deterministically() {
        let fixture = Fixture::new();
        fixture.prepare();
        let mut sprite = RgbaImage::new(3, 5);
        sprite.put_pixel(1, 2, Rgba([150, 20, 40, 128]));
        let bytes = encode_png(&sprite).unwrap();
        fs::write(fixture.0.join("sprite.png"), &bytes).unwrap();
        let mut cfg = config();
        cfg.output_directory = "particle-pack".into();
        cfg.emitter_sprite = Some("sprite.png".into());
        cfg.dynamic_layer_configuration.neutral_emitter = Some(EmitterConfiguration {
            style: EmitterStyle::Twinkling,
            density: EmitterDensity::Low,
            physics_effect: EmitterPhysics::Off,
        });
        let cfg_path = fixture.0.join("particles.json");
        fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        let pack = prepare(&cfg_path).unwrap();
        let deployment = load_deployment(&pack).unwrap();
        assert!(deployment.contains_emitter);
        assert_eq!(deployment.resources.len(), 4);
        assert_eq!(deployment.resources[3].filename, "neutralEmitter.png");
        assert_eq!(decode_png(&deployment.resources[3].bytes).unwrap(), sprite);
        assert_eq!(
            deployment.original_metadata,
            fs::read(fixture.0.join("pass.json")).unwrap()
        );
        assert!(parse_unique(&deployment.candidate_metadata).unwrap()["dynamicLayerConfiguration"]["neutralEmitter"].is_object());
        assert_eq!(
            deployment.manifest_sha256,
            sha256(&fs::read(pack.join("manifest.json")).unwrap())
        );
        let first = fixture.0.join("first-particle-pack");
        fs::rename(&pack, &first).unwrap();
        let pack = prepare(&cfg_path).unwrap();
        for entry in fs::read_dir(first).unwrap() {
            let entry = entry.unwrap();
            assert_eq!(
                fs::read(entry.path()).unwrap(),
                fs::read(pack.join(entry.file_name())).unwrap()
            );
        }
        update_manifest(&pack, |v| v["resources"][3]["width"] = 2.into());
        assert!(inspect(&pack).is_err());
        assert!(prepare_sprite(RgbaImage::new(3, 5)).is_err());
        assert!(prepare_sprite(RgbaImage::from_pixel(3, 5, Rgba([0, 0, 0, 255]))).is_err());
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

    #[test]
    fn preview_and_artwork_export_work_without_metadata_and_match_pack_pixels() {
        let fixture = Fixture::new();
        let pack = fixture.prepare();
        let expected = preview_pack(&pack).unwrap();
        fs::remove_file(fixture.0.join("pass.json")).unwrap();
        let mut config = config();
        config.output_directory = "artwork-only".into();
        let preview = preview(&config, &fixture.0).unwrap();
        assert_eq!(preview.background, expected.background);
        assert_eq!(preview.foreground, expected.foreground);
        assert_eq!(preview.fallback, expected.fallback);
        let (output, _) = prepare_artwork(&config, &fixture.0).unwrap();
        assert!(!output.join("pass.json").exists());
        assert!(!output.join("original-pass.json").exists());
        assert!(load_deployment(&output).is_err());
        let manifest_bytes = fs::read(output.join("artwork-manifest.json")).unwrap();
        assert_eq!(
            fs::read(output.join("artwork-manifest.sha256")).unwrap(),
            sha256(&manifest_bytes).as_bytes()
        );
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest["deployable"], false);
        for resource in manifest["resources"].as_array().unwrap() {
            let bytes = fs::read(output.join(resource["filename"].as_str().unwrap())).unwrap();
            assert_eq!(sha256(&bytes), resource["sha256"].as_str().unwrap());
        }
        assert!(prepare_artwork(&config, &fixture.0).is_err());
    }
}
