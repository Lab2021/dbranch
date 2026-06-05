use crate::error;
use std::fs::File;

pub trait CopyRef {
    fn copy_ref(&self, src: &File, dest: &File) -> Result<(), error::AppError>;
}

pub struct CopyRefOperator {}

impl CopyRefOperator {
    pub fn new() -> Self {
        Self {}
    }
}

impl CopyRef for CopyRefOperator {
    #[cfg(target_os = "linux")]
    fn copy_ref(&self, src: &File, dest: &File) -> Result<(), error::AppError> {
        use std::os::fd::AsRawFd;

        let len = src.metadata().map_err(|e| error::AppError::FileSystem {
            message: format!("Failed to read src metadata: {}", e),
        })?.len() as usize;

        // https://man7.org/linux/man-pages/man2/copy_file_range.2.html
        let ret = unsafe {
            nix::libc::copy_file_range(src.as_raw_fd(), &mut 0, dest.as_raw_fd(), &mut 0, len, 0)
        };

        if ret == -1 {
            let err = std::io::Error::last_os_error();
            return Err(error::AppError::FileSystem {
                message: format!("copy_file_range failed: {}", err),
            });
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn copy_ref(&self, _src: &File, _dest: &File) -> Result<(), error::AppError> {
        Err(error::AppError::FileSystem {
            message: "copy_ref is only supported on Linux".into(),
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use crate::fiemap::{FiemapFlags, check_file};

    use super::*;
    use std::{
        fs,
        io::{BufWriter, Write},
    };

    #[test]
    fn test_copy_ref_basic() {
        let operator = CopyRefOperator::new();

        let dir = std::path::Path::new("./test_data");
        fs::create_dir_all(dir).unwrap();
        let src_path = dir.join("source.txt");
        let dest_path = dir.join("dest.txt");
        const MSG: &str = "Eu gosto de memes\n";
        const FILE_SIZE: usize = 2 * 1024 * 1024;

        let mut writer = BufWriter::new(File::create(&src_path).unwrap());
        let chunk_size = MSG.len();
        let mut written = 0;

        while written < FILE_SIZE {
            let remaining = FILE_SIZE - written;
            if remaining < chunk_size {
                writer.write_all(&MSG.as_bytes()[..remaining]).unwrap();
                written += remaining;
            } else {
                writer.write_all(MSG.as_bytes()).unwrap();
                written += chunk_size;
            }
        }
        writer.flush().unwrap();

        let src = File::open(&src_path).unwrap();
        let dest = File::create(&dest_path).unwrap();

        operator.copy_ref(&src, &dest).expect("copy_ref failed");

        let src_content = fs::read_to_string(&src_path).unwrap();
        let dest_content = fs::read_to_string(&dest_path).unwrap();
        assert_eq!(src_content, dest_content);
        assert_eq!(
            src.metadata().unwrap().len(),
            dest.metadata().unwrap().len()
        );

        let to_tuples = |xs: Vec<crate::fiemap::Fiemap>| -> Vec<(u64, u64, u64, bool)> {
            xs.into_iter()
                .map(|f| {
                    (
                        f.extent.fe_logical,
                        f.extent.fe_physical,
                        f.extent.fe_length,
                        f.flags.contains(&FiemapFlags::Shared),
                    )
                })
                .collect()
        };

        let src_extents = to_tuples(check_file(src).unwrap());
        let dest_extents = to_tuples(check_file(dest).unwrap());
        assert_eq!(src_extents, dest_extents);
    }
}
