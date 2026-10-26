//! Filesystem helpers shared by the writers of original files.

use std::io::ErrorKind;
use std::path::Path;

/// Whether a failed `hard_link` should be retried as check-then-rename: every error except
/// `AlreadyExists`. Some filesystems (vfat, exFAT) report EPERM for hard links; if the
/// problem is a real permission error the rename fails too and that error is reported.
fn should_fallback(e: &std::io::Error) -> bool {
    e.kind() != ErrorKind::AlreadyExists
}

/// Moves `from` to `to` without ever replacing an existing `to`.
///
/// Uses `hard_link` (fails if `to` exists, so a racing creator is never overwritten)
/// followed by removing `from`. If `hard_link` fails for any other reason it falls back to check-then-`rename`, which is acceptable because all library
/// writes go through the single write queue.
pub(crate) fn move_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            return std::fs::remove_file(from).inspect_err(|_| {
                if let Err(e) = std::fs::remove_file(to) {
                    tracing::warn!(from = %from.display(), to = %to.display(), error = %e,
                        "rollback failed: both link names remain");
                }
            });
        }
        Err(e) if !should_fallback(&e) => return Err(e),
        Err(_) => {}
    }
    if to.symlink_metadata().is_ok() {
        return Err(ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_decision() {
        use std::io::{Error, ErrorKind::*};
        assert!(!should_fallback(&Error::from(AlreadyExists)));
        assert!(should_fallback(&Error::from(PermissionDenied)));
        assert!(should_fallback(&Error::from(Unsupported)));
    }
}
