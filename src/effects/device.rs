use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::artwork::{self, sha256};
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
    journal.kind = JournalKind::ManagedTransition;
    journal.transition_sha256 = Some(sha256(b"illustrative QA pack"));
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
        journal.binding.card_id, ".pkpass"
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
    Ok((source, link))
}

fn write_pairs(source: &str, link: &str, leaves: &[String]) -> Vec<(String, String)> {
    let mut pairs = vec![(format!("../../{source}/p0/p1/p2/link"), link.into())];
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
            sync(afc, session, &write_pairs(&source, &link, &names))
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
        let pairs = write_pairs(&source, &link, &names);
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
        &write_pairs(&source, &link, &["pass.json".into()]),
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

fn capture_metadata_for_binding(dir: &Path, binding: Binding) -> Result<()> {
    binding.validate()?;
    ensure!(
        crate::scanner::is_valid_card_hash(&binding.card_id),
        "Select a valid Wallet card"
    );
    let (session, mut afc) = open(&binding)?;
    fs::create_dir(dir).context("Capture directory must be new")?;
    let mut journal = Journal {
        schema_version: 2,
        kind: JournalKind::Capture,
        last_capture_indices: Vec::new(),
        transition_sha256: None,
        observed_metadata_sha256: None,
        binding,
        status: "CapturePrepared".into(),
        books: media::snapshot_books(&mut afc)?,
        roots: Vec::new(),
        held: format!("{}{}", airlift::RECOVERED_PREFIX, random_token()?),
        assets: inventory(&JournalKind::Capture)
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
    let ordinary = recovered_ordinary_indices(&journal);
    if ordinary.is_empty() {
        journal.status = "OrdinaryExportUnprovenMetadataNotTargeted".into();
        save(dir, &journal)?;
        anyhow::bail!(
            "Ordinary artwork export recovered no files; metadata was not targeted. Recovery directory: {}",
            dir.display()
        );
    }
    // Some cards carry only a subset of the ordinary PNG/PDF variants. Prove
    // return and fresh re-export of every observed file before touching metadata;
    // unreturned slots remain Unknown and cannot authorize later writes.
    let recovered = capture_and_return(dir, &mut journal, &mut afc, &session, &ordinary)?;
    require_roundtrip(&recovered, &ordinary)?;
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

fn recovered_ordinary_indices(journal: &Journal) -> Vec<usize> {
    journal.assets[..3]
        .iter()
        .enumerate()
        .filter_map(|(index, asset)| asset.original_sha256.is_some().then_some(index))
        .collect()
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
        sync(afc, session, &write_pairs(&source, &link, &names))
    })();
    media::finish_with_cleanup(transfer, cleanup(dir, journal, afc))
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
    journal.kind == JournalKind::Capture
        && journal.roots.is_empty()
        && matches!(
            journal.status.as_str(),
            "MetadataCapturedAndReappliedAwaitingVisualConfirmation"
                | "CapturedMetadataAndAssetsReappliedAwaitingVisualConfirmation"
        )
}

/// An absent journal or a fully returned capture has no installation to undo.
pub(super) fn no_installation_to_recover(directory: &Path) -> bool {
    directory.try_exists().is_ok_and(|exists| !exists)
        || load(directory).is_ok_and(|journal| installation_ready(&journal))
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::effects) fn recovery_capture_fixture() -> Journal {
        serde_json::from_value(serde_json::json!({
            "schema_version":2, "kind":"Capture",
             "last_capture_indices":[],
            "binding":{"udid":"test-phone","build":"23G83","product_type":"iPhone15,4",
                "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=","transport":"Usb"},
            "status":"MetadataCapturedAndReappliedAwaitingVisualConfirmation",
            "books":{"files":{},"directories":{},"directory_entries":{}},
            "roots":[],"held":"airlift-recovered-0123456789abcdef0123",
            "protected_readback_verified":false,
            "assets":inventory(&JournalKind::Capture).iter().map(|leaf|
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
            "schema_version":2, "kind":"Capture",   "last_capture_indices":[],
            "binding":{"udid":"test-phone", "build":"23G83", "product_type":"iPhone15,4", "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=", "transport":"Usb"},
            "status":"ProtectedCaptureIntent", "books":{"files":{},"directories":{},"directory_entries":{}},
            "roots":[], "held":"airlift-recovered-0123456789abcdef0123", "protected_readback_verified":false,
            "assets":inventory(&JournalKind::Capture).iter().map(|leaf| serde_json::json!({"leaf":leaf,"original_sha256":null,"candidate_sha256":sha256(b"")})).collect::<Vec<_>>()
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
        assert!(original_payloads(&dir, &journal, Some(&[41])).is_err());
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
            "schema_version":2, "kind":"ManagedTransition",   "last_capture_indices":[],
            "transition_sha256":sha256(b"pack"),
            "binding":{"udid":"test-phone", "build":"23G83", "product_type":"iPhone15,4", "card_id":"OM6NYhwXMZrAw0sRUjR62wmF4ZQ=", "transport":"Usb"},
            "status":"ParallaxCandidateReadbackIntent", "books":{"files":{},"directories":{},"directory_entries":{}},
            "roots":[], "held":"airlift-recovered-0123456789abcdef0123", "protected_readback_verified":false,
            "assets":inventory(&JournalKind::ManagedTransition).iter().map(|leaf| serde_json::json!({"leaf":leaf,"original_sha256":sha256(original),"candidate_sha256":sha256(candidate)})).collect::<Vec<_>>()
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
        journal.kind = JournalKind::Capture;
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
                vec![16, 17, 18],
                vec![22, 23, 24],
                vec![28, 29, 30],
                vec![34, 35, 36],
                vec![37]
            ]
        );
        assert!(require_roundtrip(&[], &[3]).is_err());
        journal.kind = JournalKind::ManagedTransition;
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
        let names = inventory(&JournalKind::Capture);
        assert_eq!(names.len(), 41);
        assert_eq!(names[3], "pass.json");
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
        assert_eq!(names.iter().filter(|n| n.ends_with(".pdf")).count(), 19);
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
        let pairs = write_pairs("source", "link", &["pass.json".into()]);
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
    fn ordinary_capture_proof_tracks_every_observed_variant_without_requiring_missing_siblings() {
        for mask in 0..8 {
            let mut journal = recovery_capture_fixture();
            for index in 0..3 {
                journal.assets[index].original_sha256 =
                    (mask & (1 << index) != 0).then(|| sha256(b"original"));
            }
            let expected: Vec<_> = (0..3).filter(|index| mask & (1 << index) != 0).collect();
            let indices = recovered_ordinary_indices(&journal);
            assert_eq!(indices, expected);
            assert!(require_roundtrip(&indices, &expected).is_ok());
            if !indices.is_empty() {
                assert!(require_roundtrip(&indices[1..], &expected).is_err());
            }
        }
    }

    #[test]
    fn combined_delivery_and_restore_publish_link_before_payloads_in_same_manifest() {
        let names: Vec<_> = crate::wallet_backup::CARD_ARTWORK_ASSETS
            .iter()
            .map(|name| (*name).into())
            .collect();
        let combined = write_pairs("source", "link", &names);
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
    }
}
