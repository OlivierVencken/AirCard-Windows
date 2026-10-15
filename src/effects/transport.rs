//! Checked Media I/O for effects transfers and Books recovery

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::artwork::sha256;
use crate::airlift::TRACKED_BOOKS_FILES;

pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 8192;
const BOOKS_DIRECTORIES: [&str; 3] = ["Books", "Books/Sync", "Books/Sync/Database"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    pub kind: FileKind,
    pub size: u64,
}

/// Propagate access errors and return complete listings
/// Unlink only the named entry
pub trait MediaIo {
    fn stat(&mut self, path: &str) -> Result<FileInfo>;
    fn list(&mut self, path: &str) -> Result<Vec<String>>;
    fn read(&mut self, path: &str, limit: usize) -> Result<Vec<u8>>;
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<()>;
    fn mkdir(&mut self, path: &str) -> Result<()>;
    fn unlink(&mut self, path: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", deny_unknown_fields)]
pub enum OriginalState {
    Present {
        bytes: Vec<u8>,
        sha256: String,
    },
    VerifiedAbsent {
        readable_parent: String,
        missing_component: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", deny_unknown_fields)]
pub enum DirectoryState {
    Present,
    VerifiedAbsent {
        readable_parent: String,
        missing_component: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BooksSnapshot {
    pub files: BTreeMap<String, OriginalState>,
    pub directories: BTreeMap<String, DirectoryState>,
    /// Complete sorted child names for every originally present directory
    pub directory_entries: BTreeMap<String, Vec<String>>,
}

pub fn validate_leaf(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "Unsafe directory entry"
    );
    Ok(())
}

pub fn validate_media_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 1024 && path.split('/').count() <= 32,
        "Unsafe Media path length/depth"
    );
    for component in path.split('/') {
        validate_leaf(component)?;
    }
    Ok(())
}

pub fn collect_directory(mut next: impl FnMut() -> Result<Option<String>>) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    let mut reads = 0;
    while let Some(name) = next()? {
        reads += 1;
        // Include dot entries in the bound so a malformed endless stream fails
        ensure!(
            reads <= MAX_DIRECTORY_ENTRIES + 2,
            "Directory exceeds entry limit; results are incomplete"
        );
        if name == "." || name == ".." {
            continue;
        }
        validate_leaf(&name)?;
        ensure!(
            names.len() < MAX_DIRECTORY_ENTRIES,
            "Directory exceeds entry limit; results are incomplete"
        );
        ensure!(
            names.insert(name),
            "Duplicate directory entry; results are ambiguous"
        );
    }
    Ok(names.into_iter().collect())
}

/// Keep transfer/read errors and close/cleanup errors together
pub fn finish_with_cleanup<T>(operation: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            bail!("Operation failed: {error:#}; cleanup also failed: {cleanup:#}")
        }
    }
}

pub fn read_exact_bounded(
    size: u64,
    limit: usize,
    mut read: impl FnMut(&mut [u8]) -> Result<usize>,
) -> Result<Vec<u8>> {
    ensure!(
        limit <= MAX_FILE_BYTES && size <= limit as u64,
        "Read exceeds effects file limit"
    );
    let size = usize::try_from(size)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size)?;
    bytes.resize(size, 0);
    let mut offset = 0;
    while offset < size {
        let end = (offset + 64 * 1024).min(size);
        let length = read(&mut bytes[offset..end])?;
        ensure!(
            length > 0 && length <= end - offset,
            "Short read or invalid native read count"
        );
        offset += length;
    }
    ensure!(read(&mut [0u8; 1])? == 0, "File grew during capture");
    Ok(bytes)
}

enum Located {
    Present(FileInfo),
    Absent { parent: String, component: String },
}

/// Verify every ancestor from the readable AFC Media root
fn locate(io: &mut impl MediaIo, path: &str) -> Result<Located> {
    validate_media_path(path)?;
    let mut parent = "/".to_string();
    let components: Vec<_> = path.split('/').collect();
    for (index, component) in components.iter().enumerate() {
        ensure!(
            io.stat(&parent)?.kind == FileKind::Directory,
            "Ancestor is not a real directory: {parent}"
        );
        let entries = io.list(&parent)?;
        let mut iterator = entries.into_iter();
        let entries = collect_directory(|| Ok(iterator.next()))?;
        if !entries.iter().any(|entry| entry == component) {
            return Ok(Located::Absent {
                parent,
                component: (*component).into(),
            });
        }
        parent = if parent == "/" {
            (*component).into()
        } else {
            format!("{parent}/{component}")
        };
        let info = io.stat(&parent)?;
        if index + 1 == components.len() {
            return Ok(Located::Present(info));
        }
        ensure!(
            info.kind == FileKind::Directory,
            "Refusing symlink/non-directory ancestor: {parent}"
        );
    }
    unreachable!()
}

pub fn observe(io: &mut impl MediaIo, path: &str) -> OriginalState {
    let mut capture = || -> Result<OriginalState> {
        match locate(io, path)? {
            Located::Absent { parent, component } => Ok(OriginalState::VerifiedAbsent {
                readable_parent: parent,
                missing_component: component,
            }),
            Located::Present(info) => {
                ensure!(info.kind == FileKind::Regular, "Not a regular file: {path}");
                ensure!(
                    info.size <= MAX_FILE_BYTES as u64,
                    "File exceeds capture limit: {path}"
                );
                let bytes = io.read(path, MAX_FILE_BYTES)?;
                ensure!(
                    bytes.len() as u64 == info.size && io.stat(path)? == info,
                    "File changed during capture: {path}"
                );
                ensure!(
                    bytes.len() <= MAX_FILE_BYTES,
                    "Transport returned an oversized file"
                );
                Ok(OriginalState::Present {
                    sha256: sha256(&bytes),
                    bytes,
                })
            }
        }
    };
    match capture() {
        Ok(state) => state,
        Err(error) => OriginalState::Unknown {
            reason: format!("{error:#}"),
        },
    }
}

pub fn require_known(state: &OriginalState) -> Result<()> {
    match state {
        OriginalState::Unknown { reason } => bail!("Original state is unknown: {reason}"),
        OriginalState::Present {
            bytes,
            sha256: hash,
        } => ensure!(
            bytes.len() <= MAX_FILE_BYTES && sha256(bytes) == *hash,
            "Original bytes/hash are corrupt"
        ),
        OriginalState::VerifiedAbsent {
            readable_parent,
            missing_component,
        } => {
            if readable_parent != "/" {
                validate_media_path(readable_parent)?;
            }
            validate_leaf(missing_component)?;
        }
    }
    Ok(())
}

pub fn snapshot_books(io: &mut impl MediaIo) -> Result<BooksSnapshot> {
    let mut files = BTreeMap::new();
    let mut total_bytes = 0;
    for path in TRACKED_BOOKS_FILES {
        let state = observe(io, path);
        require_known(&state)?;
        if let OriginalState::Present { bytes, .. } = &state {
            total_bytes += bytes.len();
        }
        ensure!(
            total_bytes <= MAX_SNAPSHOT_BYTES,
            "Books snapshot exceeds total byte limit"
        );
        files.insert((*path).into(), state);
    }
    let mut directories = BTreeMap::new();
    for path in BOOKS_DIRECTORIES {
        let state = match locate(io, path)? {
            Located::Present(info) => {
                ensure!(
                    info.kind == FileKind::Directory,
                    "Books directory is not a real directory: {path}"
                );
                io.list(path)?; // Require that it is readable before staging.
                DirectoryState::Present
            }
            Located::Absent { parent, component } => DirectoryState::VerifiedAbsent {
                readable_parent: parent,
                missing_component: component,
            },
        };
        directories.insert(path.into(), state);
    }
    let mut directory_entries = BTreeMap::new();
    // Capture the entire affected Books tree, not just conventional sync files
    fn walk(
        io: &mut impl MediaIo,
        path: &str,
        files: &mut BTreeMap<String, OriginalState>,
        directories: &mut BTreeMap<String, DirectoryState>,
        entries: &mut BTreeMap<String, Vec<String>>,
        bytes: &mut usize,
        nodes: &mut usize,
    ) -> Result<()> {
        validate_media_path(path)?;
        let mut names = io.list(path)?.into_iter();
        let names = collect_directory(|| Ok(names.next()))?;
        entries.insert(path.into(), names.clone());
        for name in &names {
            *nodes += 1;
            ensure!(
                *nodes <= MAX_DIRECTORY_ENTRIES,
                "Books tree exceeds total node limit"
            );
            let child = format!("{path}/{name}");
            match io.stat(&child)?.kind {
                FileKind::Directory => {
                    directories.insert(child.clone(), DirectoryState::Present);
                    walk(io, &child, files, directories, entries, bytes, nodes)?;
                }
                FileKind::Regular => {
                    let state = observe(io, &child);
                    require_known(&state)?;
                    ensure!(
                        matches!(state, OriginalState::Present { .. }),
                        "Books child disappeared during capture"
                    );
                    if !files.contains_key(&child)
                        && let OriginalState::Present { bytes: content, .. } = &state
                    {
                        *bytes += content.len();
                    }
                    ensure!(
                        *bytes <= MAX_SNAPSHOT_BYTES,
                        "Books snapshot exceeds total byte limit"
                    );
                    files.insert(child, state);
                }
                _ => bail!(
                    "Books contains an unsupported kind at {child}; no reversible staging route"
                ),
            }
        }
        let mut after = io.list(path)?.into_iter();
        ensure!(
            collect_directory(|| Ok(after.next()))? == names,
            "Books tree changed during capture"
        );
        Ok(())
    }
    if matches!(directories["Books"], DirectoryState::Present) {
        walk(
            io,
            "Books",
            &mut files,
            &mut directories,
            &mut directory_entries,
            &mut total_bytes,
            &mut 1,
        )?;
    }
    let snapshot = BooksSnapshot {
        files,
        directories,
        directory_entries,
    };
    validate_snapshot(&snapshot)?;
    verify_books(io, &snapshot)?;
    Ok(snapshot)
}

fn validate_absence(path: &str, parent: &str, component: &str) -> Result<()> {
    if parent != "/" {
        validate_media_path(parent)?;
    }
    validate_leaf(component)?;
    let missing = if parent == "/" {
        component.to_string()
    } else {
        format!("{parent}/{component}")
    };
    ensure!(
        path == missing || path.starts_with(&format!("{missing}/")),
        "Absence evidence does not cover destination"
    );
    Ok(())
}

pub fn validate_snapshot(snapshot: &BooksSnapshot) -> Result<()> {
    ensure!(
        snapshot.files.len() >= TRACKED_BOOKS_FILES.len()
            && snapshot.directories.len() >= BOOKS_DIRECTORIES.len()
            && snapshot.files.len() + snapshot.directories.len()
                <= MAX_DIRECTORY_ENTRIES + TRACKED_BOOKS_FILES.len() + BOOKS_DIRECTORIES.len(),
        "Incomplete Books snapshot"
    );
    let mut total_bytes = 0;
    for path in TRACKED_BOOKS_FILES {
        ensure!(
            snapshot.files.contains_key(*path),
            "Missing tracked Books file: {path}"
        );
    }
    for path in snapshot.files.keys() {
        validate_media_path(path)?;
        ensure!(path.starts_with("Books/"), "Snapshot file outside Books");
        ensure!(
            !snapshot.directories.contains_key(path),
            "Conflicting entry kinds"
        );
        let state = snapshot
            .files
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("Missing Books file: {path}"))?;
        require_known(state)?;
        match state {
            OriginalState::Present { bytes, .. } => total_bytes += bytes.len(),
            OriginalState::VerifiedAbsent {
                readable_parent,
                missing_component,
            } => validate_absence(path, readable_parent, missing_component)?,
            OriginalState::Unknown { .. } => unreachable!(),
        }
    }
    ensure!(
        total_bytes <= MAX_SNAPSHOT_BYTES,
        "Books snapshot exceeds total byte limit"
    );
    for path in BOOKS_DIRECTORIES {
        ensure!(
            snapshot.directories.contains_key(path),
            "Missing tracked Books directory: {path}"
        );
    }
    for path in snapshot.directories.keys() {
        validate_media_path(path)?;
        ensure!(
            path == "Books" || path.starts_with("Books/"),
            "Snapshot directory outside Books"
        );
        let state = snapshot
            .directories
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("Missing Books directory: {path}"))?;
        if let DirectoryState::VerifiedAbsent {
            readable_parent,
            missing_component,
        } = state
        {
            validate_absence(path, readable_parent, missing_component)?;
            for (file, state) in &snapshot.files {
                ensure!(
                    !file.starts_with(&format!("{path}/"))
                        || !matches!(state, OriginalState::Present { .. }),
                    "Snapshot contains files under an absent directory"
                );
            }
            for (directory, state) in &snapshot.directories {
                ensure!(
                    !directory.starts_with(&format!("{path}/"))
                        || !matches!(state, DirectoryState::Present),
                    "Snapshot contains directories under an absent ancestor"
                );
            }
        }
    }
    let present: BTreeSet<_> = snapshot
        .directories
        .iter()
        .filter_map(|(p, s)| matches!(s, DirectoryState::Present).then_some(p))
        .collect();
    ensure!(
        snapshot.directory_entries.keys().collect::<BTreeSet<_>>() == present,
        "Incomplete Books directory listings"
    );
    for (parent, names) in &snapshot.directory_entries {
        let mut input = names.clone().into_iter();
        ensure!(
            collect_directory(|| Ok(input.next()))? == *names,
            "Books listing is not canonical"
        );
        let expected: BTreeSet<_> = snapshot
            .files
            .iter()
            .filter_map(|(p, s)| matches!(s, OriginalState::Present { .. }).then_some(p))
            .chain(
                snapshot
                    .directories
                    .iter()
                    .filter_map(|(p, s)| matches!(s, DirectoryState::Present).then_some(p)),
            )
            .filter_map(|p| {
                p.rsplit_once('/')
                    .filter(|(dir, _)| *dir == parent)
                    .map(|(_, leaf)| leaf.to_owned())
            })
            .collect();
        ensure!(
            expected == names.iter().cloned().collect(),
            "Books listing does not cover inventory"
        );
    }
    for path in snapshot
        .files
        .iter()
        .filter_map(|(p, s)| matches!(s, OriginalState::Present { .. }).then_some(p))
        .chain(snapshot.directories.iter().filter_map(|(p, s)| {
            (p != "Books" && matches!(s, DirectoryState::Present)).then_some(p)
        }))
    {
        let parent = path.rsplit_once('/').unwrap().0;
        ensure!(
            matches!(
                snapshot.directories.get(parent),
                Some(DirectoryState::Present)
            ),
            "Books entry has uncaptured parent"
        );
    }

    Ok(())
}

pub fn verify_original(io: &mut impl MediaIo, path: &str, original: &OriginalState) -> Result<()> {
    validate_media_path(path)?;
    require_known(original)?;
    if let OriginalState::VerifiedAbsent {
        readable_parent,
        missing_component,
    } = original
    {
        validate_absence(path, readable_parent, missing_component)?;
    }
    let current = observe(io, path);
    require_known(&current)?;
    match (original, current) {
        (
            OriginalState::Present {
                bytes,
                sha256: hash,
            },
            OriginalState::Present {
                bytes: found,
                sha256: found_hash,
            },
        ) if *bytes == found && *hash == found_hash => Ok(()),
        (OriginalState::VerifiedAbsent { .. }, OriginalState::VerifiedAbsent { .. }) => Ok(()),
        _ => bail!("Original state verification failed: {path}"),
    }
}

fn ensure_directories(io: &mut impl MediaIo, path: &str) -> Result<()> {
    validate_media_path(path)?;
    let mut current = String::new();
    for component in path.split('/') {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        match locate(io, &current)? {
            Located::Present(info) => ensure!(
                info.kind == FileKind::Directory,
                "Unsafe restore ancestor: {current}"
            ),
            Located::Absent { .. } => {
                io.mkdir(&current)?;
                ensure!(
                    matches!(
                        locate(io, &current)?,
                        Located::Present(FileInfo {
                            kind: FileKind::Directory,
                            ..
                        })
                    ),
                    "Directory creation did not persist"
                );
            }
        }
    }
    Ok(())
}

pub fn restore_file(io: &mut impl MediaIo, path: &str, original: &OriginalState) -> Result<()> {
    validate_media_path(path)?;
    require_known(original)?;
    if let OriginalState::VerifiedAbsent {
        readable_parent,
        missing_component,
    } = original
    {
        validate_absence(path, readable_parent, missing_component)?;
    }
    match original {
        OriginalState::Present { bytes, .. } => {
            // Do not overwrite a symlink, directory or inaccessible entry
            let current = observe(io, path);
            require_known(&current)?;
            // Cleanup runs after every sync. Most of Books (including user books)
            // is unchanged; compare bytes before resending it over USB.
            if current == *original {
                return Ok(());
            }
            if let Some((parent, _)) = path.rsplit_once('/') {
                ensure_directories(io, parent)?;
            }
            io.write(path, bytes)?;
        }
        OriginalState::VerifiedAbsent { .. } => match locate(io, path)? {
            Located::Absent { .. } => {}
            Located::Present(info) => {
                ensure!(
                    info.kind == FileKind::Regular,
                    "Refusing unexpected file kind during removal: {path}"
                );
                io.unlink(path)?;
            }
        },
        OriginalState::Unknown { .. } => unreachable!(),
    }
    verify_original(io, path, original)
}

pub fn restore_books(io: &mut impl MediaIo, snapshot: &BooksSnapshot) -> Result<()> {
    // Validate the complete inventory and all checksums before touching Media
    validate_snapshot(snapshot)?;
    let mut errors = Vec::new();
    for (path, state) in &snapshot.files {
        if let Err(error) = restore_file(io, path, state) {
            errors.push(format!("{path}: {error:#}"));
        }
    }
    let mut paths: Vec<_> = snapshot.directories.keys().collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.split('/').count()));
    for path in paths {
        let result = (|| -> Result<()> {
            match &snapshot.directories[path] {
                DirectoryState::Present => ensure_directories(io, path)?,
                DirectoryState::VerifiedAbsent { .. } => match locate(io, path)? {
                    Located::Absent { .. } => {}
                    Located::Present(info) => {
                        ensure!(
                            info.kind == FileKind::Directory,
                            "Unexpected Books entry kind"
                        );
                        ensure!(
                            io.list(path)?.is_empty(),
                            "Books directory has untracked content; retain recovery data"
                        );
                        io.unlink(path)?;
                        ensure!(
                            matches!(locate(io, path)?, Located::Absent { .. }),
                            "Books directory removal not verified"
                        );
                    }
                },
            }
            Ok(())
        })();
        if let Err(error) = result {
            errors.push(format!("{path}: {error:#}"));
        }
    }
    if let Err(error) = verify_books(io, snapshot) {
        errors.push(format!("Final verification: {error:#}"));
    }
    ensure!(
        errors.is_empty(),
        "Books restoration incomplete: {}",
        errors.join("; ")
    );
    Ok(())
}

pub fn verify_books(io: &mut impl MediaIo, snapshot: &BooksSnapshot) -> Result<()> {
    validate_snapshot(snapshot)?;
    for (path, state) in &snapshot.files {
        verify_original(io, path, state)?;
    }
    for path in snapshot.directories.keys() {
        match (&snapshot.directories[path], locate(io, path)?) {
            (DirectoryState::Present, Located::Present(info))
                if info.kind == FileKind::Directory =>
            {
                let mut names = io.list(path)?.into_iter();
                ensure!(
                    collect_directory(|| Ok(names.next()))? == snapshot.directory_entries[path],
                    "Books directory inventory changed: {path}"
                );
            }
            (DirectoryState::VerifiedAbsent { .. }, Located::Absent { .. }) => {}
            _ => bail!("Books directory state verification failed: {path}"),
        }
    }
    Ok(())
}

/// Clean only the bounded staging root owned by this operation
/// Unlink symlinks without traversing them and propagate listing errors
pub fn cleanup_staging(io: &mut impl MediaIo, root: &str) -> Result<()> {
    ensure!(
        ["airlift-src-", "airlift-link-", "airlift-recovered-"]
            .iter()
            .any(|prefix| root.strip_prefix(prefix).is_some_and(
                |token| token.len() == 20 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            )),
        "Invalid operation staging root"
    );
    fn remove(io: &mut impl MediaIo, path: &str, depth: usize, budget: &mut usize) -> Result<()> {
        ensure!(depth <= 32 && *budget > 0, "Staging cleanup limit exceeded");
        *budget -= 1;
        if let Located::Present(info) = locate(io, path)? {
            if info.kind == FileKind::Directory {
                let children = io.list(path)?;
                let mut children = children.into_iter();
                for child in collect_directory(|| Ok(children.next()))? {
                    remove(io, &format!("{path}/{child}"), depth + 1, budget)?;
                }
                ensure!(io.list(path)?.is_empty(), "Staging changed during cleanup");
            } else {
                ensure!(
                    matches!(info.kind, FileKind::Regular | FileKind::Symlink),
                    "Unexpected staging file kind"
                );
            }
            io.unlink(path)?;
            ensure!(
                matches!(locate(io, path)?, Located::Absent { .. }),
                "Staging removal not verified"
            );
        }
        Ok(())
    }
    let mut budget = MAX_DIRECTORY_ENTRIES;
    remove(io, root, 0, &mut budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    enum Node {
        File(Vec<u8>),
        Directory,
        Link,
    }

    #[derive(Clone)]
    struct FakeMedia {
        nodes: BTreeMap<String, Node>,
        calls: Vec<String>,
        denied: BTreeSet<String>,
        fail_at: Option<usize>,
        ignore_removal: bool,
        partial_write: bool,
    }

    impl FakeMedia {
        fn new() -> Self {
            Self {
                nodes: BTreeMap::from([("/".into(), Node::Directory)]),
                calls: Vec::new(),
                denied: BTreeSet::new(),
                fail_at: None,
                ignore_removal: false,
                partial_write: false,
            }
        }

        fn call(&mut self, operation: &str, path: &str) -> Result<()> {
            self.calls.push(format!("{operation} {path}"));
            ensure!(!self.denied.contains(path), "Permission denied for {path}");
            ensure!(
                self.fail_at != Some(self.calls.len()),
                "Injected {operation} failure at {}",
                self.calls.len()
            );
            Ok(())
        }

        fn add_books(&mut self) {
            for directory in BOOKS_DIRECTORIES {
                self.nodes.insert(directory.into(), Node::Directory);
            }
            for (index, path) in TRACKED_BOOKS_FILES.iter().enumerate() {
                self.nodes
                    .insert((*path).into(), Node::File(vec![index as u8; index + 1]));
            }
        }
    }

    impl MediaIo for FakeMedia {
        fn stat(&mut self, path: &str) -> Result<FileInfo> {
            self.call("stat", path)?;
            Ok(
                match self
                    .nodes
                    .get(path)
                    .ok_or_else(|| anyhow::anyhow!("Not found: {path}"))?
                {
                    Node::File(bytes) => FileInfo {
                        kind: FileKind::Regular,
                        size: bytes.len() as u64,
                    },
                    Node::Directory => FileInfo {
                        kind: FileKind::Directory,
                        size: 0,
                    },
                    Node::Link => FileInfo {
                        kind: FileKind::Symlink,
                        size: 0,
                    },
                },
            )
        }

        fn list(&mut self, path: &str) -> Result<Vec<String>> {
            self.call("list", path)?;
            ensure!(
                matches!(self.nodes.get(path), Some(Node::Directory)),
                "Cannot list non-directory"
            );
            let prefix = if path == "/" {
                String::new()
            } else {
                format!("{path}/")
            };
            Ok(self
                .nodes
                .keys()
                .filter_map(|name| name.strip_prefix(&prefix))
                .filter(|tail| !tail.is_empty() && !tail.contains('/'))
                .map(str::to_owned)
                .collect())
        }

        fn read(&mut self, path: &str, limit: usize) -> Result<Vec<u8>> {
            self.call("read", path)?;
            let Some(Node::File(bytes)) = self.nodes.get(path) else {
                bail!("Cannot read non-file");
            };
            ensure!(bytes.len() <= limit, "Read exceeds limit");
            Ok(bytes.clone())
        }

        fn write(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
            self.call("write", path)?;
            let written = if self.partial_write {
                &bytes[..bytes.len().min(1)]
            } else {
                bytes
            };
            self.nodes.insert(path.into(), Node::File(written.to_vec()));
            Ok(())
        }

        fn mkdir(&mut self, path: &str) -> Result<()> {
            self.call("mkdir", path)?;
            ensure!(!self.nodes.contains_key(path), "Directory already exists");
            self.nodes.insert(path.into(), Node::Directory);
            Ok(())
        }

        fn unlink(&mut self, path: &str) -> Result<()> {
            self.call("unlink", path)?;
            if !self.ignore_removal {
                self.nodes.remove(path);
            }
            Ok(())
        }
    }

    #[test]
    fn absence_requires_readable_ancestors_and_errors_stay_unknown() {
        let mut media = FakeMedia::new();
        let absent = observe(&mut media, "Books/Sync/Books.plist");
        assert_eq!(
            absent,
            OriginalState::VerifiedAbsent {
                readable_parent: "/".into(),
                missing_component: "Books".into()
            }
        );
        media.add_books();
        media.denied.insert("Books/Sync/Books.plist".into());
        assert!(matches!(
            observe(&mut media, "Books/Sync/Books.plist"),
            OriginalState::Unknown { .. }
        ));
        assert!(snapshot_books(&mut media).is_err());
        media.denied.clear();
        media.denied.insert("Books/Sync".into());
        assert!(matches!(
            observe(&mut media, "Books/Sync/not-present"),
            OriginalState::Unknown { .. }
        ));
        let mut inaccessible = FakeMedia::new();
        inaccessible.denied.insert("/".into());
        assert!(matches!(
            observe(&mut inaccessible, "Books/missing"),
            OriginalState::Unknown { .. }
        ));
        assert!(
            require_known(&OriginalState::Unknown {
                reason: "denied".into()
            })
            .is_err()
        );
        let proof_for_another_file = OriginalState::VerifiedAbsent {
            readable_parent: "/".into(),
            missing_component: "other".into(),
        };
        assert!(verify_original(&mut media, "Books/missing", &proof_for_another_file).is_err());
    }

    #[test]
    fn directory_reads_reject_partial_errors_duplicates_and_overflow() {
        let mut calls = 0;
        assert!(
            collect_directory(|| {
                calls += 1;
                if calls == 1 {
                    Ok(Some("first".into()))
                } else {
                    bail!("Interrupted read")
                }
            })
            .is_err()
        );
        let mut names = ["same", "same"].into_iter();
        assert!(collect_directory(|| Ok(names.next().map(str::to_owned))).is_err());
        let mut names = [".", "..", "ordinary"].into_iter();
        assert_eq!(
            collect_directory(|| Ok(names.next().map(str::to_owned))).unwrap(),
            ["ordinary"]
        );
        let mut index = 0;
        assert!(
            collect_directory(|| {
                index += 1;
                Ok(Some(format!("entry-{index}")))
            })
            .is_err()
        );
        assert_eq!(index, MAX_DIRECTORY_ENTRIES + 1);
        let mut index = 0;
        assert_eq!(
            collect_directory(|| {
                index += 1;
                Ok((index <= MAX_DIRECTORY_ENTRIES).then(|| format!("entry-{index}")))
            })
            .unwrap()
            .len(),
            MAX_DIRECTORY_ENTRIES
        );
        for name in ["../outside", "unsafe/name", "unsafe\\name", "", "\0"] {
            assert!(collect_directory(|| Ok(Some(name.into()))).is_err());
        }
    }

    #[test]
    fn bounded_reads_detect_truncation_growth_and_invalid_counts() {
        let payload = b"abcdef";
        let mut offset = 0;
        let found = read_exact_bounded(6, 6, |buffer| {
            let length = (payload.len() - offset).min(buffer.len()).min(2);
            buffer[..length].copy_from_slice(&payload[offset..offset + length]);
            offset += length;
            Ok(length)
        })
        .unwrap();
        assert_eq!(found, payload);
        assert!(
            read_exact_bounded(MAX_FILE_BYTES as u64 + 1, MAX_FILE_BYTES, |_| panic!(
                "must not allocate/read"
            ))
            .is_err()
        );
        assert!(read_exact_bounded(3, 3, |_| Ok(0)).is_err());
        assert!(read_exact_bounded(3, 3, |buffer| Ok(buffer.len() + 1)).is_err());
        assert!(read_exact_bounded(0, 0, |_| Ok(1)).is_err());
        assert!(read_exact_bounded(3, 3, |buffer| Ok(buffer.len())).is_err()); // Trailing byte: grew.
        assert!(read_exact_bounded(3, 3, |_| bail!("read error")).is_err());
    }

    #[test]
    fn books_restoration_verifies_bytes_and_original_directory_absence() {
        let mut media = FakeMedia::new();
        let empty = snapshot_books(&mut media).unwrap();
        media.add_books();
        restore_books(&mut media, &empty).unwrap();
        assert_eq!(media.nodes.len(), 1);
        restore_books(&mut media, &empty).unwrap();
        media.add_books();
        let snapshot = snapshot_books(&mut media).unwrap();
        media.nodes.insert(
            "Books/Sync/Books.plist".into(),
            Node::File(b"edited".to_vec()),
        );
        media.nodes.remove("Books/Books.plist");
        restore_books(&mut media, &snapshot).unwrap();
        for path in TRACKED_BOOKS_FILES {
            verify_original(&mut media, path, &snapshot.files[*path]).unwrap();
        }
        restore_books(&mut media, &snapshot).unwrap();
        media.partial_write = true;
        media.nodes.insert(
            "Books/Sync/Books.plist".into(),
            Node::File(b"needs restoration".to_vec()),
        );
        assert!(restore_books(&mut media, &snapshot).is_err());
    }

    #[test]
    fn corrupt_or_incomplete_snapshots_are_rejected_before_mutation() {
        let mut media = FakeMedia::new();
        media.add_books();
        let mut snapshot = snapshot_books(&mut media).unwrap();
        media.calls.clear();
        if let OriginalState::Present { bytes, .. } =
            snapshot.files.get_mut("Books/Books.plist").unwrap()
        {
            bytes.push(42);
        }
        assert!(restore_books(&mut media, &snapshot).is_err());
        assert!(media.calls.is_empty());
        snapshot.files.remove("Books/Sync/Books.plist");
        assert!(restore_books(&mut media, &snapshot).is_err());
        assert!(media.calls.is_empty());
        let mut empty_media = FakeMedia::new();
        let mut empty = snapshot_books(&mut empty_media).unwrap();
        empty.files.insert(
            "Books/Books.plist".into(),
            OriginalState::VerifiedAbsent {
                readable_parent: "/".into(),
                missing_component: "unrelated".into(),
            },
        );
        media.calls.clear();
        assert!(restore_books(&mut media, &empty).is_err());
        assert!(media.calls.is_empty());
    }

    #[test]
    fn ack_without_removal_untracked_content_and_symlinks_block_restore() {
        let mut media = FakeMedia::new();
        let snapshot = snapshot_books(&mut media).unwrap();
        media.add_books();
        media.ignore_removal = true;
        assert!(restore_books(&mut media, &snapshot).is_err());
        media.ignore_removal = false;
        media.nodes.insert(
            "Books/user-book.epub".into(),
            Node::File(b"untracked".to_vec()),
        );
        assert!(restore_books(&mut media, &snapshot).is_err());
        assert!(matches!(media.nodes["Books/user-book.epub"], Node::File(_)));
        let mut media = FakeMedia::new();
        media.add_books();
        media.nodes.insert("Books/Sync".into(), Node::Link);
        assert!(snapshot_books(&mut media).is_err());
        assert!(!media.calls.iter().any(|call| call == "list Books/Sync"));
        for path in [
            "/Books",
            "Books/../escape",
            "Books//empty",
            "Books/./same",
            "Books\\Sync",
        ] {
            assert!(validate_media_path(path).is_err());
        }
    }

    #[test]
    fn complete_books_tree_preserves_nested_files_and_detects_new_content() {
        let mut media = FakeMedia::new();
        media.add_books();
        media.nodes.insert("Books/Library".into(), Node::Directory);
        media.nodes.insert(
            "Books/Library/Novel.epub".into(),
            Node::File(b"original novel".to_vec()),
        );
        let snapshot = snapshot_books(&mut media).unwrap();
        assert!(snapshot.files.contains_key("Books/Library/Novel.epub"));
        media.calls.clear();
        restore_books(&mut media, &snapshot).unwrap();
        assert!(!media.calls.iter().any(|call| call.starts_with("write ")));
        media.nodes.insert(
            "Books/Library/Novel.epub".into(),
            Node::File(b"changed".to_vec()),
        );
        restore_books(&mut media, &snapshot).unwrap();
        assert!(
            media
                .calls
                .iter()
                .any(|call| call == "write Books/Library/Novel.epub")
        );
        media.nodes.insert(
            "Books/Library/new.epub".into(),
            Node::File(b"keep unknown".to_vec()),
        );
        assert!(verify_books(&mut media, &snapshot).is_err());
        assert!(restore_books(&mut media, &snapshot).is_err());
        assert!(media.nodes.contains_key("Books/Library/new.epub"));
        let mut corrupt = snapshot.clone();
        corrupt
            .directory_entries
            .get_mut("Books/Library")
            .unwrap()
            .clear();
        media.calls.clear();
        assert!(restore_books(&mut media, &corrupt).is_err());
        assert!(media.calls.is_empty());
        media.nodes.insert("Books/Library/link".into(), Node::Link);
        assert!(snapshot_books(&mut media).is_err());
        assert!(!media.calls.iter().any(|c| c == "list Books/Library/link"));
    }

    #[test]
    fn staging_cleanup_unlinks_links_without_following_them() {
        let mut media = FakeMedia::new();
        let root = "airlift-src-0123456789abcdefabcd";
        media.nodes.insert(root.into(), Node::Directory);
        media.nodes.insert(format!("{root}/child"), Node::Directory);
        media.nodes.insert(format!("{root}/child/link"), Node::Link);
        media.nodes.insert(
            format!("{root}/payload"),
            Node::File(b"disposable".to_vec()),
        );
        media
            .nodes
            .insert("unrelated".into(), Node::File(b"keep".to_vec()));
        cleanup_staging(&mut media, root).unwrap();
        assert!(media.nodes.contains_key("unrelated"));
        assert!(!media.nodes.contains_key(root));
        assert!(
            !media
                .calls
                .iter()
                .any(|call| call == &format!("list {root}/child/link"))
        );
        cleanup_staging(&mut media, root).unwrap();
        media.calls.clear();
        for root in [
            "Books",
            "/",
            "airlift-src-bad-token",
            "airlift-src-0123456789abcdefabcd/child",
        ] {
            assert!(cleanup_staging(&mut media, root).is_err());
        }
        assert!(media.calls.is_empty());
    }

    #[test]
    fn all_capture_and_restore_io_boundaries_fail_closed_and_restore_can_retry() {
        let mut original = FakeMedia::new();
        original.add_books();
        let snapshot = snapshot_books(&mut original).unwrap();
        let capture_calls = original.calls.len();
        original.calls.clear();
        for boundary in 1..=capture_calls {
            let mut media = original.clone();
            media.fail_at = Some(boundary);
            assert!(
                snapshot_books(&mut media).is_err(),
                "capture boundary {boundary}"
            );
            assert!(!media.calls.iter().any(|call| call.starts_with("write ")
                || call.starts_with("unlink ")
                || call.starts_with("mkdir ")));
        }
        let mut baseline = original.clone();
        restore_books(&mut baseline, &snapshot).unwrap();
        let restore_calls = baseline.calls.len();
        for boundary in 1..=restore_calls {
            let mut media = original.clone();
            media.fail_at = Some(boundary);
            assert!(
                restore_books(&mut media, &snapshot).is_err(),
                "restore boundary {boundary}"
            );
            media.fail_at = None;
            restore_books(&mut media, &snapshot).unwrap();
        }
        let error = finish_with_cleanup::<()>(
            Err(anyhow::anyhow!("write failed")),
            Err(anyhow::anyhow!("Books restore failed")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("write failed") && error.contains("Books restore failed"));
    }
}
