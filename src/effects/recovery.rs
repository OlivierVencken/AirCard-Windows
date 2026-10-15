//! Device binding, exclusive operation locks, and durable local recovery files

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
        ensure!(
            unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 1 | 8) } != 0,
            "Journal rename failed: {}",
            std::io::Error::last_os_error()
        );
    }

    #[cfg(not(windows))]
    {
        fs::rename(source, destination)?;
        File::open(destination.parent().context("Journal has no parent")?)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn leaves() -> Vec<String> {
    [
        "backgroundParallax",
        "foregroundParallax",
        "dynamicLayerStaticFallback",
    ]
    .into_iter()
    .flat_map(|name| ["", "@2x", "@3x"].map(|scale| format!("{name}{scale}.png")))
    .collect()
}

#[derive(Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum JournalKind {
    // Serialized names remain stable so prior application backups still load
    #[default]
    DynamicPng,
    ArtworkControl,
    MetadataCapture,
    MetadataParallax,
}

pub(super) fn inventory(kind: &JournalKind) -> Vec<String> {
    match kind {
        JournalKind::DynamicPng => leaves(),
        JournalKind::ArtworkControl => crate::wallet_backup::CARD_ARTWORK_ASSETS
            .iter()
            .map(|name| (*name).into())
            .collect(),
        JournalKind::MetadataCapture | JournalKind::MetadataParallax => {
            crate::wallet_backup::CARD_ARTWORK_ASSETS
                .iter()
                .map(|name| (*name).into())
                .chain(std::iter::once("pass.json".into()))
                .chain(
                    [
                        "backgroundParallax",
                        "foregroundParallax",
                        "dynamicLayerStaticFallback",
                    ]
                    .into_iter()
                    .flat_map(|base| {
                        [".png", "@2x.png", "@3x.png", ".pdf", "@2x.pdf", "@3x.pdf"]
                            .map(move |suffix| format!("{base}{suffix}"))
                    }),
                )
                .collect()
        }
    }
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
    #[serde(default)]
    pub(super) kind: JournalKind,
    #[serde(default)]
    pub(super) combined_sync: bool,
    #[serde(default)]
    pub(super) invalidate_caches: bool,
    #[serde(default)]
    pub(super) last_capture_indices: Vec<usize>,
    // Retain the serialized name for compatibility with existing recovery journals.
    #[serde(default)]
    pub(super) trial_pack_sha256: Option<String>,
    #[serde(default)]
    pub(super) observed_metadata_sha256: Option<String>,
    pub(super) binding: Binding,
    pub(super) status: String,
    pub(super) books: BooksSnapshot,
    pub(super) roots: Vec<String>,
    pub(super) held: String,
    pub(super) assets: Vec<Asset>,
    // A missing export slot remains Unknown
    pub(super) protected_readback_verified: bool,
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
        journal.schema_version == 1
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
        journal.kind != JournalKind::ArtworkControl
            || journal
                .assets
                .iter()
                .all(|asset| asset.original_sha256.is_some()),
        "Artwork control requires every original"
    );
    ensure!(
        !journal.protected_readback_verified,
        "Asset recovery cannot claim protected readback"
    );
    ensure!(
        !journal.invalidate_caches || journal.combined_sync,
        "Cache invalidation requires combined asset delivery"
    );
    ensure!(
        !matches!(
            journal.kind,
            JournalKind::MetadataCapture | JournalKind::MetadataParallax
        ) || (journal.combined_sync && journal.invalidate_caches),
        "Invalid metadata capture transfer policy"
    );
    ensure!(
        journal.kind != JournalKind::MetadataParallax
            || journal
                .trial_pack_sha256
                .as_ref()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "Parallax installation requires a bound pack checksum"
    );
    if journal.kind == JournalKind::MetadataParallax {
        ensure!(
            journal
                .assets
                .iter()
                .enumerate()
                .filter(|(i, a)| *i <= 3 || a.leaf.ends_with(".png"))
                .all(|(_, a)| a.original_sha256.is_some()),
            "Parallax recovery requires every overwritten original including metadata"
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

    #[test]
    #[ignore = "Read-only check of an explicitly selected existing recovery journal"]
    fn existing_application_journal_still_loads() {
        let path = PathBuf::from(
            std::env::var_os("AIRCARD_EFFECTS_QA_RECOVERY").expect("Set recovery directory"),
        );
        let journal = load(&path).expect("Existing journal must remain compatible");
        journal.binding.validate().unwrap();
        assert!(matches!(
            journal.kind,
            JournalKind::MetadataCapture | JournalKind::MetadataParallax
        ));
    }
}
