//! The app's `AVAudioSession`: the playback category, so audio keeps going with the screen
//! locked or the app in the background, and the interruption and route-change
//! notifications, turned into [`Command`]s for the engine.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionInterruptionNotification,
    AVAudioSessionInterruptionOptionKey, AVAudioSessionInterruptionOptions,
    AVAudioSessionInterruptionType, AVAudioSessionInterruptionTypeKey,
    AVAudioSessionRouteChangeNotification, AVAudioSessionRouteChangeReason,
    AVAudioSessionRouteChangeReasonKey,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNumber, NSString};

use super::guarded;
use crate::audio::{Command, event_value, lock};
use crate::report::SharedReport;

/// Set the session's category to playback and activate it. Call before the first output
/// stream is built.
pub fn activate_playback() -> Result<(), String> {
    // SAFETY: `sharedInstance` returns the process-wide session, valid for the process.
    let session = unsafe { AVAudioSession::sharedInstance() };
    // SAFETY: reading an immutable framework constant.
    let category = unsafe { AVAudioSessionCategoryPlayback }
        .ok_or("AVAudioSessionCategoryPlayback is missing")?;
    // SAFETY: `category` is one of the framework's category names.
    unsafe { session.setCategory_error(category) }
        .map_err(|e| format!("setCategory: {}", e.localizedDescription()))?;
    reactivate()
}

/// Activate the session (again, after an interruption ends).
fn reactivate() -> Result<(), String> {
    // SAFETY: as in `activate_playback`; `setActive:error:` may be called from any thread.
    let session = unsafe { AVAudioSession::sharedInstance() };
    unsafe { session.setActive_error(true) }
        .map_err(|e| format!("setActive: {}", e.localizedDescription()))
}

/// The notification observers. Keep it for as long as the app runs.
pub struct Observers {
    _tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

/// Watch for interruptions and route changes, send the matching [`Command`] on `tx`, and
/// count them in `report` (`audio.interruptions`, `audio.route_changes`). The
/// notifications arrive on whichever thread posts them.
pub fn observe(tx: Sender<Command>, report: SharedReport) -> Observers {
    let center = NSNotificationCenter::defaultCenter();
    let mut tokens = Vec::new();

    let interruption = {
        let tx = tx.clone();
        let report = report.clone();
        let count = AtomicU64::new(0);
        RcBlock::new(move |notification: NonNull<NSNotification>| {
            guarded("interruption observer", (), || {
                // SAFETY: the center passes a valid notification for the call's duration.
                let notification = unsafe { notification.as_ref() };
                let Some((cmd, detail)) = interruption_command(notification) else {
                    return;
                };
                if let Command::InterruptionEnded {
                    should_resume: true,
                } = cmd
                    && let Err(e) = reactivate()
                {
                    super::log::log(&format!("audio session: {e}"));
                }
                let _ = tx.send(cmd);
                let n = count.fetch_add(1, Ordering::Relaxed) + 1;
                lock(&report).set("audio.interruptions", event_value(n, &detail));
            });
        })
    };
    // SAFETY: reading an immutable framework constant.
    if let Some(name) = unsafe { AVAudioSessionInterruptionNotification } {
        // SAFETY: the block is `Send`-safe (it only uses a channel, an atomic and a mutex)
        // and runs on the posting thread because no queue is given.
        tokens.push(unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &interruption)
        });
    }

    let route_change = {
        let count = AtomicU64::new(0);
        RcBlock::new(move |notification: NonNull<NSNotification>| {
            guarded("route change observer", (), || {
                // SAFETY: the center passes a valid notification for the call's duration.
                let notification = unsafe { notification.as_ref() };
                // SAFETY: reading an immutable framework constant.
                let key = unsafe { AVAudioSessionRouteChangeReasonKey };
                let reason =
                    user_info_number(notification, key).map(AVAudioSessionRouteChangeReason);
                if reason == Some(AVAudioSessionRouteChangeReason::OldDeviceUnavailable) {
                    let _ = tx.send(Command::OldDeviceUnavailable);
                }
                let n = count.fetch_add(1, Ordering::Relaxed) + 1;
                let detail = reason.map_or("no reason".to_owned(), route_reason_name);
                lock(&report).set("audio.route_changes", event_value(n, &detail));
            });
        })
    };
    // SAFETY: reading an immutable framework constant.
    if let Some(name) = unsafe { AVAudioSessionRouteChangeNotification } {
        // SAFETY: as for the interruption observer.
        tokens.push(unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &route_change)
        });
    }

    Observers { _tokens: tokens }
}

/// The command for an interruption notification, with a word on it for the report.
fn interruption_command(notification: &NSNotification) -> Option<(Command, String)> {
    // SAFETY: reading immutable framework constants.
    let (type_key, option_key) = unsafe {
        (
            AVAudioSessionInterruptionTypeKey,
            AVAudioSessionInterruptionOptionKey,
        )
    };
    let kind = AVAudioSessionInterruptionType(user_info_number(notification, type_key)?);
    if kind == AVAudioSessionInterruptionType::Began {
        return Some((Command::InterruptionBegan, "began".to_owned()));
    }
    if kind != AVAudioSessionInterruptionType::Ended {
        return None;
    }
    let options =
        AVAudioSessionInterruptionOptions(user_info_number(notification, option_key).unwrap_or(0));
    let should_resume = options.contains(AVAudioSessionInterruptionOptions::ShouldResume);
    Some((
        Command::InterruptionEnded { should_resume },
        format!("ended, should_resume={should_resume}"),
    ))
}

/// The number stored under `key` in the notification's `userInfo`.
fn user_info_number(notification: &NSNotification, key: Option<&NSString>) -> Option<usize> {
    let info = notification.userInfo()?;
    let value = info.objectForKey(key?)?;
    Some(value.downcast::<NSNumber>().ok()?.unsignedIntegerValue())
}

fn route_reason_name(reason: AVAudioSessionRouteChangeReason) -> String {
    let name = match reason {
        AVAudioSessionRouteChangeReason::NewDeviceAvailable => "NewDeviceAvailable",
        AVAudioSessionRouteChangeReason::OldDeviceUnavailable => "OldDeviceUnavailable",
        AVAudioSessionRouteChangeReason::CategoryChange => "CategoryChange",
        AVAudioSessionRouteChangeReason::Override => "Override",
        AVAudioSessionRouteChangeReason::WakeFromSleep => "WakeFromSleep",
        AVAudioSessionRouteChangeReason::NoSuitableRouteForCategory => "NoSuitableRouteForCategory",
        AVAudioSessionRouteChangeReason::RouteConfigurationChange => "RouteConfigurationChange",
        other => return format!("reason {}", other.0),
    };
    name.to_owned()
}
