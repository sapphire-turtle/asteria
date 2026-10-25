//! The photo library through PhotoKit: authorization, counts, the most recent assets and
//! streamed reads of their original resources. The callbacks run on PhotoKit's queues;
//! they only send on a channel and ask egui to repaint.

use std::ptr::NonNull;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use corundum_framework::egui;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSError, NSPredicate, NSSortDescriptor, NSString};
use objc2_photos::{
    PHAccessLevel, PHAsset, PHAssetMediaSubtype, PHAssetMediaType, PHAssetResource,
    PHAssetResourceManager, PHAssetResourceRequestOptions, PHAuthorizationStatus, PHFetchOptions,
    PHPhotoLibrary,
};

use super::guarded;
use crate::photos::{
    AssetInfo, Auth, FetchEvent, FetchResult, ResourceInfo, ResourceKind, StreamHash,
};

/// How often a read reports its progress at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// The current read-write authorization, without asking the person.
pub fn current_auth() -> Auth {
    // SAFETY: a class method with no preconditions, callable from any thread.
    auth(unsafe { PHPhotoLibrary::authorizationStatusForAccessLevel(PHAccessLevel::ReadWrite) })
}

/// Ask for read-write access (the system prompt shows only while the status is
/// undetermined) and send the outcome on `tx`.
pub fn request_auth(tx: Sender<Auth>, ctx: egui::Context) {
    let handler = RcBlock::new(move |status: PHAuthorizationStatus| {
        guarded("photo authorization handler", (), || {
            let _ = tx.send(auth(status));
            ctx.request_repaint();
        });
    });
    // SAFETY: PhotoKit copies the block and calls it once on an arbitrary queue; the
    // block only uses a channel and the egui context, both thread-safe.
    unsafe {
        PHPhotoLibrary::requestAuthorizationForAccessLevel_handler(
            PHAccessLevel::ReadWrite,
            &handler,
        )
    };
}

fn auth(status: PHAuthorizationStatus) -> Auth {
    match status {
        PHAuthorizationStatus::NotDetermined => Auth::NotDetermined,
        PHAuthorizationStatus::Restricted => Auth::Restricted,
        PHAuthorizationStatus::Authorized => Auth::Authorized,
        PHAuthorizationStatus::Limited => Auth::Limited,
        // Denied, and anything newer than this code: treat it as no access.
        _ => Auth::Denied,
    }
}

/// The number of images, videos and Live Photos the app can see.
pub fn counts() -> (usize, usize, usize) {
    // SAFETY: fetches with no options (or options built here) may run on any thread.
    unsafe {
        let images = PHAsset::fetchAssetsWithMediaType_options(PHAssetMediaType::Image, None);
        let videos = PHAsset::fetchAssetsWithMediaType_options(PHAssetMediaType::Video, None);
        // The predicate is built from a literal, so no variadic arguments are needed.
        let format = NSString::from_str(&format!(
            "(mediaSubtypes & {}) != 0",
            PHAssetMediaSubtype::PhotoLive.bits()
        ));
        let options = PHFetchOptions::new();
        options.setPredicate(Some(&NSPredicate::predicateWithFormat_argumentArray(
            &format, None,
        )));
        let live =
            PHAsset::fetchAssetsWithMediaType_options(PHAssetMediaType::Image, Some(&options));
        (images.count(), videos.count(), live.count())
    }
}

/// The `limit` most recently created assets, newest first.
pub fn recent(limit: usize) -> Vec<AssetInfo> {
    // SAFETY: as in `counts`; the fetch result and its assets stay alive while read.
    unsafe {
        let options = PHFetchOptions::new();
        let newest_first = NSSortDescriptor::sortDescriptorWithKey_ascending(
            Some(&NSString::from_str("creationDate")),
            false,
        );
        options.setSortDescriptors(Some(&NSArray::from_retained_slice(&[newest_first])));
        options.setFetchLimit(limit);
        let result = PHAsset::fetchAssetsWithOptions(Some(&options));
        (0..result.count())
            .map(|i| asset_info(&result.objectAtIndex(i)))
            .collect()
    }
}

fn asset_info(asset: &PHAsset) -> AssetInfo {
    // SAFETY: plain property reads of a fetched asset.
    unsafe {
        AssetInfo {
            local_id: asset.localIdentifier().to_string(),
            created: asset
                .creationDate()
                .map(|date| date.description().to_string()),
            is_live: asset
                .mediaSubtypes()
                .contains(PHAssetMediaSubtype::PhotoLive),
            resources: PHAssetResource::assetResourcesForAsset(asset)
                .iter()
                .map(|resource| ResourceInfo {
                    kind: ResourceKind::from_raw(resource.r#type().0 as i64),
                    original_filename: resource.originalFilename().to_string(),
                    // `contentType` replaces it only from iOS 26; the app targets iOS 16.
                    #[allow(deprecated)]
                    uti: resource.uniformTypeIdentifier().to_string(),
                })
                .collect(),
        }
    }
}

/// The resource at `resource_index` of the asset `local_id`, if both still exist and it
/// is still the resource `expected` describes.
fn find_resource(
    local_id: &str,
    resource_index: usize,
    expected: &ResourceInfo,
) -> Result<Retained<PHAssetResource>, String> {
    let ids = NSArray::from_retained_slice(&[NSString::from_str(local_id)]);
    // SAFETY: as in `counts`.
    unsafe {
        let asset = PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, None)
            .firstObject()
            .ok_or("asset not found")?;
        let resources = PHAssetResource::assetResourcesForAsset(&asset);
        if resource_index >= resources.count() {
            return Err(format!("resource {resource_index} not found"));
        }
        let resource = resources.objectAtIndex(resource_index);
        let kind = ResourceKind::from_raw(resource.r#type().0 as i64);
        let name = resource.originalFilename().to_string();
        if kind != expected.kind || name != expected.original_filename {
            return Err(format!(
                "resource {resource_index} is now {kind:?} {name}, not {:?} {}",
                expected.kind, expected.original_filename
            ));
        }
        Ok(resource)
    }
}

/// What the handlers of one read share.
struct Reading {
    hash: Option<StreamHash>,
    last_progress: Instant,
    from_icloud: bool,
}

impl Reading {
    /// Whether enough time passed since the last progress event to send another.
    fn progress_due(&mut self) -> bool {
        let due = self.last_progress.elapsed() >= PROGRESS_INTERVAL;
        if due {
            self.last_progress = Instant::now();
        }
        due
    }
}

/// Identifies a read in flight, for [`cancel`].
pub type RequestId = i32;

/// Read resource `resource_index` of asset `local_id` (downloading it from iCloud if
/// needed), hashing each chunk as it arrives so the file is never held whole. The
/// resource must still be the one `expected` describes. Sends [`FetchEvent::Progress`]
/// and [`FetchEvent::Downloading`] at most every 100 ms and then one
/// [`FetchEvent::Done`]. Returns the request, or `None` if the read failed to start (its
/// `Done` is already sent).
pub fn fetch_original(
    local_id: &str,
    resource_index: usize,
    expected: &ResourceInfo,
    tx: Sender<FetchEvent>,
    ctx: egui::Context,
) -> Option<RequestId> {
    let started = Instant::now();
    let resource = match find_resource(local_id, resource_index, expected) {
        Ok(resource) => resource,
        Err(error) => {
            let _ = tx.send(FetchEvent::Done(FetchResult {
                bytes: 0,
                hash: String::new(),
                secs: 0.0,
                error: Some(error),
                from_icloud: false,
            }));
            ctx.request_repaint();
            return None;
        }
    };
    let reading = Arc::new(Mutex::new(Reading {
        hash: Some(StreamHash::new()),
        last_progress: started,
        from_icloud: false,
    }));

    let on_download = {
        let (reading, tx, ctx) = (reading.clone(), tx.clone(), ctx.clone());
        RcBlock::new(move |fraction: f64| {
            guarded("photo download progress handler", (), || {
                let mut reading = reading.lock().unwrap_or_else(|e| e.into_inner());
                reading.from_icloud = true;
                if reading.progress_due() || fraction >= 1.0 {
                    let _ = tx.send(FetchEvent::Downloading(fraction));
                    ctx.request_repaint();
                }
            });
        })
    };
    let on_data = {
        let (reading, tx, ctx) = (reading.clone(), tx.clone(), ctx.clone());
        RcBlock::new(move |data: NonNull<NSData>| {
            guarded("photo data handler", (), || {
                // SAFETY: PhotoKit passes valid, immutable data for the call's duration.
                let chunk = unsafe { data.as_ref().as_bytes_unchecked() };
                let mut reading = reading.lock().unwrap_or_else(|e| e.into_inner());
                let Some(hash) = reading.hash.as_mut() else {
                    return;
                };
                hash.update(chunk);
                let bytes = hash.bytes();
                if reading.progress_due() {
                    let _ = tx.send(FetchEvent::Progress(bytes));
                    ctx.request_repaint();
                }
            });
        })
    };
    let on_done = RcBlock::new(move |error: *mut NSError| {
        guarded("photo completion handler", (), || {
            // SAFETY: PhotoKit passes null or a valid error for the call's duration.
            let error = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
            let (hash, from_icloud) = {
                let mut reading = reading.lock().unwrap_or_else(|e| e.into_inner());
                (reading.hash.take().unwrap_or_default(), reading.from_icloud)
            };
            let (bytes, hash) = hash.finish();
            let _ = tx.send(FetchEvent::Done(FetchResult {
                bytes,
                hash,
                secs: started.elapsed().as_secs_f64(),
                error,
                from_icloud,
            }));
            ctx.request_repaint();
        });
    });
    // SAFETY: the options copy the progress block and the manager copies the other two;
    // PhotoKit calls them on its own queues, and they only use a mutex, a channel and the
    // egui context. `resource` came from a fetch just now.
    unsafe {
        let options = PHAssetResourceRequestOptions::new();
        options.setNetworkAccessAllowed(true);
        options.setProgressHandler(RcBlock::as_ptr(&on_download));
        let request = PHAssetResourceManager::defaultManager()
            .requestDataForAssetResource_options_dataReceivedHandler_completionHandler(
                &resource,
                Some(&options),
                &on_data,
                &on_done,
            );
        Some(request)
    }
}

/// Cancel the read `request`. PhotoKit then calls its completion handler with an error.
pub fn cancel(request: RequestId) {
    // SAFETY: cancelling an unknown or finished request is a no-op.
    unsafe { PHAssetResourceManager::defaultManager().cancelDataRequest(request) };
}
