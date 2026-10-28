//! The thread that owns the [`Engine`]: commands in, state out.
//!
//! The loop waits for a command for as long as `pump` asked (~10 ms while playing, so the
//! 200 ms ring in `output` holds twenty rounds), pumps after every command and every
//! timeout, and publishes the state when it changed. The window is asked to repaint on every
//! change but the position, which only asks every 250 ms.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use corundum_framework::egui;

use super::engine::{Engine, Opener, OutputFactory};
use super::{Command, PlayerState};

/// How often a moving position repaints the window at most.
const POSITION_REPAINT: Duration = Duration::from_millis(250);

enum Message {
    Command(Command),
    Quit,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The playback engine on a thread of its own.
pub struct Player {
    tx: mpsc::Sender<Message>,
    state: Arc<Mutex<PlayerState>>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Runs `Engine::new(opener, outputs)` on a thread named "asteria-music-player".
    /// `ctx` is asked to repaint when the state changes (the position at most every 250 ms).
    pub fn spawn(
        opener: Box<dyn Opener>,
        outputs: Box<dyn OutputFactory>,
        ctx: egui::Context,
    ) -> std::io::Result<Player> {
        let (tx, rx) = mpsc::channel();
        let engine = Engine::new(opener, outputs);
        let state = Arc::new(Mutex::new(engine.state().clone()));
        let shared = state.clone();
        let thread = std::thread::Builder::new()
            .name("asteria-music-player".into())
            .spawn(move || run(engine, rx, shared, ctx))?;
        Ok(Player {
            tx,
            state,
            thread: Some(thread),
        })
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send(Message::Command(command));
    }

    /// A copy of the state as of the last publish.
    pub fn state(&self) -> PlayerState {
        lock(&self.state).clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Command(Command::Stop));
        let _ = self.tx.send(Message::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    mut engine: Engine,
    rx: mpsc::Receiver<Message>,
    shared: Arc<Mutex<PlayerState>>,
    ctx: egui::Context,
) {
    let mut last_repaint = Instant::now();
    let mut wait: Option<Duration> = None;
    loop {
        let message = match wait {
            Some(d) => match rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
        };
        match message {
            Some(Message::Quit) => return,
            Some(Message::Command(c)) => engine.handle(c),
            None => {}
        }
        wait = engine.pump();
        publish(&engine, &shared, &ctx, &mut last_repaint);
    }
}

/// Copies the engine's state to `shared` when it changed, and repaints.
fn publish(
    engine: &Engine,
    shared: &Mutex<PlayerState>,
    ctx: &egui::Context,
    last_repaint: &mut Instant,
) {
    let now = engine.state();
    let mut s = lock(shared);
    if *s == *now {
        return;
    }
    let only_position = s.queue == now.queue
        && s.current == now.current
        && s.status == now.status
        && s.duration_ms == now.duration_ms
        && s.volume == now.volume
        && s.error == now.error;
    *s = now.clone();
    drop(s);
    if !only_position || last_repaint.elapsed() >= POSITION_REPAINT {
        *last_repaint = Instant::now();
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::player::fakes::{FakeOpener, FakeOutputs, entry};
    use crate::player::{Command, Status};

    fn player() -> (Player, FakeOutputs) {
        let outs = FakeOutputs::new();
        let p = Player::spawn(
            Box::new(FakeOpener::new()),
            Box::new(outs.clone()),
            corundum_framework::egui::Context::default(),
        )
        .unwrap();
        (p, outs)
    }

    fn wait_for(p: &Player, ok: impl Fn(&PlayerState) -> bool) -> PlayerState {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let s = p.state();
            if ok(&s) || Instant::now() > end {
                return s;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn commands_reach_the_engine_and_the_state_comes_back() {
        let (p, outs) = player();
        p.send(Command::Replace {
            entries: vec![entry("a"), entry("b")],
            start: 1,
        });
        let s = wait_for(&p, |s| s.status == Status::Playing);
        assert_eq!(s.queue.len(), 2);
        assert_eq!(s.current, Some(1));
        assert!(outs.current().is_some());
        p.send(Command::SetVolume(0.5));
        let s = wait_for(&p, |s| s.volume == 0.5);
        assert_eq!(s.volume, 0.5);
        p.send(Command::Pause);
        let s = wait_for(&p, |s| s.status == Status::Paused);
        assert_eq!(s.status, Status::Paused);
    }

    #[test]
    fn dropping_the_player_joins_its_thread() {
        let (p, _outs) = player();
        p.send(Command::Replace {
            entries: vec![entry("a")],
            start: 0,
        });
        let start = Instant::now();
        drop(p);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
