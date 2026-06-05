use std::{
    fs::{self, File},
    path::Path,
};

use crate::error::AppError;
// from https://github.com/torvalds/linux/blob/cbf658dd09419f1ef9de11b9604e950bdd5c170b/include/uapi/linux/fiemap.h

#[repr(u32)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FiemapFlags {
    Last = 0x00000001,        // Last extent in file
    Unknown = 0x00000002,     // Data location unknown
    Delalloc = 0x00000004,    // Location still pending
    Encoded = 0x00000008,     // Data compressed/encrypted
    DataCrypted = 0x00000080, // Data is encrypted
    NotAligned = 0x00000100,  // Extent not aligned
    DataInline = 0x00000200,  // Data mixed with metadata
    DataTail = 0x00000400,    // Multiple files in block
    Unwritten = 0x00000800,   // Space allocated, no data
    Merged = 0x00001000,      // File does not natively support extents
    Shared = 0x00002000,      // Space shared with other files (reflink/CoW)
}

impl FiemapFlags {
    pub fn from_bits(flags: u32) -> Vec<FiemapFlags> {
        const ALL: &[FiemapFlags] = &[
            FiemapFlags::Last,
            FiemapFlags::Unknown,
            FiemapFlags::Delalloc,
            FiemapFlags::Encoded,
            FiemapFlags::DataCrypted,
            FiemapFlags::NotAligned,
            FiemapFlags::DataInline,
            FiemapFlags::DataTail,
            FiemapFlags::Unwritten,
            FiemapFlags::Merged,
            FiemapFlags::Shared,
        ];
        ALL.iter()
            .copied()
            .filter(|f| flags & (*f as u32) != 0)
            .collect()
    }
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct FiemapExtent {
    // byte offset of the extent in the file
    pub fe_logical: u64,
    // byte offset of extent on disk
    pub fe_physical: u64,
    // length in bytes for this extent
    pub fe_length: u64,

    fe_reserved64: [u64; 2],
    // flags for this extent
    pub fe_flags: u32,

    fe_reserved32: [u32; 3],
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct FiemapRequest {
    //  byte offset (inclusive) at which to start mapping (in)
    fm_start: u64,
    // logical length of mapping which userspace wants (in)
    fm_length: u64,
    // FIEMAP_FLAG_* flags for request (in/out)
    fm_flags: u32,
    // number of extents that were mapped (out)
    fm_mapped_extents: u32,
    // size of fm_extents array (in)
    fm_extent_count: u32,
    /* private: */
    fm_reserved: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct FiemapRequestFull {
    pub request: FiemapRequest,
    /// array of mapped extents (out)
    /// 32 is the most that `Default` gives us ootb.
    pub fm_extents: [FiemapExtent; 32],
}

#[derive(Debug)]
pub struct Fiemap {
    pub extent: FiemapExtent,
    pub flags: Vec<FiemapFlags>,
}

#[cfg(target_os = "linux")]
pub fn check_file(f: File) -> Result<Vec<Fiemap>, AppError> {
    use std::os::fd::AsRawFd;

    let file_size = f
        .metadata()
        .map_err(|e| AppError::FileSystem {
            message: format!("Failed to stat file: {}", e),
        })?
        .len();
    const FS_IOC_FIEMAP: u64 = nix::libc::_IOWR::<FiemapRequest>(0x66, 11);

    let mut all_extents: Vec<Fiemap> = Vec::new();
    let mut current_offset: u64 = 0;

    if file_size == 0 {
        return Ok(all_extents);
    }

    loop {
        let mut fr = Box::new(FiemapRequestFull::default());
        fr.request.fm_start = current_offset;
        fr.request.fm_length = file_size - current_offset;
        fr.request.fm_flags = 0;
        fr.request.fm_extent_count = 32;

        let ret = unsafe { nix::libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP, &mut *fr) };

        if ret == -1 {
            let errno = std::io::Error::last_os_error();
            return Err(AppError::FileSystem {
                message: format!("FIEMAP ioctl failed: {}", errno),
            });
        }

        if fr.request.fm_mapped_extents == 0 {
            break;
        }

        let mut found_last = false;
        for i in 0..fr.request.fm_mapped_extents as usize {
            let extent = fr.fm_extents[i];
            all_extents.push(Fiemap {
                extent,
                flags: FiemapFlags::from_bits(extent.fe_flags),
            });

            if extent.fe_flags & FiemapFlags::Last as u32 != 0 {
                found_last = true;
                break;
            }

            current_offset = extent.fe_logical + extent.fe_length;
        }

        if found_last || fr.request.fm_mapped_extents < 32 {
            break;
        }
    }

    Ok(all_extents)
}

#[cfg(not(target_os = "linux"))]
pub fn check_file(_f: File) -> Result<Vec<Fiemap>, AppError> {
    Err(AppError::FileSystem {
        message: "FIEMAP is only supported on Linux".into(),
    })
}

pub struct FileInfo {
    pub real_size: u64,
    pub shared_size: u64,
    pub is_compressed: bool,
    pub name: String,
}

pub struct FolderInfo {
    pub logical_size: u64,
    pub shared_size: u64,
    pub files: Vec<FileInfo>,
}

pub fn get_folder_size(path: &Path) -> Option<FolderInfo> {
    if !path.is_dir() {
        return None;
    }

    let mut fi = FolderInfo {
        logical_size: 0,
        shared_size: 0,
        files: Vec::new(),
    };

    let entries = fs::read_dir(path).ok()?;
    for entry in entries.flatten() {
        let entry_path = entry.path();

        if entry_path.is_dir() {
            if let Some(sub) = get_folder_size(&entry_path) {
                fi.logical_size += sub.logical_size;
                fi.shared_size += sub.shared_size;
                fi.files.extend(sub.files);
            }
            continue;
        }

        let metadata = match fs::metadata(&entry_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let real_size = metadata.len();

        let extents = fs::File::open(&entry_path).ok().and_then(|f| check_file(f).ok());
        let (shared_size, is_compressed) = match &extents {
            Some(es) => (
                es.iter()
                    .filter(|f| f.flags.contains(&FiemapFlags::Shared))
                    .map(|f| f.extent.fe_length)
                    .sum::<u64>(),
                es.iter().any(|f| f.flags.contains(&FiemapFlags::Encoded)),
            ),
            None => (0, false),
        };

        fi.logical_size += real_size;
        fi.shared_size += shared_size;
        fi.files.push(FileInfo {
            real_size,
            shared_size,
            is_compressed,
            name: entry_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        });
    }

    Some(fi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn from_bits_extracts_individual_flags() {
        let flags = FiemapFlags::from_bits(FiemapFlags::Last as u32);
        assert_eq!(flags, vec![FiemapFlags::Last]);

        let flags = FiemapFlags::from_bits(FiemapFlags::Shared as u32);
        assert_eq!(flags, vec![FiemapFlags::Shared]);
    }

    #[test]
    fn from_bits_combines_flags() {
        let combined = FiemapFlags::Last as u32 | FiemapFlags::Shared as u32;
        let flags = FiemapFlags::from_bits(combined);
        assert!(flags.contains(&FiemapFlags::Last));
        assert!(flags.contains(&FiemapFlags::Shared));
        assert_eq!(flags.len(), 2);
    }

    #[test]
    fn from_bits_returns_empty_for_zero() {
        assert!(FiemapFlags::from_bits(0).is_empty());
    }

    #[test]
    fn get_folder_size_returns_none_for_non_existing() {
        assert!(get_folder_size(Path::new("/path/that/definitely/does/not/exist")).is_none());
    }

    #[test]
    fn get_folder_size_returns_none_for_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hi").unwrap();
        assert!(get_folder_size(&file).is_none());
    }

    #[test]
    fn get_folder_size_sums_logical_sizes() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");

        let mut fa = std::fs::File::create(&a).unwrap();
        fa.write_all(&[0u8; 100]).unwrap();
        let mut fb = std::fs::File::create(&b).unwrap();
        fb.write_all(&[0u8; 250]).unwrap();

        let info = get_folder_size(dir.path()).unwrap();
        assert_eq!(info.logical_size, 350);
        assert_eq!(info.files.len(), 2);
    }

    #[test]
    fn get_folder_size_recurses_into_subdirs() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        std::fs::write(dir.path().join("top.txt"), &[0u8; 10][..]).unwrap();
        std::fs::write(sub.join("inner.txt"), &[0u8; 40][..]).unwrap();

        let info = get_folder_size(dir.path()).unwrap();
        assert_eq!(info.logical_size, 50);
        assert_eq!(info.files.len(), 2);
    }
}
