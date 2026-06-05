//! Reflink-aware directory snapshot.
//!
//! Strategy per OS:
//!
//! * **Linux**: walk the tree, `copy_file_range(2)` each file — kernel
//!   detects same-fs and produces reflinks on btrfs/xfs/ext4-with-CoW.
//! * **macOS**: a single `clonefile(2)` call on the source directory. The
//!   syscall is recursive AND atomic and produces real APFS reflinks
//!   (shared extents until one side is written), so we don't need to walk.
//! * **Other**: not supported.

use std::path::Path;
use tracing::debug;

use crate::error::AppError;

#[cfg(target_os = "linux")]
pub fn snapshot(src: &Path, dst: &Path) -> Result<(), AppError> {
    use std::fs;

    use crate::copy_ref::{CopyRef, CopyRefOperator};

    debug!("Snapshot (linux walk): {:?} -> {:?}", src, dst);

    if !dst.exists() {
        fs::create_dir_all(dst).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create directory {:?}: {}", dst, e),
        })?;
    }

    let entries = fs::read_dir(src).map_err(|e| AppError::FileSystem {
        message: format!("Failed to read directory {:?}: {}", src, e),
    })?;

    let operator = CopyRefOperator::new();

    for entry in entries {
        let entry = entry.map_err(|e| AppError::FileSystem {
            message: format!("Failed to read directory entry: {}", e),
        })?;
        let path = entry.path();

        if path.is_dir() {
            let new_dst = dst.join(entry.file_name());
            fs::create_dir_all(&new_dst).map_err(|e| AppError::FileSystem {
                message: format!("Failed to create directory {:?}: {}", new_dst, e),
            })?;
            snapshot(&path, &new_dst)?;
        } else {
            let src_file = fs::File::open(&path).map_err(|e| AppError::FileSystem {
                message: format!("Failed to open source file {:?}: {}", path, e),
            })?;
            let dst_file_path = dst.join(entry.file_name());
            let dst_file = fs::File::create(&dst_file_path).map_err(|e| AppError::FileSystem {
                message: format!("Failed to create destination file {:?}: {}", dst_file_path, e),
            })?;

            operator.copy_ref(&src_file, &dst_file)?;
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
pub fn snapshot(src: &Path, dst: &Path) -> Result<(), AppError> {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    debug!("Snapshot (macOS clonefile): {:?} -> {:?}", src, dst);

    if !src.exists() {
        return Err(AppError::FileSystem {
            message: format!("Source {:?} does not exist", src),
        });
    }

    // clonefile requires the parent of dst to exist.
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::FileSystem {
            message: format!("Failed to create parent {:?}: {}", parent, e),
        })?;
    }

    // clonefile requires dst to NOT exist. Tolerate an empty pre-created
    // dst (common when the volume dir was created by another code path);
    // refuse to clobber a populated one.
    if dst.exists() {
        let is_empty_dir = dst.is_dir()
            && fs::read_dir(dst)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
        if is_empty_dir {
            fs::remove_dir(dst).map_err(|e| AppError::FileSystem {
                message: format!("Failed to remove empty dst {:?}: {}", dst, e),
            })?;
        } else if dst.is_file() {
            fs::remove_file(dst).map_err(|e| AppError::FileSystem {
                message: format!("Failed to remove dst file {:?}: {}", dst, e),
            })?;
        } else {
            return Err(AppError::FileSystem {
                message: format!(
                    "Destination {:?} exists and is not empty; refusing to overwrite",
                    dst
                ),
            });
        }
    }

    let src_c = CString::new(src.as_os_str().as_bytes()).map_err(|e| AppError::FileSystem {
        message: format!("src path contains NUL: {}", e),
    })?;
    let dst_c = CString::new(dst.as_os_str().as_bytes()).map_err(|e| AppError::FileSystem {
        message: format!("dst path contains NUL: {}", e),
    })?;

    // SAFETY: both pointers are valid NUL-terminated paths owned by the
    // CStrings above; flags=0 is the documented default for a recursive
    // clone of the source tree.
    let ret = unsafe { nix::libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if ret == -1 {
        let err = std::io::Error::last_os_error();
        return Err(AppError::FileSystem {
            message: format!("clonefile {:?} -> {:?} failed: {}", src, dst, err),
        });
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn snapshot(_src: &Path, _dst: &Path) -> Result<(), AppError> {
    Err(AppError::FileSystem {
        message: "snapshot is only supported on Linux and macOS".into(),
    })
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn snapshot_copies_flat_directory() {
        let src = tempdir().unwrap();
        let dst_root = tempdir().unwrap();
        // On macOS clonefile requires the dst to NOT exist — use a fresh
        // child of a tempdir so cleanup still works.
        let dst = dst_root.path().join("snap");

        std::fs::File::create(src.path().join("a.txt"))
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        std::fs::File::create(src.path().join("b.txt"))
            .unwrap()
            .write_all(b"world")
            .unwrap();

        snapshot(src.path(), &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(std::fs::read_to_string(dst.join("b.txt")).unwrap(), "world");
    }

    #[test]
    fn snapshot_creates_nested_directories() {
        let src = tempdir().unwrap();
        let dst_root = tempdir().unwrap();
        let dst = dst_root.path().join("created/by/snapshot");

        let sub = src.path().join("nested/dir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::File::create(sub.join("deep.txt"))
            .unwrap()
            .write_all(b"deep")
            .unwrap();

        snapshot(src.path(), &dst).unwrap();

        let copied = dst.join("nested/dir/deep.txt");
        assert!(copied.exists(), "expected {:?} to exist", copied);
        assert_eq!(std::fs::read_to_string(copied).unwrap(), "deep");
    }

    #[test]
    fn snapshot_fails_when_source_missing() {
        let dst_root = tempdir().unwrap();
        let dst = dst_root.path().join("nope");
        let result = snapshot(Path::new("/nonexistent/source/path/xyz"), &dst);
        assert!(result.is_err());
    }

    #[test]
    fn snapshot_into_pre_existing_empty_dir() {
        let src = tempdir().unwrap();
        let dst_root = tempdir().unwrap();
        let dst = dst_root.path().join("pre");

        // Pre-create an empty dst (simulates a volume dir created by
        // PostgresOperator before snapshot runs).
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.path().join("x.txt"), b"hi").unwrap();

        snapshot(src.path(), &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("x.txt")).unwrap(), "hi");
    }
}
