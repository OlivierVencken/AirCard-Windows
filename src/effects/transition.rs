use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::artwork::sha256;
use super::configuration::Selection;
use super::managed::{Blob, Snapshot};
use super::metadata::parse_unique;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TrialPolicy {
    pub particles: bool,
    pub transaction: bool,
    pub cross_dissolve: bool,
}

impl TrialPolicy {
    pub fn validate(self, selection: Selection) -> Result<()> {
        ensure!(
            u8::from(self.particles) + u8::from(self.transaction) + u8::from(self.cross_dissolve)
                <= 1,
            "Missing-original trial policies cannot be combined automatically"
        );
        ensure!(
            !self.particles || selection.particles,
            "Particle trial requires particles"
        );
        ensure!(
            !self.transaction || selection.transaction,
            "Transaction trial requires transaction effects"
        );
        ensure!(
            !self.cross_dissolve
                || (selection.cross_dissolve && !selection.particles && !selection.transaction),
            "Alternate-original trial cannot combine other effect exceptions"
        );
        Ok(())
    }

    pub fn permits(self, leaf: &str) -> bool {
        (self.particles && png_family(leaf, "neutralEmitter"))
            || (self.transaction
                && (png_family(leaf, "transactionEffectEmitter")
                    || leaf == "transactionEffectShape.svg"))
            || (self.cross_dissolve && png_family(leaf, "backgroundParallaxCrossDissolve"))
    }
}

fn png_family(leaf: &str, base: &str) -> bool {
    [".png", "@2x.png", "@3x.png"]
        .iter()
        .any(|suffix| leaf == format!("{base}{suffix}"))
}

pub(super) fn selection(bytes: &[u8]) -> Result<Option<Selection>> {
    let value = parse_unique(bytes)?;
    let object = value
        .as_object()
        .context("Pass metadata must be an object")?;
    ensure!(
        !object.is_empty(),
        "Complete captured pass metadata is required"
    );
    let Some(dynamic) = object.get("dynamicLayerConfiguration") else {
        return Ok(Some(Selection::default()));
    };
    // A resemblance is not ownership. Only the exact currently authored presets
    // can participate in automatic replacement after separate history checks.
    for bits in 1..16 {
        let candidate = from_bits(bits);
        if dynamic == &serde_json::to_value(candidate.native_configuration().unwrap())? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

pub(super) fn from_bits(bits: u8) -> Selection {
    Selection {
        parallax: bits & 1 != 0,
        cross_dissolve: bits & 2 != 0,
        particles: bits & 4 != 0,
        transaction: bits & 8 != 0,
    }
}

pub(super) fn resource_names(selection: Selection) -> BTreeSet<String> {
    selection
        .requirements(false)
        .resources
        .into_iter()
        .flat_map(|resource| resource.candidate_filenames())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Disposition {
    Install,
    Restore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    pub leaf: String,
    pub expected_preimage: Option<String>,
    pub after: Blob,
    pub disposition: Disposition,
}

pub(super) struct Plan {
    pub metadata: Vec<u8>,
    pub changes: Vec<Change>,
    pub retained: BTreeSet<String>,
    pub inert: BTreeSet<String>,
    pub policy: TrialPolicy,
}

impl Plan {
    pub fn is_noop(&self, source: &Snapshot) -> bool {
        sha256(&self.metadata) == source.metadata.sha256 && self.changes.is_empty()
    }
}

pub(super) fn reconcile(source: &Snapshot, expected: &Snapshot) -> Result<()> {
    ensure!(
        source.binding == expected.binding,
        "Managed device/build/card binding changed"
    );
    ensure!(
        source.metadata.sha256 == expected.metadata.sha256,
        "Card metadata drifted since the last verified operation"
    );
    for (leaf, expected) in &expected.resources {
        if let Some(expected) = expected {
            ensure!(
                source
                    .resources
                    .get(leaf)
                    .and_then(Option::as_ref)
                    .is_some_and(|found| found.sha256 == expected.sha256),
                "Card resource drifted or could not be recovered: {leaf}"
            );
        }
    }
    Ok(())
}

/// `expected` is supplied only by validated managed history, never by a draft
/// or metadata resemblance.
pub(super) fn apply(
    source: &Snapshot,
    expected: Option<&Snapshot>,
    baseline: Option<&Snapshot>,
    owned: bool,
    desired: Selection,
    artwork: &BTreeMap<String, Blob>,
    policy: TrialPolicy,
) -> Result<Plan> {
    plan_apply(
        source, expected, baseline, owned, desired, artwork, policy, false,
    )
}

/// Explicit user-requested removal is distinct from claiming managed ownership.
/// It removes only the exact supported configuration captured immediately before
/// reset, preserves unrelated metadata, and never deletes unknown-origin files.
pub(super) fn reset(
    source: &Snapshot,
    expected: Option<&Snapshot>,
    baseline: Option<&Snapshot>,
    artwork: &BTreeMap<String, Blob>,
) -> Result<Plan> {
    plan_apply(
        source,
        expected,
        baseline,
        false,
        Selection::default(),
        artwork,
        TrialPolicy::default(),
        true,
    )
}

fn plan_apply(
    source: &Snapshot,
    expected: Option<&Snapshot>,
    baseline: Option<&Snapshot>,
    owned: bool,
    desired: Selection,
    artwork: &BTreeMap<String, Blob>,
    policy: TrialPolicy,
    explicit_removal: bool,
) -> Result<Plan> {
    ensure!(
        !explicit_removal || desired.is_empty(),
        "Explicit reset only permits all-off"
    );
    source.validate()?;
    if let Some(expected) = expected {
        reconcile(source, expected)?;
    }
    if let Some(baseline) = baseline {
        ensure!(
            baseline.binding == source.binding,
            "Baseline binding changed"
        );
        baseline.validate()?;
    }
    policy.validate(desired)?;
    let original = source.metadata.read()?;
    let current =
        selection(&original)?.context("Unsupported existing native effects configuration")?;
    ensure!(
        current.is_empty() || (owned && expected.is_some()) || explicit_removal,
        "Existing effects are not owned by verified AirCard history"
    );
    let metadata = super::metadata::replace_managed(&original, &source.metadata.sha256, desired)?;
    let target_names: BTreeSet<_> = if desired.is_empty() {
        crate::wallet_backup::CARD_ARTWORK_ASSETS
            .iter()
            .map(|leaf| (*leaf).to_owned())
            .collect()
    } else {
        resource_names(desired)
    };
    ensure!(
        artwork.keys().cloned().collect::<BTreeSet<_>>() == target_names,
        "Target artwork inventory does not match the desired effects"
    );
    let mut plan = Plan {
        metadata,
        changes: Vec::new(),
        retained: BTreeSet::new(),
        inert: BTreeSet::new(),
        policy,
    };
    // Reject preferred PDFs for resources the target renderer will reference.
    for leaf in resource_names(desired) {
        if let Some(stem) = leaf.strip_suffix(".png") {
            ensure!(
                source
                    .resources
                    .get(&format!("{stem}.pdf"))
                    .and_then(Option::as_ref)
                    .is_none(),
                "Recovered dynamic PDF overrides target PNG: {stem}"
            );
        }
    }
    for (leaf, after) in artwork {
        after.read()?;
        let before = source
            .resources
            .get(leaf)
            .context("Target outside captured inventory")?
            .as_ref();
        ensure!(
            before.is_some() || policy.permits(leaf),
            "Every overwritten resource requires a recoverable preimage: {leaf}"
        );
        add_change(&mut plan, leaf, before, after, Disposition::Install);
    }
    for leaf in resource_names(current).difference(&resource_names(desired)) {
        let before = source
            .resources
            .get(leaf)
            .context("Source resource outside captured inventory")?
            .as_ref();
        let original = baseline
            .and_then(|snapshot| snapshot.resources.get(leaf))
            .and_then(Option::as_ref);
        if let Some(original) = original {
            // Never overwrite an unobserved current resource while removing one.
            ensure!(
                before.is_some(),
                "Removed resource preimage is unknown: {leaf}"
            );
            original.read()?;
            add_change(&mut plan, leaf, before, original, Disposition::Restore);
        } else {
            plan.inert.insert(leaf.clone());
        }
    }
    for leaf in source.resources.keys() {
        if !plan.changes.iter().any(|change| change.leaf == *leaf) && !plan.inert.contains(leaf) {
            plan.retained.insert(leaf.clone());
        }
    }
    Ok(plan)
}

fn add_change(
    plan: &mut Plan,
    leaf: &str,
    before: Option<&Blob>,
    after: &Blob,
    disposition: Disposition,
) {
    if before.is_some_and(|before| before.sha256 == after.sha256) {
        plan.retained.insert(leaf.to_owned());
    } else {
        plan.changes.push(Change {
            leaf: leaf.to_owned(),
            expected_preimage: before.map(|blob| blob.sha256.clone()),
            after: after.clone(),
            disposition,
        });
    }
}

/// Undo/baseline restoration uses exact proven snapshots, including pre-existing
/// native configurations. Unknown source preimages are left inert, never deleted.
pub(super) fn restore(source: &Snapshot, expected: &Snapshot, target: &Snapshot) -> Result<Plan> {
    source.validate()?;
    target.validate()?;
    reconcile(source, expected)?;
    ensure!(
        source.binding == target.binding,
        "Restore target belongs to another card/build"
    );
    let mut plan = Plan {
        metadata: target.metadata.read()?,
        changes: Vec::new(),
        retained: BTreeSet::new(),
        inert: BTreeSet::new(),
        policy: TrialPolicy::default(),
    };
    for (leaf, before) in &source.resources {
        match target.resources.get(leaf).and_then(Option::as_ref) {
            Some(after) => {
                ensure!(
                    before.is_some(),
                    "Restore would overwrite an unknown current preimage: {leaf}"
                );
                add_change(
                    &mut plan,
                    leaf,
                    before.as_ref(),
                    after,
                    Disposition::Restore,
                );
            }
            None if before.is_some() => {
                plan.inert.insert(leaf.clone());
            }
            None => {
                plan.retained.insert(leaf.clone());
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::super::managed::{self, Mode, Phase, tests::Fixture};
    use super::*;

    #[test]
    fn all_256_pairs_cover_install_restore_retain_and_exact_metadata() {
        let fixture = Fixture::new();
        let baseline = fixture.snapshot(0, "baseline");
        let sources: Vec<_> = (0..16)
            .map(|bits| fixture.snapshot(bits, "source"))
            .collect();
        let targets: Vec<_> = (0..16)
            .map(|bits| fixture.artwork(bits, "target"))
            .collect();
        for source_bits in 0..16u8 {
            for target_bits in 0..16u8 {
                let source = &sources[source_bits as usize];
                let plan = apply(
                    source,
                    Some(source),
                    Some(&baseline),
                    true,
                    from_bits(target_bits),
                    &targets[target_bits as usize],
                    Default::default(),
                )
                .unwrap();
                assert_eq!(
                    selection(&plan.metadata).unwrap(),
                    Some(from_bits(target_bits))
                );
                let value = parse_unique(&plan.metadata).unwrap();
                assert_eq!(
                    value["unrelated"],
                    serde_json::json!({"array":[null,1,true]})
                );
                assert_eq!(
                    value.get("dynamicLayerConfiguration").is_none(),
                    target_bits == 0
                );
                assert!(plan.inert.is_empty());
                let removed = resource_names(from_bits(source_bits))
                    .difference(&resource_names(from_bits(target_bits)))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let restored = plan
                    .changes
                    .iter()
                    .filter(|change| change.disposition == Disposition::Restore)
                    .map(|change| change.leaf.clone())
                    .collect::<BTreeSet<_>>();
                assert_eq!(removed, restored, "{source_bits} -> {target_bits}");
                for change in &plan.changes {
                    assert_eq!(
                        change.expected_preimage.as_ref(),
                        Some(&source.resources[&change.leaf].as_ref().unwrap().sha256)
                    );
                }
            }
        }
    }

    #[test]
    fn explicit_reset_without_ancestry_does_not_grant_ordinary_apply_ownership() {
        let fixture = Fixture::new();
        let source = fixture.snapshot(15, "current");
        let artwork = fixture.artwork(0, "saved-original");
        assert!(
            apply(
                &source,
                Some(&source),
                None,
                false,
                Selection::default(),
                &artwork,
                TrialPolicy::default()
            )
            .is_err()
        );
        let plan = reset(&source, Some(&source), None, &artwork).unwrap();
        assert_eq!(
            selection(&plan.metadata).unwrap(),
            Some(Selection::default())
        );
        assert_eq!(
            parse_unique(&plan.metadata).unwrap()["serialNumber"],
            "preserved"
        );
        assert!(
            !plan.inert.is_empty(),
            "Unknown origins remain incomplete, never deleted"
        );
        assert!(plan.changes.iter().all(|change| {
            crate::wallet_backup::CARD_ARTWORK_ASSETS.contains(&change.leaf.as_str())
        }));
        for change in &plan.changes {
            assert_eq!(change.after, artwork[&change.leaf]);
        }
        let drifted = fixture.snapshot(15, "drift");
        assert!(reset(&drifted, Some(&source), None, &artwork).is_err());
        let mut unsupported = source.clone();
        unsupported.metadata = fixture.blob(
            br#"{"serialNumber":"preserved","dynamicLayerConfiguration":{"unknownEffect":true}}"#,
        );
        assert!(reset(&unsupported, None, None, &artwork).is_err());
    }

    #[test]
    fn drift_unknown_members_and_unowned_effects_are_never_overwritten() {
        let fixture = Fixture::new();
        let source = fixture.snapshot(15, "source");
        let target = fixture.artwork(1, "target");
        assert!(
            apply(
                &source,
                None,
                None,
                false,
                from_bits(1),
                &target,
                Default::default()
            )
            .is_err()
        );
        let mut drifted = source.clone();
        drifted.resources.insert(
            "neutralEmitter.png".into(),
            Some(fixture.blob(b"external edit")),
        );
        assert!(
            apply(
                &drifted,
                Some(&source),
                None,
                true,
                from_bits(1),
                &target,
                Default::default()
            )
            .is_err()
        );
        let mut unknown = parse_unique(&source.metadata.read().unwrap()).unwrap();
        unknown["dynamicLayerConfiguration"]["unknown"] = true.into();
        drifted = source.clone();
        drifted.metadata = fixture.blob(&serde_json::to_vec(&unknown).unwrap());
        assert!(
            apply(
                &drifted,
                Some(&drifted),
                None,
                true,
                from_bits(1),
                &target,
                Default::default()
            )
            .is_err()
        );
        let mut wrong_build = source.clone();
        wrong_build.binding.build = "23G84".into();
        assert!(reconcile(&wrong_build, &source).is_err());
    }

    #[test]
    fn trial_cleanup_is_incomplete_without_deletion_or_fabricated_originals() {
        let fixture = Fixture::new();
        let source = fixture.snapshot(5, "source");
        let mut baseline = fixture.snapshot(0, "baseline");
        for leaf in resource_names(from_bits(4)) {
            if png_family(&leaf, "neutralEmitter") {
                baseline.resources.insert(leaf, None);
            }
        }
        let artwork = fixture.artwork(1, "target");
        let plan = apply(
            &source,
            Some(&source),
            Some(&baseline),
            true,
            from_bits(1),
            &artwork,
            Default::default(),
        )
        .unwrap();
        assert_eq!(plan.inert.len(), 3);
        assert!(
            plan.changes
                .iter()
                .all(|change| !png_family(&change.leaf, "neutralEmitter"))
        );
        let mut store = fixture.store();
        store.index.expected = Some(source.clone());
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let reference = store
            .prepare(&directory, Mode::Apply, source, plan, None)
            .unwrap();
        managed::set_phase(&reference, Phase::Verified).unwrap();
        assert!(store.commit(&reference).unwrap());
        assert!(store.index.pending.is_some());
        assert_eq!(managed::phase(&reference).unwrap(), Phase::Incomplete);
        assert!(store.begin(&fixture.directory()).is_err());
    }

    #[test]
    fn exact_repeats_are_byte_preserving_and_narrow_policies_cannot_expand() {
        let fixture = Fixture::new();
        for bits in 0..16 {
            let source = fixture.snapshot(bits, "source");
            let names = if bits == 0 {
                crate::wallet_backup::CARD_ARTWORK_ASSETS
                    .iter()
                    .map(|leaf| (*leaf).to_owned())
                    .collect()
            } else {
                resource_names(from_bits(bits))
            };
            let artwork = names
                .into_iter()
                .map(|leaf| (leaf.clone(), source.resources[&leaf].clone().unwrap()))
                .collect();
            let plan = apply(
                &source,
                Some(&source),
                None,
                true,
                from_bits(bits),
                &artwork,
                Default::default(),
            )
            .unwrap();
            assert!(plan.is_noop(&source));
            assert_eq!(plan.metadata, source.metadata.read().unwrap());
        }
        let policy = TrialPolicy {
            particles: true,
            ..Default::default()
        };
        assert!(policy.validate(from_bits(15)).is_ok());
        assert!(policy.permits("neutralEmitter@3x.png"));
        for leaf in [
            "pass.json",
            "neutralEmitter.pdf",
            "backgroundParallax.png",
            "transactionEffectShape.svg",
        ] {
            assert!(!policy.permits(leaf));
        }
        assert!(
            TrialPolicy {
                particles: true,
                transaction: true,
                ..Default::default()
            }
            .validate(from_bits(15))
            .is_err()
        );
        let mut source = fixture.snapshot(0, "source");
        source.resources.insert("neutralEmitter.png".into(), None);
        let artwork = fixture.artwork(5, "target");
        assert!(
            apply(
                &source,
                None,
                None,
                false,
                from_bits(5),
                &artwork,
                Default::default()
            )
            .is_err()
        );
        assert!(apply(&source, None, None, false, from_bits(5), &artwork, policy).is_ok());
        source
            .resources
            .insert("backgroundParallax.png".into(), None);
        assert!(apply(&source, None, None, false, from_bits(5), &artwork, policy).is_err());
    }
}
