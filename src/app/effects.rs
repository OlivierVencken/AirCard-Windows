use super::*;

impl AirCardApp {
    pub(super) fn effects_target(&self) -> Option<Binding> {
        let device = self
            .devices
            .iter()
            .find(|device| Some(&device.udid) == self.selected_udid.as_ref())?;
        if !crate::scanner::is_valid_card_hash(self.card_hash.trim()) {
            return None;
        }
        let binding = Binding {
            udid: device.udid.clone(),
            build: device.build_version.clone(),
            product_type: device.product_type.clone(),
            card_id: self.card_hash.trim().to_owned(),
            transport: DeviceTransport::Usb,
        };
        binding.validate().ok()?;
        Some(binding)
    }

    pub(super) fn wallet_effects_blocked(&self) -> bool {
        self.selected_udid.as_ref().is_some_and(|udid| {
            self.overlay_editor
                .blocks_wallet(udid, self.card_hash.trim())
                || self.overlay_editor.device_has_pending_recovery(udid)
        })
    }

    pub(super) fn can_apply_effects(&self) -> bool {
        !self.is_busy
            && !self.scanning_syslog
            && self
                .overlay_binding()
                .as_ref()
                .is_some_and(|binding| self.overlay_editor.matches_target(binding))
            && self.overlay_editor.apply_issue().is_none()
    }

    pub(super) fn show_effects_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let language = self.language;
        ui.heading(language.text("Effects"));
        ui.add_space(6.0);
        let target = self.effects_target();
        self.overlay_editor.select_target(target.clone());
        if target.is_none() {
            ui.label(language.text("Select a card and device in Wallet first."));
            if ui.button(language.text("Go to Wallet")).clicked() {
                self.current_tab = AppTab::Wallet;
            }
            self.overlay_editor.show_feedback(ctx, ui, language);
            return;
        }
        if !self.overlay_editor.matches_target(target.as_ref().unwrap()) {
            ui.label(language.text("Wait for the current operation to finish."));
            self.overlay_editor.show_feedback(ctx, ui, language);
            return;
        }
        let device_name = self
            .devices
            .iter()
            .find(|device| Some(&device.udid) == self.selected_udid.as_ref())
            .map(|device| device.name.as_str())
            .unwrap_or_default();
        let card_name = self
            .saved_cards
            .iter()
            .find(|card| card.hash == self.card_hash.trim())
            .map(|card| card.name.as_str())
            .unwrap_or_default();
        ui.add(
            egui::Label::new(egui::RichText::new(format!("{device_name}  {card_name}")).strong())
                .truncate(),
        );
        ui.label(self.card_hash.trim());
        ui.small(language.text("Draft changes do not change the phone until you apply."));
        ui.add_space(8.0);

        // Actions precede the scrollable editor, keeping recovery and progress
        // reachable at small window sizes and with long input paths.
        ui.horizontal_wrapped(|ui| {
            let apply_label = if self.overlay_editor.all_effects_off() {
                "Turn off effects"
            } else {
                "Apply effects"
            };
            if ui
                .add_enabled(
                    self.can_apply_effects(),
                    egui::Button::new(
                        egui::RichText::new(language.text(apply_label))
                            .strong()
                            .color(md3::ON_PRIMARY),
                    )
                    .fill(md3::PRIMARY),
                )
                .clicked()
                && let Some(binding) = self.overlay_binding()
            {
                self.overlay_editor.apply(ctx, binding);
            }
            let binding = self.overlay_binding();
            let can_restore = !self.is_busy
                && !self.scanning_syslog
                && binding
                    .as_ref()
                    .is_some_and(|binding| self.overlay_editor.can_restore(binding));
            let recovery_label = if self.overlay_editor.recovery_pending() {
                "Recover interrupted operation"
            } else {
                "Undo last apply"
            };
            if ui
                .add_enabled(
                    can_restore,
                    egui::Button::new(language.text(recovery_label)),
                )
                .clicked()
                && let Some(binding) = binding
            {
                self.overlay_editor.restore(ctx, binding);
            }
            let binding = self.overlay_binding();
            let can_baseline = !self.is_busy
                && !self.scanning_syslog
                && binding
                    .as_ref()
                    .is_some_and(|binding| self.overlay_editor.can_restore_baseline(binding));
            if ui
                .add_enabled(
                    can_baseline,
                    egui::Button::new(language.text("Restore before AirCard effects")),
                )
                .on_disabled_hover_text(
                    language
                        .text("Baseline restore is available only when its ancestry is proven."),
                )
                .clicked()
                && let Some(binding) = binding
            {
                self.overlay_editor.restore_baseline(ctx, binding);
            }
        });
        if !self
            .overlay_editor
            .can_restore_baseline(target.as_ref().unwrap())
        {
            ui.small(language.text("If baseline history is incomplete, you can still remove all managed effects below."));
        }
        let binding = self.overlay_binding();
        let can_reset = !self.is_busy
            && !self.scanning_syslog
            && binding
                .as_ref()
                .is_some_and(|b| self.overlay_editor.can_reset(b));
        let has_original = target
            .as_ref()
            .is_some_and(|b| crate::wallet_backup::complete_backup_exists(&b.udid, &b.card_id));
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    can_reset,
                    egui::Button::new(language.text("Remove all effects")),
                )
                .clicked()
                && let Some(binding) = binding.clone()
            {
                self.overlay_editor.reset(ctx, binding, false);
            }
            if ui
                .add_enabled(
                    can_reset && has_original,
                    egui::Button::new(language.text("Reset to saved original artwork")),
                )
                .clicked()
                && let Some(binding) = binding.clone()
            {
                self.overlay_editor.reset(ctx, binding, true);
            }
        });
        ui.small(language.text("Remove all effects uses the selected background and crop. With no artwork selected, it uses the recovered background without the overlay. Reset restores saved original artwork when available."));
        if !has_original {
            ui.small(language.text("No complete original artwork backup is available. Remove effects first, then use Wallet artwork controls. A factory reset cannot be reconstructed from missing originals."));
        }
        ui.small(language.text("Unknown original effect files may remain. Incomplete cleanup retains recovery and blocks further writes; protected-file deletion is not proven."));
        if self.overlay_editor.uses_captured_static() {
            ui.label(language.text("No artwork selection is required for removal. All-off keeps ordinary artwork, or uses the recovered background layer without the overlay."));
        }
        if self.is_busy || self.scanning_syslog {
            ui.label(language.text("Wait for the current operation to finish."));
        } else if self.overlay_binding().is_none() {
            ui.label(language.text("Effects require one iPhone connected by USB only."));
        } else if let Some(issue) = self.overlay_editor.apply_issue() {
            ui.label(format!(
                "{}: {}",
                language.text("Apply effects"),
                language.text(issue)
            ));
        }
        self.overlay_editor.show_feedback(ctx, ui, language);
        ui.add_space(10.0);
        self.overlay_editor
            .update_preview(ctx, self.is_busy || self.scanning_syslog);
        let locked = self.is_busy || self.scanning_syslog || self.overlay_editor.busy();
        if ui.available_width() >= 920.0 {
            ui.columns(2, |columns| {
                egui::ScrollArea::vertical().id_salt("effects_inputs").show(
                    &mut columns[0],
                    |ui| {
                        m3_card(ui, |ui| self.show_effects_inputs(ui, locked));
                    },
                );
                m3_card(&mut columns[1], |ui| self.show_effects_preview(ui, 440.0));
            });
        } else {
            egui::ScrollArea::vertical()
                .id_salt("effects_stacked")
                .show(ui, |ui| {
                    m3_card(ui, |ui| self.show_effects_preview(ui, 300.0));
                    ui.add_space(8.0);
                    m3_card(ui, |ui| self.show_effects_inputs(ui, locked));
                });
        }
    }

    fn show_effects_inputs(&mut self, ui: &mut egui::Ui, locked: bool) {
        ui.set_min_width(ui.available_width());
        let language = self.language;
        ui.label(
            egui::RichText::new(language.text("Configuration"))
                .strong()
                .size(16.0),
        );
        ui.add_space(8.0);
        ui.add_enabled_ui(!locked, |ui| {
            if ui
                .add_enabled(
                    self.source_path.is_some(),
                    egui::Button::new(language.text("Use Wallet artwork and crop")),
                )
                .clicked()
            {
                self.overlay_editor
                    .use_wallet_artwork(self.source_path.clone(), self.crop_focus);
            }
            self.overlay_editor.show_inputs(ui, language);
        });
    }

    fn show_effects_preview(&self, ui: &mut egui::Ui, max_width: f32) {
        ui.set_min_width(ui.available_width());
        let language = self.language;
        ui.label(
            egui::RichText::new(language.text("Static preview"))
                .strong()
                .size(16.0),
        );
        ui.small(
            language.text("Artwork only. Native motion and payment animation run on the phone."),
        );
        ui.add_space(12.0);
        let width = ui.available_width().min(max_width).max(1.0);
        if let Some(texture) = self.overlay_editor.preview_texture() {
            ui.add(
                egui::Image::new(texture)
                    .fit_to_exact_size(egui::vec2(width, width * 969.0 / 1536.0))
                    .corner_radius(16),
            );
        } else {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(width, width * 969.0 / 1536.0),
                egui::Sense::hover(),
            );
            ui.painter()
                .rect_filled(rect, 16.0, md3::SURFACE_CONTAINER_HIGH);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                language.text("No valid composition yet"),
                egui::FontId::proportional(14.0),
                md3::ON_SURFACE_VARIANT,
            );
        }
        self.overlay_editor.show_preview_feedback(ui, language);
    }
}
