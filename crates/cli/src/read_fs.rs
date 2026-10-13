//! Bounded reads shared by the source loader, the saved-plan reader and the build-control check.

use dfm_model::Refusal;
use dfm_model::digest::hex;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Reads at most `limit + 1` bytes of a regular file, so the caller can tell that the file is
/// over the limit. The open does not follow a final symlink and does not block on a FIFO, and
/// fstat then confirms the type, so an entry that changed type after it was listed is refused
/// (`not_regular`) instead of read. An I/O failure becomes the refusal that `io` builds.
pub fn read_regular_bounded(
    path: &Path,
    limit: u64,
    io: impl Fn(std::io::Error) -> Refusal,
    not_regular: impl FnOnce() -> Refusal,
) -> Result<Vec<u8>, Refusal> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(dfm_platform_macos::O_NOFOLLOW | dfm_platform_macos::O_NONBLOCK)
        .open(path)
        .map_err(&io)?;
    if !file.metadata().map_err(&io)?.is_file() {
        return Err(not_regular());
    }
    let mut buf = Vec::new();
    file.take(limit + 1).read_to_end(&mut buf).map_err(&io)?;
    Ok(buf)
}

/// SHA-256 of a file as lowercase hex, streamed so the file is never held in memory.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 16]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(h.finalize().as_slice()));
        }
        h.update(&buf[..n]);
    }
}
