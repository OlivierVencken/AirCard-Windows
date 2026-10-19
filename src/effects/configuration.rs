use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::artwork::{
    CrossDissolveConfiguration, DynamicConfiguration, EmitterConfiguration, EmitterDensity,
    EmitterPhysics, EmitterStyle, TransactionConfiguration,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Selection {
    pub parallax: bool,
    pub cross_dissolve: bool,
    pub particles: bool,
    pub transaction: bool,
}

impl Selection {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// All-off means a static transition, never an empty native configuration.
    pub fn native_configuration(self) -> Option<DynamicConfiguration> {
        (!self.is_empty()).then(|| DynamicConfiguration {
            version: 2,
            parallax_enabled: self.parallax,
            cross_dissolve: self
                .cross_dissolve
                .then(CrossDissolveConfiguration::default),
            neutral_emitter: self.particles.then_some(EmitterConfiguration {
                style: EmitterStyle::Twinkling,
                density: EmitterDensity::Low,
                physics_effect: EmitterPhysics::Off,
            }),
            transaction_effect: self.transaction.then(TransactionConfiguration::default),
        })
    }

    pub fn requirements(self, static_foreground: bool) -> Requirements {
        let mut resources = Vec::new();
        if !self.is_empty() {
            // Dynamic fallback is separate from ordinary cardBackground artwork.
            resources.extend([Resource::Background, Resource::Fallback]);
            if self.parallax || static_foreground {
                resources.push(Resource::Foreground);
            }
            if self.cross_dissolve {
                resources.push(Resource::AlternateBackground);
            }
            if self.particles {
                resources.push(Resource::NeutralEmitter);
            }
            if self.transaction {
                resources.extend([Resource::TransactionEmitter, Resource::TransactionShape]);
            }
        }
        Requirements {
            resources,
            parallax_bleed: self.parallax,
            // This flag is deliberately independent of input completeness. The
            // retained evidence supports a candidate contract, not phone playback.
            needs_non_parallax_validation: !self.is_empty() && !self.parallax,
            needs_static_foreground_validation: !self.is_empty()
                && static_foreground
                && !self.parallax,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resource {
    Background,
    Foreground,
    Fallback,
    AlternateBackground,
    NeutralEmitter,
    TransactionEmitter,
    TransactionShape,
}

impl Resource {
    pub fn basename(self) -> &'static str {
        match self {
            Self::Background => "backgroundParallax",
            Self::Foreground => "foregroundParallax",
            Self::Fallback => "dynamicLayerStaticFallback",
            Self::AlternateBackground => "backgroundParallaxCrossDissolve",
            Self::NeutralEmitter => "neutralEmitter",
            Self::TransactionEmitter => "transactionEffectEmitter",
            Self::TransactionShape => "transactionEffectShape",
        }
    }

    pub fn candidate_filenames(self) -> Vec<String> {
        let base = self.basename();
        if self == Self::TransactionShape {
            vec![format!("{base}.svg")]
        } else {
            ["", "@2x", "@3x"]
                .map(|scale| format!("{base}{scale}.png"))
                .into()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Requirements {
    pub resources: Vec<Resource>,
    pub parallax_bleed: bool,
    pub needs_non_parallax_validation: bool,
    pub needs_static_foreground_validation: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Inputs {
    pub background: Option<PathBuf>,
    pub foreground: Option<PathBuf>,
    pub alternate_background: Option<PathBuf>,
    pub particle_sprite: Option<PathBuf>,
    pub transaction_sprite: Option<PathBuf>,
    pub transaction_shape: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct DesiredEffects {
    pub selection: Selection,
    pub inputs: Inputs,
    pub static_foreground: bool,
}

impl DesiredEffects {
    /// Selection completeness only. Decode, alpha, SVG, crop, frozen-byte and
    /// capture verification remain mandatory during managed installation.
    pub fn validate_selected_inputs(&self) -> Result<()> {
        let enabled = self.selection;
        if enabled.is_empty() {
            return Ok(());
        }
        if enabled.parallax || self.static_foreground {
            self.inputs
                .foreground
                .as_ref()
                .context("Choose a transparent overlay PNG")?;
        }
        if enabled.cross_dissolve {
            self.inputs
                .alternate_background
                .as_ref()
                .context("Choose an alternate background image for cross-dissolve")?;
        }
        if enabled.particles {
            self.inputs
                .particle_sprite
                .as_ref()
                .context("Choose a transparent particle PNG")?;
        }
        if enabled.transaction {
            self.inputs
                .transaction_sprite
                .as_ref()
                .context("Choose a transaction particle PNG")?;
            self.inputs
                .transaction_shape
                .as_ref()
                .context("Choose a transaction shape SVG")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(bits: u8) -> Selection {
        Selection {
            parallax: bits & 1 != 0,
            cross_dissolve: bits & 2 != 0,
            particles: bits & 4 != 0,
            transaction: bits & 8 != 0,
        }
    }

    #[test]
    fn every_subset_has_explicit_metadata_resources_and_validation_gates() {
        for bits in 0..16 {
            let selected = selection(bits);
            let plan = selected.requirements(false);
            assert_eq!(plan.parallax_bleed, selected.parallax);
            assert_eq!(
                plan.needs_non_parallax_validation,
                bits != 0 && bits & 1 == 0
            );
            assert!(!plan.needs_static_foreground_validation);
            let configuration = selected.native_configuration();
            if bits == 0 {
                assert!(configuration.is_none());
                assert!(plan.resources.is_empty());
                continue;
            }
            let value = serde_json::to_value(configuration.unwrap()).unwrap();
            assert_eq!(value["version"], 2);
            assert_eq!(value["parallaxEnabled"], selected.parallax);
            assert_eq!(
                value.get("crossDissolve").is_some(),
                selected.cross_dissolve
            );
            assert_eq!(value.get("neutralEmitter").is_some(), selected.particles);
            assert_eq!(
                value.get("transactionEffect").is_some(),
                selected.transaction
            );
            let mut expected = vec![Resource::Background, Resource::Fallback];
            if bits & 1 != 0 {
                expected.push(Resource::Foreground);
            }
            if bits & 2 != 0 {
                expected.push(Resource::AlternateBackground);
            }
            if bits & 4 != 0 {
                expected.push(Resource::NeutralEmitter);
            }
            if bits & 8 != 0 {
                expected.extend([Resource::TransactionEmitter, Resource::TransactionShape]);
            }
            assert_eq!(plan.resources, expected);
            let names: Vec<_> = plan
                .resources
                .iter()
                .flat_map(|r| r.candidate_filenames())
                .collect();
            let mut unique = names.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), names.len());
        }
    }

    #[test]
    fn standalone_emitters_need_no_foreground_or_user_background() {
        for bits in [4, 8, 12] {
            let mut desired = DesiredEffects {
                selection: selection(bits),
                ..Default::default()
            };
            assert!(desired.validate_selected_inputs().is_err());
            desired.inputs.particle_sprite = Some("particle.png".into());
            desired.inputs.transaction_sprite = Some("transaction.png".into());
            desired.inputs.transaction_shape = Some("shape.svg".into());
            desired.validate_selected_inputs().unwrap();
            assert!(
                !desired
                    .selection
                    .requirements(false)
                    .resources
                    .contains(&Resource::Foreground)
            );
            assert!(desired.inputs.background.is_none());
        }
    }

    #[test]
    fn disabled_inputs_survive_roundtrip_and_do_not_block_all_off() {
        let desired = DesiredEffects {
            inputs: Inputs {
                foreground: Some("missing.png".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        desired.validate_selected_inputs().unwrap();
        let restored: DesiredEffects =
            serde_json::from_slice(&serde_json::to_vec(&desired).unwrap()).unwrap();
        assert_eq!(restored, desired);
        let mut enabled = restored;
        enabled.selection.transaction = true;
        assert!(enabled.validate_selected_inputs().is_err());
        enabled.selection.transaction = false;
        enabled.validate_selected_inputs().unwrap();
    }

    #[test]
    fn every_subset_requires_exactly_its_enabled_inputs() {
        for bits in 0..16 {
            let desired = DesiredEffects {
                selection: selection(bits),
                inputs: Inputs {
                    background: None, // Recovered-current-artwork request.
                    foreground: Some("foreground.png".into()),
                    alternate_background: Some("alternate.png".into()),
                    particle_sprite: Some("particle.png".into()),
                    transaction_sprite: Some("transaction.png".into()),
                    transaction_shape: Some("shape.svg".into()),
                },
                static_foreground: false,
            };
            desired.validate_selected_inputs().unwrap();
            for (index, required) in [
                desired.selection.parallax,
                desired.selection.cross_dissolve,
                desired.selection.particles,
                desired.selection.transaction,
                desired.selection.transaction,
            ]
            .into_iter()
            .enumerate()
            {
                let mut missing = desired.clone();
                let inputs = &mut missing.inputs;
                let paths = [
                    &mut inputs.foreground,
                    &mut inputs.alternate_background,
                    &mut inputs.particle_sprite,
                    &mut inputs.transaction_sprite,
                    &mut inputs.transaction_shape,
                ];
                *paths.into_iter().nth(index).unwrap() = None;
                assert_eq!(
                    missing.validate_selected_inputs().is_err(),
                    required,
                    "subset {bits}, input {index}"
                );
            }
        }
    }

    #[test]
    fn static_foreground_is_optional_and_does_not_enable_motion() {
        let selected = selection(4);
        let plan = selected.requirements(true);
        assert!(plan.resources.contains(&Resource::Foreground));
        assert!(plan.needs_static_foreground_validation);
        assert!(!plan.parallax_bleed);
        assert!(!selected.native_configuration().unwrap().parallax_enabled);
        let mut desired = DesiredEffects {
            selection: selected,
            static_foreground: true,
            inputs: Inputs {
                particle_sprite: Some("sprite.png".into()),
                ..Default::default()
            },
        };
        assert!(desired.validate_selected_inputs().is_err());
        desired.inputs.foreground = Some("overlay.png".into());
        desired.validate_selected_inputs().unwrap();
        desired.selection = Selection::default();
        desired.inputs = Inputs::default();
        desired.validate_selected_inputs().unwrap();
        assert!(desired.selection.native_configuration().is_none());
        assert!(desired.selection.requirements(true).resources.is_empty());
    }

    #[test]
    fn resource_names_and_scale_contract_are_exact() {
        for (resource, base) in [
            (Resource::Background, "backgroundParallax"),
            (Resource::Foreground, "foregroundParallax"),
            (Resource::Fallback, "dynamicLayerStaticFallback"),
            (
                Resource::AlternateBackground,
                "backgroundParallaxCrossDissolve",
            ),
            (Resource::NeutralEmitter, "neutralEmitter"),
            (Resource::TransactionEmitter, "transactionEffectEmitter"),
        ] {
            assert_eq!(
                resource.candidate_filenames(),
                [
                    format!("{base}.png"),
                    format!("{base}@2x.png"),
                    format!("{base}@3x.png")
                ]
            );
        }
        assert_eq!(
            Resource::TransactionShape.candidate_filenames(),
            ["transactionEffectShape.svg"]
        );
    }
}
