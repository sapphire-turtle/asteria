use std::path::Path;

use opus_probe::{
    Candidate, ProbeError, SAMPLE, decode, libopus_reference, measure, read_ogg_opus, snr_db,
};

fn rms(pcm: &[f32]) -> f64 {
    (pcm.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
}

#[test]
fn reads_head_of_the_sample() {
    let s = read_ogg_opus(SAMPLE).unwrap();
    assert_eq!(s.channels, 2);
    assert!(s.pre_skip > 0);
    assert!(!s.packets.is_empty());
}

#[test]
fn every_supported_candidate_decodes_the_full_length_and_not_silence() {
    let s = read_ogg_opus(SAMPLE).unwrap();
    let want = (s.final_granule - u64::from(s.pre_skip)) as usize * 2;
    let mut supported = 0;
    for c in Candidate::ALL {
        match decode(c, &s) {
            Err(ProbeError::Unsupported(why)) => eprintln!("{} unsupported: {why}", c.name()),
            Err(e) => panic!("{}: {e}", c.name()),
            Ok(pcm) => {
                supported += 1;
                assert_eq!(pcm.len(), want, "{} length", c.name());
                assert!(rms(&pcm) > 0.01, "{} is silent", c.name());
            }
        }
    }
    assert!(supported > 0, "no candidate can decode");
}

#[test]
fn snr_of_identical_signals_is_infinite_and_of_noise_is_low() {
    let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
    assert_eq!(snr_db(&tone, &tone), f64::INFINITY);
    // A different tone is uncorrelated with the first, so its SNR is far below 10 dB.
    let other: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.37 + 1.0).sin()).collect();
    assert!(snr_db(&tone, &other) < 10.0);
}

#[test]
fn rejects_non_opus_ogg_and_garbage() {
    assert!(read_ogg_opus(&[]).is_err());
    assert!(read_ogg_opus(b"OggS").is_err());
    assert!(read_ogg_opus(&[0xff; 4096]).is_err());
    // A valid Ogg stream (correct CRCs) whose first packet is not OpusHead.
    let mut writer = ogg::PacketWriter::new(Vec::new());
    writer
        .write_packet(b"NotOpus!".to_vec(), 1, ogg::PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(
            b"audio".to_vec(),
            1,
            ogg::PacketWriteEndInfo::EndStream,
            960,
        )
        .unwrap();
    let err = read_ogg_opus(&writer.into_inner()).unwrap_err();
    assert!(matches!(err, ProbeError::NotOpus(_)), "got {err:?}");
    // Corrupting the sample's magic breaks the page CRC: rejected by the Ogg layer.
    let mut bytes = SAMPLE.to_vec();
    let at = bytes.windows(8).position(|w| w == b"OpusHead").unwrap();
    bytes[at..at + 8].copy_from_slice(b"NotOpus!");
    assert!(read_ogg_opus(&bytes).is_err());
    // Truncated mid-stream must not panic.
    let _ = read_ogg_opus(&SAMPLE[..SAMPLE.len() / 2]);
}

#[test]
fn close_to_libopus_when_ffmpeg_is_available() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample.opus");
    let reference = match libopus_reference(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping the libopus comparison: {e}");
            return;
        }
    };
    let s = read_ogg_opus(SAMPLE).unwrap();
    // ffmpeg trims the pre-skip and the tail itself. On the sample it matches
    // the granule arithmetic exactly (960000 samples = 10 s); allow one 20 ms
    // frame of difference in case another ffmpeg build trims the tail differently.
    let want = (s.final_granule - u64::from(s.pre_skip)) as usize * 2;
    assert!(
        reference.len().abs_diff(want) <= 960 * 2,
        "reference has {} samples, expected {want}",
        reference.len()
    );
    // The SNR is the spike's answer, not a pass/fail: only print it.
    for c in Candidate::ALL {
        let m = measure(c, &s, Some(&reference));
        eprintln!("{}: snr_db={:?} error={:?}", c.name(), m.snr_db, m.error);
    }
}
