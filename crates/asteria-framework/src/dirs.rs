//! Moving an app's settings and data to where this version keeps them, before any is read.

/// Runs `migrate` for `app` (corundum's `AppDirs::migrate_from_roaming`, which moves Windows'
/// Roaming directories to Local and does nothing elsewhere). On failure the app must stop
/// without reading its settings or data, so the error says so; the caller reports it and exits.
pub fn migrate_dirs(
    app: &str,
    migrate: impl FnOnce(&str) -> std::io::Result<()>,
) -> Result<(), String> {
    migrate(app).map_err(|e| {
        format!(
            "cannot move {app}'s settings and data from Roaming to Local: {e}; nothing was read"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_migration_stops_before_anything_is_read() {
        let e =
            migrate_dirs("asteria-photos", |_| Err(std::io::Error::other("locked"))).unwrap_err();
        assert!(e.contains("asteria-photos"), "{e}");
        assert!(e.contains("locked"), "{e}");
        assert!(e.contains("nothing was read"), "{e}");
    }

    #[test]
    fn a_migration_that_succeeds_lets_the_app_start() {
        let mut seen = None;
        migrate_dirs("asteria-music", |app| {
            seen = Some(app.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.as_deref(), Some("asteria-music"));
    }
}
