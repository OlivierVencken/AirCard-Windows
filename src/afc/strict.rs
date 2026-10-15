//! Checked AFC operations for effects transfers and Books recovery

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::ptr;

use anyhow::{Context, Result, ensure};

use super::AfcClient;
use crate::device::ActiveDeviceSession;
use crate::effects::transport::{
    FileInfo, FileKind, MAX_FILE_BYTES, MediaIo, collect_directory, finish_with_cleanup,
    read_exact_bounded, validate_media_path,
};

fn status(code: i32, operation: &str) -> Result<()> {
    ensure!(code == 0, "{operation} failed with native AFC code {code}");
    Ok(())
}

fn parse_info(values: &BTreeMap<String, String>) -> Result<FileInfo> {
    let kind = match values.get("st_ifmt").map(String::as_str) {
        Some("S_IFREG") => FileKind::Regular,
        Some("S_IFDIR") => FileKind::Directory,
        Some("S_IFLNK") => FileKind::Symlink,
        Some(_) => FileKind::Other,
        None => anyhow::bail!("AFC metadata has no st_ifmt; file kind is unknown"),
    };
    let size = match values.get("st_size") {
        Some(size) => size.parse::<u64>().context("Invalid AFC st_size")?,
        None if kind != FileKind::Regular => 0,
        None => anyhow::bail!("Regular file metadata has no st_size"),
    };
    Ok(FileInfo { kind, size })
}

impl AfcClient {
    pub fn new_for_effects(session: &ActiveDeviceSession) -> Result<Self> {
        let client = Self::new(session)?;
        // Apply the native timeout for both secure and ordinary AFC sockets
        status(
            unsafe { (client.libs.afc_connection_set_io_timeout)(client.conn, 30) },
            "AFCConnectionSetIOTimeout",
        )?;
        Ok(client)
    }

    fn strict_info(&self, path: &str) -> Result<FileInfo> {
        if path != "/" {
            validate_media_path(path)?;
        }
        self.native_info(path)
    }

    // Raw helpers are private: public Media operations validate relative paths
    fn native_info(&self, path: &str) -> Result<FileInfo> {
        let path = CString::new(path)?;
        let mut handle = ptr::null_mut();
        let open = unsafe { (self.libs.afc_file_info_open)(self.conn, path.as_ptr(), &mut handle) };
        let opened = status(open, "AFCFileInfoOpen").and_then(|_| {
            ensure!(!handle.is_null(), "AFCFileInfoOpen returned a null handle");
            Ok(())
        });
        if let Err(error) = opened {
            let close = if handle.is_null() {
                Ok(())
            } else {
                status(
                    unsafe { (self.libs.afc_key_value_close)(handle) },
                    "AFCKeyValueClose",
                )
            };
            return finish_with_cleanup(Err(error), close);
        }
        let operation = (|| -> Result<FileInfo> {
            let mut values = BTreeMap::new();
            for index in 0..=64 {
                let mut key = ptr::null();
                let mut value = ptr::null();
                status(
                    unsafe { (self.libs.afc_key_value_read)(handle, &mut key, &mut value) },
                    "AFCKeyValueRead",
                )?;
                if key.is_null() && value.is_null() {
                    return parse_info(&values);
                }
                ensure!(
                    !key.is_null() && !value.is_null(),
                    "Incomplete AFC metadata pair"
                );
                ensure!(index < 64, "AFC metadata exceeds entry limit");
                let key = unsafe { CStr::from_ptr(key) }.to_str()?.to_string();
                let value = unsafe { CStr::from_ptr(value) }.to_str()?.to_string();
                ensure!(
                    key.len() <= 256 && value.len() <= 4096,
                    "Oversized AFC metadata value"
                );
                ensure!(
                    values.insert(key, value).is_none(),
                    "Duplicate AFC metadata key"
                );
            }
            unreachable!()
        })();
        let close = status(
            unsafe { (self.libs.afc_key_value_close)(handle) },
            "AFCKeyValueClose",
        );
        finish_with_cleanup(operation, close)
    }

    fn strict_list(&self, path: &str) -> Result<Vec<String>> {
        if path != "/" {
            validate_media_path(path)?;
        }
        self.native_list(path)
    }

    fn native_list(&self, path: &str) -> Result<Vec<String>> {
        ensure!(
            self.native_info(path)?.kind == FileKind::Directory,
            "Refusing to list a non-directory/symlink"
        );
        let path = CString::new(path)?;
        let mut handle = ptr::null_mut();
        let open = unsafe { (self.libs.afc_directory_open)(self.conn, path.as_ptr(), &mut handle) };
        let opened = status(open, "AFCDirectoryOpen").and_then(|_| {
            ensure!(!handle.is_null(), "AFCDirectoryOpen returned a null handle");
            Ok(())
        });
        if let Err(error) = opened {
            let close = if handle.is_null() {
                Ok(())
            } else {
                status(
                    unsafe { (self.libs.afc_directory_close)(self.conn, handle) },
                    "AFCDirectoryClose",
                )
            };
            return finish_with_cleanup(Err(error), close);
        }
        let result = collect_directory(|| {
            let mut entry = ptr::null();
            status(
                unsafe { (self.libs.afc_directory_read)(self.conn, handle, &mut entry) },
                "AFCDirectoryRead",
            )?;
            if entry.is_null() {
                Ok(None)
            } else {
                Ok(Some(unsafe { CStr::from_ptr(entry) }.to_str()?.to_string()))
            }
        });
        let close = status(
            unsafe { (self.libs.afc_directory_close)(self.conn, handle) },
            "AFCDirectoryClose",
        );
        finish_with_cleanup(result, close)
    }

    fn strict_read(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        validate_media_path(path)?;
        self.native_read(path, limit)
    }

    fn native_read(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        let info = self.native_info(path)?;
        ensure!(
            info.kind == FileKind::Regular,
            "Refusing to read a non-regular file"
        );
        ensure!(
            limit <= MAX_FILE_BYTES && info.size <= limit as u64,
            "AFC read exceeds effects limit"
        );
        let c_path = CString::new(path)?;
        let mut handle = 0;
        let open =
            unsafe { (self.libs.afc_file_ref_open)(self.conn, c_path.as_ptr(), 1, &mut handle) };
        let opened = status(open, "AFCFileRefOpen").and_then(|_| {
            ensure!(handle != 0, "AFCFileRefOpen returned a null handle");
            Ok(())
        });
        if let Err(error) = opened {
            let close = if handle == 0 {
                Ok(())
            } else {
                status(
                    unsafe { (self.libs.afc_file_ref_close)(self.conn, handle) },
                    "AFCFileRefClose",
                )
            };
            return finish_with_cleanup(Err(error), close);
        }
        let result = read_exact_bounded(info.size, limit, |buffer| {
            let mut length = buffer.len() as isize;
            status(
                unsafe {
                    (self.libs.afc_file_ref_read)(
                        self.conn,
                        handle,
                        buffer.as_mut_ptr(),
                        &mut length,
                    )
                },
                "AFCFileRefRead",
            )?;
            ensure!(
                length >= 0 && length as usize <= buffer.len(),
                "Invalid native AFC read length"
            );
            Ok(length as usize)
        });
        let close = status(
            unsafe { (self.libs.afc_file_ref_close)(self.conn, handle) },
            "AFCFileRefClose",
        );
        let bytes = finish_with_cleanup(result, close)?;
        ensure!(
            self.native_info(path)? == info,
            "File metadata changed during AFC read"
        );
        Ok(bytes)
    }
}

impl MediaIo for AfcClient {
    fn stat(&mut self, path: &str) -> Result<FileInfo> {
        self.strict_info(path)
    }
    fn list(&mut self, path: &str) -> Result<Vec<String>> {
        self.strict_list(path)
    }
    fn read(&mut self, path: &str, limit: usize) -> Result<Vec<u8>> {
        self.strict_read(path, limit)
    }
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        validate_media_path(path)?;
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "AFC write exceeds effects limit"
        );
        let c_path = CString::new(path)?;
        let mut handle = 0;
        let open =
            unsafe { (self.libs.afc_file_ref_open)(self.conn, c_path.as_ptr(), 3, &mut handle) };
        let operation = status(open, "AFCFileRefOpen").and_then(|_| {
            ensure!(handle != 0, "AFCFileRefOpen returned a null handle");
            for chunk in bytes.chunks(64 * 1024) {
                status(
                    unsafe {
                        (self.libs.afc_file_ref_write)(
                            self.conn,
                            handle,
                            chunk.as_ptr(),
                            chunk.len() as isize,
                        )
                    },
                    "AFCFileRefWrite",
                )?;
            }
            Ok(())
        });
        let close = if handle == 0 {
            Ok(())
        } else {
            status(
                unsafe { (self.libs.afc_file_ref_close)(self.conn, handle) },
                "AFCFileRefClose",
            )
        };
        finish_with_cleanup(operation, close)
    }
    fn mkdir(&mut self, path: &str) -> Result<()> {
        validate_media_path(path)?;
        let path = CString::new(path)?;
        status(
            unsafe { (self.libs.afc_directory_create)(self.conn, path.as_ptr()) },
            "AFCDirectoryCreate",
        )
    }
    fn unlink(&mut self, path: &str) -> Result<()> {
        validate_media_path(path)?;
        let path = CString::new(path)?;
        status(
            unsafe { (self.libs.afc_remove_path)(self.conn, path.as_ptr()) },
            "AFCRemovePath",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_metadata_requires_file_kind_and_regular_file_size() {
        let mut values = BTreeMap::new();
        assert!(parse_info(&values).is_err());
        values.insert("st_ifmt".into(), "S_IFREG".into());
        assert!(parse_info(&values).is_err());
        values.insert("st_size".into(), "-1".into());
        assert!(parse_info(&values).is_err());
        values.insert("st_size".into(), "42".into());
        assert_eq!(
            parse_info(&values).unwrap(),
            FileInfo {
                kind: FileKind::Regular,
                size: 42
            }
        );
        values.insert("st_ifmt".into(), "S_IFLNK".into());
        assert_eq!(parse_info(&values).unwrap().kind, FileKind::Symlink);
        values.insert("st_ifmt".into(), "S_IFDIR".into());
        assert_eq!(parse_info(&values).unwrap().kind, FileKind::Directory);
        values.insert("st_ifmt".into(), "new-kind".into());
        assert_eq!(parse_info(&values).unwrap().kind, FileKind::Other);
        assert!(status(10, "read").is_err());
        assert!(status(8, "stat").is_err()); // No native status is proof of absence.
    }
}
