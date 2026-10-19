use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::artwork::sha256;
use super::transport::{self as media, BooksSnapshot};
use crate::airlift;
use crate::device::DeviceTransport;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub udid: String,
    pub build: String,
    pub product_type: String,
    pub card_id: String,
    pub transport: DeviceTransport,
}

impl Binding {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.udid, &self.build, &self.product_type, &self.card_id] {
            media::validate_leaf(value)?;
            ensure!(
                value.len() <= 128 && value.is_ascii() && value != "Unknown",
                "Invalid device/build/card binding"
            );
        }
        ensure!(
            matches!(self.transport, DeviceTransport::Usb | DeviceTransport::Wifi),
            "Unrecognized bound transport"
        );
        Ok(())
    }
}

pub(crate) struct DeviceLock {
    _file: File,
}

impl DeviceLock {
    pub(crate) fn acquire(binding: &Binding) -> Result<Self> {
        binding.validate()?;
        let directory =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?)
                .join("AirCard/wallet-effects-locks");
        fs::create_dir_all(&directory)?;
        ensure!(
            fs::symlink_metadata(&directory)?.is_dir()
                && !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
            "Unsafe lock directory"
        );
        let path = directory.join(format!("{}.lock", sha256(binding.udid.as_bytes())));
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Unsafe lock file"
            );
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = options
            .open(path)
            .context("Device operation locked or inaccessible")?;
        #[cfg(not(windows))]
        file.try_lock()
            .context("Device operation lock unavailable")?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn regular_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= limit as u64,
        "Unsafe or oversized recovery file"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "Recovery file grew beyond limit");
    Ok(bytes)
}

pub(crate) fn durable_replace(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        }
        let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<_> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
       
        for attempt in 0..8 {
            if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 1 | 8) } != 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if attempt == 7 || !matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
                return Err(error).context("Journal rename failed");
            }
            std::thread::sleep(std::time::Duration::from_millis(10 << attempt.min(4)));
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(source, destination)?;
        File::open(destination.parent().context("Journal has no parent")?)?.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum JournalKind {
    Capture,
    ManagedTransition,
}

impl JournalKind {
    pub(super) fn is_installation(self) -> bool {
        self == Self::ManagedTransition
    }
}

pub(super) fn inventory(_: &JournalKind) -> Vec<String> {
    crate::wallet_backup::CARD_ARTWORK_ASSETS
        .iter()
        .map(|name| (*name).into())
        .chain(std::iter::once("pass.json".into()))
        .chain(
            [
                "backgroundParallax",
                "foregroundParallax",
                "dynamicLayerStaticFallback",
                "backgroundParallaxCrossDissolve",
                "transactionEffectEmitter",
            ]
            .into_iter()
            .flat_map(|base| {
                [".png", "@2x.png", "@3x.png", ".pdf", "@2x.pdf", "@3x.pdf"]
                    .map(move |suffix| format!("{base}{suffix}"))
            }),
        )
        .chain(std::iter::once("transactionEffectShape.svg".into()))
        .chain(
            [".png", "@2x.png", "@3x.png", ".pdf", "@2x.pdf", "@3x.pdf"]
                .map(|suffix| format!("neutralEmitter{suffix}")),
        )
        .collect()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Asset {
    pub(super) leaf: String,
    pub(super) original_sha256: Option<String>,
    pub(super) candidate_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub(super) schema_version: u32,
    pub(super) kind: JournalKind,
    pub(super) last_capture_indices: Vec<usize>,
    pub(super) transition_sha256: Option<String>,
    pub(super) observed_metadata_sha256: Option<String>,
    pub(super) binding: Binding,
    pub(super) status: String,
    pub(super) books: BooksSnapshot,
    pub(super) roots: Vec<String>,
    pub(super) held: String,
    pub(super) assets: Vec<Asset>,
    // A missing export slot remains Unknown
    pub(super) protected_readback_verified: bool,
    // Explicit operator opt-in; absence remains Unknown, never a fabricated original.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) allow_missing_cross_dissolve_originals: bool,
    // Narrow transaction trial policy, independent of the alternate-background exception.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) allow_missing_transaction_originals: bool,
    // Independent opt-in, restricted to the three neutral emitter PNGs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) allow_missing_particle_originals: bool,
}

impl Journal {
    pub(super) fn allows_missing_original(&self, asset: &Asset) -> bool {
        (self.allow_missing_particle_originals
            && matches!(
                asset.leaf.as_str(),
                "neutralEmitter.png" | "neutralEmitter@2x.png" | "neutralEmitter@3x.png"
            ))
            || (self.allow_missing_transaction_originals
                && matches!(
                    asset.leaf.as_str(),
                    "transactionEffectEmitter.png"
                        | "transactionEffectEmitter@2x.png"
                        | "transactionEffectEmitter@3x.png"
                        | "transactionEffectShape.svg"
                ))
            || (self.allow_missing_cross_dissolve_originals
                && matches!(
                    asset.leaf.as_str(),
                    "backgroundParallaxCrossDissolve.png"
                        | "backgroundParallaxCrossDissolve@2x.png"
                        | "backgroundParallaxCrossDissolve@3x.png"
                ))
    }

    pub(super) fn incomplete_recovery(&self) -> bool {
        self.kind.is_installation()
            && self.assets.iter().any(|asset| {
                asset.original_sha256.is_none()
                    && self.allows_missing_original(asset)
                    && asset.candidate_sha256 != sha256(b"")
            })
    }
}

pub(super) fn save(dir: &Path, journal: &Journal) -> Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    let envelope = serde_json::to_vec(
        &serde_json::json!({"sha256":sha256(&bytes),"journal":String::from_utf8(bytes)?}),
    )?;
    let temp = dir.join("journal.next");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    file.write_all(&envelope)?;
    file.sync_all()?;
    drop(file);
    durable_replace(&temp, &dir.join("journal.json"))
}

pub(super) fn load(dir: &Path) -> Result<Journal> {
    let envelope: serde_json::Value =
        serde_json::from_slice(&regular_read(&dir.join("journal.json"), 256 * 1024 * 1024)?)?;
    let bytes = envelope["journal"]
        .as_str()
        .context("Invalid journal envelope")?
        .as_bytes();
    ensure!(
        envelope["sha256"].as_str() == Some(sha256(bytes).as_str()),
        "Journal checksum mismatch"
    );
    let journal: Journal = serde_json::from_slice(bytes)?;
    ensure!(
        journal
            .observed_metadata_sha256
            .as_ref()
            .is_none_or(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
        "Invalid observed metadata checksum"
    );
    journal.binding.validate()?;
    ensure!(
        journal.schema_version == 2
            && journal.binding.transport == DeviceTransport::Usb
            && crate::scanner::is_valid_card_hash(&journal.binding.card_id),
        "Invalid asset recovery binding"
    );
    ensure!(
        journal
            .assets
            .iter()
            .map(|a| a.leaf.clone())
            .collect::<Vec<_>>()
            == inventory(&journal.kind),
        "Invalid asset recovery inventory"
    );
    ensure!(
        !journal.protected_readback_verified,
        "Asset recovery cannot claim protected readback"
    );
    ensure!(
        !journal.kind.is_installation()
            || journal
                .transition_sha256
                .as_ref()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "Managed installation requires a bound transition checksum"
    );
    if journal.kind == JournalKind::ManagedTransition {
        ensure!(
            u8::from(journal.allow_missing_particle_originals)
                + u8::from(journal.allow_missing_transaction_originals)
                + u8::from(journal.allow_missing_cross_dissolve_originals)
                <= 1,
            "Managed transitions cannot combine trial policies"
        );
        ensure!(
            journal.assets[3].original_sha256.is_some()
                && journal
                    .assets
                    .iter()
                    .filter(|asset| asset.candidate_sha256 != sha256(b""))
                    .all(|asset| asset.original_sha256.is_some()
                        || journal.allows_missing_original(asset)),
            "Managed recovery requires preimages for every overwritten resource"
        );
    }
    for asset in &journal.assets {
        for hash in std::iter::once(&asset.candidate_sha256).chain(asset.original_sha256.iter()) {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid resource checksum"
            );
        }
    }
    for root in journal.roots.iter().chain(std::iter::once(&journal.held)) {
        validate_root(root)?;
    }
    ensure!(
        journal
            .last_capture_indices
            .iter()
            .all(|i| *i < journal.assets.len())
            && journal
                .last_capture_indices
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
        "Invalid capture result indices"
    );
    Ok(journal)
}

fn validate_root(root: &str) -> Result<()> {
    ensure!(
        [
            airlift::SOURCE_PREFIX,
            airlift::LINK_PREFIX,
            airlift::RECOVERED_PREFIX
        ]
        .iter()
        .any(|prefix| root
            .strip_prefix(prefix)
            .is_some_and(|s| s.len() == 20 && s.bytes().all(|b| b.is_ascii_hexdigit()))),
        "Invalid recovery staging root"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn journal_replacement_waits_for_transient_windows_handle_and_preserves_failed_files() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::Duration;

        let directory =
            std::env::temp_dir().join(format!("aircard-rename-contention-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join("journal.json");
        let pending = directory.join("journal.next");
        fs::write(&destination, b"previous durable journal").unwrap();
        fs::write(&pending, b"next durable journal").unwrap();
        // Allow reads/writes, but deny delete/rename sharing.
        let held = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&destination)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        durable_replace(&pending, &destination).unwrap();
        release.join().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"next durable journal");
        assert!(!pending.exists());

        fs::write(&pending, b"must remain recoverable").unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&destination)
            .unwrap();
        let error = durable_replace(&pending, &destination).unwrap_err();
        assert!(format!("{error:#}").contains("Journal rename failed"));
        assert_eq!(fs::read(&destination).unwrap(), b"next durable journal");
        assert_eq!(fs::read(&pending).unwrap(), b"must remain recoverable");
        drop(held);
        // A retry after contention clears can publish the same pending bytes.
        durable_replace(&pending, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"must remain recoverable");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retired_journal_kinds_and_schema_are_rejected_without_rewriting_them() {
        for kind in [
            "DynamicPng",
            "ArtworkControl",
            "MetadataCapture",
            "MetadataParallax",
            "MetadataCrossDissolveTransactionParticles",
            "FrontFaceCache",
        ] {
            assert!(serde_json::from_value::<JournalKind>(serde_json::json!(kind)).is_err());
        }
        let directory =
            std::env::temp_dir().join(format!("aircard-retired-journal-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let mut journal = super::super::device::tests::recovery_capture_fixture();
        journal.schema_version = 1;
        save(&directory, &journal).unwrap();
        let before = fs::read(directory.join("journal.json")).unwrap();
        assert!(load(&directory).is_err());
        assert_eq!(fs::read(directory.join("journal.json")).unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn device_binding_and_exclusive_lock_reject_conflicting_operations() {
        let mut binding = Binding {
            udid: format!("effects-lock-test-{}", std::process::id()),
            build: "23G83".into(),
            product_type: "iPhone15,4".into(),
            card_id: "test-card".into(),
            transport: DeviceTransport::Usb,
        };
        let lock = DeviceLock::acquire(&binding).unwrap();
        assert!(DeviceLock::acquire(&binding).is_err());
        drop(lock);
        drop(DeviceLock::acquire(&binding).unwrap());
        let path = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
            .join("AirCard/wallet-effects-locks")
            .join(format!("{}.lock", sha256(binding.udid.as_bytes())));
        fs::remove_file(path).unwrap();
        binding.card_id = "../another-card".into();
        assert!(binding.validate().is_err());
        assert!(DeviceLock::acquire(&binding).is_err());
    }
}
