//! The photo library through PhotoKit: authorization, counts, the most recent assets and
//! streamed reads of their original resources. The callbacks run on PhotoKit's queues;
//! they only send on a channel and ask egui to repaint.

use std::ptr::NonNull;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use corundum_gui::egui;
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

/// The resource at `resource_index` of the asset `local_id`, if both still exist.
fn find_resource(local_id: &str, resource_index: usize) -> Option<Retained<PHAssetResource>> {
    let ids = NSArray::from_retained_slice(&[NSString::from_str(local_id)]);
    // SAFETY: as in `counts`.
    unsafe {
        let asset = PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, None).firstObject()?;
        let resources = PHAssetResource::assetResourcesForAsset(&asset);
        (resource_index < resources.count()).then(|| resources.objectAtIndex(resource_index))
    }
}

/// What the data handler and the completion handler share.
struct Reading {
    hash: Option<StreamHash>,
    last_progress: Instant,
}

/// Read resource `resource_index` of asset `local_id` (downloading it from iCloud if
/// needed), hashing each chunk as it arrives so the file is never held whole. Sends
/// [`FetchEvent::Progress`] at most every 100 ms and then one [`FetchEvent::Done`].
pub fn fetch_original(
    local_id: &str,
    resource_index: usize,
    tx: Sender<FetchEvent>,
    ctx: egui::Context,
) {
    let started = Instant::now();
    let Some(resource) = find_resource(local_id, resource_index) else {
        let _ = tx.send(FetchEvent::Done(FetchResult {
            bytes: 0,
            hash: String::new(),
            secs: 0.0,
            error: Some("asset or resource not found".to_owned()),
        }));
        ctx.request_repaint();
        return;
    };
    let reading = Arc::new(Mutex::new(Reading {
        hash: Some(StreamHash::new()),
        last_progress: started,
    }));

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
                if reading.last_progress.elapsed() >= PROGRESS_INTERVAL {
                    reading.last_progress = Instant::now();
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
            let hash = reading
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .hash
                .take()
                .unwrap_or_default();
            let (bytes, hash) = hash.finish();
            let _ = tx.send(FetchEvent::Done(FetchResult {
                bytes,
                hash,
                secs: started.elapsed().as_secs_f64(),
                error,
            }));
            ctx.request_repaint();
        });
    });
    // SAFETY: the manager copies both blocks and calls them on its own queue; they only
    // use a mutex, a channel and the egui context. `resource` came from a fetch just now.
    unsafe {
        let options = PHAssetResourceRequestOptions::new();
        options.setNetworkAccessAllowed(true);
        PHAssetResourceManager::defaultManager()
            .requestDataForAssetResource_options_dataReceivedHandler_completionHandler(
                &resource,
                Some(&options),
                &on_data,
                &on_done,
            );
    }
}
