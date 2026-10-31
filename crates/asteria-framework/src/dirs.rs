//! Moving an app's settings and data to where this version keeps them, before any is read.

use std::io;
use std::path::{Path, PathBuf};

/// Runs `migrate` for `app` (corundum's `AppDirs::migrate_from_roaming`, which moves Windows'
/// Roaming directories to Local and does nothing elsewhere) and returns what it left in
/// Roaming, for the caller to report. On failure the app must stop without reading its
/// settings or data, so the error says so; the caller reports it and exits.
pub fn migrate_dirs(
    app: &str,
    migrate: impl FnOnce(&str) -> io::Result<Vec<PathBuf>>,
) -> Result<Vec<PathBuf>, String> {
    let dirs = roaming_and_local(app);
    migrate_in(
        app,
        dirs.as_ref().map(|(r, l)| (r.as_path(), l.as_path())),
        migrate,
    )
}

/// Tells the person, on standard error, what [`migrate_dirs`] left in Roaming: the app runs
/// without it, so they can move it by hand. Standard error, since a server's log is not set
/// up yet when it moves its directories.
pub fn report_left(app: &str, left: &[PathBuf]) {
    for path in left {
        eprintln!(
            "warning: {app} left {} in Roaming; it is not used any more, move it to Local by hand if you need it",
            path.display()
        );
    }
}

/// [`migrate_dirs`] with the Roaming and Local directories of `app` given (Windows only).
fn migrate_in(
    app: &str,
    dirs: Option<(&Path, &Path)>,
    migrate: impl FnOnce(&str) -> io::Result<Vec<PathBuf>>,
) -> Result<Vec<PathBuf>, String> {
    let failed = |e: io::Error| {
        format!(
            "cannot move {app}'s settings and data from Roaming to Local: {e}; nothing was read"
        )
    };
    if let Some((roaming, local)) = dirs {
        clear_stale_cache(local, roaming).map_err(failed)?;
    }
    migrate(app).map_err(failed)
}

/// `%APPDATA%\<app>` and `%LOCALAPPDATA%\<app>` on Windows; `None` elsewhere.
fn roaming_and_local(app: &str) -> Option<(PathBuf, PathBuf)> {
    if !cfg!(windows) {
        return None;
    }
    let dir = |var| {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join(app))
    };
    Some((dir("APPDATA")?, dir("LOCALAPPDATA")?))
}

/// Before an earlier version's Roaming directory merges into Local: removes the directories
/// at the top of `local` that `roaming` also has and that hold no server state (`journal`,
/// `uploads`, `server.lock`), returning them. Before corundum 0.4.0 the Local directory held
/// only the cache, the server's under the same `<library id>` names as its state in Roaming,
/// and a merge leaves in Roaming whatever Local already has. Files are left alone.
pub fn clear_stale_cache(local: &Path, roaming: &Path) -> io::Result<Vec<PathBuf>> {
    let mut cleared = Vec::new();
    let entries = match std::fs::read_dir(roaming) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cleared),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let name = entry?.file_name();
        let old = local.join(&name);
        let is_dir = std::fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir());
        if !is_dir
            || ["journal", "uploads", "server.lock"]
                .iter()
                .any(|s| old.join(s).exists())
        {
            continue;
        }
        std::fs::remove_dir_all(&old)?;
        cleared.push(old);
    }
    cleared.sort();
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_failed_migration_stops_before_anything_is_read() {
        let e = migrate_dirs("asteria-photos", |_| Err(io::Error::other("locked"))).unwrap_err();
        assert!(e.contains("asteria-photos"), "{e}");
        assert!(e.contains("locked"), "{e}");
        assert!(e.contains("nothing was read"), "{e}");
    }

    #[test]
    fn a_migration_that_succeeds_lets_the_app_start() {
        let mut seen = None;
        let left = migrate_dirs("asteria-music", |app| {
            seen = Some(app.to_owned());
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(seen.as_deref(), Some("asteria-music"));
        assert!(left.is_empty());
    }

    #[test]
    fn what_the_migration_left_is_returned() {
        let left =
            migrate_dirs("asteria-music", |_| Ok(vec![PathBuf::from("Roaming/lib")])).unwrap();
        assert_eq!(left, [PathBuf::from("Roaming/lib")]);
    }

    /// Before corundum 0.4.0 the server's cache lived at `Local\<app>\<library>`, and its
    /// journal and uploads at `Roaming\<app>\<library>`: the same name on both sides, which a
    /// merge leaves in Roaming. The old cache goes first, so the state can move.
    #[test]
    fn an_earlier_versions_cache_gives_way_to_the_moving_state() {
        let d = tempfile::tempdir().unwrap();
        let (local, roaming) = (d.path().join("local"), d.path().join("roaming"));
        // An old cache, named like a library whose state is in Roaming: removed.
        fs::create_dir_all(local.join("lib1/thumbs")).unwrap();
        fs::write(local.join("lib1/cache.redb"), b"cache").unwrap();
        fs::create_dir_all(roaming.join("lib1/journal")).unwrap();
        // Local already holds state (a journal, a server lock): never removed.
        fs::create_dir_all(local.join("lib2/journal")).unwrap();
        fs::create_dir_all(roaming.join("lib2/uploads")).unwrap();
        fs::create_dir_all(local.join("lib4")).unwrap();
        fs::write(local.join("lib4/server.lock"), b"").unwrap();
        fs::create_dir_all(roaming.join("lib4")).unwrap();
        // A cache with no counterpart in Roaming: left alone.
        fs::create_dir_all(local.join("lib3/thumbs")).unwrap();
        // Files are settings, never cache: left alone.
        fs::write(local.join("servers.toml"), b"local").unwrap();
        fs::write(roaming.join("servers.toml"), b"roaming").unwrap();

        let cleared = clear_stale_cache(&local, &roaming).unwrap();
        assert_eq!(cleared, [local.join("lib1")]);
        assert!(!local.join("lib1").exists());
        assert!(local.join("lib2/journal").is_dir());
        assert!(local.join("lib3/thumbs").is_dir());
        assert!(local.join("lib4/server.lock").is_file());
        assert_eq!(fs::read(local.join("servers.toml")).unwrap(), b"local");
        assert!(roaming.join("lib1/journal").is_dir());
    }

    #[test]
    fn no_local_or_roaming_directory_clears_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (local, roaming) = (d.path().join("local"), d.path().join("roaming"));
        assert!(clear_stale_cache(&local, &roaming).unwrap().is_empty());
        fs::create_dir_all(local.join("lib1")).unwrap();
        assert!(clear_stale_cache(&local, &roaming).unwrap().is_empty());
        assert!(local.join("lib1").is_dir());
    }

    #[test]
    fn the_cache_is_cleared_before_the_move() {
        let d = tempfile::tempdir().unwrap();
        let (local, roaming) = (d.path().join("local"), d.path().join("roaming"));
        fs::create_dir_all(local.join("lib1/thumbs")).unwrap();
        fs::create_dir_all(roaming.join("lib1/journal")).unwrap();
        let mut stale_at_move = None;
        migrate_in("asteria-photos", Some((&roaming, &local)), |_| {
            stale_at_move = Some(local.join("lib1/thumbs").exists());
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(stale_at_move, Some(false));
    }
}
