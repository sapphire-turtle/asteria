//! The rating queue kept on disk, so a rating made offline is sent once the server is back.

// Nothing outside the tests uses the file yet; the sender thread (next task) does.
#[allow(dead_code)]
mod file;

use std::path::{Path, PathBuf};

use crate::media_cache::safe_component;

/// `<data_root>/queue/<library_id>/<profile_id>.jsonl`, or `None` if either id is not a safe
/// file name. One queue per library and profile, so two users of one server never share one.
pub fn queue_path(data_root: &Path, library_id: &str, profile_id: &str) -> Option<PathBuf> {
    (safe_component(library_id) && safe_component(profile_id)).then(|| {
        data_root
            .join("queue")
            .join(library_id)
            .join(format!("{profile_id}.jsonl"))
    })
}
