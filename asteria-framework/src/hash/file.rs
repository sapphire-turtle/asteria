//! `file/1`: BLAKE3 over the whole file.

use std::fs::File;
use std::path::Path;

use super::ContentHash;
use crate::Result;

pub(crate) fn file_hash(path: &Path) -> Result<ContentHash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut file)?;
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}
