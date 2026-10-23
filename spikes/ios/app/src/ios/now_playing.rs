//! The lock screen and Control Center: `MPRemoteCommandCenter` handlers that turn play,
//! pause and play/pause into [`Command`]s, and the `MPNowPlayingInfoCenter` entry that
//! shows what plays.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPRemoteCommand, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};

use super::guarded;
use crate::audio::{Command, event_value, lock};
use crate::report::SharedReport;

/// The handlers' targets. Keep it for as long as the app runs.
pub struct RemoteTargets {
    _targets: Vec<Retained<AnyObject>>,
}

/// Enable play, pause and play/pause on the lock screen and in Control Center. Each sends
/// its [`Command`] on `tx`, counts itself in `report` under `audio.remote.<kind>` and
/// reports success.
pub fn register(tx: Sender<Command>, report: SharedReport) -> RemoteTargets {
    // SAFETY: the shared command center lives for the whole process.
    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    // SAFETY: the getters return the center's own command objects.
    let commands: [(Retained<MPRemoteCommand>, &str, Command); 3] = unsafe {
        [
            (center.playCommand(), "play", Command::Resume),
            (center.pauseCommand(), "pause", Command::Pause),
            (
                center.togglePlayPauseCommand(),
                "toggle_play_pause",
                Command::Toggle,
            ),
        ]
    };
    let mut targets = Vec::new();
    for (command, kind, cmd) in commands {
        let tx = tx.clone();
        let report = report.clone();
        let count = AtomicU64::new(0);
        let key = format!("audio.remote.{kind}");
        let handler = RcBlock::new(
            move |_event: NonNull<MPRemoteCommandEvent>| -> MPRemoteCommandHandlerStatus {
                guarded("remote command handler", (), || {
                    let _ = tx.send(cmd);
                    let n = count.fetch_add(1, Ordering::Relaxed) + 1;
                    lock(&report).set(&key, event_value(n, ""));
                });
                MPRemoteCommandHandlerStatus::Success
            },
        );
        // SAFETY: the command copies the block and calls it on the main thread; the block
        // only uses a channel, an atomic and a mutex.
        unsafe {
            command.setEnabled(true);
            targets.push(command.addTargetWithHandler(&handler));
        }
    }
    RemoteTargets { _targets: targets }
}

/// Show `title` as playing or paused at `elapsed_secs`. Callable from any thread; the
/// update itself happens on the main queue.
pub fn update(title: &str, playing: bool, elapsed_secs: f64) {
    let title = title.to_owned();
    DispatchQueue::main().exec_async(move || {
        guarded("now playing update", (), || {
            let title = NSString::from_str(&title);
            let rate = NSNumber::numberWithDouble(if playing { 1.0 } else { 0.0 });
            let elapsed = NSNumber::numberWithDouble(elapsed_secs);
            // SAFETY: reading immutable framework constants.
            let keys: [&NSString; 3] = unsafe {
                [
                    MPMediaItemPropertyTitle,
                    MPNowPlayingInfoPropertyPlaybackRate,
                    MPNowPlayingInfoPropertyElapsedPlaybackTime,
                ]
            };
            let objects: [&AnyObject; 3] = [title.as_ref(), rate.as_ref(), elapsed.as_ref()];
            let info = NSDictionary::from_slices(&keys, &objects);
            // SAFETY: on the main queue; the values have the types the keys document
            // (a string and two numbers).
            unsafe { MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(Some(&info)) };
        });
    });
}
