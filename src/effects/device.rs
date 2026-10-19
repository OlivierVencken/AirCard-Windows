//! Parallax capture, installation, readback and recovery
//! Retain originals and use ordinary Apply cache invalidation
//! AirTraffic acknowledgement confirms delivery only

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::artwork::{self, sha256};
#[cfg(test)]
use super::recovery::leaves;
use super::recovery::{
    Asset, Binding, DeviceLock, Journal, JournalKind, inventory, load, regular_read, save,
};
use super::transport::{self as media, MediaIo};
use crate::afc::AfcClient;
use crate::airlift;
use crate::device::{ActiveDeviceSession, ConnectionMode, DeviceTransport};

mod managed_device;
pub(crate) use managed_device::{
    ManagedOutcome, managed_apply, managed_reset, managed_restore, managed_status,
};

#[cfg(test)]
pub(super) fn qa_completed_recovery(directory: &Path, binding: &Binding) -> Result<()> {
    fs::create_dir_all(directory)?;
    let mut journal = tests::recovery_capture_fixture();
    journal.binding = binding.clone();
    journal.kind = JournalKind::MetadataParallax;
    journal.trial_pack_sha256 = Some(sha256(b"illustrative QA pack"));
    journal.status = "ParallaxCandidateReadbackMatchedAwaitingManualObservation".into();
    save(directory, &journal)
}

pub(crate) fn experimental_cross_dissolve_enabled() -> bool {
    std::env::var("AIRCARD_EXPERIMENTAL_CROSS_DISSOLVE_NO_ALTERNATE_BACKUP").as_deref() == Ok("1")
}

pub(crate) fn experimental_transaction_enabled() -> bool {
    std::env::var("AIRCARD_EXPERIMENTAL_TRANSACTION_NO_ASSET_BACKUP").as_deref() == Ok("1")
}

pub(crate) fn experimental_particles_enabled() -> bool {
    std::env::var("AIRCARD_EXPERIMENTAL_PARTICLES_NO_EMITTER_BACKUP").as_deref() == Ok("1")
}

fn random_token() -> Result<String> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut std::ffi::c_void,
            buffer: *mut u8,
            length: u32,
            flags: u32,
        ) -> i32;
    }
    let mut bytes = [0u8; 10];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            2,
        )
    };
    ensure!(status >= 0, "Native random token generation failed");
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn property(session: &ActiveDeviceSession, key: &str) -> Result<String> {
    let key = session.libs.create_cf_string(key)?;
    let raw =
        unsafe { (session.libs.am_device_copy_value)(session.device, std::ptr::null(), key.raw) };
    ensure!(!raw.is_null(), "Bound device property unavailable");
    let value = session.libs.to_rust_string(raw);
    unsafe {
        (session.libs.cf_release)(raw);
    }
    ensure!(!value.is_empty(), "Bound device property empty");
    Ok(value)
}

fn open(binding: &Binding) -> Result<(ActiveDeviceSession, AfcClient)> {
    let devices = crate::device::list_connected_devices()?;
    ensure!(
        devices.len() == 1
            && devices[0].udid == binding.udid
            && devices[0].transports == [DeviceTransport::Usb],
        "Bound USB-only phone unavailable"
    );
    let session = ActiveDeviceSession::open(Some(&binding.udid), ConnectionMode::Usb)?;
    ensure!(
        property(&session, "UniqueDeviceID")? == binding.udid
            && property(&session, "BuildVersion")? == binding.build
            && property(&session, "ProductType")? == binding.product_type,
        "Phone identity/build changed"
    );
    let afc = AfcClient::new_for_effects(&session)?;
    Ok((session, afc))
}

fn sync(
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    pairs: &[(String, String)],
) -> Result<()> {
    let identifiers: Vec<_> = pairs.iter().map(|(source, _)| source.clone()).collect();
    let plist = airlift::build_books_plist(&identifiers)?;
    afc.make_directory_recursive("Books/Sync")?;
    afc.write("Books/Sync/Books.plist", &plist)?;
    ensure!(
        afc.read("Books/Sync/Books.plist", media::MAX_FILE_BYTES)? == plist,
        "Books staging readback mismatch"
    );
    let refs: Vec<_> = pairs
        .iter()
        .map(|(s, d)| (s.as_str(), d.as_str()))
        .collect();
    crate::airtraffic::sync_assets_serialized(&session.udid, session.transport, &refs, |_| {})
}

fn conduit(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    items: &[(&str, &[u8])],
) -> Result<(String, String)> {
    let target = format!(
        "/var/mobile/Library/Passes/Cards/{}{}",
        journal.binding.card_id,
        journal.kind.bundle_extension()
    );
    conduit_to(dir, journal, afc, session, &target, items)
}

fn conduit_to(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    target: &str,
    items: &[(&str, &[u8])],
) -> Result<(String, String)> {
    let token = random_token()?;
    let source = format!("{}{token}", airlift::SOURCE_PREFIX);
    let link = format!("{}{token}", airlift::LINK_PREFIX);
    let entries = afc.list("/")?;
    ensure!(
        !entries.contains(&source)
            && !entries.contains(&link)
            && !entries.contains(&format!("{}{token}", airlift::RECOVERED_PREFIX)),
        "Staging token collision"
    );
    // Persist cleanup ownership before staging or Books changes
    journal.roots.extend([
        source.clone(),
        link.clone(),
        format!("{}{token}", airlift::RECOVERED_PREFIX),
    ]);
    save(dir, journal)?;
    // Ordinary Apply uses the singular payload object for one-file writes
    let archive = if items.len() == 1 {
        airlift::build_streaming_zip_archive(target, items[0].1)?
    } else {
        airlift::build_streaming_zip_archive_multi(target, items)?
    };
    airlift::stage_streaming_zip(session, &source, &archive)?;
    ensure!(
        afc.list(&source)?.contains(&"p0".to_string()),
        "Staging conduit missing"
    );
    for (index, (_, bytes)) in items.iter().enumerate() {
        let payload = if items.len() == 1 {
            "payload".into()
        } else {
            format!("payload_{index}")
        };
        ensure!(
            afc.read(&format!("{source}/{payload}"), media::MAX_FILE_BYTES)? == *bytes,
            "Staged payload readback mismatch"
        );
    }
    if !journal.combined_sync {
        sync(
            afc,
            session,
            &[(format!("../../{source}/p0/p1/p2/link"), link.clone())],
        )?;
    }
    Ok((source, link))
}

fn write_pairs(
    source: &str,
    link: &str,
    leaves: &[String],
    combined: bool,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    if combined {
        pairs.push((format!("../../{source}/p0/p1/p2/link"), link.into()));
    }
    pairs.extend(leaves.iter().enumerate().map(|(i, leaf)| {
        (
            if leaves.len() == 1 {
                format!("../../{source}/payload")
            } else {
                format!("../../{source}/payload_{i}")
            },
            format!("{link}/{leaf}"),
        )
    }));
    pairs
}

fn cleanup(dir: &Path, journal: &mut Journal, afc: &mut AfcClient) -> Result<()> {
    let mut errors = Vec::new();
    for root in &journal.roots {
        if let Err(e) = media::cleanup_staging(afc, root) {
            errors.push(format!("{root}: {e:#}"));
        }
    }
    if let Err(e) = media::restore_books(afc, &journal.books) {
        errors.push(format!("Books: {e:#}"));
    }
    ensure!(
        errors.is_empty(),
        "Overlay cleanup incomplete: {}",
        errors.join("; ")
    );
    journal.roots.clear();
    save(dir, journal)
}

fn invalidate_caches(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    if !journal.invalidate_caches {
        return Ok(());
    }
    let names: Vec<String> = crate::flasher::CACHE_FILES
        .iter()
        .map(|s| (*s).into())
        .collect();
    let items: Vec<_> = names
        .iter()
        .map(|leaf| (leaf.as_str(), crate::flasher::CACHE_INVALIDATION_PAYLOAD))
        .collect();
    for ext in crate::flasher::CACHE_EXTENSIONS {
        journal.status = format!("CacheInvalidationIntent{ext}");
        save(dir, journal)?;
        let target = format!(
            "/var/mobile/Library/Passes/Cards/{}{ext}",
            journal.binding.card_id
        );
        let transfer = (|| -> Result<()> {
            let (source, link) = conduit_to(dir, journal, afc, session, &target, &items)?;
            sync(afc, session, &write_pairs(&source, &link, &names, true))
        })();
        media::finish_with_cleanup(transfer, cleanup(dir, journal, afc))?;
    }
    Ok(())
}

fn recover_held(dir: &Path, journal: &mut Journal, afc: &mut AfcClient) -> Result<()> {
    // On restart, capture any file moved before its host snapshot was persisted
    let root_entries = afc.list("/")?;
    if !root_entries.contains(&journal.held) {
        return Ok(());
    }
    for entry in afc.list(&journal.held)? {
        let index: usize = entry
            .strip_suffix(".bin")
            .context("Unexpected held file")?
            .parse()?;
        ensure!(
            index < journal.assets.len(),
            "Held index outside recovery inventory"
        );
        let bytes = afc.read(
            &format!("{}/{}", journal.held, entry),
            media::MAX_FILE_BYTES,
        )?;
        retain_held_bytes(dir, journal, index, &bytes)?;
    }
    Ok(())
}

fn retain_held_bytes(dir: &Path, journal: &mut Journal, index: usize, bytes: &[u8]) -> Result<()> {
    let hash = sha256(bytes);
    if index == 3
        && journal.kind.is_installation()
        && journal.observed_metadata_sha256.as_ref() == Some(&hash)
    {
        // Keep changed post-boot JSON separate from immutable originals
        let snapshot = dir.join(format!("current-metadata-{hash}.json"));
        ensure!(
            regular_read(&snapshot, super::metadata::MAX_JSON_BYTES)? == bytes,
            "Observed metadata snapshot differs"
        );
        return Ok(());
    }
    if journal.kind.is_installation()
        && hash == journal.assets[index].candidate_sha256
        && journal.assets[index].original_sha256.as_ref() != Some(&hash)
    {
        // Trial readback must never replace the immutable original snapshot
        return write_new_or_same(&dir.join(format!("observed-candidate-{index}.bin")), bytes);
    }
    ensure!(
        !(journal.kind.is_installation()
            && journal.assets[index].original_sha256.is_none()
            && journal.allows_missing_original(&journal.assets[index])),
        "Unknown-original trial file {} differs from the journaled candidate; retained for investigation",
        journal.assets[index].leaf
    );
    if let Some(expected) = &journal.assets[index].original_sha256 {
        ensure!(hash == *expected, "Held original changed");
    }
    write_new_or_same(&dir.join(format!("original-{index}.bin")), bytes)?;
    journal.assets[index].original_sha256 = Some(hash);
    save(dir, journal)
}

fn write_new_or_same(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        ensure!(
            regular_read(path, media::MAX_FILE_BYTES)? == bytes,
            "Original snapshot conflict"
        );
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn restore_originals(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    restore_selected_originals(dir, journal, afc, session, None)
}

fn restore_and_verify_originals(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    // Keep this outside the low-level restoration used by capture error handling:
    // a failed verification must not recursively begin another verification.
    restore_originals(dir, journal, afc, session)?;
    if !journal.combined_sync {
        // Historical artwork-only journals use acknowledgement-only restoration.
        // Preserve their transfer policy; protected readback requires combined sync.
        // Metadata journals require combined sync when loaded and always verify.
        return Ok(());
    }
    if has_captured_metadata(journal) {
        verify_captured_originals(dir, journal, afc, session)
    } else {
        // Capture may have failed before metadata was recovered. Verify only the
        // artwork that is available, without inventing a metadata preimage.
        verify_original_artwork(dir, journal, afc, session)?;
        invalidate_caches(dir, journal, afc, session)
    }
}

fn has_captured_metadata(journal: &Journal) -> bool {
    journal
        .assets
        .get(3)
        .is_some_and(|asset| asset.leaf == "pass.json" && asset.original_sha256.is_some())
}

fn original_payloads(
    dir: &Path,
    journal: &Journal,
    selected: Option<&[usize]>,
) -> Result<Vec<(String, Vec<u8>)>> {
    if let Some(indices) = selected {
        ensure!(
            indices.iter().all(|i| *i < journal.assets.len()),
            "Invalid original slot"
        );
    }
    let mut payloads = Vec::new();
    for (index, asset) in journal.assets.iter().enumerate() {
        if let Some(hash) = &asset.original_sha256 {
            let bytes = regular_read(
                &dir.join(format!("original-{index}.bin")),
                media::MAX_FILE_BYTES,
            )?;
            ensure!(sha256(&bytes) == *hash, "Original checksum mismatch");
            // Validate every immutable original, even when returning a smaller batch.
            if selected.is_none_or(|indices| indices.contains(&index)) {
                payloads.push((asset.leaf.clone(), bytes));
            }
        }
    }
    Ok(payloads)
}

fn restore_selected_originals(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    selected: Option<&[usize]>,
) -> Result<()> {
    recover_held(dir, journal, afc)?;
    let payloads = original_payloads(dir, journal, selected)?;
    // After validating all retained bytes, restore metadata before artwork
    let metadata: Vec<_> = payloads
        .iter()
        .filter(|(leaf, _)| leaf == "pass.json")
        .collect();
    let artwork: Vec<_> = payloads
        .iter()
        .filter(|(leaf, _)| leaf != "pass.json")
        .collect();
    for group in [metadata, artwork] {
        if group.is_empty() {
            continue;
        }
        let items: Vec<_> = group
            .iter()
            .map(|(leaf, bytes)| (leaf.as_str(), bytes.as_slice()))
            .collect();
        let (source, link) = conduit(dir, journal, afc, session, &items)?;
        let names: Vec<_> = group.iter().map(|(leaf, _)| leaf.clone()).collect();
        let pairs = write_pairs(&source, &link, &names, journal.combined_sync);
        sync(afc, session, &pairs)?;
    }
    media::cleanup_staging(afc, &journal.held)?;
    cleanup(dir, journal, afc)?;
    // Successful verification returns the same bytes it moved. Defer cache work
    // to the artwork phase boundary. Full error/manual recovery still invalidates.
    if selected.is_none() {
        invalidate_caches(dir, journal, afc, session)?;
    }
    Ok(())
}

fn capture_pairs(
    source: &str,
    link: &str,
    held: &str,
    indices: &[(usize, String)],
) -> Vec<(String, String)> {
    std::iter::once((format!("../../{source}/p0/p1/p2/link"), link.into()))
        .chain(indices.iter().map(|(index, leaf)| {
            (
                format!("../../{link}/{leaf}"),
                format!("{held}/{index}.bin"),
            )
        }))
        .collect()
}

fn capture_and_return(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    indices: &[usize],
) -> Result<Vec<usize>> {
    capture_and_return_inner(dir, journal, afc, session, indices, false)
}

fn capture_and_return_inner(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    indices: &[usize],
    metadata_only: bool,
) -> Result<Vec<usize>> {
    ensure!(
        !metadata_only || indices == [3],
        "Metadata-only capture requires slot 3"
    );
    ensure!(journal.combined_sync, "Combined capture required");
    ensure!(
        !afc.list("/")?.contains(&journal.held),
        "Held staging token collision"
    );
    // Require the previous held root to be absent before using a new destination
    let held = format!("{}{}", airlift::RECOVERED_PREFIX, random_token()?);
    ensure!(
        !afc.list("/")?.contains(&held),
        "New held staging token collision"
    );
    journal.held = held;
    journal.status = "ProtectedCaptureIntent".into();
    save(dir, journal)?;
    let capture = (|| -> Result<Vec<usize>> {
        afc.mkdir(&journal.held)?;
        let names: Vec<_> = indices
            .iter()
            .map(|i| (*i, journal.assets[*i].leaf.clone()))
            .collect();
        let (source, link) = conduit(
            dir,
            journal,
            afc,
            session,
            &[("payload", b"metadata-capture")],
        )?;
        sync(
            afc,
            session,
            &capture_pairs(&source, &link, &journal.held, &names),
        )?;
        recover_held(dir, journal, afc)?;
        let recovered = recovered_slots(&afc.list(&journal.held)?, indices)?;
        for index in &recovered {
            let bytes = afc.read(
                &format!("{}/{index}.bin", journal.held),
                media::MAX_FILE_BYTES,
            )?;
            require_original_bytes(&journal.assets[*index], &bytes)?;
        }
        journal.last_capture_indices = recovered.clone();
        save(dir, journal)?;
        Ok(recovered)
    })();
    let restoration = if metadata_only {
        restore_metadata_only(dir, journal, afc, session)
    } else if let Ok(recovered) = &capture {
        // A successful capture moved only these slots. Return those exact originals;
        // unrelated protected files are still in place. Keep full restoration for
        // errors/restarts where the extent of the mutation may be uncertain.
        restore_selected_originals(dir, journal, afc, session, Some(recovered))
    } else {
        restore_originals(dir, journal, afc, session)
    };
    media::finish_with_cleanup(capture, restoration)
}

fn restore_metadata_only(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    if afc.list("/")?.contains(&journal.held) {
        recovered_slots(&afc.list(&journal.held)?, &[3])?;
    }
    recover_held(dir, journal, afc)?;
    let bytes = regular_read(&dir.join("original-3.bin"), super::metadata::MAX_JSON_BYTES)?;
    require_original_bytes(&journal.assets[3], &bytes)?;
    journal.status = "OriginalMetadataOnlyWriteIntent".into();
    save(dir, journal)?;
    let (source, link) = conduit(dir, journal, afc, session, &[("pass.json", &bytes)])?;
    sync(
        afc,
        session,
        &write_pairs(&source, &link, &["pass.json".into()], true),
    )?;
    media::cleanup_staging(afc, &journal.held)?;
    cleanup(dir, journal, afc)
}

fn verify_original_metadata_roundtrips(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    restore_metadata_only(dir, journal, afc, session)?;
    for _ in 1..=2 {
        let recovered = capture_and_return_inner(dir, journal, afc, session, &[3], true)?;
        if let Err(error) = require_roundtrip(&recovered, &[3]) {
            journal.status = "MetadataOnlyReadbackIncompleteOriginalReapplied".into();
            save(dir, journal)?;
            return Err(error);
        }
    }
    Ok(())
}

fn recovered_slots(entries: &[String], expected: &[usize]) -> Result<Vec<usize>> {
    let mut recovered = Vec::new();
    for entry in entries {
        let index: usize = entry
            .strip_suffix(".bin")
            .context("Unexpected capture slot")?
            .parse()?;
        ensure!(
            expected.contains(&index) && !recovered.contains(&index),
            "Unexpected or duplicate capture slot"
        );
        recovered.push(index);
    }
    recovered.sort_unstable();
    Ok(recovered)
}

fn require_roundtrip(recovered: &[usize], expected: &[usize]) -> Result<()> {
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    ensure!(
        recovered == expected,
        "Incomplete fresh export after writeback; missing protected slots remain Unknown"
    );
    Ok(())
}

fn require_original_bytes(asset: &Asset, bytes: &[u8]) -> Result<()> {
    ensure!(
        asset.original_sha256.as_deref() == Some(&sha256(bytes)),
        "Original restoration readback differs for {}",
        asset.leaf
    );
    Ok(())
}

fn capture_metadata_for_binding(dir: &Path, binding: Binding, kind: JournalKind) -> Result<()> {
    ensure!(
        matches!(
            kind.without_particles(),
            JournalKind::MetadataCapture
                | JournalKind::MetadataCrossDissolveCapture
                | JournalKind::MetadataTransactionCapture
                | JournalKind::MetadataCrossDissolveTransactionCapture
        ),
        "Invalid capture kind"
    );
    binding.validate()?;
    ensure!(
        crate::scanner::is_valid_card_hash(&binding.card_id),
        "Select a valid Wallet card"
    );
    let (session, mut afc) = open(&binding)?;
    fs::create_dir(dir).context("Capture directory must be new")?;
    let mut journal = Journal {
        schema_version: 1,
        kind,
        combined_sync: true,
        invalidate_caches: true,
        last_capture_indices: Vec::new(),
        trial_pack_sha256: None,
        observed_metadata_sha256: None,
        binding,
        status: "CapturePrepared".into(),
        books: media::snapshot_books(&mut afc)?,
        roots: Vec::new(),
        held: format!("{}{}", airlift::RECOVERED_PREFIX, random_token()?),
        assets: inventory(&kind)
            .into_iter()
            .map(|leaf| Asset {
                leaf,
                original_sha256: None,
                candidate_sha256: sha256(b""),
            })
            .collect(),
        protected_readback_verified: false,
        allow_missing_cross_dissolve_originals: false,
        allow_missing_transaction_originals: false,
        allow_missing_particle_originals: false,
    };
    save(dir, &journal)?;
    capture_and_return(dir, &mut journal, &mut afc, &session, &[0, 1, 2])?;
    if journal.assets[..3]
        .iter()
        .any(|asset| asset.original_sha256.is_none())
    {
        journal.status = "OrdinaryExportUnprovenMetadataNotTargeted".into();
        save(dir, &journal)?;
        anyhow::bail!(
            "Ordinary export did not recover all three known artwork files; metadata was not targeted. Recovery directory: {}",
            dir.display()
        );
    }
    let recovered = capture_and_return(dir, &mut journal, &mut afc, &session, &[0, 1, 2])?;
    require_roundtrip(&recovered, &[0, 1, 2])?;
    let indices: Vec<_> = (3..journal.assets.len()).collect();
    capture_and_return(dir, &mut journal, &mut afc, &session, &indices)?;
    if journal.assets[3].original_sha256.is_none() {
        journal.status = "MetadataNotRecovered".into();
        save(dir, &journal)?;
        anyhow::bail!(
            "No pass.json recovered; missing slots remain Unknown. Metadata was not edited. Recovery directory: {}",
            dir.display()
        );
    }
    retain_metadata(dir, &journal)?;
    // Verify restored bytes before cleanup; retain Media originals on conflict
    verify_captured_originals(dir, &mut journal, &mut afc, &session)?;
    finish_metadata_capture(dir, &mut journal)
}

fn finish_metadata_capture(dir: &Path, journal: &mut Journal) -> Result<()> {
    let bytes = regular_read(&dir.join("original-3.bin"), super::metadata::MAX_JSON_BYTES)?;
    let value = super::metadata::parse_unique(&bytes)?;
    ensure!(
        value.as_object().is_some_and(|o| !o.is_empty()),
        "Captured pass.json is not a nonempty object; exact bytes retained and reapplied"
    );
    write_new_or_same(&dir.join("original-pass.json"), &bytes)?;
    journal.status = "MetadataCapturedAndReappliedAwaitingVisualConfirmation".into();
    save(dir, journal)?;
    Ok(())
}

fn retain_metadata(dir: &Path, journal: &Journal) -> Result<()> {
    let bytes = regular_read(&dir.join("original-3.bin"), super::metadata::MAX_JSON_BYTES)?;
    ensure!(
        journal.assets[3].original_sha256.as_deref() == Some(&sha256(&bytes)),
        "Retained metadata checksum mismatch"
    );
    let value = super::metadata::parse_unique(&bytes)?;
    ensure!(
        value.as_object().is_some_and(|o| !o.is_empty()),
        "Captured pass JSON must be a nonempty object"
    );
    write_new_or_same(&dir.join("captured-pass.json"), &bytes)
}

fn artwork_verification_groups(journal: &Journal) -> Vec<Vec<usize>> {
    let indices: Vec<_> = journal
        .assets
        .iter()
        .enumerate()
        .filter_map(|(i, a)| (a.leaf != "pass.json" && a.original_sha256.is_some()).then_some(i))
        .collect();
    if journal.kind.is_installation() {
        vec![indices]
    } else {
        indices.chunks(3).map(|group| group.to_vec()).collect()
    }
}

fn verify_captured_originals(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    verify_original_artwork(dir, journal, afc, session)?;
    // Finish with two metadata-only checks and original reapplication
    // Keep cache and artwork batches before this sequence
    invalidate_caches(dir, journal, afc, session)?;
    verify_original_metadata_roundtrips(dir, journal, afc, session)
}

fn verify_original_artwork(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
) -> Result<()> {
    // Verify artwork before metadata; combined reexports can miss pass.json
    let groups = artwork_verification_groups(journal);
    for group in groups {
        let recovered = capture_and_return(dir, journal, afc, session, &group)?;
        if let Err(error) = require_roundtrip(&recovered, &group) {
            journal.status = "CaptureRoundtripIncompleteOriginalsReapplied".into();
            save(dir, journal)?;
            return Err(error);
        }
    }
    Ok(())
}

fn candidate_payloads(
    dir: &Path,
    journal: &Journal,
    indices: &[usize],
) -> Result<Vec<(String, Vec<u8>)>> {
    indices
        .iter()
        .map(|index| {
            let asset = &journal.assets[*index];
            let bytes = regular_read(
                &dir.join(format!("candidate-{index}.bin")),
                media::MAX_FILE_BYTES,
            )?;
            ensure!(
                sha256(&bytes) == asset.candidate_sha256,
                "Candidate checksum mismatch"
            );
            Ok((asset.leaf.clone(), bytes))
        })
        .collect()
}

fn write_candidates(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    indices: &[usize],
) -> Result<()> {
    let payloads = candidate_payloads(dir, journal, indices)?;
    let items: Vec<_> = payloads
        .iter()
        .map(|(leaf, bytes)| (leaf.as_str(), bytes.as_slice()))
        .collect();
    journal.status = "ParallaxCandidateWriteIntent".into();
    save(dir, journal)?;
    let transfer = (|| -> Result<()> {
        let (source, link) = conduit(dir, journal, afc, session, &items)?;
        let names: Vec<_> = payloads.iter().map(|(leaf, _)| leaf.clone()).collect();
        sync(afc, session, &write_pairs(&source, &link, &names, true))
    })();
    media::finish_with_cleanup(transfer, cleanup(dir, journal, afc))
}

#[cfg(test)]
fn verify_candidates(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    indices: &[usize],
) -> Result<()> {
    verify_candidate_export(dir, journal, afc, session, indices)?;
    write_candidates(dir, journal, afc, session, &[3])?;
    let artwork: Vec<_> = indices
        .iter()
        .copied()
        .filter(|index| *index != 3)
        .collect();
    if !artwork.is_empty() {
        write_candidates(dir, journal, afc, session, &artwork)?;
    }
    media::cleanup_staging(afc, &journal.held)?;
    cleanup(dir, journal, afc)?;
    invalidate_caches(dir, journal, afc, session)?;
    // Finish with metadata-only verification and return
    // Keep cache and artwork batches before this step
    write_candidates(dir, journal, afc, session, &[3])?;
    for _ in 1..=2 {
        verify_candidate_export(dir, journal, afc, session, &[3])?;
        write_candidates(dir, journal, afc, session, &[3])?;
        media::cleanup_staging(afc, &journal.held)?;
        cleanup(dir, journal, afc)?;
    }
    Ok(())
}

fn verify_candidate_export(
    dir: &Path,
    journal: &mut Journal,
    afc: &mut AfcClient,
    session: &ActiveDeviceSession,
    indices: &[usize],
) -> Result<()> {
    ensure!(
        !afc.list("/")?.contains(&journal.held),
        "Pending held originals require restore"
    );
    let held = format!("{}{}", airlift::RECOVERED_PREFIX, random_token()?);
    ensure!(!afc.list("/")?.contains(&held), "Held token collision");
    journal.held = held;
    journal.status = "ParallaxCandidateReadbackIntent".into();
    save(dir, journal)?;
    afc.mkdir(&journal.held)?;
    let names: Vec<_> = indices
        .iter()
        .map(|index| (*index, journal.assets[*index].leaf.clone()))
        .collect();
    let (source, link) = conduit(
        dir,
        journal,
        afc,
        session,
        &[("payload", b"candidate-readback")],
    )?;
    sync(
        afc,
        session,
        &capture_pairs(&source, &link, &journal.held, &names),
    )?;
    let recovered = recovered_slots(&afc.list(&journal.held)?, indices)?;
    journal.last_capture_indices = recovered.clone();
    save(dir, journal)?;
    require_roundtrip(&recovered, indices)?;
    for index in indices {
        let bytes = afc.read(
            &format!("{}/{index}.bin", journal.held),
            media::MAX_FILE_BYTES,
        )?;
        if *index == 3 {
            // Preserve observed metadata even when boot replaced the candidate
            write_new_or_same(
                &dir.join(format!("current-metadata-{}.json", sha256(&bytes))),
                &bytes,
            )?;
            journal.observed_metadata_sha256 = Some(sha256(&bytes));
            save(dir, journal)?;
        }
        ensure!(
            sha256(&bytes) == journal.assets[*index].candidate_sha256,
            "Protected candidate readback differs for {}",
            journal.assets[*index].leaf
        );
        write_new_or_same(&dir.join(format!("observed-candidate-{index}.bin")), &bytes)?;
    }
    Ok(())
}

fn installation_ready(journal: &Journal) -> bool {
    journal.roots.is_empty()
        && match journal.kind.without_particles() {
            JournalKind::MetadataCapture
            | JournalKind::MetadataCrossDissolveCapture
            | JournalKind::MetadataTransactionCapture
            | JournalKind::MetadataCrossDissolveTransactionCapture => matches!(
                journal.status.as_str(),
                "MetadataCapturedAndReappliedAwaitingVisualConfirmation"
                    | "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation"
            ),
            JournalKind::MetadataParallax
            | JournalKind::MetadataCrossDissolve
            | JournalKind::MetadataTransaction
            | JournalKind::MetadataCrossDissolveTransaction => {
                journal.status
                    == "ParallaxOriginalsReadbackMatchedAndReappliedAwaitingVisualConfirmation"
            }
            _ => false,
        }
}

/// Purely local decision: a verified capture with no installation can keep the
/// prior UI recovery selection. Unfinished mutation/recovery must remain selected.
pub(super) fn no_installation_to_recover(directory: &Path) -> bool {
    directory.try_exists().is_ok_and(|exists| !exists)
        || load(directory)
            .is_ok_and(|journal| !journal.kind.is_installation() && installation_ready(&journal))
}

#[cfg(test)]
fn plan_overlay_installation(
    journal: &Journal,
    deployment: &artwork::PreparedDeployment,
    original: &[u8],
) -> Result<Vec<(usize, Vec<u8>)>> {
    let cross_dissolve = journal.kind.cross_dissolve();
    ensure!(
        cross_dissolve == deployment.contains_cross_dissolve
            && journal.kind.transaction() == deployment.contains_transaction
            && journal.kind.particles() == deployment.contains_emitter,
        "Pack does not match captured effect inventory"
    );
    ensure!(
        !journal.kind.is_installation()
            || journal.trial_pack_sha256.as_ref() == Some(&deployment.manifest_sha256),
        "A repeated installation must use the identical verified pack"
    );
    ensure!(
        deployment.original_metadata == original
            && journal.assets[3].original_sha256.as_deref() == Some(&sha256(original)),
        "Pack does not bind the exact captured original JSON"
    );
    let mut planned = vec![(3, deployment.candidate_metadata.clone())];
    for (index, asset) in journal.assets.iter().enumerate().skip(4) {
        if asset.leaf.ends_with(".pdf") {
            ensure!(
                asset.original_sha256.is_none(),
                "Captured dynamic PDFs are not supported for overlay installation"
            );
            continue;
        }
        ensure!(
            asset.original_sha256.is_some() || journal.allows_missing_original(asset),
            "Every overwritten dynamic resource requires a captured original: {}",
            asset.leaf
        );
        let base = asset.leaf.replace("@2x", "").replace("@3x", "");
        let resource = deployment
            .resources
            .iter()
            .find(|r| r.filename == base)
            .context("Pack lacks a dynamic resource")?;
        planned.push((index, resource.bytes.clone()));
    }
    ensure!(
        planned.len()
            == 10
                + if deployment.contains_emitter { 3 } else { 0 }
                + if cross_dissolve { 3 } else { 0 }
                + if deployment.contains_transaction {
                    4
                } else {
                    0
                },
        "Overlay candidate inventory does not match the selected effect"
    );
    Ok(planned)
}

#[cfg(test)]
fn record_trial_policy(
    dir: &Path,
    journal: &mut Journal,
    deployment: &artwork::PreparedDeployment,
    cross_opt_in: bool,
    transaction_opt_in: bool,
    particle_opt_in: bool,
) -> Result<()> {
    ensure!(
        journal.kind.transaction() == deployment.contains_transaction
            && journal.kind.cross_dissolve() == deployment.contains_cross_dissolve
            && journal.kind.particles() == deployment.contains_emitter,
        "Pack does not match captured effect inventory"
    );
    let particles = deployment.contains_emitter && particle_opt_in;
    let transaction = deployment.contains_transaction && transaction_opt_in;
    let cross_dissolve = deployment.contains_cross_dissolve
        && !deployment.contains_transaction
        && !deployment.contains_emitter
        && cross_opt_in;
    if particles || transaction || cross_dissolve {
        journal.allow_missing_particle_originals |= particles;
        journal.allow_missing_transaction_originals |= transaction;
        journal.allow_missing_cross_dissolve_originals |= cross_dissolve;
        // Persist all selected policies before installation preflight or a native session.
        save(dir, journal)?;
    }
    Ok(())
}

#[cfg(test)]
fn install_overlay(dir: &Path, pack: &Path, needs_recovery: &mut bool) -> Result<()> {
    let mut journal = load(dir)?;
    ensure!(
        installation_ready(&journal),
        "A complete verified metadata capture is required"
    );
    let deployment = artwork::load_deployment(pack)?;
    record_trial_policy(
        dir,
        &mut journal,
        &deployment,
        experimental_cross_dissolve_enabled(),
        experimental_transaction_enabled(),
        experimental_particles_enabled(),
    )?;
    let original = regular_read(
        &dir.join("original-pass.json"),
        super::metadata::MAX_JSON_BYTES,
    )?;
    let planned = plan_overlay_installation(&journal, &deployment, &original)?;
    let (session, mut afc) = open(&journal.binding)?;
    // Recheck preimages before mutation and restore them on any result
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
    for (index, bytes) in &planned {
        write_new_or_same(&dir.join(format!("candidate-{index}.bin")), bytes)?;
        journal.assets[*index].candidate_sha256 = sha256(bytes);
    }
    journal.kind = JournalKind::for_effects(
        deployment.contains_cross_dissolve,
        deployment.contains_transaction,
        true,
    )
    .with_particles(deployment.contains_emitter);
    journal.trial_pack_sha256 = Some(deployment.manifest_sha256);
    journal.status = "ParallaxInstallIntent".into();
    save(dir, &journal)?;
    let indices: Vec<_> = planned.iter().map(|(index, _)| *index).collect();
    let artwork = &indices[1..];
    let installation = (|| -> Result<()> {
        write_candidates(dir, &mut journal, &mut afc, &session, artwork)?;
        write_candidates(dir, &mut journal, &mut afc, &session, &[3])?;
        verify_candidates(dir, &mut journal, &mut afc, &session, &indices)
    })();
    if let Err(error) = installation {
        journal.status = "ParallaxInstallFailedRestoreRequired".into();
        save(dir, &journal)?;
        // The caller holds the device lock and performs one verified recovery.
        return Err(error);
    }
    journal.status = "ParallaxCandidateReadbackMatchedAwaitingManualObservation".into();
    save(dir, &journal)?;
    Ok(())
}

/// Allocate a durable local destination before the UI starts any device work
pub(crate) fn new_application_directory() -> Result<PathBuf> {
    let parent =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?)
            .join("AirCard")
            .join("effects-applications");
    fs::create_dir_all(&parent)?;
    let directory = parent.join(random_token()?);
    fs::create_dir(&directory)?;
    Ok(directory)
}

/// Hold the device lock through capture, install, verification and cleanup
/// Persist recovery before every device mutation
#[cfg(test)]
#[allow(dead_code)] // Retain the old pack-based path for historical trial checks.
pub(crate) fn apply(
    binding: Binding,
    mut config: artwork::PrepareConfig,
    directory: &Path,
    progress: impl Fn(&str),
) -> Result<bool> {
    binding.validate()?;
    ensure!(
        binding.transport == DeviceTransport::Usb,
        "Effects currently require USB"
    );
    let capture = directory.join("recovery");
    progress("Preparing selected images…");
    // Freeze artwork before device work using the same processing as preview
    config.output_directory = directory.join("artwork");
    let (frozen_artwork, frozen_layers) = artwork::prepare_artwork(&config, Path::new("."))?;
    config.background = frozen_artwork.join("backgroundParallax.png");
    config.foreground = frozen_artwork.join("foregroundParallax.png");
    if config.emitter_sprite.is_some() {
        config.emitter_sprite = Some(frozen_artwork.join("neutralEmitter.png"));
    }
    if config.alternate_background.is_some() {
        config.alternate_background =
            Some(frozen_artwork.join("backgroundParallaxCrossDissolve.png"));
    }
    if config.transaction_sprite.is_some() {
        config.transaction_sprite = Some(frozen_artwork.join("transactionEffectEmitter.png"));
        config.transaction_shape = Some(frozen_artwork.join("transactionEffectShape.svg"));
    }
    config.original_pass_json = capture.join("original-pass.json");
    config.output_directory = directory.join("pack");
    let _lock = DeviceLock::acquire(&binding)?;
    let mut needs_recovery = false;
    let operation = (|| -> Result<()> {
        progress("Backing up current card metadata and artwork…");
        needs_recovery = true;
        let kind = JournalKind::for_effects(
            config.dynamic_layer_configuration.cross_dissolve.is_some(),
            config
                .dynamic_layer_configuration
                .transaction_effect
                .is_some(),
            false,
        )
        .with_particles(config.dynamic_layer_configuration.neutral_emitter.is_some());
        capture_metadata_for_binding(&capture, binding, kind)?;
        needs_recovery = false;
        progress("Preparing overlay skin…");
        let pack =
            artwork::prepare_configuration_from_preview(&config, Path::new("."), frozen_layers)?;
        progress("Applying and verifying overlay skin…");
        install_overlay(&capture, &pack, &mut needs_recovery)
    })();
    if capture.join("journal.json").exists() {
        let result = finish_apply_result(operation, needs_recovery, || {
            progress("Recovering previous card data…");
            let mut journal = load(&capture)?;
            let (session, mut afc) = open(&journal.binding)?;
            restore_and_verify_originals(&capture, &mut journal, &mut afc, &session)?;
            journal.status = "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation".into();
            save(&capture, &journal)
        })
        .with_context(|| format!("Recovery backup: {}", capture.display()));
        let limited = load(&capture)?.incomplete_recovery();
        return result.with_context(|| if limited {
            "Experimental recovery is incomplete: known originals can be restored, but effect files may remain"
        } else { "Overlay operation failed" }).map(|()| limited);
    }
    operation.map(|()| false)
}

#[cfg(test)]
fn finish_apply_result(
    operation: Result<()>,
    needs_recovery: bool,
    recover: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match operation {
        Ok(()) => Ok(()),
        Err(error) => {
            let recovery = if needs_recovery { recover() } else { Ok(()) };
            media::finish_with_cleanup(Err(error), recovery)
        }
    }
}

pub(crate) fn restore(directory: &Path, expected: &Binding) -> Result<bool> {
    let mut journal = load(directory)?;
    ensure!(
        journal.binding == *expected,
        "Recovery belongs to a different phone or card"
    );
    let _lock = DeviceLock::acquire(&journal.binding)?;
    let (session, mut afc) = open(&journal.binding)?;
    restore_and_verify_originals(directory, &mut journal, &mut afc, &session)?;
    journal.status = "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation".into();
    save(directory, &journal)?;
    Ok(journal.incomplete_recovery())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn particle_trial_preserves_other_effects_and_has_narrow_durable_recovery() {
        for cross in [false, true] {
            for transaction in [false, true] {
                let base = JournalKind::for_effects(cross, transaction, false);
                let mut journal = recovery_capture_fixture();
                journal.kind = base.with_particles(true);
                let names = inventory(&journal.kind);
                assert_eq!(&names[..inventory(&base).len()], inventory(&base));
                assert_eq!(
                    names.len(),
                    28 + if cross { 6 } else { 0 } + if transaction { 7 } else { 0 }
                );
                journal.assets = names
                    .into_iter()
                    .map(|leaf| Asset {
                        original_sha256: (!(leaf.starts_with("neutralEmitter")
                            || leaf.ends_with(".pdf"))
                            || leaf.starts_with("cardBackgroundCombined"))
                        .then(|| sha256(b"original")),
                        candidate_sha256: sha256(b"candidate"),
                        leaf,
                    })
                    .collect();
                let deployment = artwork::PreparedDeployment {
                    manifest_sha256: sha256(b"particle-pack"),
                    original_metadata: b"original".to_vec(),
                    candidate_metadata: b"candidate".to_vec(),
                    contains_emitter: true,
                    contains_cross_dissolve: cross,
                    contains_transaction: transaction,
                    resources: [
                        "backgroundParallax.png",
                        "foregroundParallax.png",
                        "dynamicLayerStaticFallback.png",
                        "backgroundParallaxCrossDissolve.png",
                        "transactionEffectEmitter.png",
                        "transactionEffectShape.svg",
                        "neutralEmitter.png",
                    ]
                    .into_iter()
                    .map(|name| artwork::PreparedResource {
                        filename: name.into(),
                        bytes: name.as_bytes().to_vec(),
                    })
                    .collect(),
                };
                let dir = std::env::temp_dir()
                    .join(format!("particle-trial-{}", random_token().unwrap()));
                fs::create_dir(&dir).unwrap();
                // Normal launches accept particles when every emitter preimage is known.
                for asset in &mut journal.assets {
                    if asset.leaf.starts_with("neutralEmitter") && asset.leaf.ends_with(".png") {
                        asset.original_sha256 = Some(sha256(b"original"));
                    }
                }
                assert!(plan_overlay_installation(&journal, &deployment, b"original").is_ok());
                for asset in &mut journal.assets {
                    if asset.leaf.starts_with("neutralEmitter") && asset.leaf.ends_with(".png") {
                        asset.original_sha256 = None;
                    }
                }
                assert!(
                    serde_json::to_value(&journal)
                        .unwrap()
                        .get("allow_missing_particle_originals")
                        .is_none()
                );
                // Other trial opt-ins cannot waive particle or combined alternate originals.
                record_trial_policy(&dir, &mut journal, &deployment, true, false, false).unwrap();
                assert!(!journal.allow_missing_cross_dissolve_originals);
                assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
                record_trial_policy(&dir, &mut journal, &deployment, false, false, true).unwrap();
                let mut journal = load(&dir).unwrap();
                assert!(journal.allow_missing_particle_originals);
                assert!(!journal.allow_missing_transaction_originals);
                let plan = plan_overlay_installation(&journal, &deployment, b"original").unwrap();
                assert_eq!(
                    plan.len(),
                    13 + if cross { 3 } else { 0 } + if transaction { 4 } else { 0 }
                );
                for index in 3..journal.assets.len() {
                    let asset = &journal.assets[index];
                    if asset.leaf.ends_with(".pdf") {
                        assert!(!journal.allows_missing_original(asset));
                        journal.assets[index].original_sha256 = Some(sha256(b"pdf"));
                        assert!(
                            plan_overlay_installation(&journal, &deployment, b"original").is_err()
                        );
                        journal.assets[index].original_sha256 = None;
                    } else if !asset.leaf.starts_with("neutralEmitter") {
                        let original = journal.assets[index].original_sha256.take();
                        assert!(
                            plan_overlay_installation(&journal, &deployment, b"original").is_err()
                        );
                        journal.assets[index].original_sha256 = original;
                    }
                }
                let before = fs::read(dir.join("journal.json")).unwrap();
                journal.kind = base;
                assert!(
                    record_trial_policy(&dir, &mut journal, &deployment, false, false, true)
                        .is_err()
                );
                assert_eq!(fs::read(dir.join("journal.json")).unwrap(), before);
                save(&dir, &journal).unwrap();
                assert!(
                    load(&dir).is_err(),
                    "Policy cannot be attached to an old inventory"
                );
                journal.kind =
                    JournalKind::for_effects(cross, transaction, true).with_particles(true);
                journal.trial_pack_sha256 = Some(deployment.manifest_sha256.clone());
                for (index, bytes) in &plan {
                    journal.assets[*index].candidate_sha256 = sha256(bytes);
                }
                save(&dir, &journal).unwrap();
                let mut journal = load(&dir).unwrap();
                assert!(journal.incomplete_recovery());
                for (index, bytes) in &plan {
                    if !journal.assets[*index].leaf.starts_with("neutralEmitter") {
                        continue;
                    }
                    retain_held_bytes(&dir, &mut journal, *index, bytes).unwrap();
                    assert!(journal.assets[*index].original_sha256.is_none());
                    assert!(!dir.join(format!("original-{index}.bin")).exists());
                    assert!(
                        retain_held_bytes(&dir, &mut journal, *index, b"conflicting file").is_err()
                    );
                }
                journal.status =
                    "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation".into();
                save(&dir, &journal).unwrap();
                assert!(
                    load(&dir).unwrap().incomplete_recovery(),
                    "Restore must retain its limit after restart"
                );
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn transaction_trial_is_durable_narrow_and_keeps_unknown_originals_unknown() {
        for cross in [false, true] {
            let mut journal = recovery_capture_fixture();
            journal.kind = JournalKind::for_effects(cross, true, false);
            journal.assets = inventory(&journal.kind)
                .into_iter()
                .map(|leaf| Asset {
                    original_sha256: (!(leaf.starts_with("transaction") || leaf.ends_with(".pdf"))
                        || leaf.starts_with("cardBackgroundCombined"))
                    .then(|| sha256(b"original")),
                    candidate_sha256: sha256(b"candidate"),
                    leaf,
                })
                .collect();
            let resources = [
                "backgroundParallax.png",
                "foregroundParallax.png",
                "dynamicLayerStaticFallback.png",
                "backgroundParallaxCrossDissolve.png",
                "transactionEffectEmitter.png",
                "transactionEffectShape.svg",
            ];
            let deployment = artwork::PreparedDeployment {
                manifest_sha256: sha256(b"transaction-trial-pack"),
                original_metadata: b"original".to_vec(),
                candidate_metadata: b"candidate".to_vec(),
                contains_emitter: false,
                contains_cross_dissolve: cross,
                contains_transaction: true,
                resources: resources
                    .iter()
                    .map(|name| artwork::PreparedResource {
                        filename: (*name).into(),
                        bytes: name.as_bytes().to_vec(),
                    })
                    .collect(),
            };
            let dir = std::env::temp_dir().join(format!(
                "transaction-trial-policy-{}",
                random_token().unwrap()
            ));
            fs::create_dir(&dir).unwrap();
            assert!(
                !serde_json::to_value(&journal)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("allow_missing_transaction_originals")
            );
            record_trial_policy(&dir, &mut journal, &deployment, true, false, false).unwrap();
            assert!(
                !journal.allow_missing_transaction_originals
                    && !journal.allow_missing_cross_dissolve_originals
            );
            assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
            record_trial_policy(&dir, &mut journal, &deployment, true, true, false).unwrap();
            let mut journal = load(&dir).unwrap(); // Policy survives reopening without an environment variable.
            assert!(
                journal.allow_missing_transaction_originals
                    && !journal.allow_missing_cross_dissolve_originals
            );
            let plan = plan_overlay_installation(&journal, &deployment, b"original").unwrap();
            assert_eq!(plan.len(), if cross { 17 } else { 14 });
            for index in 3..journal.assets.len() {
                let asset = &journal.assets[index];
                if asset.leaf.ends_with(".pdf") {
                    assert!(!journal.allows_missing_original(asset));
                } else if !asset.leaf.starts_with("transaction") {
                    let original = journal.assets[index].original_sha256.take();
                    assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
                    journal.assets[index].original_sha256 = original;
                }
            }
            let pdf = journal
                .assets
                .iter()
                .position(|a| a.leaf == "transactionEffectEmitter.pdf")
                .unwrap();
            journal.assets[pdf].original_sha256 = Some(sha256(b"pdf"));
            assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
            journal.assets[pdf].original_sha256 = None;
            // Wrong journal kinds cannot acquire a policy that would make recovery unreadable.
            let before = fs::read(dir.join("journal.json")).unwrap();
            journal.kind = JournalKind::MetadataCapture;
            assert!(
                record_trial_policy(&dir, &mut journal, &deployment, false, true, false).is_err()
            );
            assert_eq!(fs::read(dir.join("journal.json")).unwrap(), before);
            journal.kind = JournalKind::for_effects(cross, true, true);
            journal.trial_pack_sha256 = Some(deployment.manifest_sha256.clone());
            for (index, bytes) in &plan {
                journal.assets[*index].candidate_sha256 = sha256(bytes);
            }
            save(&dir, &journal).unwrap();
            let mut journal = load(&dir).unwrap();
            assert!(journal.incomplete_recovery());
            for (index, bytes) in &plan {
                if !journal.assets[*index].leaf.starts_with("transaction") {
                    continue;
                }
                assert!(journal.assets[*index].original_sha256.is_none());
                retain_held_bytes(&dir, &mut journal, *index, bytes).unwrap();
                assert!(journal.assets[*index].original_sha256.is_none());
                assert!(!dir.join(format!("original-{index}.bin")).exists());
                assert!(
                    retain_held_bytes(&dir, &mut journal, *index, b"conflicting held bytes")
                        .is_err()
                );
                assert_eq!(
                    fs::read(dir.join(format!("observed-candidate-{index}.bin"))).unwrap(),
                    *bytes
                );
            }
            for (index, asset) in journal.assets.iter().enumerate() {
                if asset.original_sha256.is_some() {
                    write_new_or_same(&dir.join(format!("original-{index}.bin")), b"original")
                        .unwrap();
                }
            }
            assert!(
                original_payloads(&dir, &journal, None)
                    .unwrap()
                    .iter()
                    .all(|(leaf, _)| !leaf.starts_with("transaction"))
            );
            save(&dir, &journal).unwrap();
            assert!(load(&dir).unwrap().incomplete_recovery());
            for kind in [
                JournalKind::MetadataCapture,
                JournalKind::MetadataParallax,
                JournalKind::MetadataCrossDissolveCapture,
                JournalKind::MetadataCrossDissolve,
                JournalKind::DynamicPng,
                JournalKind::ArtworkControl,
                JournalKind::FrontFaceCache,
            ] {
                journal.kind = kind;
                save(&dir, &journal).unwrap();
                assert!(load(&dir).is_err());
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn transaction_requires_svg_and_every_png_original_with_no_backup_exception() {
        for cross in [false, true] {
            let mut journal = recovery_capture_fixture();
            let legacy = inventory(&JournalKind::MetadataCapture);
            journal.kind = JournalKind::for_effects(cross, true, false);
            let names = inventory(&journal.kind);
            assert_eq!(names.len(), if cross { 35 } else { 29 });
            assert_eq!(&names[..22], &legacy);
            journal.assets = names
                .iter()
                .map(|leaf| Asset {
                    leaf: leaf.clone(),
                    original_sha256: (!leaf.ends_with(".pdf")
                        || leaf.starts_with("cardBackgroundCombined"))
                    .then(|| sha256(b"original")),
                    candidate_sha256: sha256(b"candidate"),
                })
                .collect();
            let mut deployment = artwork::PreparedDeployment {
                manifest_sha256: sha256(b"pack"),
                original_metadata: b"original".to_vec(),
                candidate_metadata: b"candidate".to_vec(),
                contains_emitter: false,
                contains_cross_dissolve: cross,
                contains_transaction: true,
                resources: [
                    "backgroundParallax.png",
                    "foregroundParallax.png",
                    "dynamicLayerStaticFallback.png",
                    "backgroundParallaxCrossDissolve.png",
                    "transactionEffectEmitter.png",
                    "transactionEffectShape.svg",
                ]
                .iter()
                .map(|name| artwork::PreparedResource {
                    filename: (*name).into(),
                    bytes: name.as_bytes().to_vec(),
                })
                .collect(),
            };
            assert!(installation_ready(&journal));
            let plan = plan_overlay_installation(&journal, &deployment, b"original").unwrap();
            assert_eq!(plan.len(), if cross { 17 } else { 14 });
            assert_eq!(plan.last().unwrap().1, b"transactionEffectShape.svg");
            assert!(
                artwork_verification_groups(&journal)
                    .concat()
                    .contains(&(names.len() - 1))
            );
            for index in 4..journal.assets.len() {
                if journal.assets[index].leaf.ends_with(".pdf") {
                    continue;
                }
                journal.assets[index].original_sha256 = None;
                assert!(!journal.allows_missing_original(&journal.assets[index]));
                assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
                journal.assets[index].original_sha256 = Some(sha256(b"original"));
            }
            let pdf = names
                .iter()
                .position(|n| n == "transactionEffectEmitter.pdf")
                .unwrap();
            journal.assets[pdf].original_sha256 = Some(sha256(b"pdf"));
            assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
            journal.assets[pdf].original_sha256 = None;
            deployment.contains_transaction = false;
            assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
            deployment.contains_transaction = true;
            let dir = std::env::temp_dir()
                .join(format!("transaction-journal-{}", random_token().unwrap()));
            fs::create_dir(&dir).unwrap();
            save(&dir, &journal).unwrap();
            assert!(load(&dir).is_ok());
            assert!(no_installation_to_recover(&dir));
            journal.allow_missing_cross_dissolve_originals = true;
            save(&dir, &journal).unwrap();
            assert!(load(&dir).is_err());
            journal.allow_missing_cross_dissolve_originals = false;
            journal.kind = JournalKind::for_effects(cross, true, true);
            journal.trial_pack_sha256 = Some(deployment.manifest_sha256.clone());
            save(&dir, &journal).unwrap();
            assert!(load(&dir).is_ok());
            assert!(!no_installation_to_recover(&dir));
            journal.assets.last_mut().unwrap().original_sha256 = None;
            save(&dir, &journal).unwrap();
            assert!(load(&dir).is_err());
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn front_face_cache_capture_is_bound_to_one_cache_leaf() {
        assert_eq!(inventory(&JournalKind::FrontFaceCache), ["FrontFace"]);
        assert_eq!(JournalKind::FrontFaceCache.bundle_extension(), ".cache");
        for kind in [
            JournalKind::MetadataCapture,
            JournalKind::MetadataParallax,
            JournalKind::MetadataCrossDissolveCapture,
            JournalKind::MetadataCrossDissolve,
        ] {
            assert_eq!(kind.bundle_extension(), ".pkpass");
        }
        assert!(!JournalKind::FrontFaceCache.has_metadata());
        assert!(!JournalKind::FrontFaceCache.is_installation());
    }

    #[test]
    #[ignore = "Journaled capture and byte-identical return of the bound card's .cache/FrontFace; no invalidation or candidate installation"]
    fn cross_dissolve_native_capture_front_face_cache() {
        let result = (|| -> Result<()> {
            let application = PathBuf::from(
                std::env::var_os("AIRCARD_CROSS_DISSOLVE_TRIAL_APPLICATION")
                    .context("Set the bound trial application")?,
            );
            let binding = load(&application.join("recovery"))?.binding;
            let _lock = DeviceLock::acquire(&binding)?;
            let (session, mut afc) = open(&binding)?;
            let directory = new_application_directory()?;
            let capture = directory.join("recovery");
            fs::create_dir(&capture)?;
            println!(
                "FrontFace cache observation directory: {}",
                directory.display()
            );
            let mut journal = Journal {
                schema_version: 1,
                kind: JournalKind::FrontFaceCache,
                combined_sync: true,
                invalidate_caches: false,
                last_capture_indices: Vec::new(),
                trial_pack_sha256: None,
                observed_metadata_sha256: None,
                binding,
                status: "FrontFaceCacheCapturePrepared".into(),
                books: media::snapshot_books(&mut afc)?,
                roots: Vec::new(),
                held: format!("{}{}", airlift::RECOVERED_PREFIX, random_token()?),
                assets: vec![Asset {
                    leaf: "FrontFace".into(),
                    original_sha256: None,
                    candidate_sha256: sha256(b""),
                }],
                protected_readback_verified: false,
                allow_missing_cross_dissolve_originals: false,
                allow_missing_transaction_originals: false,
                allow_missing_particle_originals: false,
            };
            save(&capture, &journal)?;
            let operation = (|| -> Result<Vec<usize>> {
                let recovered =
                    capture_and_return(&capture, &mut journal, &mut afc, &session, &[0])?;
                if recovered == [0] {
                    let verified =
                        capture_and_return(&capture, &mut journal, &mut afc, &session, &[0])?;
                    require_roundtrip(&verified, &[0])?;
                }
                Ok(recovered)
            })();
            let recovered = match operation {
                Ok(recovered) => recovered,
                Err(error) => {
                    return media::finish_with_cleanup(
                        Err(error),
                        restore_originals(&capture, &mut journal, &mut afc, &session),
                    );
                }
            };
            journal.status = if recovered == [0] {
                "FrontFaceCacheCapturedReappliedAndReadbackMatched"
            } else {
                "FrontFaceCacheUnknown"
            }
            .into();
            save(&capture, &journal)?;
            let summary = if recovered == [0] {
                let bytes = regular_read(&capture.join("original-0.bin"), media::MAX_FILE_BYTES)?;
                serde_json::json!({"state":"RecoveredAndVerified","bytes":bytes.len(),"sha256":sha256(&bytes),
                    "is_invalidation_marker":bytes==crate::flasher::CACHE_INVALIDATION_PAYLOAD,
                    "prefix_hex":bytes.iter().take(24).map(|byte|format!("{byte:02x}")).collect::<String>(),
                    "invalidation_performed":false,"trial_recovery_selection_unchanged":true})
            } else {
                serde_json::json!({"state":"Unknown","invalidation_performed":false})
            };
            write_new_or_same(
                &directory.join("front-face-cache-observation.json"),
                &serde_json::to_vec_pretty(&summary)?,
            )?;
            println!("{}", serde_json::to_string(&summary)?);
            Ok(())
        })();
        if let Err(error) = result {
            panic!("FrontFace cache capture failed: {error:#}");
        }
    }

    #[test]
    #[ignore = "Restore known pretrial originals on the bound USB phone; alternate-file recovery remains incomplete"]
    fn cross_dissolve_native_restore_known_originals() {
        let result = (|| -> Result<()> {
            let application = PathBuf::from(
                std::env::var_os("AIRCARD_CROSS_DISSOLVE_TRIAL_APPLICATION")
                    .context("Set the trial application directory")?,
            );
            let directory = application.join("recovery");
            let journal = load(&directory)?;
            ensure!(
                journal.kind == JournalKind::MetadataCrossDissolve
                    && journal.allow_missing_cross_dissolve_originals,
                "Select the explicitly authorized cross-dissolve trial"
            );
            let limited = restore(&directory, &journal.binding)?;
            write_new_or_same(
                &application.join("trial-restoration-result.json"),
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "known_originals_restored_and_verified":true,
                    "recovery_incomplete":limited,
                    "protected_alternate_file_removal_performed":false,
                    "visible_restoration_confirmed":false
                }))?,
            )?;
            println!(
                "Known pretrial originals restored and verified. Alternate-file recovery incomplete: {limited}. Reopen Wallet and Apple Pay to confirm appearance and parallax."
            );
            Ok(())
        })();
        if let Err(error) = result {
            panic!("Trial restoration failed: {error:#}");
        }
    }

    #[test]
    #[ignore = "Journaled post-boot capture on the bound USB phone; relocates and reapplies current files"]
    fn cross_dissolve_native_post_boot_capture() {
        let result = (|| -> Result<()> {
            let source = PathBuf::from(
                std::env::var_os("AIRCARD_CROSS_DISSOLVE_TRIAL_APPLICATION")
                    .context("Set the installed trial application directory")?,
            );
            let installed = load(&source.join("recovery"))?;
            ensure!(
                installed.kind == JournalKind::MetadataCrossDissolve && installed.roots.is_empty(),
                "Source is not a clean cross-dissolve installation"
            );
            let deployment = artwork::load_deployment(&source.join("pack"))?;
            ensure!(
                installed.trial_pack_sha256.as_ref() == Some(&deployment.manifest_sha256),
                "Trial pack binding differs"
            );
            let directory = new_application_directory()?;
            let capture = directory.join("recovery");
            println!("Post-boot observation directory: {}", directory.display());
            write_new_or_same(
                &directory.join("observation-source.json"),
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "installed_trial":source, "purpose":"Capture and reapply post-boot bytes; preserve trial recovery selection",
                    "installed_manifest_sha256":deployment.manifest_sha256
                }))?,
            )?;
            let _lock = DeviceLock::acquire(&installed.binding)?;
            let operation = capture_metadata_for_binding(
                &capture,
                installed.binding.clone(),
                JournalKind::MetadataCrossDissolveCapture,
            );
            finish_apply_result(operation, true, || {
                if !capture.join("journal.json").exists() {
                    return Ok(());
                }
                let mut journal = load(&capture)?;
                let (session, mut afc) = open(&journal.binding)?;
                restore_and_verify_originals(&capture, &mut journal, &mut afc, &session)
            })?;
            let observed = load(&capture)?;
            let metadata = regular_read(
                &capture.join("original-pass.json"),
                super::super::metadata::MAX_JSON_BYTES,
            )?;
            let parsed = super::super::metadata::parse_unique(&metadata)?;
            let resource_comparisons: Vec<_> = installed.assets.iter().zip(&observed.assets).enumerate()
                .filter(|(index, _)| source.join(format!("recovery/candidate-{index}.bin")).is_file())
                .map(|(_, (candidate, actual))| serde_json::json!({
                    "leaf":candidate.leaf,
                    "state":match &actual.original_sha256 { None => "Unknown", Some(hash) if hash==&candidate.candidate_sha256 => "CandidateMatched", Some(_) => "DifferentBytes" },
                    "candidate_sha256":candidate.candidate_sha256, "observed_sha256":actual.original_sha256
                })).collect();
            let summary = serde_json::json!({
                "metadata_matches_installed_candidate":metadata==deployment.candidate_metadata,
                "observed_dynamic_configuration":parsed.get("dynamicLayerConfiguration"),
                "resources":resource_comparisons, "pending_roots":observed.roots.len(),
                "trial_recovery_selection_unchanged":true
            });
            write_new_or_same(
                &directory.join("post-boot-comparison.json"),
                &serde_json::to_vec_pretty(&summary)?,
            )?;
            println!("{}", serde_json::to_string(&summary)?);
            Ok(())
        })();
        if let Err(error) = result {
            panic!("Post-boot capture failed: {error:#}");
        }
    }

    #[test]
    #[ignore = "Operator-authorized cross-dissolve installation on the bound USB phone; requires explicit source and opt-in"]
    fn cross_dissolve_native_trial_from_saved_artwork() {
        let result = (|| -> Result<()> {
            ensure!(
                experimental_cross_dissolve_enabled(),
                "Explicit cross-dissolve trial opt-in is required"
            );
            let source = PathBuf::from(
                std::env::var_os("AIRCARD_CROSS_DISSOLVE_TRIAL_SOURCE")
                    .context("Set the prior application directory")?,
            );
            let previous = load(&source.join("recovery"))?;
            ensure!(
                previous.kind == JournalKind::MetadataCrossDissolveCapture
                    && installation_ready(&previous),
                "Source must be a completed cross-dissolve capture with no candidate installation"
            );
            original_payloads(&source.join("recovery"), &previous, None)?;
            let deployment = artwork::load_deployment(&source.join("pack"))?;
            ensure!(
                deployment.contains_cross_dissolve && !deployment.contains_emitter,
                "Source is not a background-only cross-dissolve pack"
            );
            let original = regular_read(
                &source.join("recovery/original-pass.json"),
                super::super::metadata::MAX_JSON_BYTES,
            )?;
            ensure!(
                deployment.original_metadata == original
                    && previous.assets[3].original_sha256.as_deref() == Some(&sha256(&original)),
                "Source metadata binding differs"
            );
            let mut config = artwork::trial_configuration(&source.join("pack"))?;
            let frozen = artwork::preview_pack(&source.join("pack"))?;
            let directory = new_application_directory()?;
            let capture = directory.join("recovery");
            println!("Trial application directory: {}", directory.display());
            write_new_or_same(
                &directory.join("trial-source.json"),
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "source_application":source, "source_manifest_sha256":deployment.manifest_sha256,
                    "allow_missing_cross_dissolve_originals":true,
                    "scope":"Only unknown alternate-background PNG originals; metadata and base artwork remain required",
                    "visual_motion_observed":false
                }))?,
            )?;
            let artwork_directory = directory.join("artwork");
            fs::create_dir(&artwork_directory)?;
            for resource in &deployment.resources {
                write_new_or_same(&artwork_directory.join(&resource.filename), &resource.bytes)?;
            }
            config.background = artwork_directory.join("backgroundParallax.png");
            config.foreground = artwork_directory.join("foregroundParallax.png");
            config.alternate_background =
                Some(artwork_directory.join("backgroundParallaxCrossDissolve.png"));
            config.original_pass_json = capture.join("original-pass.json");
            config.output_directory = directory.join("pack");
            // Persist the UI recovery pointer before any phone mutation.
            super::super::editor::remember_trial_recovery(
                previous.binding.clone(),
                capture.clone(),
            )?;
            let _lock = DeviceLock::acquire(&previous.binding)?;
            let mut needs_recovery = true;
            let operation = (|| -> Result<()> {
                println!("Capturing and verifying the current bound card...");
                capture_metadata_for_binding(
                    &capture,
                    previous.binding.clone(),
                    JournalKind::MetadataCrossDissolveCapture,
                )?;
                needs_recovery = false;
                let current = load(&capture)?;
                ensure!(
                    previous
                        .assets
                        .iter()
                        .zip(&current.assets)
                        .all(|(old, new)| old.original_sha256.is_none()
                            || old.original_sha256 == new.original_sha256),
                    "Known preimages changed since the selected source capture; trial stopped after verified capture"
                );
                let pack =
                    artwork::prepare_configuration_from_preview(&config, Path::new("."), frozen)?;
                println!("Installing and verifying the cross-dissolve candidate...");
                install_overlay(&capture, &pack, &mut needs_recovery)
            })();
            let operation = finish_apply_result(operation, needs_recovery, || {
                if !capture.join("journal.json").exists() {
                    return Ok(());
                }
                let mut journal = load(&capture)?;
                let (session, mut afc) = open(&journal.binding)?;
                restore_and_verify_originals(&capture, &mut journal, &mut afc, &session)?;
                journal.status =
                    "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation".into();
                save(&capture, &journal)
            });
            write_new_or_same(
                &directory.join("trial-result.json"),
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "candidate_installation_and_readbacks_passed":operation.is_ok(),
                    "error":operation.as_ref().err().map(|error| format!("{error:#}")),
                    "recovery_directory":capture, "visual_motion_observed":false,
                    "recovery_limit":"Unknown alternate originals cannot be restored; effect files may remain"
                }))?,
            )?;
            operation?;
            println!(
                "Candidate readbacks passed. Reopen Wallet and Apple Pay and inspect tilt. Motion is not yet observed; alternate-file recovery is incomplete."
            );
            Ok(())
        })();
        if let Err(error) = result {
            panic!("Cross-dissolve trial failed: {error:#}");
        }
    }

    #[test]
    fn cross_dissolve_planning_requires_all_preimages_and_matching_inventory() {
        let mut journal = recovery_capture_fixture();
        let legacy = inventory(&JournalKind::MetadataCapture);
        journal.kind = JournalKind::MetadataCrossDissolveCapture;
        let expanded = inventory(&journal.kind);
        assert_eq!(expanded.len(), 28);
        assert_eq!(&expanded[..22], &legacy);
        journal
            .assets
            .extend(expanded[22..].iter().map(|leaf| Asset {
                leaf: leaf.clone(),
                original_sha256: leaf.ends_with(".png").then(|| sha256(b"original")),
                candidate_sha256: sha256(b"candidate"),
            }));
        let deployment = artwork::PreparedDeployment {
            manifest_sha256: sha256(b"cross-pack"),
            original_metadata: b"original".to_vec(),
            candidate_metadata: b"candidate".to_vec(),
            contains_emitter: false,
            contains_cross_dissolve: true,
            contains_transaction: false,
            resources: [
                "backgroundParallax",
                "foregroundParallax",
                "dynamicLayerStaticFallback",
                "backgroundParallaxCrossDissolve",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, base)| artwork::PreparedResource {
                filename: format!("{base}.png"),
                bytes: vec![index as u8],
            })
            .collect(),
        };
        assert!(installation_ready(&journal));
        let plan = plan_overlay_installation(&journal, &deployment, b"original").unwrap();
        assert_eq!(plan.len(), 13);
        assert_eq!(plan[0], (3, b"candidate".to_vec()));
        for index in 22..25 {
            assert_eq!(plan.iter().find(|(i, _)| *i == index).unwrap().1, vec![3]);
            journal.assets[index].original_sha256 = None;
            assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
            journal.allow_missing_cross_dissolve_originals = true;
            assert!(plan_overlay_installation(&journal, &deployment, b"original").is_ok());
            assert!(
                journal.assets[index].original_sha256.is_none(),
                "Opt-in must not invent an original"
            );
            journal.allow_missing_cross_dissolve_originals = false;
            journal.assets[index].original_sha256 = Some(sha256(b"original"));
        }
        journal.assets[25].original_sha256 = Some(sha256(b"competing pdf"));
        assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
        journal.assets[25].original_sha256 = None;
        assert!(plan_overlay_installation(&journal, &deployment, b"other metadata").is_err());
        journal.kind = JournalKind::MetadataCapture;
        assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
        journal.kind = JournalKind::MetadataCrossDissolve;
        journal.trial_pack_sha256 = Some(sha256(b"other pack"));
        assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
        journal.trial_pack_sha256 = Some(deployment.manifest_sha256.clone());
        assert!(plan_overlay_installation(&journal, &deployment, b"original").is_ok());

        // New recovery journals retain their inventory on restart and never accept
        // unknown overwritten PNG originals. This test opens no native session.
        let dir = std::env::temp_dir().join(format!(
            "aircard-cross-recovery-{}",
            random_token().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        save(&dir, &journal).unwrap();
        assert_eq!(load(&dir).unwrap().kind, JournalKind::MetadataCrossDissolve);
        journal.assets[22].original_sha256 = None;
        save(&dir, &journal).unwrap();
        assert!(load(&dir).is_err());
        journal.allow_missing_cross_dissolve_originals = true;
        save(&dir, &journal).unwrap();
        assert!(load(&dir).unwrap().incomplete_recovery());
        retain_held_bytes(&dir, &mut journal, 22, b"candidate").unwrap();
        assert!(!dir.join("original-22.bin").exists());
        assert!(journal.assets[22].original_sha256.is_none());
        assert!(retain_held_bytes(&dir, &mut journal, 22, b"replacement file").is_err());
        for index in [3, 4] {
            journal.assets[index].original_sha256 = None;
            save(&dir, &journal).unwrap();
            assert!(
                load(&dir).is_err(),
                "Metadata and base parallax originals remain mandatory"
            );
            journal.assets[index].original_sha256 = Some(sha256(b"original"));
        }
        journal.assets[25].original_sha256 = Some(sha256(b"pdf"));
        assert!(plan_overlay_installation(&journal, &deployment, b"original").is_err());
        journal.assets[25].original_sha256 = None;
        journal.kind = JournalKind::MetadataParallax;
        save(&dir, &journal).unwrap();
        assert!(
            load(&dir).is_err(),
            "The cross-dissolve opt-in cannot alter parallax journals"
        );
        journal.kind = JournalKind::MetadataCrossDissolve;
        journal.allow_missing_cross_dissolve_originals = false;
        journal.assets[22].original_sha256 = Some(sha256(b"original"));
        write_new_or_same(&dir.join("original-22.bin"), b"original").unwrap();
        retain_held_bytes(&dir, &mut journal, 22, b"candidate").unwrap();
        assert_eq!(fs::read(dir.join("original-22.bin")).unwrap(), b"original");
        assert_eq!(
            fs::read(dir.join("observed-candidate-22.bin")).unwrap(),
            b"candidate"
        );
        assert!(retain_held_bytes(&dir, &mut journal, 22, b"unknown bytes").is_err());
        assert_eq!(
            artwork_verification_groups(&journal),
            vec![vec![0, 1, 2, 4, 5, 6, 10, 11, 12, 16, 17, 18, 22, 23, 24]]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_metadata_detection_preserves_artwork_only_journals() {
        let mut journal = recovery_capture_fixture();
        assert!(has_captured_metadata(&journal));
        journal.assets[3].original_sha256 = None;
        assert!(!has_captured_metadata(&journal));
        journal.kind = JournalKind::DynamicPng;
        journal.assets = leaves()
            .into_iter()
            .map(|leaf| Asset {
                leaf,
                original_sha256: Some(sha256(b"original")),
                candidate_sha256: sha256(b"candidate"),
            })
            .collect();
        assert!(!has_captured_metadata(&journal));
        assert_eq!(
            artwork_verification_groups(&journal).concat(),
            (0..9).collect::<Vec<_>>()
        );
        journal.kind = JournalKind::ArtworkControl;
        journal.assets.truncate(3);
        assert!(!has_captured_metadata(&journal));
        assert_eq!(
            artwork_verification_groups(&journal).concat(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn completed_capture_preflight_refusal_does_not_restore_again() {
        let recoveries = std::cell::Cell::new(0);
        for refusal in ["missing original artwork", "invalid prepared pack"] {
            let error = finish_apply_result(Err(anyhow::anyhow!(refusal)), false, || {
                recoveries.set(recoveries.get() + 1);
                Ok(())
            })
            .unwrap_err();
            assert_eq!(error.to_string(), refusal);
        }
        assert_eq!(recoveries.get(), 0);
        // Success must never invoke recovery even if mutation was entered.
        finish_apply_result(Ok(()), true, || {
            recoveries.set(recoveries.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(recoveries.get(), 0);
    }

    #[test]
    fn mutation_failure_recovers_once_and_retains_verification_failure() {
        let recoveries = std::cell::Cell::new(0);
        let error =
            finish_apply_result(Err(anyhow::anyhow!("candidate write failed")), true, || {
                recoveries.set(recoveries.get() + 1);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(recoveries.get(), 1);
        assert_eq!(error.to_string(), "candidate write failed");

        let error =
            finish_apply_result(Err(anyhow::anyhow!("candidate write failed")), true, || {
                recoveries.set(recoveries.get() + 1);
                anyhow::bail!("second metadata readback incomplete")
            })
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("candidate write failed"));
        assert!(message.contains("second metadata readback incomplete"));
        assert_eq!(recoveries.get(), 2);
    }

    pub(super) fn recovery_capture_fixture() -> Journal {
        serde_json::from_value(serde_json::json!({
            "schema_version":1, "kind":"MetadataCapture", "combined_sync":true,
            "invalidate_caches":true,
            "binding":{"udid":"test-phone","build":"23G83","product_type":"iPhone15,4",
                "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=","transport":"Usb"},
            "status":"MetadataCapturedAndReappliedAwaitingVisualConfirmation",
            "books":{"files":{},"directories":{},"directory_entries":{}},
            "roots":[],"held":"airlift-recovered-0123456789abcdef0123",
            "protected_readback_verified":false,
            "assets":inventory(&JournalKind::MetadataCapture).iter().map(|leaf|
                serde_json::json!({"leaf":leaf,
                    "original_sha256":if leaf.ends_with(".pdf") && !leaf.starts_with("cardBackgroundCombined") { None } else { Some(sha256(b"original")) },
                    "candidate_sha256":sha256(b"candidate")})).collect::<Vec<_>>()
        })).unwrap()
    }

    #[test]
    fn capture_return_selects_moved_slots_but_recovery_validates_all_originals() {
        let dir = std::env::temp_dir().join(format!(
            "aircard-selective-return-{}",
            random_token().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let mut journal: Journal = serde_json::from_value(serde_json::json!({
            "schema_version":1, "kind":"MetadataCapture", "combined_sync":true, "invalidate_caches":true,
            "binding":{"udid":"test-phone", "build":"23G83", "product_type":"iPhone15,4", "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=", "transport":"Usb"},
            "status":"ProtectedCaptureIntent", "books":{"files":{},"directories":{},"directory_entries":{}},
            "roots":[], "held":"airlift-recovered-0123456789abcdef0123", "protected_readback_verified":false,
            "assets":inventory(&JournalKind::MetadataCapture).iter().map(|leaf| serde_json::json!({"leaf":leaf,"original_sha256":null,"candidate_sha256":sha256(b"")})).collect::<Vec<_>>()
        })).unwrap();
        for index in [0, 3, 4, 5, 6] {
            let bytes = vec![index as u8; 16];
            write_new_or_same(&dir.join(format!("original-{index}.bin")), &bytes).unwrap();
            journal.assets[index].original_sha256 = Some(sha256(&bytes));
        }
        let returned = original_payloads(&dir, &journal, Some(&[4, 5, 6])).unwrap();
        assert_eq!(
            returned.iter().map(|(leaf, _)| leaf).collect::<Vec<_>>(),
            [4, 5, 6]
                .iter()
                .map(|i| &journal.assets[*i].leaf)
                .collect::<Vec<_>>()
        );
        assert_eq!(original_payloads(&dir, &journal, None).unwrap().len(), 5);
        assert!(
            original_payloads(&dir, &journal, Some(&[]))
                .unwrap()
                .is_empty()
        );
        assert!(original_payloads(&dir, &journal, Some(&[22])).is_err());
        // Damage outside the smaller return batch must still stop device writes.
        fs::write(dir.join("original-0.bin"), b"damaged").unwrap();
        assert!(original_payloads(&dir, &journal, Some(&[4, 5, 6])).is_err());
        assert!(original_payloads(&dir, &journal, None).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_candidate_readback_preserves_original_and_rejects_unknown_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "aircard-candidate-recovery-{}",
            random_token().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let original = b" {\"serialNumber\":\"test\"}\n";
        let candidate = b"{\"serialNumber\":\"test\",\"dynamicLayerConfiguration\":{\"version\":2,\"parallaxEnabled\":true}}";
        let mut journal: Journal = serde_json::from_value(serde_json::json!({
            "schema_version":1, "kind":"MetadataParallax", "combined_sync":true, "invalidate_caches":true,
            "trial_pack_sha256":sha256(b"pack"),
            "binding":{"udid":"test-phone", "build":"23G83", "product_type":"iPhone15,4", "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=", "transport":"Usb"},
            "status":"ParallaxCandidateReadbackIntent", "books":{"files":{},"directories":{},"directory_entries":{}},
            "roots":[], "held":"airlift-recovered-0123456789abcdef0123", "protected_readback_verified":false,
            "assets":inventory(&JournalKind::MetadataParallax).iter().map(|leaf| serde_json::json!({"leaf":leaf,"original_sha256":sha256(original),"candidate_sha256":sha256(candidate)})).collect::<Vec<_>>()
        })).unwrap();
        write_new_or_same(&dir.join("original-3.bin"), original).unwrap();
        retain_held_bytes(&dir, &mut journal, 3, candidate).unwrap();
        assert_eq!(fs::read(dir.join("original-3.bin")).unwrap(), original);
        assert_eq!(journal.assets[3].original_sha256, Some(sha256(original)));
        assert_eq!(
            fs::read(dir.join("observed-candidate-3.bin")).unwrap(),
            candidate
        );
        assert!(require_original_bytes(&journal.assets[3], candidate).is_err());
        assert!(require_original_bytes(&journal.assets[3], original).is_ok());
        save(&dir, &journal).unwrap();
        assert!(load(&dir).is_ok());
        // Reproduce missing JSON in a combined reexport
        // Verify artwork separately from the metadata-only checks
        journal.kind = JournalKind::MetadataCapture;
        for (index, asset) in journal.assets.iter_mut().enumerate() {
            if index > 3 && asset.leaf.ends_with(".pdf") {
                asset.original_sha256 = None;
            }
        }
        assert_eq!(
            artwork_verification_groups(&journal),
            vec![
                vec![0, 1, 2],
                vec![4, 5, 6],
                vec![10, 11, 12],
                vec![16, 17, 18]
            ]
        );
        assert!(require_roundtrip(&[], &[3]).is_err());
        journal.kind = JournalKind::MetadataParallax;
        assert!(!installation_ready(&journal));
        journal.status =
            "ParallaxOriginalsReadbackMatchedAndReappliedAwaitingVisualConfirmation".into();
        assert!(installation_ready(&journal));
        journal
            .roots
            .push("airlift-src-0123456789abcdef0123".into());
        assert!(!installation_ready(&journal));
        save(&dir, &journal).unwrap();
        journal.roots.clear();
        journal.assets[3].original_sha256 = None;
        save(&dir, &journal).unwrap();
        assert!(load(&dir).is_err());
        journal.assets[3].original_sha256 = Some(sha256(original));
        assert!(retain_held_bytes(&dir, &mut journal, 3, b"unexpected phone edit").is_err());
        let boot_metadata = b"{\"serialNumber\":\"test\",\"bootReconciled\":true}";
        let boot_hash = sha256(boot_metadata);
        journal.observed_metadata_sha256 = Some(boot_hash.clone());
        let boot_path = dir.join(format!("current-metadata-{boot_hash}.json"));
        assert!(retain_held_bytes(&dir, &mut journal, 3, boot_metadata).is_err());
        write_new_or_same(&boot_path, boot_metadata).unwrap();
        retain_held_bytes(&dir, &mut journal, 3, boot_metadata).unwrap();
        assert_eq!(fs::read(dir.join("original-3.bin")).unwrap(), original);
        assert_eq!(journal.assets[3].original_sha256, Some(sha256(original)));
        fs::write(&boot_path, b"corrupted snapshot").unwrap();
        assert!(retain_held_bytes(&dir, &mut journal, 3, boot_metadata).is_err());
        write_new_or_same(&dir.join("candidate-3.bin"), candidate).unwrap();
        assert!(candidate_payloads(&dir, &journal, &[3]).is_ok());
        fs::write(dir.join("candidate-3.bin"), b"damaged").unwrap();
        assert!(candidate_payloads(&dir, &journal, &[3]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn metadata_capture_inventory_and_manifest_bind_slots_without_metadata_payloads() {
        let names = inventory(&JournalKind::MetadataCapture);
        assert_eq!(names.len(), 22);
        assert_eq!(names[3], "pass.json");
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
        assert_eq!(names.iter().filter(|n| n.ends_with(".pdf")).count(), 10);
        let pairs = capture_pairs(
            "source",
            "link",
            "held",
            &[
                (3, "pass.json".into()),
                (7, "backgroundParallax.pdf".into()),
            ],
        );
        assert_eq!(
            pairs,
            vec![
                ("../../source/p0/p1/p2/link".into(), "link".into()),
                ("../../link/pass.json".into(), "held/3.bin".into()),
                (
                    "../../link/backgroundParallax.pdf".into(),
                    "held/7.bin".into()
                ),
            ]
        );
    }

    #[test]
    fn single_metadata_write_matches_ordinary_single_payload_archive() {
        let pairs = write_pairs("source", "link", &["pass.json".into()], true);
        assert_eq!(
            pairs,
            vec![
                ("../../source/p0/p1/p2/link".into(), "link".into()),
                ("../../source/payload".into(), "link/pass.json".into()),
            ]
        );
        let archive = airlift::build_streaming_zip_archive(
            "/var/mobile/Library/Passes/Cards/example.pkpass",
            b"captured-json",
        )
        .unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
        use std::io::Read;
        let mut bytes = Vec::new();
        zip.by_name("payload")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"captured-json");
        assert!(zip.by_name("payload_0").is_err());
    }

    #[test]
    fn fresh_roundtrip_requires_every_requested_slot_not_previous_journal_hashes() {
        let expected = [0, 1, 2, 3];
        let fresh = recovered_slots(&["3.bin".into(), "0.bin".into()], &expected).unwrap();
        assert!(require_roundtrip(&fresh, &expected).is_err());
        assert!(recovered_slots(&["4.bin".into()], &expected).is_err());
        assert!(recovered_slots(&["0.bin".into(), "0.bin".into()], &expected).is_err());
        assert!(recovered_slots(&["../pass.json".into()], &expected).is_err());
        let fresh = recovered_slots(
            &[
                "3.bin".into(),
                "2.bin".into(),
                "1.bin".into(),
                "0.bin".into(),
            ],
            &expected,
        )
        .unwrap();
        assert!(require_roundtrip(&fresh, &expected).is_ok());
    }

    #[test]
    fn historical_journals_keep_cache_policy_and_new_trials_require_combined_invalidation() {
        let dir =
            std::env::temp_dir().join(format!("aircard-cache-journal-{}", random_token().unwrap()));
        fs::create_dir(&dir).unwrap();
        let historical = serde_json::json!({
            "schema_version": 1,
            "binding": {"udid":"test-phone", "build":"23G83", "product_type":"iPhone15,4",
                "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=", "transport":"Usb"},
            "status":"AwaitingManualObservation",
            "books":{"files":{}, "directories":{}, "directory_entries":{}},
            "roots":[], "held":"airlift-recovered-0123456789abcdef0123",
            "assets":leaves().iter().map(|leaf| serde_json::json!({
                "leaf":leaf, "original_sha256":null, "candidate_sha256":sha256(b"marker")
            })).collect::<Vec<_>>(),
            "protected_readback_verified":false
        });
        let journal: Journal = serde_json::from_value(historical).unwrap();
        save(&dir, &journal).unwrap();
        let mut journal = load(&dir).unwrap();
        assert!(!journal.invalidate_caches);
        assert!(!journal.combined_sync);
        journal.combined_sync = true;
        journal.invalidate_caches = true;
        save(&dir, &journal).unwrap();
        assert!(load(&dir).unwrap().invalidate_caches);
        journal.combined_sync = false;
        save(&dir, &journal).unwrap();
        assert!(load(&dir).is_err());
        journal.combined_sync = true;
        journal.kind = JournalKind::ArtworkControl;
        journal.assets = inventory(&journal.kind)
            .into_iter()
            .map(|leaf| Asset {
                leaf,
                original_sha256: Some(sha256(b"original")),
                candidate_sha256: sha256(b"marker"),
            })
            .collect();
        save(&dir, &journal).unwrap();
        assert!(load(&dir).unwrap().invalidate_caches);
        journal.combined_sync = false;
        save(&dir, &journal).unwrap();
        assert!(load(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn combined_delivery_and_restore_publish_link_before_payloads_in_same_manifest() {
        let names = inventory(&JournalKind::ArtworkControl);
        let combined = write_pairs("source", "link", &names, true);
        assert_eq!(combined.len(), 4);
        assert_eq!(
            combined[0],
            ("../../source/p0/p1/p2/link".into(), "link".into())
        );
        assert_eq!(
            combined[1],
            (
                "../../source/payload_0".into(),
                "link/cardBackgroundCombined@3x.png".into()
            )
        );
        assert_eq!(
            combined[3],
            (
                "../../source/payload_2".into(),
                "link/cardBackgroundCombined.pdf".into()
            )
        );
        assert_eq!(write_pairs("source", "link", &names, false), combined[1..]);
    }
}
