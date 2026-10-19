use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::afc::AfcClient;
use crate::device::{ActiveDeviceSession, ConnectionMode};

pub const CARD_ARTWORK_ASSETS: [&str; 3] = [
    "cardBackgroundCombined@3x.png",
    "cardBackgroundCombined@2x.png",
    "cardBackgroundCombined.pdf",
];

fn backup_root() -> PathBuf {
    let local_app_data = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| r"C:\Users\Default\AppData\Local".to_string());
    PathBuf::from(local_app_data)
        .join("AirCard")
        .join("wallet-backups")
}

fn backup_dir(udid: &str, card_hash: &str) -> PathBuf {
    backup_root().join(format!(
        "{}-{}",
        safe_component(udid),
        stable_hash(card_hash)
    ))
}

fn safe_component(value: &str) -> String {
    let component: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if component.is_empty() {
        "unknown".to_string()
    } else {
        component
    }
}

fn stable_hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn backup_exists(udid: &str, card_hash: &str) -> bool {
    let dir = backup_dir(udid, card_hash);
    CARD_ARTWORK_ASSETS
        .iter()
        .any(|asset| dir.join(asset).is_file())
}

/// Reset needs every ordinary scale candidate, especially the preferred PDF.
/// A partial backup must not leave custom artwork taking precedence.
pub fn complete_backup_exists(udid: &str, card_hash: &str) -> bool {
    let dir = backup_dir(udid, card_hash);
    CARD_ARTWORK_ASSETS
        .iter()
        .all(|asset| dir.join(asset).is_file())
}

pub fn load_complete_original_assets(
    udid: &str,
    card_hash: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    load_complete_original_directory(&backup_dir(udid, card_hash))
}

fn load_complete_original_directory(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    use std::io::Read;
    CARD_ARTWORK_ASSETS.iter().map(|asset| {
        let path = dir.join(asset);
        let metadata = fs::symlink_metadata(&path)
            .context("Complete original artwork backup unavailable; remove effects while keeping current artwork instead")?;
        anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink(), "Original artwork backup is not a regular file");
        const LIMIT: u64 = 32 * 1024 * 1024;
        anyhow::ensure!(metadata.len() > 0 && metadata.len() <= LIMIT, "Original artwork backup exceeds size limits");
        let mut bytes = Vec::new();
        fs::File::open(&path)?.take(LIMIT + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 == metadata.len(), "Original artwork backup changed while reading");
        Ok(((*asset).to_owned(), bytes))
    }).collect()
}

fn card_hash_candidates(card_hash: &str) -> Vec<String> {
    let trimmed = card_hash.trim_end_matches('=');
    let mut candidates = vec![card_hash.to_string(), trimmed.to_string()];
    if !trimmed.is_empty() {
        candidates.push(format!("{trimmed}="));
        candidates.push(format!("{trimmed}=="));
    }
    candidates.dedup();
    candidates
}

fn find_device_card_hash(afc: &AfcClient, card_hash: &str) -> Result<Option<String>> {
    for candidate in card_hash_candidates(card_hash) {
        let pkpass_dir = format!("/var/mobile/Library/Passes/Cards/{candidate}.pkpass");
        if afc.exists(&pkpass_dir) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

pub fn capture_original_card<L>(
    udid: &str,
    connection_mode: ConnectionMode,
    card_hash: &str,
    mut log: L,
) -> Result<Option<String>>
where
    L: FnMut(&str),
{
    let dir = backup_dir(udid, card_hash);
    fs::create_dir_all(&dir)
        .with_context(|| format!("Could not create backup directory {}", dir.display()))?;

    log("Checking for an existing original Wallet card face backup...");
    let session = ActiveDeviceSession::open(Some(udid), connection_mode)
        .context("Failed to open device session for original card backup")?;
    let afc = AfcClient::new(&session).context("Failed to open AFC for original card backup")?;
    let resolved_hash = match find_device_card_hash(&afc, card_hash)? {
        Some(hash) => hash,
        None if backup_exists(udid, card_hash) => {
            log("Original backup exists, but the current Wallet card directory is not visible.");
            return Ok(Some(card_hash.to_string()));
        }
        None => {
            log(&format!(
                "Wallet card directory not found for hash {} (also tried padded/unpadded variants); continuing without an original backup.",
                card_hash
            ));
            return Ok(None);
        }
    };
    if resolved_hash != card_hash {
        log(&format!(
            "Using device Wallet card hash variant {} for the card path.",
            resolved_hash
        ));
    }
    let pkpass_dir = format!("/var/mobile/Library/Passes/Cards/{}.pkpass", resolved_hash);
    let mut available_assets = 0;

    for asset in CARD_ARTWORK_ASSETS {
        let backup_path = dir.join(asset);
        if backup_path.is_file() {
            available_assets += 1;
            log(&format!("Original backup already contains {}", asset));
            continue;
        }

        let device_path = format!("{pkpass_dir}/{asset}");
        if !afc.exists(&device_path) {
            log(&format!(
                "Original card asset not present on device: {}",
                asset
            ));
            continue;
        }

        let data = afc
            .read_file(&device_path)
            .with_context(|| format!("Failed to read original card asset {}", asset))?;
        write_backup_file(&backup_path, &data)
            .with_context(|| format!("Failed to save original card asset {}", asset))?;
        available_assets += 1;
        log(&format!("Backed up original card asset: {}", asset));
    }

    if available_assets == 0 {
        log(
            "Original Wallet card directory exists, but no backup artwork assets are available; continuing without a restore backup.",
        );
    }

    Ok(Some(resolved_hash))
}

pub fn load_original_assets(udid: &str, card_hash: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let dir = backup_dir(udid, card_hash);
    let mut assets = Vec::new();

    for asset in CARD_ARTWORK_ASSETS {
        let path = dir.join(asset);
        if !path.is_file() {
            continue;
        }
        let data = fs::read(&path)
            .with_context(|| format!("Could not read backup file {}", path.display()))?;
        assets.push((asset.to_string(), data));
    }

    if assets.is_empty() {
        bail!(
            "Original card face backup not found for card hash {}",
            card_hash
        );
    }

    Ok(assets)
}

fn write_backup_file(path: &Path, data: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, data)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_requires_complete_nonempty_backup_and_preserves_exact_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "aircard-reset-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        assert!(load_complete_original_directory(&dir).is_err());
        for (index, leaf) in CARD_ARTWORK_ASSETS.iter().enumerate() {
            fs::write(dir.join(leaf), format!("original-{index}")).unwrap();
            if index < 2 {
                assert!(load_complete_original_directory(&dir).is_err());
            }
        }
        let assets = load_complete_original_directory(&dir).unwrap();
        for (index, (leaf, bytes)) in assets.iter().enumerate() {
            assert_eq!(leaf, CARD_ARTWORK_ASSETS[index]);
            assert_eq!(bytes, format!("original-{index}").as_bytes());
        }
        fs::write(dir.join(CARD_ARTWORK_ASSETS[2]), b"").unwrap();
        assert!(load_complete_original_directory(&dir).is_err());
        for leaf in CARD_ARTWORK_ASSETS {
            fs::remove_file(dir.join(leaf)).unwrap();
        }
        fs::remove_dir(&dir).unwrap();
    }
}
