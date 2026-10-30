//! [`Saver`]: only the newest value is written, and a failed write comes back.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_framework_client::saver::Saver;
use corundum_framework::egui;

const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn only_the_latest_value_is_written_and_drop_waits_for_it() {
    let written = Arc::new(Mutex::new(Vec::new()));
    let seen = written.clone();
    let saver = Saver::spawn("test-saver", egui::Context::default(), move |v: &u32| {
        seen.lock().unwrap().push(*v);
        Ok(())
    })
    .unwrap();
    saver.save(1);
    saver.save(2);
    saver.save(3);
    drop(saver);
    let written = written.lock().unwrap();
    assert_eq!(written.last(), Some(&3));
    assert!(written.len() <= 3);
}

#[test]
fn a_failed_write_is_handed_back() {
    let saver = Saver::spawn("test-saver", egui::Context::default(), |_: &u32| {
        Err("disk full".to_string())
    })
    .unwrap();
    assert_eq!(saver.take_error(), None);
    saver.save(1);
    let start = Instant::now();
    let error = loop {
        if let Some(e) = saver.take_error() {
            break e;
        }
        assert!(start.elapsed() < DEADLINE, "no error came back");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(error, "disk full");
    assert_eq!(saver.take_error(), None);
}
