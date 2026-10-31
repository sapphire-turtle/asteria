//! What the asteria client apps share: server profiles, the RPC client and the thumbnail cache.

pub mod errors;
mod fsutil;
pub mod i18n;
pub mod images;
pub mod media_cache;
pub mod profiles;
pub mod rpc;
pub mod saver;
pub mod session;
pub mod settings;
pub mod upload;
pub mod widgets;
mod worker;

/// Moves `app`'s settings, data and cache from Windows' Roaming directories to Local (a no-op
/// elsewhere). A client calls this first, before anything opens its settings; on `Err` it
/// reports the message and exits without reading them. What the move left in Roaming is
/// reported on standard error.
pub fn migrate_dirs(app: &str) -> Result<(), String> {
    use corundum_framework::platform::dirs::{AppDirs, Migrated};
    let left = asteria_framework::dirs::migrate_dirs(app, |a| {
        AppDirs::migrate_from_roaming(a).map(|m| match m {
            Migrated::Moved { left } | Migrated::Merged { left } => left,
            Migrated::Nothing => Vec::new(),
        })
    })?;
    asteria_framework::dirs::report_left(app, &left);
    Ok(())
}
