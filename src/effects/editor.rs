//! Two-layer parallax editor with automatic preview and device backup

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};

use super::artwork::{self, Alignment, Canvas, EditorPreview, EffectsPreview, PrepareConfig};
use super::configuration::{DesiredEffects, Inputs, Selection};
use super::device;
use super::recovery::{self, Binding};
use crate::i18n::Language;

#[derive(Clone, Serialize, Deserialize)]
struct Recovery {
    binding: Binding,
    directory: PathBuf,
}

// Ignore obsolete developer settings when loading older drafts
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Draft {
    background: Option<PathBuf>,
    foreground: Option<PathBuf>,
    alignment: Alignment,
    recovery: Option<Recovery>,
    prior_recovery: Option<Recovery>,
    cross_dissolve: bool,
    alternate_background: Option<PathBuf>,
    transaction: bool,
    transaction_sprite: Option<PathBuf>,
    transaction_shape: Option<PathBuf>,
    neutral_particles_enabled: bool,
    particle_sprite: Option<PathBuf>,
}

impl Draft {
    // Legacy drafts are unbound input choices, never verified installed state.
    // Preserve the old mandatory-parallax meaning until the new workflow lands.
    fn desired_effects(&self) -> DesiredEffects {
        DesiredEffects {
            selection: Selection {
                parallax: true,
                cross_dissolve: self.cross_dissolve,
                particles: self.neutral_particles_enabled,
                transaction: self.transaction,
            },
            inputs: Inputs {
                background: self.background.clone(),
                foreground: self.foreground.clone(),
                alternate_background: self.alternate_background.clone(),
                particle_sprite: self.particle_sprite.clone(),
                transaction_sprite: self.transaction_sprite.clone(),
                transaction_shape: self.transaction_shape.clone(),
            },
            static_foreground: false,
        }
    }

    fn configuration(&self) -> Result<PrepareConfig> {
        let desired = self.desired_effects();
        desired.validate_selected_inputs()?;
        Ok(PrepareConfig {
            schema_version: 1,
            parallax_bleed: true,
            dynamic_layer_configuration: desired
                .selection
                .native_configuration()
                .context("Legacy overlay requires parallax")?,
            background: self
                .background
                .clone()
                .context("Choose a background image")?,
            foreground: self
                .foreground
                .clone()
                .context("Choose a transparent overlay PNG")?,
            emitter_sprite: if self.neutral_particles_enabled {
                Some(
                    self.particle_sprite
                        .clone()
                        .context("Choose a transparent particle PNG")?,
                )
            } else {
                None
            },
            transaction_sprite: if self.transaction {
                Some(
                    self.transaction_sprite
                        .clone()
                        .context("Choose a transaction particle PNG")?,
                )
            } else {
                None
            },
            transaction_shape: if self.transaction {
                Some(
                    self.transaction_shape
                        .clone()
                        .context("Choose a transaction shape SVG")?,
                )
            } else {
                None
            },
            alternate_background: if self.cross_dissolve {
                Some(
                    self.alternate_background
                        .clone()
                        .context("Choose an alternate background image for cross-dissolve")?,
                )
            } else {
                None
            },
            original_pass_json: PathBuf::new(),
            output_directory: PathBuf::new(),
            canvas: Canvas::default(),
            alignment: self.alignment.clone(),
        })
    }
}

enum Completed {
    Preview(Box<EditorPreview>),
    Applied(bool),
    Restored(bool),
}
enum Message {
    Progress(String),
    Done(Result<Completed, String>),
}

pub(crate) struct OverlayEditor {
    draft: Draft,
    preview_texture: Option<egui::TextureHandle>,
    alternate_texture: Option<egui::TextureHandle>,
    transaction_texture: Option<egui::TextureHandle>,
    particle_texture: Option<egui::TextureHandle>,
    transaction_outline: Option<super::transaction::Shape>,
    show_alternate: bool,
    dirty: bool,
    status: Option<String>,
    error: Option<String>,
    preview_issue: Option<&'static str>,
    receiver: Option<Receiver<Message>>,
}

impl Default for OverlayEditor {
    fn default() -> Self {
        let mut draft: Draft = recovery::regular_read(&draft_path(), 16 * 1024)
            .ok()
            .and_then(|bytes| super::metadata::parse_unique(&bytes).ok())
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        if draft.prior_recovery.is_some()
            && draft
                .recovery
                .as_ref()
                .is_some_and(|r| device::no_installation_to_recover(&r.directory))
        {
            draft.recovery = draft.prior_recovery.take();
        }
        Self {
            draft,
            preview_texture: None,
            alternate_texture: None,
            transaction_texture: None,
            particle_texture: None,
            transaction_outline: None,
            show_alternate: false,
            dirty: true,
            status: None,
            error: None,
            preview_issue: None,
            receiver: None,
        }
    }
}

fn draft_path() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| ".".into()))
        .join("AirCard")
        .join("effects-editor.json")
}

#[cfg(test)]
pub(super) fn remember_trial_recovery(binding: Binding, directory: PathBuf) -> Result<()> {
    let mut editor = OverlayEditor::default();
    editor.draft.recovery = Some(Recovery { binding, directory });
    editor.remember()
}

impl OverlayEditor {
    pub fn busy(&self) -> bool {
        self.receiver.is_some()
    }

    #[cfg(test)]
    pub(crate) fn preview_ready(&self) -> bool {
        self.preview_texture.is_some()
    }

    #[cfg(test)]
    pub(crate) fn show_alternate_for_qa(&mut self) {
        assert!(self.alternate_texture.is_some());
        self.show_alternate = true;
    }

    fn remember(&self) -> Result<()> {
        let path = draft_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let temporary = path.with_extension("tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&self.draft)?)?;
        file.sync_all()?;
        recovery::durable_replace(&temporary, &path)
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce(&dyn Fn(&str)) -> Result<Completed> + Send + 'static,
    ) {
        if self.busy() {
            return;
        }
        self.error = None;
        self.status = None;
        let (sender, receiver) = channel();
        self.receiver = Some(receiver);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let progress = |message: &str| {
                let _ = sender.send(Message::Progress(message.to_owned()));
                ctx.request_repaint();
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&progress)))
                .map_err(|_| "Overlay worker stopped unexpectedly. Use Restore previous skin if a backup is available.".to_owned())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            if let Err(error) = &result {
                // Keep error details after the window closes
                let path = draft_path().with_file_name("effects-last-error.txt");
                let _ = std::fs::write(path, error);
            }
            let _ = sender.send(Message::Done(result));
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        loop {
            let result = self.receiver.as_ref().map(|receiver| receiver.try_recv());
            match result {
                Some(Ok(Message::Progress(message))) => self.status = Some(message),
                Some(Ok(Message::Done(result))) => {
                    self.receiver = None;
                    match result {
                        Ok(Completed::Preview(preview)) => {
                            let preview = *preview;
                            self.preview_issue = preview.issue;
                            self.install(ctx, preview.artwork);
                            self.status = None;
                        }
                        Ok(Completed::Applied(limited)) => {
                            self.draft.prior_recovery = None;
                            if let Err(error) = self.remember() {
                                self.error =
                                    Some(format!("Could not save overlay settings: {error:#}"));
                            }
                            self.status = Some(if limited {
                                "Experimental skin applied and bytes verified. Effect-file recovery is incomplete. Reopen Wallet and Apple Pay and check the effects."
                            } else { "Skin applied. Force-close and reopen Wallet and Apple Pay to see the change." }.into());
                        }
                        Ok(Completed::Restored(limited)) => {
                            self.status = Some(if limited {
                                "Known originals restored and verified. Recovery is incomplete: effect files may remain. Reopen Wallet and Apple Pay and check appearance."
                            } else { "Previous skin restored. Force-close and reopen Wallet and Apple Pay." }.into());
                            if !limited {
                                self.draft.recovery = None;
                            }
                            if let Err(error) = self.remember() {
                                self.error =
                                    Some(format!("Could not save editor settings: {error:#}"));
                            }
                        }
                        Err(error) => {
                            if self.draft.prior_recovery.is_some()
                                && self.draft.recovery.as_ref().is_some_and(|r| {
                                    device::no_installation_to_recover(&r.directory)
                                })
                            {
                                self.draft.recovery = self.draft.prior_recovery.take();
                                if let Err(save_error) = self.remember() {
                                    self.error = Some(format!(
                                        "{error}; Could not save overlay settings: {save_error:#}"
                                    ));
                                    break;
                                }
                            }
                            self.status = None;
                            self.error = Some(error);
                        }
                    }
                    break;
                }
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                    self.receiver = None;
                    self.error = Some("Overlay worker stopped unexpectedly".into());
                    break;
                }
                _ => break,
            }
        }
    }

    fn install(&mut self, ctx: &egui::Context, preview: EffectsPreview) {
        self.particle_texture = preview.sprite.as_ref().map(|image| {
            ctx.load_texture(
                "wallet-neutral-particle",
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            )
        });
        self.transaction_texture = preview.transaction_sprite.as_ref().map(|image| {
            ctx.load_texture(
                "wallet-transaction-sprite",
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            )
        });
        self.transaction_outline = preview
            .transaction_shape
            .as_ref()
            .and_then(|bytes| super::transaction::validate(bytes).ok());
        self.alternate_texture = preview.alternate_composite().map(|image| {
            ctx.load_texture(
                "wallet-cross-dissolve-alternate",
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            )
        });
        // Use the exact flattened fallback produced by pack preparation.
        let image = preview.fallback;
        let dimensions = [image.width() as usize, image.height() as usize];
        self.preview_texture = Some(ctx.load_texture(
            "wallet-overlay-composite",
            egui::ColorImage::from_rgba_unmultiplied(dimensions, image.as_raw()),
            egui::TextureOptions::LINEAR,
        ));
    }

    pub fn preview_texture(&self) -> Option<&egui::TextureHandle> {
        if self.show_alternate {
            self.alternate_texture
                .as_ref()
                .or(self.preview_texture.as_ref())
        } else {
            self.preview_texture.as_ref()
        }
    }
    /// Use the Wallet image picker for the background and crop
    pub fn sync_background(&mut self, path: Option<PathBuf>, focus: [f32; 2]) {
        if self.draft.background != path
            || self.draft.alignment.focus_x != f64::from(focus[0])
            || self.draft.alignment.focus_y != f64::from(focus[1])
        {
            self.draft.background = path;
            self.draft.alignment = Alignment {
                focus_x: focus[0].into(),
                focus_y: focus[1].into(),
            };
            self.invalidate_preview();
        }
    }

    fn invalidate_preview(&mut self) {
        self.dirty = true;
        self.preview_texture = None;
        self.alternate_texture = None;
        self.transaction_texture = None;
        self.particle_texture = None;
        self.transaction_outline = None;
        self.error = None;
        self.status = None;
        self.preview_issue = None;
    }

    pub fn overlay_path(&self) -> Option<&Path> {
        self.draft.foreground.as_deref()
    }

    pub fn set_overlay_path(&mut self, path: PathBuf) {
        self.draft.foreground = Some(path);
        self.invalidate_preview();
    }

    pub fn choose_overlay(&mut self) {
        if self.busy() {
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Transparent PNG", &["png"])
            .pick_file()
        {
            self.set_overlay_path(path);
            if let Err(error) = self.remember() {
                self.error = Some(format!("Could not save overlay settings: {error:#}"));
            }
        }
    }

    pub(crate) fn set_cross_dissolve(&mut self, enabled: bool) {
        self.draft.cross_dissolve = enabled;
        self.show_alternate = false;
        self.invalidate_preview();
    }

    pub(crate) fn set_alternate_background(&mut self, path: PathBuf) {
        self.draft.alternate_background = Some(path);
        self.invalidate_preview();
    }

    pub fn show_cross_dissolve_controls(&mut self, ui: &mut egui::Ui, language: Language) {
        let mut enabled = self.draft.cross_dissolve;
        if ui
            .checkbox(&mut enabled, language.text("Cross-dissolve"))
            .changed()
        {
            self.set_cross_dissolve(enabled);
            if let Err(error) = self.remember() {
                self.error = Some(format!("Could not save overlay settings: {error:#}"));
            }
        }
        if !enabled {
            return;
        }
        if device::experimental_cross_dissolve_enabled() {
            ui.colored_label(ui.visuals().warn_fg_color, language.text("Experimental trial: missing alternate originals are allowed. Restore may leave alternate effect files behind."));
        }
        if ui
            .button(language.text("Choose Alternate Background..."))
            .clicked()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("Image", &["png", "jpg", "jpeg", "webp"])
                .pick_file()
        {
            self.set_alternate_background(path);
            if let Err(error) = self.remember() {
                self.error = Some(format!("Could not save overlay settings: {error:#}"));
            }
        }
        if let Some(path) = &self.draft.alternate_background {
            ui.add(
                egui::Label::new(
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("image"),
                )
                .truncate(),
            )
            .on_hover_text(path.display().to_string());
        } else {
            ui.small(language.text("Choose an alternate background image for cross-dissolve"));
        }
        ui.small(
            language.text("The alternate background shows at rest; tilt reveals the base artwork."),
        );
        ui.add_enabled_ui(self.alternate_texture.is_some(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(
                    &mut self.show_alternate,
                    false,
                    language.text("Base artwork"),
                );
                ui.selectable_value(
                    &mut self.show_alternate,
                    true,
                    language.text("Alternate artwork"),
                );
            });
        });
        ui.small(
            language.text("Preview shows artwork only; the static fallback uses the base artwork."),
        );
    }

    pub(crate) fn set_particles(&mut self, enabled: bool) {
        self.draft.neutral_particles_enabled = enabled;
        self.invalidate_preview();
    }

    pub(crate) fn set_particle_sprite(&mut self, path: PathBuf) {
        self.draft.particle_sprite = Some(path);
        self.invalidate_preview();
    }

    pub fn show_particle_controls(&mut self, ui: &mut egui::Ui, language: Language) {
        let mut enabled = self.draft.neutral_particles_enabled;
        let mut changed = ui
            .checkbox(&mut enabled, language.text("Particles (experimental)"))
            .changed();
        if changed {
            self.set_particles(enabled);
        }
        if enabled {
            if device::experimental_particles_enabled() {
                ui.colored_label(ui.visuals().warn_fg_color, language.text("Experimental trial: particle originals may be unknown. Restore may leave the three neutral emitter PNGs behind."));
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button(language.text("Choose Particle PNG...")).clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Transparent PNG", &["png"])
                        .pick_file()
                {
                    self.set_particle_sprite(path);
                    changed = true;
                }
                if let Some(path) = &self.draft.particle_sprite {
                    ui.add(
                        egui::Label::new(path.file_name().unwrap_or_default().to_string_lossy())
                            .truncate(),
                    )
                    .on_hover_text(path.display().to_string());
                }
                if let Some(texture) = &self.particle_texture {
                    ui.add(egui::Image::new(texture).fit_to_exact_size(
                        egui::vec2(28.0, 28.0)
                            * (texture.size_vec2() / texture.size_vec2().max_elem()),
                    ));
                }
            });
            ui.small(
                language.text("Native twinkling at low density, physics off. Preview is static."),
            );
        }
        if changed && let Err(error) = self.remember() {
            self.error = Some(format!("Could not save overlay settings: {error:#}"));
        }
    }

    pub(crate) fn set_transaction(&mut self, enabled: bool) {
        self.draft.transaction = enabled;
        self.invalidate_preview();
    }

    pub(crate) fn set_transaction_sprite(&mut self, path: PathBuf) {
        self.draft.transaction_sprite = Some(path);
        self.invalidate_preview();
    }

    pub(crate) fn set_transaction_shape(&mut self, path: PathBuf) {
        self.draft.transaction_shape = Some(path);
        self.invalidate_preview();
    }

    pub fn show_transaction_controls(&mut self, ui: &mut egui::Ui, language: Language) {
        let mut enabled = self.draft.transaction;
        let mut changed = ui
            .checkbox(
                &mut enabled,
                language.text("Transaction effect (experimental)"),
            )
            .changed();
        if changed {
            self.set_transaction(enabled);
        }
        if enabled {
            if device::experimental_transaction_enabled() {
                ui.colored_label(ui.visuals().warn_fg_color, language.text("Experimental trial: transaction originals may be unknown. Restore may leave the three emitter PNGs and shape SVG behind."));
            }
            ui.small(language.text("An expanding particle outline during payment results. Phone playback is not yet validated."));
            ui.horizontal(|ui| {
                let has_preview = self.transaction_texture.is_some() && self.transaction_outline.is_some();
                let picker_width = (ui.available_width() - if has_preview { 108.0 } else { 0.0 }).max(160.0);
                ui.allocate_ui_with_layout(egui::vec2(picker_width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button(language.text("Particle PNG...")).clicked()
                            && let Some(path) = rfd::FileDialog::new().add_filter("Transparent PNG", &["png"]).pick_file() {
                            self.set_transaction_sprite(path); changed = true;
                        }
                        if let Some(path) = &self.draft.transaction_sprite {
                            ui.add(egui::Label::new(path.file_name().unwrap_or_default().to_string_lossy()).truncate()).on_hover_text(path.display().to_string());
                        } else { ui.small(language.text("Choose a transaction particle PNG")); }
                    });
                    ui.horizontal(|ui| {
                        if ui.button(language.text("Shape SVG...")).on_hover_text(language.text("SVG needs width and height, with one direct path and only a d attribute. Flatten transforms and groups before importing.")).clicked()
                            && let Some(path) = rfd::FileDialog::new().add_filter("Single-path SVG", &["svg"]).pick_file() {
                            self.set_transaction_shape(path); changed = true;
                        }
                        if let Some(path) = &self.draft.transaction_shape {
                            ui.add(egui::Label::new(path.file_name().unwrap_or_default().to_string_lossy()).truncate()).on_hover_text(path.display().to_string());
                        } else { ui.small(language.text("Choose a transaction shape SVG")); }
                    });
                });
                if let (Some(texture), Some(shape)) = (&self.transaction_texture, &self.transaction_outline) {
                    ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(24.0, 24.0) * (texture.size_vec2() / texture.size_vec2().max_elem())));
                    let size = egui::vec2(64.0, 64.0);
                    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                    let painter = ui.painter_at(rect);
                    for contour in &shape.contours {
                        let points = contour.iter().map(|p| rect.min + egui::vec2(p[0], p[1]) * size).collect();
                        painter.add(egui::Shape::line(points, egui::Stroke::new(1.5_f32, ui.visuals().text_color())));
                    }
                }
            });
            ui.small(
                language.text(
                    "Particle and outline preview only; payment animation runs on the phone.",
                ),
            );
        }
        if changed && let Err(error) = self.remember() {
            self.error = Some(format!("Could not save overlay settings: {error:#}"));
        }
    }

    pub fn update_preview(&mut self, ctx: &egui::Context, device_busy: bool) {
        if self.dirty
            && !self.busy()
            && !device_busy
            && let Ok(config) = self.draft.configuration()
        {
            self.dirty = false;
            self.start(ctx, move |progress| {
                progress("Loading overlay preview…");
                Ok(Completed::Preview(Box::new(artwork::preview_for_editor(
                    &config,
                    Path::new("."),
                )?)))
            });
        }
    }

    pub fn ready(&self) -> bool {
        !self.busy()
            && !self.dirty
            && self.preview_texture.is_some()
            && self.preview_issue.is_none()
    }

    pub fn preview_invalid(&self) -> bool {
        self.preview_issue.is_some()
    }

    pub fn apply(&mut self, ctx: &egui::Context, binding: Binding) {
        if !self.ready() {
            return;
        }
        let result = (|| -> Result<_> {
            let config = self.draft.configuration()?;
            let directory = device::new_application_directory()?;
            self.draft.prior_recovery = self.draft.recovery.clone();
            self.draft.recovery = Some(Recovery {
                binding: binding.clone(),
                directory: directory.join("recovery"),
            });
            self.remember()?;
            Ok((config, directory))
        })();
        match result {
            Ok((config, directory)) => self.start(ctx, move |progress| {
                Ok(Completed::Applied(device::apply(
                    binding, config, &directory, progress,
                )?))
            }),
            Err(error) => {
                if self.draft.prior_recovery.is_some()
                    && self
                        .draft
                        .recovery
                        .as_ref()
                        .is_some_and(|r| device::no_installation_to_recover(&r.directory))
                {
                    self.draft.recovery = self.draft.prior_recovery.take();
                }
                self.error = Some(format!("Could not start Apply: {error:#}"));
            }
        }
    }

    pub fn can_restore(&self, binding: &Binding) -> bool {
        !self.busy()
            && self.draft.recovery.as_ref().is_some_and(|recovery| {
                recovery.binding == *binding && recovery.directory.join("journal.json").is_file()
            })
    }

    pub fn restore(&mut self, ctx: &egui::Context, binding: Binding) {
        if !self.can_restore(&binding) {
            return;
        }
        let recovery = self.draft.recovery.clone().unwrap();
        self.start(ctx, move |progress| {
            progress("Restoring previous skin…");
            Ok(Completed::Restored(device::restore(
                &recovery.directory,
                &binding,
            )?))
        });
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn show_feedback(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, language: Language) {
        if self.busy() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(language.text(self.status.as_deref().unwrap_or("Preparing overlay…")));
            });
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if let Some(status) = &self.status {
            ui.label(language.text(status));
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, language.text(error));
            if ui
                .small_button(language.text("Copy error details"))
                .clicked()
            {
                ctx.copy_text(error.clone());
            }
            if !self.busy()
                && self.preview_texture.is_none()
                && ui.button(language.text("Retry preview")).clicked()
            {
                self.dirty = true;
                ctx.request_repaint();
            }
        }
    }

    pub fn show_preview_feedback(&self, ui: &mut egui::Ui, language: Language) {
        if self.preview_texture.is_some()
            && let Some(issue) = self.preview_issue
        {
            ui.colored_label(ui.visuals().error_fg_color, language.text(issue));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_draft_migrates_without_requiring_metadata_or_particles() {
        let draft: Draft = serde_json::from_value(serde_json::json!({
            "background":"background.png", "foreground":"foreground.png",
            "particles":true, "sprite":null, "metadata":"old-pass.json", "session":"old-session.json"
        })).unwrap();
        let config = draft.configuration().unwrap();
        assert!(config.dynamic_layer_configuration.neutral_emitter.is_none());
        assert!(config.emitter_sprite.is_none());
        assert!(config.original_pass_json.as_os_str().is_empty());
        assert!(draft.recovery.is_none());
    }

    #[test]
    fn legacy_adapter_preserves_disabled_inputs_and_recovery_links() {
        let value = serde_json::json!({
            "background": "background.png", "foreground": "foreground.png",
            "particle_sprite": "missing-particle.png",
            "transaction_sprite": "missing-transaction.png",
            "transaction_shape": "missing-shape.svg",
            "alternate_background": "missing-alternate.png",
            "recovery": {
                "binding": {"udid":"phone", "build":"23G83", "product_type":"iPhone15,4",
                    "card_id":"card", "transport":"Usb"},
                "directory":"selected/recovery"
            },
            "prior_recovery": {
                "binding": {"udid":"phone", "build":"23G83", "product_type":"iPhone15,4",
                    "card_id":"card", "transport":"Usb"},
                "directory":"previous/recovery"
            }
        });
        let draft: Draft = serde_json::from_value(value.clone()).unwrap();
        let desired = draft.desired_effects();
        assert_eq!(
            desired.selection,
            Selection {
                parallax: true,
                ..Default::default()
            }
        );
        assert_eq!(desired.inputs.particle_sprite, draft.particle_sprite);
        assert_eq!(
            desired.inputs.alternate_background,
            draft.alternate_background
        );
        let config = draft.configuration().unwrap();
        assert!(config.dynamic_layer_configuration.parallax_enabled);
        assert!(config.emitter_sprite.is_none());
        assert!(config.transaction_sprite.is_none());
        assert!(config.transaction_shape.is_none());
        assert!(config.alternate_background.is_none());
        let saved = serde_json::to_value(draft).unwrap();
        assert_eq!(saved["recovery"], value["recovery"]);
        assert_eq!(saved["prior_recovery"], value["prior_recovery"]);
    }

    #[test]
    fn selecting_different_size_layers_loads_and_paints_preview_without_a_button() {
        let directory =
            std::env::temp_dir().join(format!("aircard-editor-preview-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let background = directory.join("background.png");
        let foreground = directory.join("foreground.png");
        image::RgbaImage::from_pixel(24, 16, image::Rgba([10, 20, 30, 255]))
            .save(&background)
            .unwrap();
        let mut front = image::RgbaImage::new(32, 20);
        front.put_pixel(16, 10, image::Rgba([200, 100, 20, 220]));
        front.save(&foreground).unwrap();
        let mut editor = OverlayEditor {
            draft: Draft {
                background: Some(background),
                foreground: Some(foreground.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let render = |editor: &mut OverlayEditor| {
            ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    editor.update_preview(ctx, false);
                    if let Some(texture) = editor.preview_texture() {
                        ui.add(egui::Image::new(texture).corner_radius(16));
                    }
                    editor.show_preview_feedback(ui, Language::English);
                });
            })
        };
        let _ = render(&mut editor);
        assert!(editor.busy());
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!editor.busy(), "Preview timed out");
        assert!(editor.error.is_none(), "{:?}", editor.error);
        let texture = editor
            .preview_texture
            .as_ref()
            .expect("Automatic preview installed the composite");
        let ids = [texture.id()];
        assert_eq!(texture.size(), [1536, 969]);
        let output = render(&mut editor);
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        for id in ids {
            assert!(primitives.iter().any(|primitive| matches!(&primitive.primitive,
                egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == id && !mesh.indices.is_empty())));
        }
        assert!(editor.ready());
        // An opaque PNG must remain inspectable without becoming applicable
        image::RgbaImage::from_pixel(32, 20, image::Rgba([160, 160, 160, 255]))
            .save(&foreground)
            .unwrap();
        editor.set_overlay_path(foreground.clone());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!editor.busy());
        assert!(
            editor.preview_ready(),
            "Invalid alpha must not leave a blank preview"
        );
        assert!(editor.preview_issue.unwrap().contains("no transparency"));
        assert!(
            !editor.ready(),
            "Visible artwork is not proof of a valid overlay"
        );
        // A replacement with real transparency clears the issue and re-enables Apply
        front.save(&foreground).unwrap();
        editor.set_overlay_path(foreground);
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(editor.error.is_none(), "{:?}", editor.error);
        assert!(editor.ready());
        editor.set_cross_dissolve(true);
        assert!(!editor.ready());
        assert!(
            editor.draft.configuration().is_err(),
            "An enabled effect needs its alternate image"
        );
        let alternate = directory.join("alternate.png");
        image::RgbaImage::from_pixel(40, 24, image::Rgba([100, 40, 120, 255]))
            .save(&alternate)
            .unwrap();
        editor.set_alternate_background(alternate.clone());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(editor.ready(), "{:?}", editor.error);
        let base_id = editor.preview_texture().unwrap().id();
        editor.show_alternate_for_qa();
        assert_ne!(editor.preview_texture().unwrap().id(), base_id);
        assert!(
            editor.ready(),
            "Inspecting the alternate does not change Apply eligibility"
        );
        let saved: Draft =
            serde_json::from_slice(&serde_json::to_vec(&editor.draft).unwrap()).unwrap();
        assert!(saved.cross_dissolve);
        assert_eq!(saved.alternate_background.as_ref(), Some(&alternate));
        editor.set_alternate_background(directory.join("missing.png"));
        assert!(!editor.ready());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(editor.error.is_some());
        assert!(!editor.ready());
        assert!(
            editor.preview_texture().is_none(),
            "A stale alternate must never be shown"
        );
        editor.set_alternate_background(alternate.clone());
        editor.set_cross_dissolve(false);
        assert_eq!(editor.draft.alternate_background.as_ref(), Some(&alternate));
        assert!(
            editor
                .draft
                .configuration()
                .unwrap()
                .alternate_background
                .is_none()
        );
        editor.set_transaction(true);
        assert!(editor.draft.configuration().is_err());
        let transaction_shape = directory.join("shape.svg");
        std::fs::write(
            &transaction_shape,
            br#"<svg width="100" height="100"><path d="M50 5L95 95L5 95Z"/></svg>"#,
        )
        .unwrap();
        editor.set_transaction_shape(transaction_shape.clone());
        assert!(editor.draft.configuration().is_err());
        editor.set_transaction_sprite(editor.overlay_path().unwrap().to_path_buf());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(editor.ready(), "{:?}", editor.error);
        assert!(editor.transaction_texture.is_some() && editor.transaction_outline.is_some());
        let saved: Draft =
            serde_json::from_slice(&serde_json::to_vec(&editor.draft).unwrap()).unwrap();
        assert!(saved.transaction);
        assert_eq!(saved.transaction_shape, Some(transaction_shape.clone()));
        editor.set_transaction_shape(directory.join("missing.svg"));
        assert!(!editor.ready());
        assert!(editor.transaction_texture.is_none() && editor.transaction_outline.is_none());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!editor.ready() && editor.error.is_some());
        editor.set_transaction_shape(transaction_shape.clone());
        editor.set_transaction(false);
        let config = editor.draft.configuration().unwrap();
        assert!(config.transaction_shape.is_none() && config.transaction_sprite.is_none());
        assert_eq!(editor.draft.transaction_shape, Some(transaction_shape));
        editor.set_cross_dissolve(true);
        editor.set_particles(true);
        assert!(!editor.ready());
        assert!(editor.draft.configuration().is_err());
        editor.set_particle_sprite(editor.overlay_path().unwrap().to_path_buf());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(editor.ready(), "{:?}", editor.error);
        assert!(editor.particle_texture.is_some());
        let saved: Draft =
            serde_json::from_slice(&serde_json::to_vec(&editor.draft).unwrap()).unwrap();
        assert!(saved.neutral_particles_enabled && saved.cross_dissolve);
        editor.set_particle_sprite(directory.join("missing-particle.png"));
        assert!(!editor.ready() && editor.particle_texture.is_none());
        let _ = render(&mut editor);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while editor.busy() && std::time::Instant::now() < deadline {
            editor.poll(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!editor.ready() && editor.error.is_some());
        editor.set_particles(false);
        assert!(editor.draft.particle_sprite.is_some());
        assert!(
            editor
                .draft
                .configuration()
                .unwrap()
                .emitter_sprite
                .is_none()
        );
        editor.sync_background(Some(directory.join("replacement.jpg")), [0.25, 0.5]);
        assert!(
            !editor.ready(),
            "A new Wallet background must invalidate the old composite"
        );
        assert!(editor.preview_texture.is_none());
        assert!(
            editor.overlay_path().is_some(),
            "Keep the selected overlay when replacing the background"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
