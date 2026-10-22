//! Content hashing: hash types, format detection and the dispatcher.

mod detect;
mod file;
mod jpeg;

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use detect::{detect_format, detect_format_file};
pub(crate) use file::file_hash;

use crate::{Error, Result};

const PREFIX: &str = "blake3:";

/// A BLAKE3 digest, written as `blake3:<64 lowercase hex digits>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ContentHash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::InvalidHash(s.to_string());
        let hex = s.strip_prefix(PREFIX).ok_or_else(bad)?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
        }
        Ok(Self(out))
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What a [`ContentHash`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashScope {
    /// Encoded media data only, excluding metadata.
    MediaPayload1,
    /// The whole file.
    File1,
}

impl fmt::Display for HashScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MediaPayload1 => "media-payload/1",
            Self::File1 => "file/1",
        })
    }
}

impl FromStr for HashScope {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "media-payload/1" => Ok(Self::MediaPayload1),
            "file/1" => Ok(Self::File1),
            _ => Err(Error::InvalidHashScope(s.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFormat {
    Jpeg,
    Png,
    WebP,
    Flac,
    Mp3,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashOutcome {
    pub hash: ContentHash,
    pub scope: HashScope,
    /// Set when a recognised format had to fall back to `file/1`.
    pub fallback_reason: Option<String>,
}

/// Hashes the media payload of a recognised format; `Ok(None)` means the
/// format has no payload hash and the caller should use `file/1`.
/// Each format's task adds its arm here.
fn payload_hash(path: &Path, format: MediaFormat) -> Result<Option<ContentHash>> {
    match format {
        MediaFormat::Jpeg => {
            jpeg::jpeg_payload_hash(&mut BufReader::new(File::open(path)?)).map(Some)
        }
        MediaFormat::Png
        | MediaFormat::WebP
        | MediaFormat::Flac
        | MediaFormat::Mp3
        | MediaFormat::Unknown => Ok(None),
    }
}

/// Computes the content hash of `path`, preferring the media payload hash and
/// falling back to `file/1` for unknown or malformed media.
pub fn content_hash(path: &Path) -> Result<HashOutcome> {
    let format = detect_format_file(path)?;
    let fallback_reason = match payload_hash(path, format) {
        Ok(Some(hash)) => {
            return Ok(HashOutcome {
                hash,
                scope: HashScope::MediaPayload1,
                fallback_reason: None,
            });
        }
        Ok(None) => None,
        Err(e @ Error::MalformedMedia { .. }) => Some(e.to_string()),
        Err(e) => return Err(e),
    };
    Ok(HashOutcome {
        hash: file_hash(path)?,
        scope: HashScope::File1,
        fallback_reason,
    })
}
