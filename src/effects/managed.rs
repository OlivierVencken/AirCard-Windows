use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::artwork::sha256;
use super::recovery::{self, Binding, JournalKind};
use super::transition::Plan;

const VERSION: u32 = 2;
const LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Blob {
    pub path: PathBuf,
    pub sha256: String,
    pub byte_count: usize,
}

impl Blob {
    pub fn existing(path: PathBuf, expected: &str) -> Result<Self> {
        let bytes = recovery::regular_read(&path, super::transport::MAX_FILE_BYTES)?;
        ensure!(
            sha256(&bytes) == expected,
            "Immutable managed bytes changed"
        );
        Ok(Self {
            path,
            sha256: expected.to_owned(),
            byte_count: bytes.len(),
        })
    }

    pub fn freeze(path: &Path, bytes: &[u8]) -> Result<Self> {
        ensure!(
            path.is_absolute() && bytes.len() <= super::transport::MAX_FILE_BYTES,
            "Invalid managed payload path/size"
        );
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Self::existing(path.to_owned(), &sha256(bytes))
    }

    pub fn read(&self) -> Result<Vec<u8>> {
        ensure!(
            self.path.is_absolute(),
            "Managed bytes need an absolute path"
        );
        let bytes = recovery::regular_read(&self.path, super::transport::MAX_FILE_BYTES)?;
        ensure!(
            bytes.len() == self.byte_count && sha256(&bytes) == self.sha256,
            "Managed payload checksum/size mismatch: {}",
            self.path.display()
        );
        Ok(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    pub binding: Binding,
    pub metadata: Blob,
    // None is Unknown, never known absence or an instruction to delete.
    pub resources: BTreeMap<String, Option<Blob>>,
}

pub(super) fn resource_inventory() -> BTreeSet<String> {
    recovery::inventory(&JournalKind::ManagedTransition)
        .into_iter()
        .filter(|leaf| leaf != "pass.json")
        .collect()
}

impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        ensure!(
            self.resources.keys().cloned().collect::<BTreeSet<_>>() == resource_inventory(),
            "Managed snapshot inventory changed"
        );
        let bytes = self.metadata.read()?;
        let value = super::metadata::parse_unique(&bytes)?;
        ensure!(
            value.as_object().is_some_and(|object| !object.is_empty()),
            "Managed metadata must be a complete nonempty object"
        );
        for blob in self.resources.values().flatten() {
            blob.read()?;
        }
        Ok(())
    }

    pub fn has_effects(&self) -> Result<bool> {
        Ok(super::metadata::parse_unique(&self.metadata.read()?)?
            .get("dynamicLayerConfiguration")
            .is_some())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {
    pub directory: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Mode {
    Apply,
    Undo,
    Baseline,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Operation {
    pub schema_version: u32,
    pub mode: Mode,
    pub source: Snapshot,
    pub target: Snapshot,
    pub previous_head: Option<Reference>,
    pub next_head: Option<Reference>,
    pub inert: BTreeSet<String>,
    pub changes: Vec<super::transition::Change>,
    pub policy: super::transition::TrialPolicy,
    pub recovery: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Phase {
    Prepared,
    Installing,
    Verified,
    Committed,
    Recovered,
    Incomplete,
    RecoveryIncomplete,
    Aborted,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseJournal {
    schema_version: u32,
    manifest_sha256: String,
    phase: Phase,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Index {
    pub schema_version: u32,
    pub binding: Binding,
    pub head: Option<Reference>,
    pub baseline: Option<Snapshot>,
    pub baseline_restore_available: bool,
    pub expected: Option<Snapshot>,
    pub pending: Option<PathBuf>,
}

pub(super) struct Store {
    pub path: PathBuf,
    pub index: Index,
}

/// Also used by ordinary Wallet/Passcode guards after a process restart. A
/// draft's older recovery link cannot hide a newer interrupted Undo on a card.
pub(super) fn pending_for_device(udid: &str) -> Result<bool> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?)
        .join("AirCard/effects-state");
    if !root.try_exists()? {
        return Ok(false);
    }
    validate_directory(&root)?;
    let entries = fs::read_dir(&root)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= 4096,
        "Managed index inventory exceeds limit"
    );
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let index: Index = decode(&recovery::regular_read(&path, LIMIT)?)?;
        ensure!(
            index.schema_version == VERSION,
            "Unsupported managed index version"
        );
        index.binding.validate()?;
        if index.binding.udid == udid && index.pending.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        path.is_absolute() && metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Unsafe managed history directory"
    );
    Ok(())
}

fn envelope<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= LIMIT / 2,
        "Managed record exceeds size limit"
    );
    Ok(serde_json::to_vec(
        &serde_json::json!({"sha256":sha256(&bytes), "data":String::from_utf8(bytes)?}),
    )?)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let value = super::metadata::parse_unique(bytes)?;
    ensure!(
        value.as_object().is_some_and(|object| object.len() == 2),
        "Invalid managed envelope"
    );
    let data = value["data"]
        .as_str()
        .context("Invalid managed record")?
        .as_bytes();
    ensure!(
        value["sha256"].as_str() == Some(sha256(data).as_str()),
        "Managed record checksum mismatch"
    );
    Ok(serde_json::from_value(super::metadata::parse_unique(
        data,
    )?)?)
}

fn atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    validate_directory(path.parent().context("Managed record has no parent")?)?;
    if path.exists() {
        recovery::regular_read(path, LIMIT)?;
    }
    let bytes = envelope(value)?;
    let temporary = path.with_extension("next");
    if temporary.exists() {
        recovery::regular_read(&temporary, LIMIT)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    recovery::durable_replace(&temporary, path)
}

impl Store {
    pub fn default_path(binding: &Binding) -> Result<PathBuf> {
        let root =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?)
                .join("AirCard/effects-state");
        let key = sha256(&serde_json::to_vec(binding)?);
        Ok(root.join(format!("{key}.json")))
    }

    pub fn open(path: PathBuf, binding: &Binding) -> Result<Self> {
        binding.validate()?;
        let index = if path.try_exists()? {
            let index: Index = decode(&recovery::regular_read(&path, LIMIT)?)?;
            ensure!(
                index.schema_version == VERSION && index.binding == *binding,
                "Managed index binding/version mismatch"
            );
            if let Some(snapshot) = &index.expected {
                ensure!(
                    snapshot.binding == *binding,
                    "Expected snapshot binding mismatch"
                );
                snapshot.validate()?;
            }
            if let Some(snapshot) = &index.baseline {
                ensure!(snapshot.binding == *binding, "Baseline binding mismatch");
                snapshot.validate()?;
            }
            if let Some(reference) = &index.head {
                super::transition::reconcile(
                    index
                        .expected
                        .as_ref()
                        .context("Managed head has no expected state")?,
                    &load_operation(reference, binding)?.target,
                )?;
                let mut next = Some(reference.clone());
                let mut visited = BTreeSet::new();
                let mut child_source: Option<Snapshot> = None;
                while let Some(reference) = next {
                    ensure!(
                        visited.len() < 1024 && visited.insert(reference.directory.clone()),
                        "Managed history is cyclic or too deep"
                    );
                    let operation = load_operation(&reference, binding)?;
                    if let Some(source) = child_source {
                        super::transition::reconcile(&source, &operation.target)?;
                    }
                    child_source = Some(operation.source);
                    next = operation.previous_head;
                }
            }
            if let Some(pending) = &index.pending {
                validate_directory(pending)?;
            }
            index
        } else {
            Index {
                schema_version: VERSION,
                binding: binding.clone(),
                head: None,
                baseline: None,
                baseline_restore_available: false,
                expected: None,
                pending: None,
            }
        };
        Ok(Self { path, index })
    }

    pub fn save(&self) -> Result<()> {
        let parent = self.path.parent().context("Managed index has no parent")?;
        fs::create_dir_all(parent)?;
        atomic(&self.path, &self.index)
    }

    pub fn begin(&mut self, directory: &Path) -> Result<()> {
        ensure!(
            self.index.pending.is_none(),
            "Recover the pending managed operation first"
        );
        validate_directory(directory)?;
        self.index.pending = Some(directory.to_owned());
        self.save()
    }

    pub fn ensure_device_ready(&self) -> Result<()> {
        let Some(parent) = self.path.parent() else {
            anyhow::bail!("Managed index has no parent");
        };
        if !parent.try_exists()? {
            return Ok(());
        }
        validate_directory(parent)?;
        let entries = fs::read_dir(parent)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            entries.len() <= 4096,
            "Managed device index inventory exceeds limit"
        );
        for entry in entries {
            let path = entry.path();
            if path == self.path
                || path.extension().and_then(|extension| extension.to_str()) != Some("json")
            {
                continue;
            }
            let index: Index = decode(&recovery::regular_read(&path, LIMIT)?)?;
            ensure!(
                index.schema_version == VERSION,
                "Unsupported managed index version"
            );
            index.binding.validate()?;
            ensure!(
                index.binding.udid != self.index.binding.udid || index.pending.is_none(),
                "Another card/build on this device has pending recovery: {}",
                index.binding.card_id
            );
        }
        Ok(())
    }

    /// Per-resource origins come from the first proven write in the active
    /// chain. An Unknown first preimage stays Unknown even if a later capture
    /// recovers matching candidate bytes.
    pub fn resource_baseline(&self) -> Result<Option<Snapshot>> {
        let Some(mut baseline) = self.index.baseline.clone() else {
            return Ok(None);
        };
        let mut chain = Vec::new();
        let mut next = self.index.head.clone();
        let mut visited = BTreeSet::new();
        while let Some(reference) = next {
            ensure!(
                visited.len() < 1024 && visited.insert(reference.directory.clone()),
                "Invalid resource ancestry"
            );
            let operation = load_operation(&reference, &self.index.binding)?;
            next = operation.previous_head.clone();
            chain.push(operation);
        }
        let mut touched = BTreeSet::new();
        for operation in chain.into_iter().rev() {
            for change in operation.changes {
                if touched.insert(change.leaf.clone()) {
                    baseline.resources.insert(
                        change.leaf.clone(),
                        operation.source.resources[&change.leaf].clone(),
                    );
                }
            }
        }
        Ok(Some(baseline))
    }

    pub fn prepare(
        &self,
        directory: &Path,
        mode: Mode,
        source: Snapshot,
        plan: Plan,
        next_head: Option<Reference>,
    ) -> Result<Reference> {
        ensure!(
            self.index.pending.as_deref() == Some(directory),
            "Managed intent was not persisted"
        );
        let mut target = source.clone();
        if sha256(&plan.metadata) != source.metadata.sha256 {
            target.metadata = Blob::freeze(&directory.join("target-pass.json"), &plan.metadata)?;
        }
        for change in &plan.changes {
            target
                .resources
                .insert(change.leaf.clone(), Some(change.after.clone()));
        }
        let operation = Operation {
            schema_version: VERSION,
            mode,
            source,
            target,
            previous_head: self.index.head.clone(),
            next_head,
            inert: plan.inert,
            changes: plan.changes,
            policy: plan.policy,
            recovery: directory.join("recovery"),
        };
        self.freeze_operation(directory, &operation)
    }

    pub fn freeze_operation(&self, directory: &Path, operation: &Operation) -> Result<Reference> {
        operation.validate(&self.index.binding)?;
        let bytes = envelope(operation)?;
        Blob::freeze(&directory.join("transition.json"), &bytes)?;
        let reference = Reference {
            directory: directory.to_owned(),
            sha256: sha256(&bytes),
        };
        set_phase(&reference, Phase::Prepared)?;
        Ok(reference)
    }

    /// Commit only after the durable verification phase. Repeating this after a
    /// crash between verification and index replacement is safe and idempotent.
    pub fn commit(&mut self, reference: &Reference) -> Result<bool> {
        let phase = phase(reference)?;
        ensure!(
            matches!(
                phase,
                Phase::Verified | Phase::Committed | Phase::Incomplete
            ),
            "Managed operation has not been verified"
        );
        let operation = load_operation(reference, &self.index.binding)?;
        let is_apply = operation.mode == Mode::Apply;
        let next_head = if is_apply {
            Some(reference.clone())
        } else {
            operation.next_head.clone()
        };
        if self.index.pending.is_none()
            && self.index.expected.as_ref() == Some(&operation.target)
            && self.index.head == next_head
        {
            set_phase(reference, Phase::Committed)?;
            return Ok(false);
        }
        ensure!(
            self.index.pending.as_deref() == Some(reference.directory.as_path()),
            "Managed pending pointer changed"
        );
        ensure!(
            self.index.head == operation.previous_head,
            "Managed history changed during an operation"
        );
        if !operation.inert.is_empty() {
            set_phase(reference, Phase::Incomplete)?;
            // Retain the pending selection; no competing write may proceed.
            return Ok(true);
        }
        if self.index.baseline.is_none() && is_apply && self.index.head.is_none() {
            self.index.baseline_restore_available = !operation.source.has_effects()?;
            self.index.baseline = Some(operation.source.clone());
        }
        self.index.expected = Some(operation.target);
        self.index.head = next_head;
        self.index.pending = None;
        self.save()?;
        set_phase(reference, Phase::Committed)?;
        Ok(false)
    }

    pub fn abort_after_verified_recovery(&mut self) -> Result<()> {
        self.index.pending = None;
        self.save()
    }
}

impl Operation {
    fn validate(&self, binding: &Binding) -> Result<()> {
        ensure!(
            self.schema_version == VERSION
                && self.source.binding == *binding
                && self.target.binding == *binding,
            "Invalid managed transition binding/version"
        );
        self.source.validate()?;
        self.target.validate()?;
        let mut names = BTreeSet::new();
        for change in &self.changes {
            ensure!(
                names.insert(&change.leaf) && self.source.resources.contains_key(&change.leaf),
                "Duplicate/unknown managed resource"
            );
            ensure!(
                change.expected_preimage
                    == self.source.resources[&change.leaf]
                        .as_ref()
                        .map(|blob| blob.sha256.clone()),
                "Managed preimage does not match captured bytes"
            );
            ensure!(
                self.target.resources[&change.leaf].as_ref() == Some(&change.after),
                "Managed target payload differs"
            );
            change.after.read()?;
        }
        for (leaf, target) in &self.target.resources {
            if !names.contains(leaf) {
                ensure!(
                    self.source.resources[leaf] == *target,
                    "Unjournaled resource change: {leaf}"
                );
            }
        }
        ensure!(
            self.inert
                .iter()
                .all(|leaf| self.source.resources.contains_key(leaf) && !names.contains(leaf)),
            "Invalid inert-resource inventory"
        );
        Ok(())
    }
}

pub(super) fn load_operation(reference: &Reference, binding: &Binding) -> Result<Operation> {
    validate_directory(&reference.directory)?;
    let bytes = recovery::regular_read(&reference.directory.join("transition.json"), LIMIT)?;
    ensure!(
        sha256(&bytes) == reference.sha256,
        "Managed transition manifest changed"
    );
    let operation: Operation = decode(&bytes)?;
    operation.validate(binding)?;
    Ok(operation)
}

pub(super) fn reference(directory: &Path) -> Result<Reference> {
    let bytes = recovery::regular_read(&directory.join("transition.json"), LIMIT)?;
    Ok(Reference {
        directory: directory.to_owned(),
        sha256: sha256(&bytes),
    })
}

pub(super) fn set_phase(reference: &Reference, phase: Phase) -> Result<()> {
    atomic(
        &reference.directory.join("transition-state.json"),
        &PhaseJournal {
            schema_version: VERSION,
            manifest_sha256: reference.sha256.clone(),
            phase,
        },
    )
}

pub(super) fn phase(reference: &Reference) -> Result<Phase> {
    let journal: PhaseJournal = decode(&recovery::regular_read(
        &reference.directory.join("transition-state.json"),
        LIMIT,
    )?)?;
    ensure!(
        journal.schema_version == VERSION && journal.manifest_sha256 == reference.sha256,
        "Transition phase belongs to another manifest"
    );
    Ok(journal.phase)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    pub struct Fixture {
        pub root: PathBuf,
        next: Cell<usize>,
        pub binding: Binding,
    }
    impl Fixture {
        pub fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "aircard-managed-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self {
                root,
                next: Cell::new(0),
                binding: Binding {
                    udid: "managed-fixture".into(),
                    build: "23G83".into(),
                    product_type: "iPhone15,4".into(),
                    card_id: "OM6NYhwXMZrAw0sRUjR62wmF4ZQ=".into(),
                    transport: crate::device::DeviceTransport::Usb,
                },
            }
        }
        pub fn blob(&self, bytes: &[u8]) -> Blob {
            let next = self.next.get();
            self.next.set(next + 1);
            Blob::freeze(&self.root.join(format!("blob-{next}.bin")), bytes).unwrap()
        }
        pub fn snapshot(&self, bits: u8, prefix: &str) -> Snapshot {
            let mut value = serde_json::json!({"serialNumber":"preserved", "unrelated":{"array":[null,1,true]}});
            if let Some(dynamic) = super::super::transition::from_bits(bits).native_configuration()
            {
                value["dynamicLayerConfiguration"] = serde_json::to_value(dynamic).unwrap();
            }
            Snapshot {
                binding: self.binding.clone(),
                metadata: self.blob(&serde_json::to_vec(&value).unwrap()),
                resources: resource_inventory()
                    .into_iter()
                    .map(|leaf| {
                        let blob = if leaf.ends_with(".pdf") && leaf != "cardBackgroundCombined.pdf"
                        {
                            None
                        } else {
                            Some(self.blob(format!("{prefix}:{leaf}").as_bytes()))
                        };
                        (leaf, blob)
                    })
                    .collect(),
            }
        }
        pub fn artwork(&self, bits: u8, prefix: &str) -> BTreeMap<String, Blob> {
            let names = if bits == 0 {
                crate::wallet_backup::CARD_ARTWORK_ASSETS
                    .iter()
                    .map(|leaf| (*leaf).to_owned())
                    .collect()
            } else {
                super::super::transition::resource_names(super::super::transition::from_bits(bits))
            };
            names
                .into_iter()
                .map(|leaf| {
                    let blob = self.blob(format!("{prefix}:{leaf}").as_bytes());
                    (leaf, blob)
                })
                .collect()
        }
        pub fn directory(&self) -> PathBuf {
            let next = self.next.get();
            self.next.set(next + 1);
            let directory = self.root.join(format!("operation-{next}"));
            fs::create_dir(&directory).unwrap();
            directory
        }
        pub fn store(&self) -> Store {
            Store::open(self.root.join("index.json"), &self.binding).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn apply(fixture: &Fixture, store: &mut Store, source: Snapshot, bits: u8) -> Reference {
        use super::super::transition;
        let directory = fixture.directory();
        let artwork = fixture.artwork(bits, &format!("target-{bits}"));
        let plan = transition::apply(
            &source,
            store.index.expected.as_ref(),
            store.index.baseline.as_ref(),
            store.index.head.is_some(),
            transition::from_bits(bits),
            &artwork,
            Default::default(),
        )
        .unwrap();
        store.begin(&directory).unwrap();
        let reference = store
            .prepare(&directory, Mode::Apply, source, plan, None)
            .unwrap();
        assert!(
            store.commit(&reference).is_err(),
            "Prepared is not a verification record"
        );
        set_phase(&reference, Phase::Verified).unwrap();
        assert!(!store.commit(&reference).unwrap());
        reference
    }

    #[test]
    fn chain_undo_baseline_and_restart_follow_proven_preimages() {
        use super::super::transition;
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let baseline = fixture.snapshot(0, "original");
        let a = apply(&fixture, &mut store, baseline.clone(), 1);
        let source = store.index.expected.clone().unwrap();
        let b = apply(&fixture, &mut store, source, 7);
        let source = store.index.expected.clone().unwrap();
        let c = apply(&fixture, &mut store, source, 15);
        assert!(store.index.baseline_restore_available);
        for (head, expected_head) in [(&c, Some(b.clone())), (&b, Some(a.clone())), (&a, None)] {
            store = fixture.store();
            assert_eq!(store.index.head.as_ref(), Some(head));
            let operation = load_operation(head, &fixture.binding).unwrap();
            let source = store.index.expected.clone().unwrap();
            let plan = transition::restore(&source, &source, &operation.source).unwrap();
            let directory = fixture.directory();
            store.begin(&directory).unwrap();
            let undo = store
                .prepare(
                    &directory,
                    Mode::Undo,
                    source,
                    plan,
                    operation.previous_head,
                )
                .unwrap();
            set_phase(&undo, Phase::Verified).unwrap();
            assert!(!store.commit(&undo).unwrap());
            assert_eq!(store.index.head, expected_head);
        }
        transition::reconcile(store.index.expected.as_ref().unwrap(), &baseline).unwrap();
        assert!(a.directory.exists() && b.directory.exists() && c.directory.exists());
        let source = store.index.expected.clone().unwrap();
        apply(&fixture, &mut store, source, 15);
        let source = store.index.expected.clone().unwrap();
        let target = store.index.baseline.clone().unwrap();
        let plan = transition::restore(&source, &source, &target).unwrap();
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let restore = store
            .prepare(&directory, Mode::Baseline, source, plan, None)
            .unwrap();
        set_phase(&restore, Phase::Verified).unwrap();
        store.commit(&restore).unwrap();
        assert!(store.index.head.is_none());
        transition::reconcile(store.index.expected.as_ref().unwrap(), &baseline).unwrap();
    }

    #[test]
    fn restart_between_verification_and_index_commit_is_idempotent() {
        use super::super::transition;
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let source = fixture.snapshot(0, "original");
        let plan = transition::apply(
            &source,
            None,
            None,
            false,
            transition::from_bits(1),
            &fixture.artwork(1, "target"),
            Default::default(),
        )
        .unwrap();
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let reference = store
            .prepare(&directory, Mode::Apply, source, plan, None)
            .unwrap();
        let mut restarted = fixture.store();
        assert!(restarted.begin(&fixture.directory()).is_err());
        assert!(restarted.commit(&reference).is_err());
        set_phase(&reference, Phase::Installing).unwrap();
        assert!(restarted.commit(&reference).is_err());
        set_phase(&reference, Phase::Verified).unwrap();
        restarted.commit(&reference).unwrap();
        // Simulate an index replacement succeeding just before its phase write.
        set_phase(&reference, Phase::Verified).unwrap();
        let mut restarted = fixture.store();
        assert!(!restarted.commit(&reference).unwrap());
        assert_eq!(restarted.index.head.as_ref(), Some(&reference));
        assert_eq!(phase(&reference).unwrap(), Phase::Committed);
    }

    #[test]
    fn explicit_reset_after_history_exhaustion_is_undoable_without_inventing_baseline() {
        use super::super::transition;
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let source = fixture.snapshot(15, "effectful-preimage");
        store.index.expected = Some(source.clone());
        store.index.baseline = Some(source.clone());
        store.index.baseline_restore_available = false;
        assert!(store.index.head.is_none());
        let plan = transition::reset(
            &source,
            Some(&source),
            Some(&source),
            &fixture.artwork(0, "saved-original"),
        )
        .unwrap();
        assert!(plan.inert.is_empty());
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let reset = store
            .prepare(&directory, Mode::Apply, source.clone(), plan, None)
            .unwrap();
        set_phase(&reset, Phase::Verified).unwrap();
        assert!(!store.commit(&reset).unwrap());
        let mut store = Store::open(store.path.clone(), &fixture.binding).unwrap();
        assert!(
            !store
                .index
                .expected
                .as_ref()
                .unwrap()
                .has_effects()
                .unwrap()
        );
        assert!(!store.index.baseline_restore_available);
        let current = store.index.expected.clone().unwrap();
        let plan = transition::restore(&current, &current, &source).unwrap();
        let undo_dir = fixture.directory();
        store.begin(&undo_dir).unwrap();
        let undo = store
            .prepare(&undo_dir, Mode::Undo, current, plan, None)
            .unwrap();
        set_phase(&undo, Phase::Verified).unwrap();
        assert!(!store.commit(&undo).unwrap());
        transition::reconcile(store.index.expected.as_ref().unwrap(), &source).unwrap();
        assert!(store.index.head.is_none() && reset.directory.exists());
    }

    #[test]
    fn failed_index_write_retains_verified_pending_state_for_restart() {
        use super::super::transition;
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let source = fixture.snapshot(0, "original");
        let plan = transition::apply(
            &source,
            None,
            None,
            false,
            transition::from_bits(1),
            &fixture.artwork(1, "target"),
            Default::default(),
        )
        .unwrap();
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let reference = store
            .prepare(&directory, Mode::Apply, source.clone(), plan, None)
            .unwrap();
        set_phase(&reference, Phase::Verified).unwrap();
        let blocker = fixture.root.join("not-a-directory");
        fs::write(&blocker, b"blocker").unwrap();
        store.path = blocker.join("index.json");
        assert!(store.commit(&reference).is_err());
        let mut restarted = fixture.store();
        assert_eq!(
            restarted.index.pending.as_deref(),
            Some(directory.as_path())
        );
        assert!(restarted.index.head.is_none());
        restarted.commit(&reference).unwrap();
        assert_eq!(restarted.index.head.as_ref(), Some(&reference));
        assert!(
            source.metadata.read().is_ok(),
            "Immutable preimages survive index failures"
        );
    }

    #[test]
    fn unknown_first_preimages_never_become_originals_from_later_readbacks() {
        use super::super::transition;
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let mut source = fixture.snapshot(0, "original");
        for leaf in [
            "neutralEmitter.png",
            "neutralEmitter@2x.png",
            "neutralEmitter@3x.png",
        ] {
            source.resources.insert(leaf.into(), None);
        }
        let artwork = fixture.artwork(5, "first-trial");
        let plan = transition::apply(
            &source,
            None,
            None,
            false,
            transition::from_bits(5),
            &artwork,
            transition::TrialPolicy {
                particles: true,
                ..Default::default()
            },
        )
        .unwrap();
        let directory = fixture.directory();
        store.begin(&directory).unwrap();
        let reference = store
            .prepare(&directory, Mode::Apply, source, plan, None)
            .unwrap();
        set_phase(&reference, Phase::Verified).unwrap();
        store.commit(&reference).unwrap();
        let source = store.index.expected.clone().unwrap();
        // Subsequent capture can recover the installed candidate, but it is a
        // later preimage and cannot fill the earliest Unknown origin.
        apply(&fixture, &mut store, source, 5);
        let baseline = store.resource_baseline().unwrap().unwrap();
        assert!(baseline.resources["neutralEmitter.png"].is_none());
        let source = store.index.expected.as_ref().unwrap();
        let plan = transition::apply(
            source,
            Some(source),
            Some(&baseline),
            true,
            transition::from_bits(1),
            &fixture.artwork(1, "remove"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(plan.inert.len(), 3);
    }

    #[test]
    fn corrupt_or_cross_bound_records_and_pending_other_cards_block_changes() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        let reference = apply(&fixture, &mut store, fixture.snapshot(0, "original"), 1);
        let mut changed_binding = fixture.binding.clone();
        changed_binding.build = "23G84".into();
        assert!(Store::open(store.path.clone(), &changed_binding).is_err());
        let mut other = Store::open(fixture.root.join("other.json"), &fixture.binding).unwrap();
        // Different card index, same device: interrupted work is not bypassed.
        other.index.binding.card_id = "another-card".into();
        other.begin(&fixture.directory()).unwrap();
        assert!(store.ensure_device_ready().is_err());
        fs::write(reference.directory.join("transition.json"), b"{}").unwrap();
        assert!(fixture.store_open_result().is_err());
    }

    impl Fixture {
        fn store_open_result(&self) -> Result<Store> {
            Store::open(self.root.join("index.json"), &self.binding)
        }
    }
}
