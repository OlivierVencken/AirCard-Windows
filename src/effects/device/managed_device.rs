use super::*;
use crate::effects::configuration::DesiredEffects;
use crate::effects::managed::{self as history, Blob, Mode, Phase, Snapshot, Store};
use crate::effects::transition::{self, TrialPolicy};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(crate) struct ManagedOutcome {
    pub verified: bool,
    pub incomplete: bool,
    pub recovery: Option<PathBuf>,
    pub selection: Option<crate::effects::configuration::Selection>,
    pub effects_active: bool,
    pub baseline_available: bool,
    pub pending: bool,
}

pub(crate) fn managed_status(binding: &Binding) -> Result<Option<ManagedOutcome>> {
    let path = Store::default_path(binding)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let store = Store::open(path, binding)?;
    if store.index.expected.is_none() && store.index.head.is_none() && store.index.pending.is_none()
    {
        return Ok(None);
    }
    Ok(Some(outcome(&store)?))
}

fn outcome(store: &Store) -> Result<ManagedOutcome> {
    let recovery = if let Some(pending) = &store.index.pending {
        Some(pending.join("recovery"))
    } else {
        store
            .index
            .head
            .as_ref()
            .map(|head| {
                history::load_operation(head, &store.index.binding)
                    .map(|operation| operation.recovery)
            })
            .transpose()?
    };
    let selection = store
        .index
        .expected
        .as_ref()
        .map(|snapshot| transition::selection(&snapshot.metadata.read()?))
        .transpose()?
        .flatten();
    let limited_originals = recovery
        .as_ref()
        .filter(|directory| directory.join("journal.json").is_file())
        .map(|directory| load(directory).map(|journal| journal.incomplete_recovery()))
        .transpose()?
        .unwrap_or(false);
    Ok(ManagedOutcome {
        verified: store.index.expected.is_some(),
        incomplete: limited_originals
            || store.index.pending.as_ref().is_some_and(|directory| {
                history::reference(directory)
                    .and_then(|reference| history::phase(&reference))
                    .is_ok_and(|phase| {
                        matches!(phase, Phase::Incomplete | Phase::RecoveryIncomplete)
                    })
            }),
        recovery,
        selection: if store.index.pending.is_some() {
            None
        } else {
            selection
        },
        effects_active: store.index.pending.is_some()
            || store
                .index
                .expected
                .as_ref()
                .map(Snapshot::has_effects)
                .transpose()?
                .unwrap_or(true),
        baseline_available: store.index.baseline_restore_available,
        pending: store.index.pending.is_some(),
    })
}

fn snapshot(directory: &Path, journal: &Journal) -> Result<Snapshot> {
    let mut resources: BTreeMap<_, _> = history::resource_inventory()
        .into_iter()
        .map(|leaf| (leaf, None))
        .collect();
    let mut metadata = None;
    for (index, asset) in journal.assets.iter().enumerate() {
        let Some(hash) = asset.original_sha256.as_ref() else {
            continue;
        };
        let blob = Blob::existing(directory.join(format!("original-{index}.bin")), hash)?;
        if asset.leaf == "pass.json" {
            metadata = Some(blob);
        } else {
            resources.insert(asset.leaf.clone(), Some(blob));
        }
    }
    let snapshot = Snapshot {
        binding: journal.binding.clone(),
        metadata: metadata.context("Managed capture lacks complete metadata")?,
        resources,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

fn frozen_inputs(desired: &DesiredEffects, directory: &Path) -> Result<DesiredEffects> {
    desired.validate_selected_inputs()?;
    let mut desired = desired.clone();
    let selected = desired.selection;
    let inputs = &mut desired.inputs;
    for (name, path, enabled) in [
        ("background", &mut inputs.background, true),
        ("foreground", &mut inputs.foreground, selected.parallax),
        (
            "alternate",
            &mut inputs.alternate_background,
            selected.cross_dissolve,
        ),
        ("particle", &mut inputs.particle_sprite, selected.particles),
        (
            "transaction",
            &mut inputs.transaction_sprite,
            selected.transaction,
        ),
        ("shape", &mut inputs.transaction_shape, selected.transaction),
    ] {
        if !enabled {
            *path = None;
            continue;
        }
        if let Some(source) = path {
            let bytes = regular_read(
                source,
                if name == "shape" {
                    crate::effects::transaction::MAX_SVG_BYTES
                } else {
                    media::MAX_FILE_BYTES
                },
            )?;
            let frozen = directory.join(format!("input-{name}"));
            Blob::freeze(&frozen, &bytes)?;
            *source = frozen;
        }
    }
    Ok(desired)
}

fn prepare_artwork(
    mut desired: DesiredEffects,
    alignment: &artwork::Alignment,
    source: &Snapshot,
    directory: &Path,
) -> Result<BTreeMap<String, Blob>> {
    if desired.selection.is_empty()
        && desired.inputs.background.is_none()
        && !source.has_effects()?
    {
        // Ordinary rendering already is the target. Keep its exact PNG/PDF
        // bytes rather than guessing which raster represents a preferred PDF.
        return crate::wallet_backup::CARD_ARTWORK_ASSETS
            .iter()
            .filter_map(|leaf| {
                source.resources[*leaf]
                    .clone()
                    .map(|blob| Ok(((*leaf).to_owned(), blob)))
            })
            .collect();
    }
    if desired.inputs.background.is_none() {
        if source.has_effects()? {
            ensure!(
                !source
                    .resources
                    .iter()
                    .any(|(leaf, blob)| leaf.starts_with("backgroundParallax")
                        && !leaf.starts_with("backgroundParallaxCrossDissolve")
                        && leaf.ends_with(".pdf")
                        && blob.is_some()),
                "A PDF overrides the captured background PNG; choose an explicit background image"
            );
            let original = captured_background(source, true).context(
                "Current background layer could not be recovered safely; choose a background image",
            )?;
            let bytes = original.read()?;
            return artwork::static_from_captured_background(&bytes)?
                .into_iter()
                .map(|(leaf, bytes)| {
                    let blob = Blob::freeze(&directory.join(format!("target-{leaf}")), &bytes)?;
                    Ok((leaf, blob))
                })
                .collect();
        }
        let original = captured_background(source, false).context(
            "Current static artwork could not be recovered safely; choose a background image",
        )?;
        original.read()?;
        desired.inputs.background = Some(original.path.clone());
    }
    freeze_artwork(&desired, alignment, directory)
}

fn captured_background(source: &Snapshot, dynamic: bool) -> Option<&Blob> {
    // This is local background recovery, not a claim about the phone's active
    // screen scale. Prefer the highest recovered raster; never use the flattened
    // dynamic fallback because it already contains the foreground.
    let names: &[&str] = if dynamic {
        &[
            "backgroundParallax@3x.png",
            "backgroundParallax@2x.png",
            "backgroundParallax.png",
        ]
    } else {
        &[
            "cardBackgroundCombined@3x.png",
            "cardBackgroundCombined@2x.png",
        ]
    };
    names
        .iter()
        .find_map(|leaf| source.resources.get(*leaf).and_then(Option::as_ref))
}

fn freeze_artwork(
    desired: &DesiredEffects,
    alignment: &artwork::Alignment,
    directory: &Path,
) -> Result<BTreeMap<String, Blob>> {
    artwork::managed_artwork(desired, alignment)?
        .into_iter()
        .map(|(leaf, bytes)| {
            let blob = Blob::freeze(&directory.join(format!("target-{leaf}")), &bytes)?;
            Ok((leaf, blob))
        })
        .collect()
}

pub(crate) fn managed_apply(
    binding: Binding,
    desired: DesiredEffects,
    alignment: artwork::Alignment,
    directory: &Path,
    progress: impl Fn(&str),
) -> Result<ManagedOutcome> {
    // Standalone nonempty native rendering remains an explicit validation gate.
    ensure!(
        desired.selection.is_empty() || desired.selection.parallax,
        "Standalone native effects still need phone validation"
    );
    ensure!(
        !desired.static_foreground,
        "Static foreground without native validation is not installable"
    );
    let _lock = DeviceLock::acquire(&binding)?;
    let desired = frozen_inputs(&desired, directory)?;
    // Validate/prepare chosen artwork before protected capture. Only the explicit
    // current-artwork request depends on newly captured immutable pixels.
    let prepared = if desired.inputs.background.is_some() {
        Some(freeze_artwork(&desired, &alignment, directory)?)
    } else {
        None
    };
    let mut store = Store::open(Store::default_path(&binding)?, &binding)?;
    store.ensure_device_ready()?;
    ensure!(
        store.index.pending.is_none(),
        "Recover the pending managed operation first"
    );
    let selected = desired.selection;
    let policy = TrialPolicy {
        particles: selected.particles && experimental_particles_enabled(),
        transaction: selected.transaction && experimental_transaction_enabled(),
        cross_dissolve: selected.cross_dissolve && experimental_cross_dissolve_enabled(),
    };
    policy.validate(selected)?;
    run(
        &mut store,
        directory,
        Mode::Apply,
        |source, store| {
            let artwork = match prepared {
                Some(artwork) => artwork,
                None => prepare_artwork(desired, &alignment, source, directory)?,
            };
            let origins = store.resource_baseline()?;
            let plan = transition::apply(
                source,
                store.index.expected.as_ref(),
                origins.as_ref(),
                store.index.head.is_some(),
                selected,
                &artwork,
                policy,
            )?;
            Ok((plan, None))
        },
        progress,
    )
}

pub(crate) fn managed_restore(
    binding: Binding,
    baseline: bool,
    progress: impl Fn(&str),
) -> Result<ManagedOutcome> {
    let _lock = DeviceLock::acquire(&binding)?;
    let mut store = Store::open(Store::default_path(&binding)?, &binding)?;
    store.ensure_device_ready()?;
    if store.index.pending.is_some() {
        ensure!(
            !baseline,
            "Recover the pending operation before baseline restoration"
        );
        return recover(&mut store, progress);
    }
    let directory = new_application_directory()?;
    let (target, next) = if baseline {
        ensure!(
            store.index.baseline_restore_available,
            "Baseline ancestry is incomplete; Undo remains available where proven"
        );
        (
            store
                .resource_baseline()?
                .context("No trustworthy baseline")?,
            None,
        )
    } else {
        let head = store
            .index
            .head
            .as_ref()
            .context("No proven managed apply to undo")?;
        let operation = history::load_operation(head, &binding)?;
        (operation.source, operation.previous_head)
    };
    run(
        &mut store,
        &directory,
        if baseline { Mode::Baseline } else { Mode::Undo },
        |source, store| {
            let plan = transition::restore(
                source,
                store
                    .index
                    .expected
                    .as_ref()
                    .context("Managed source is unknown")?,
                &target,
            )?;
            Ok((plan, next))
        },
        progress,
    )
}

/// Saved artwork is a separate backup, never fabricated factory metadata.
pub(crate) fn managed_reset(
    binding: Binding,
    saved_original: bool,
    background: Option<PathBuf>,
    alignment: artwork::Alignment,
    directory: &Path,
    progress: impl Fn(&str),
) -> Result<ManagedOutcome> {
    let _lock = DeviceLock::acquire(&binding)?;
    let prepared = if saved_original {
        Some(
            crate::wallet_backup::load_complete_original_assets(&binding.udid, &binding.card_id)?
                .into_iter()
                .map(|(leaf, bytes)| {
                    let blob = Blob::freeze(&directory.join(format!("target-{leaf}")), &bytes)?;
                    Ok((leaf, blob))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        )
    } else {
        prepare_reset_background(background, &alignment, directory)?
    };
    let mut store = Store::open(Store::default_path(&binding)?, &binding)?;
    store.ensure_device_ready()?;
    ensure!(
        store.index.pending.is_none(),
        "Recover the pending managed operation first"
    );
    run(
        &mut store,
        directory,
        Mode::Apply,
        |source, store| {
            let artwork = match prepared {
                Some(artwork) => artwork,
                None => prepare_artwork(
                    DesiredEffects::default(),
                    &artwork::Alignment::default(),
                    source,
                    directory,
                )?,
            };
            let origins = store.resource_baseline()?;
            let plan = transition::reset(
                source,
                store.index.expected.as_ref(),
                origins.as_ref(),
                &artwork,
            )?;
            Ok((plan, None))
        },
        progress,
    )
}

fn prepare_reset_background(
    background: Option<PathBuf>,
    alignment: &artwork::Alignment,
    directory: &Path,
) -> Result<Option<BTreeMap<String, Blob>>> {
    let Some(background) = background else {
        return Ok(None);
    };
    // Reset selects only the background.
    let mut desired = DesiredEffects::default();
    desired.inputs.background = Some(background);
    let frozen = frozen_inputs(&desired, directory)?;
    Ok(Some(freeze_artwork(&frozen, alignment, directory)?))
}

fn run(
    store: &mut Store,
    directory: &Path,
    mode: Mode,
    build: impl FnOnce(&Snapshot, &Store) -> Result<(transition::Plan, Option<history::Reference>)>,
    progress: impl Fn(&str),
) -> Result<ManagedOutcome> {
    store.begin(directory)?;
    let recovery = directory.join("recovery");
    let mut needs_recovery = true;
    let mut source_snapshot = None;
    let operation = (|| -> Result<()> {
        progress("Backing up current card metadata and artwork…");
        capture_metadata_for_binding(&recovery, store.index.binding.clone())?;
        needs_recovery = false;
        let source = snapshot(&recovery, &load(&recovery)?)?;
        source_snapshot = Some(source.clone());
        let planned = build(&source, store);
        match &planned {
            Ok((plan, _)) => compatibility_report(directory, &source, Some(plan), None)?,
            Err(error) => {
                compatibility_report(directory, &source, None, Some(&format!("{error:#}")))?
            }
        }
        let (plan, next) = planned?;
        if plan.is_noop(&source) && mode == Mode::Apply {
            store.index.expected = Some(source);
            store.abort_after_verified_recovery()?;
            return Ok(());
        }
        let unchanged = plan.is_noop(&source);
        let reference = store.prepare(directory, mode, source, plan, next)?;
        if !unchanged {
            progress("Applying and verifying overlay skin…");
            install_managed(&reference, &mut needs_recovery)?;
        }
        history::set_phase(&reference, Phase::Verified)?;
        store.commit(&reference)?;
        Ok(())
    })();
    if let Err(error) = operation {
        if !needs_recovery
            && history::reference(directory)
                .and_then(|reference| history::phase(&reference))
                .is_ok_and(|phase| {
                    matches!(
                        phase,
                        Phase::Verified | Phase::Committed | Phase::Incomplete
                    )
                })
        {
            return Err(error.context("Verified state needs managed history recovery"));
        }
        if !needs_recovery
            && recovery.join("journal.json").exists()
            && load(&recovery).is_ok_and(|journal| {
                journal.kind == JournalKind::ManagedTransition
                    && journal.status == "ParallaxCandidateReadbackMatchedAwaitingManualObservation"
            })
        {
            // The candidate is already verified. A phase/index failure must keep
            // its pending pointer for restart reconciliation, never discard it.
            return Err(error.context("Verified candidate needs managed history recovery"));
        }
        let mut recovered_source = false;
        if needs_recovery && recovery.join("journal.json").is_file() {
            let mut journal = load(&recovery)?;
            let (session, mut afc) = open(&journal.binding)?;
            restore_and_verify_originals(&recovery, &mut journal, &mut afc, &session)
                .with_context(|| {
                    format!(
                        "Managed failure: {error:#}; recovery remains at {}",
                        recovery.display()
                    )
                })?;
            journal.status = "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation".into();
            save(&recovery, &journal)?;
            if journal.incomplete_recovery() {
                if let Ok(reference) = history::reference(directory) {
                    history::set_phase(&reference, Phase::RecoveryIncomplete)?;
                }
                return Err(error.context("Recovery is incomplete; keep the selected recovery"));
            }
            recovered_source = true;
        }
        // Local rejection after a fully verified capture performs no second restore.
        if store.index.expected.is_none() {
            store.index.expected = source_snapshot;
        }
        if let Ok(reference) = history::reference(directory) {
            history::set_phase(
                &reference,
                if recovered_source {
                    Phase::Recovered
                } else {
                    Phase::Aborted
                },
            )?;
        }
        store.abort_after_verified_recovery()?;
        return Err(error.context(format!(
            "Managed application retained at {}",
            directory.display()
        )));
    }
    outcome(store)
}

/// Local diagnostic evidence for testing a different phone/build. This report
/// is never read as recovery state, and never implies native playback support.
fn compatibility_report(
    directory: &Path,
    source: &Snapshot,
    plan: Option<&transition::Plan>,
    error: Option<&str>,
) -> Result<()> {
    let recovered: BTreeMap<_, _> = source
        .resources
        .iter()
        .filter_map(|(leaf, blob)| blob.as_ref().map(|blob| (leaf, &blob.sha256)))
        .collect();
    let unknown: Vec<_> = source
        .resources
        .iter()
        .filter_map(|(leaf, blob)| blob.is_none().then_some(leaf))
        .collect();
    let writes: Vec<_> = plan
        .into_iter()
        .flat_map(|plan| &plan.changes)
        .map(|change| {
            serde_json::json!({
                "file": change.leaf,
                "preimage_sha256": change.expected_preimage,
                "candidate_sha256": change.after.sha256,
            })
        })
        .collect();
    let report = serde_json::json!({
        "schema_version": 1,
        "product_type": source.binding.product_type,
        "build": source.binding.build,
        "metadata_sha256": source.metadata.sha256,
        "recovered_resources": recovered,
        "unknown_resources": unknown,
        "planned_artwork_writes": writes,
        "trial_policy": plan.map(|plan| plan.policy),
        "preflight_error": error,
        "native_playback_verified": false,
    });
    Blob::freeze(
        &directory.join("compatibility.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

fn install_managed(reference: &history::Reference, needs_recovery: &mut bool) -> Result<()> {
    let operation = history::load_operation(
        reference,
        &load(&reference.directory.join("recovery"))?.binding,
    )?;
    let dir = &operation.recovery;
    let mut journal = load(dir)?;
    ensure!(
        installation_ready(&journal),
        "Managed capture did not finish verification"
    );
    let mut payloads: Vec<_> = operation
        .changes
        .iter()
        .map(|change| Ok((change.leaf.clone(), change.after.read()?)))
        .collect::<Result<_>>()?;
    payloads.push(("pass.json".into(), operation.target.metadata.read()?));
    for (leaf, bytes) in &payloads {
        let index = journal
            .assets
            .iter()
            .position(|asset| asset.leaf == *leaf)
            .context("Managed payload outside fixed inventory")?;
        write_new_or_same(&dir.join(format!("candidate-{index}.bin")), bytes)?;
        journal.assets[index].candidate_sha256 = sha256(bytes);
    }
    journal.kind = JournalKind::ManagedTransition;
    journal.transition_sha256 = Some(reference.sha256.clone());
    journal.allow_missing_particle_originals = operation.policy.particles;
    journal.allow_missing_transaction_originals = operation.policy.transaction;
    journal.allow_missing_cross_dissolve_originals = operation.policy.cross_dissolve;
    journal.status = "ManagedPrepared".into();
    save(dir, &journal)?;
    load(dir)?; // Validate persisted policies/preimages before opening a session.
    let (session, mut afc) = open(&journal.binding)?;
    let originals: Vec<_> = journal
        .assets
        .iter()
        .enumerate()
        .filter_map(|(i, asset)| (i != 3 && asset.original_sha256.is_some()).then_some(i))
        .collect();
    *needs_recovery = true;
    let recovered = capture_and_return(dir, &mut journal, &mut afc, &session, &originals)?;
    require_roundtrip(&recovered, &originals)?;
    invalidate_caches(dir, &mut journal, &mut afc, &session)?;
    verify_original_metadata_roundtrips(dir, &mut journal, &mut afc, &session)?;
    history::set_phase(reference, Phase::Installing)?;
    let indices: Vec<_> = payloads
        .iter()
        .map(|(leaf, _)| {
            journal
                .assets
                .iter()
                .position(|asset| asset.leaf == *leaf)
                .unwrap()
        })
        .collect();
    let artwork: Vec<_> = indices.iter().copied().filter(|i| *i != 3).collect();
    if !artwork.is_empty() {
        write_candidates(dir, &mut journal, &mut afc, &session, &artwork)?;
    }
    write_candidates(dir, &mut journal, &mut afc, &session, &[3])?;
    verify_managed(dir, &mut journal, &mut afc, &session, &indices)?;
    journal.status = "ParallaxCandidateReadbackMatchedAwaitingManualObservation".into();
    save(dir, &journal)?;
    *needs_recovery = false;
    Ok(())
}

fn verify_managed(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    indices: &[usize],
) -> Result<()> {
    let mut invalidated = false;
    for group in verification_groups(&indices) {
        if group == [3] && !invalidated {
            invalidate_caches(dir, journal, afc, session)?;
            invalidated = true;
        }
        verify_candidate_export(dir, journal, afc, session, &group)?;
        write_candidates(dir, journal, afc, session, &group)?;
        media::cleanup_staging(afc, &journal.held)?;
        cleanup(dir, journal, afc)?;
    }
    Ok(())
}

fn verification_groups(indices: &[usize]) -> Vec<Vec<usize>> {
    let artwork: Vec<_> = indices
        .iter()
        .copied()
        .filter(|index| *index != 3)
        .collect();
    let mut groups = Vec::new();
    if !artwork.is_empty() {
        groups.push(artwork);
    }
    groups.extend([vec![3], vec![3]]);
    groups
}

fn recover(store: &mut Store, progress: impl Fn(&str)) -> Result<ManagedOutcome> {
    let directory = store
        .index
        .pending
        .clone()
        .context("No pending operation")?;
    let reference = if directory.join("transition.json").try_exists()? {
        Some(history::reference(&directory)?)
    } else {
        None
    };
    let pending_phase = reference.as_ref().map(history::phase).transpose();
    let raw_capture = if directory.join("recovery/journal.json").is_file() {
        Some(load(&directory.join("recovery"))?)
    } else {
        None
    };
    if let Some(journal) = &raw_capture
        && !journal.kind.is_installation()
        && installation_ready(journal)
        && (pending_phase.is_err()
            || matches!(
                &pending_phase,
                Ok(None | Some(Phase::Prepared | Phase::Aborted))
            ))
    {
        // Capture finished before any installation intent. A torn local manifest
        // cannot require protected writes or discard the previous managed head.
        ensure!(
            journal.binding == store.index.binding,
            "Pending capture binding differs"
        );
        if store.index.expected.is_none() {
            store.index.expected = Some(snapshot(&directory.join("recovery"), journal)?);
        }
        store.abort_after_verified_recovery()?;
        let mut status = outcome(store)?;
        status.verified = false;
        return Ok(status);
    }
    let pending_phase = pending_phase?;
    if let Some(reference) = &reference
        && pending_phase == Some(Phase::Incomplete)
    {
        // A verified deactivation with unknown originals is not a failed write.
        // Recheck its target; never re-enable source effects as a cleanup step.
        let operation = history::load_operation(reference, &store.index.binding)?;
        let mut journal = load(&operation.recovery)?;
        ensure!(
            journal.binding == store.index.binding
                && journal.transition_sha256.as_ref() == Some(&reference.sha256),
            "Incomplete target is not bound to its recovery journal"
        );
        let (session, mut afc) = open(&journal.binding)?;
        let indices: Vec<_> = journal
            .assets
            .iter()
            .enumerate()
            .filter_map(|(index, asset)| (asset.candidate_sha256 != sha256(b"")).then_some(index))
            .collect();
        verify_managed(
            &operation.recovery,
            &mut journal,
            &mut afc,
            &session,
            &indices,
        )?;
        journal.status = "ParallaxCandidateReadbackMatchedAwaitingManualObservation".into();
        save(&operation.recovery, &journal)?;
        return outcome(store);
    }
    if let Some(reference) = &reference
        && matches!(pending_phase, Some(Phase::Verified | Phase::Committed))
    {
        let operation = history::load_operation(reference, &store.index.binding)?;
        let journal = load(&operation.recovery)?;
        let candidate_verified = journal.status
            == "ParallaxCandidateReadbackMatchedAwaitingManualObservation"
            && (journal.kind != JournalKind::ManagedTransition
                || journal.transition_sha256.as_ref() == Some(&reference.sha256));
        let unchanged_verified = !journal.kind.is_installation()
            && installation_ready(&journal)
            && operation.changes.is_empty()
            && operation.source.metadata.sha256 == operation.target.metadata.sha256;
        ensure!(
            journal.binding == store.index.binding
                && journal.roots.is_empty()
                && (candidate_verified || unchanged_verified),
            "Verification record does not match the recovery journal"
        );
        store.commit(reference)?;
        return outcome(store);
    }
    if let Some(reference) = &reference
        && pending_phase == Some(Phase::Recovered)
    {
        let operation = history::load_operation(reference, &store.index.binding)?;
        let journal = raw_capture
            .as_ref()
            .context("Recovered transition has no recovery journal")?;
        ensure!(
            journal.binding == store.index.binding
                && journal.roots.is_empty()
                && !journal.incomplete_recovery()
                && journal.status == "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation"
                && store.index.head == operation.previous_head,
            "Recovered source does not match managed history"
        );
        let restored = snapshot(&directory.join("recovery"), journal)?;
        transition::reconcile(&restored, &operation.source)?;
        if store.index.expected.is_none() {
            store.index.expected = Some(restored);
        }
        store.abort_after_verified_recovery()?;
        return outcome(store);
    }
    let recovery = directory.join("recovery");
    if recovery.join("journal.json").exists() {
        progress("Restoring previous skin…");
        let mut journal = load(&recovery)?;
        ensure!(
            journal.binding == store.index.binding,
            "Pending recovery binding differs"
        );
        let (session, mut afc) = open(&journal.binding)?;
        if journal.kind == JournalKind::ManagedTransition && !afc.list("/")?.contains(&journal.held)
        {
            // A partial write may contain either candidate or source bytes.
            // Retaining this relocation first checks those hashes and preserves
            // conflicting held bytes before any original is overwritten.
            let indices: Vec<_> = journal
                .assets
                .iter()
                .enumerate()
                .filter_map(|(index, asset)| asset.original_sha256.is_some().then_some(index))
                .collect();
            let recovered =
                capture_and_return(&recovery, &mut journal, &mut afc, &session, &indices)?;
            require_roundtrip(&recovered, &indices)?;
        }
        restore_and_verify_originals(&recovery, &mut journal, &mut afc, &session)?;
        journal.status = "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation".into();
        save(&recovery, &journal)?;
        if journal.incomplete_recovery() {
            if let Some(reference) = &reference {
                history::set_phase(reference, Phase::RecoveryIncomplete)?;
            }
            return outcome(store);
        }
        if let Some(reference) = &reference {
            history::set_phase(reference, Phase::Recovered)?;
        }
        if store.index.expected.is_none() && has_captured_metadata(&journal) {
            store.index.expected = Some(snapshot(&recovery, &journal)?);
        }
    }
    store.abort_after_verified_recovery()?;
    let mut status = outcome(store)?;
    if !recovery.join("journal.json").exists() {
        status.verified = false;
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::configuration::{Inputs, Selection};
    use history::tests::Fixture;

    #[test]
    fn managed_readbacks_keep_artwork_separate_and_end_with_two_metadata_checks() {
        assert_eq!(
            verification_groups(&[0, 1, 2, 3, 4, 22]),
            [vec![0, 1, 2, 4, 22], vec![3], vec![3]]
        );
        assert_eq!(verification_groups(&[3]), [vec![3], vec![3]]);
    }

    #[test]
    fn managed_journal_requires_only_written_preimages_and_keeps_trial_scopes_narrow() {
        let fixture = Fixture::new();
        let directory = fixture.directory();
        let mut journal = super::super::tests::recovery_capture_fixture();
        journal.kind = JournalKind::ManagedTransition;
        journal.binding = fixture.binding.clone();
        journal.transition_sha256 = Some(sha256(b"managed-manifest"));
        journal.assets = inventory(&journal.kind)
            .into_iter()
            .map(|leaf| Asset {
                original_sha256: (leaf == "pass.json").then(|| sha256(b"source")),
                candidate_sha256: if leaf == "pass.json" {
                    sha256(b"target")
                } else {
                    sha256(b"")
                },
                leaf,
            })
            .collect();
        save(&directory, &journal).unwrap();
        assert!(
            load(&directory).is_ok(),
            "Untouched Unknown slots are not overwritten preimages"
        );
        let neutral = journal
            .assets
            .iter()
            .position(|asset| asset.leaf == "neutralEmitter.png")
            .unwrap();
        journal.assets[neutral].candidate_sha256 = sha256(b"new sprite");
        save(&directory, &journal).unwrap();
        assert!(load(&directory).is_err());
        journal.allow_missing_particle_originals = true;
        save(&directory, &journal).unwrap();
        assert!(load(&directory).unwrap().incomplete_recovery());
        journal.allow_missing_transaction_originals = true;
        save(&directory, &journal).unwrap();
        assert!(
            load(&directory).is_err(),
            "Policies cannot be combined through a saved journal"
        );
        journal.allow_missing_transaction_originals = false;
        let transaction = journal
            .assets
            .iter()
            .position(|asset| asset.leaf == "transactionEffectShape.svg")
            .unwrap();
        journal.assets[transaction].candidate_sha256 = sha256(b"new shape");
        save(&directory, &journal).unwrap();
        assert!(
            load(&directory).is_err(),
            "Particle opt-in cannot cover transaction writes"
        );
    }

    #[test]
    fn frozen_inputs_ignore_disabled_paths_and_reject_invalid_enabled_inputs_before_capture() {
        let fixture = Fixture::new();
        let directory = fixture.directory();
        let mut desired = DesiredEffects {
            selection: Selection::default(),
            inputs: Inputs {
                foreground: Some("missing.png".into()),
                transaction_shape: Some("missing.svg".into()),
                ..Default::default()
            },
            static_foreground: false,
        };
        let frozen = frozen_inputs(&desired, &directory).unwrap();
        assert!(frozen.inputs.foreground.is_none() && frozen.inputs.transaction_shape.is_none());
        desired.selection.parallax = true;
        assert!(frozen_inputs(&desired, &directory).is_err());
        assert!(!directory.join("recovery").exists());
    }

    #[test]
    fn removal_honors_selected_background_crop_and_freezes_it_without_other_draft_inputs() {
        let fixture = Fixture::new();
        let input = fixture.root.join("background.png");
        let image = image::RgbaImage::from_fn(16, 64, |_, y| {
            image::Rgba(if y < 32 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            })
        });
        image.save(&input).unwrap();
        for (focus_y, pixel) in [(0.0, [255, 0, 0, 255]), (1.0, [0, 0, 255, 255])] {
            let directory = fixture.directory();
            let mut alignment = artwork::Alignment::default();
            alignment.focus_y = focus_y;
            let assets = prepare_reset_background(Some(input.clone()), &alignment, &directory)
                .unwrap()
                .unwrap();
            let image =
                image::load_from_memory(&assets["cardBackgroundCombined@3x.png"].read().unwrap())
                    .unwrap()
                    .to_rgba8();
            assert!(image.pixels().all(|p| p.0 == pixel));
            assert!(directory.join("input-background").is_file());
            assert!(!directory.join("recovery").exists());
        }
        let directory = fixture.directory();
        assert!(
            prepare_reset_background(None, &Default::default(), &directory)
                .unwrap()
                .is_none()
        );
        assert!(
            prepare_reset_background(
                Some(fixture.root.join("missing.png")),
                &Default::default(),
                &directory
            )
            .is_err()
        );
        assert!(!directory.join("recovery").exists());
    }

    #[test]
    fn removal_without_selection_uses_background_instead_of_flattened_fallback() {
        let fixture = Fixture::new();
        let input = fixture.root.join("captured-background.png");
        image::RgbaImage::from_pixel(1536, 969, image::Rgba([10, 120, 30, 255]))
            .save(&input)
            .unwrap();
        let bytes = fs::read(input).unwrap();
        let mut source = fixture.snapshot(1, "current");
        source.resources.insert(
            "backgroundParallax@3x.png".into(),
            Some(fixture.blob(&bytes)),
        );
        // The fixture fallback is deliberately not an image. Any use of the
        // flattened fallback would fail rather than silently retaining an overlay.
        let directory = fixture.directory();
        let assets =
            prepare_artwork(Default::default(), &Default::default(), &source, &directory).unwrap();
        let output =
            image::load_from_memory(&assets["cardBackgroundCombined@3x.png"].read().unwrap())
                .unwrap()
                .to_rgba8();
        assert!(output.pixels().all(|p| p.0 == [10, 120, 30, 255]));
        source
            .resources
            .insert("backgroundParallax@3x.png".into(), None);
        let directory = fixture.directory();
        assert!(
            prepare_artwork(Default::default(), &Default::default(), &source, &directory).is_err()
        );
        // A card with only the 2x filename is usable when its recovered geometry
        // is understood. Preserve the existing size/bleed guard for other sizes.
        source.resources.insert(
            "backgroundParallax@2x.png".into(),
            Some(fixture.blob(&bytes)),
        );
        let directory = fixture.directory();
        let assets =
            prepare_artwork(Default::default(), &Default::default(), &source, &directory).unwrap();
        let output =
            image::load_from_memory(&assets["cardBackgroundCombined@3x.png"].read().unwrap())
                .unwrap()
                .to_rgba8();
        assert!(output.pixels().all(|p| p.0 == [10, 120, 30, 255]));
    }

    #[test]
    fn preserving_partial_ordinary_artwork_keeps_exact_bytes_including_pdf_only_cards() {
        let fixture = Fixture::new();
        for retained in crate::wallet_backup::CARD_ARTWORK_ASSETS {
            let mut source = fixture.snapshot(0, "current");
            for leaf in crate::wallet_backup::CARD_ARTWORK_ASSETS {
                if leaf != retained {
                    source.resources.insert(leaf.to_string(), None);
                }
            }
            let directory = fixture.directory();
            let assets =
                prepare_artwork(Default::default(), &Default::default(), &source, &directory)
                    .unwrap();
            assert_eq!(assets.len(), 1);
            assert_eq!(
                assets[retained],
                *source.resources[retained].as_ref().unwrap()
            );
            let plan = transition::reset(&source, None, None, &assets).unwrap();
            assert!(plan.is_noop(&source));
        }
    }

    #[test]
    fn compatibility_report_records_preflight_and_unknowns_without_claiming_playback_or_overwriting()
     {
        let fixture = Fixture::new();
        let mut source = fixture.snapshot(0, "source");
        source
            .resources
            .insert("foregroundParallax@3x.png".into(), None);
        let directory = fixture.directory();
        compatibility_report(&directory, &source, None, Some("no recovered foreground")).unwrap();
        let bytes = fs::read(directory.join("compatibility.json")).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["build"], source.binding.build);
        assert_eq!(report["native_playback_verified"], false);
        assert_eq!(report["preflight_error"], "no recovered foreground");
        assert!(
            report["unknown_resources"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("foregroundParallax@3x.png"))
        );
        assert!(compatibility_report(&directory, &source, None, None).is_err());
        assert_eq!(
            fs::read(directory.join("compatibility.json")).unwrap(),
            bytes
        );
    }

    #[test]
    fn torn_local_manifest_after_verified_capture_clears_without_native_recovery() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let recovery = directory.join("recovery");
        fs::create_dir(&recovery).unwrap();
        let mut journal = super::super::tests::recovery_capture_fixture();
        journal.binding = fixture.binding.clone();
        for (index, asset) in journal.assets.iter_mut().enumerate() {
            if asset.original_sha256.is_some() {
                let bytes = if index == 3 {
                    br#"{"serialNumber":"captured"}"#.as_slice()
                } else {
                    b"captured original"
                };
                fs::write(recovery.join(format!("original-{index}.bin")), bytes).unwrap();
                asset.original_sha256 = Some(sha256(bytes));
            }
        }
        save(&recovery, &journal).unwrap();
        let previous = fs::read(recovery.join("journal.json")).unwrap();
        fs::write(directory.join("transition.json"), b"{partial").unwrap();
        let result = recover(&mut store, |_| {
            panic!("No native recovery is needed after verified capture")
        })
        .unwrap();
        assert!(!result.pending && !result.verified);
        assert!(store.index.head.is_none() && store.index.expected.is_some());
        assert_eq!(fs::read(recovery.join("journal.json")).unwrap(), previous);
    }
}
