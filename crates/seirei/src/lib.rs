pub mod config;
pub mod coordinator;
pub mod t3;

use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}
