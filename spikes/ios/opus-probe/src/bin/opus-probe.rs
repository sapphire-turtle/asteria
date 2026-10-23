//! `opus-probe [FILE]`: decodes FILE (default: the bundled sample) with each
//! candidate and prints one table row per decoder. Run it with `--release`:
//! the speed column means nothing in a debug build.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use opus_probe::{Candidate, SAMPLE, libopus_reference, measure, read_ogg_opus};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("opus-probe: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let file = std::env::args_os().nth(1).map(PathBuf::from);
    let (bytes, path) = match &file {
        Some(p) => (
            std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?,
            p.clone(),
        ),
        None => (
            SAMPLE.to_vec(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample.opus"),
        ),
    };
    let stream = read_ogg_opus(&bytes).map_err(|e| e.to_string())?;
    let reference = match libopus_reference(&path) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("no libopus reference ({e}); snr_db will be empty");
            None
        }
    };
    println!(
        "{:<13} {:<18} {:>10} {:>9}  {:<16}  error",
        "name", "license", "x_realtime", "snr_db", "pcm_hash"
    );
    for c in Candidate::ALL {
        let m = measure(c, &stream, reference.as_deref());
        println!(
            "{:<13} {:<18} {:>10.1} {:>9}  {:<16}  {}",
            m.candidate.name(),
            m.candidate.license(),
            m.x_realtime,
            m.snr_db.map_or("-".into(), |s| format!("{s:.1}")),
            m.pcm_hash
                .as_deref()
                .map_or("-", |h| h.strip_prefix("blake3:").map_or(h, |x| &x[..16])),
            m.error.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}
