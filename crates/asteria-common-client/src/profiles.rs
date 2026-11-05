//! The servers a user has registered, kept in `<config_dir>/servers.toml`.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use grain_id::GrainId;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::fsutil::{bad_path, write_atomic};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a valid http(s) server URL: {0}")]
    InvalidUrl(String),
    #[error("no such server profile: {0}")]
    UnknownProfile(ProfileId),
    #[error("servers.toml could not be written: {0}")]
    Io(#[from] io::Error),
    #[error("servers.toml could not be encoded: {0}")]
    Encode(#[from] toml::ser::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Chosen when the profile is created and never changed afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub GrainId);

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ProfileId {
    type Err = <GrainId as FromStr>::Err;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        s.parse().map(ProfileId)
    }
}

/// A server's access token. `Debug` hides it; `Serialize` writes the real value (that is how
/// it reaches `servers.toml`). There is deliberately no `Display`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    pub fn new(token: impl Into<String>) -> Self {
        Token(token.into())
    }

    /// The secret itself, for the `Authorization` header and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerProfile {
    pub id: ProfileId,
    pub name: String,
    pub url: Url,
    pub token: Token,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profiles {
    pub servers: Vec<ServerProfile>,
    active: Option<ProfileId>,
}

/// The on-disk shape.
#[derive(Serialize, Deserialize)]
struct File {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<ProfileId>,
    #[serde(default, rename = "server")]
    servers: Vec<ServerProfile>,
}

/// The rule every profile's URL must meet: http or https, with a host. Surrounding spaces are
/// ignored.
pub fn parse_url(s: &str) -> Result<Url> {
    match Url::parse(s.trim()) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => Ok(u),
        _ => Err(Error::InvalidUrl(s.to_string())),
    }
}

impl Profiles {
    /// A missing file is an empty list. A file that cannot be read as profiles is moved to
    /// `servers.toml.bad-<unix time>` and the list starts empty: tokens can be typed in again,
    /// which beats an app that will not start.
    pub fn load(path: &Path) -> Result<Profiles> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Profiles::default()),
            Err(e) => return Err(e.into()),
        };
        match decode(&bytes) {
            Some(file) => {
                let active = file
                    .active
                    .filter(|a| file.servers.iter().any(|s| s.id == *a));
                Ok(Profiles {
                    servers: file.servers,
                    active,
                })
            }
            None => {
                let aside = bad_path(path);
                // Never log the contents: they hold tokens.
                tracing::warn!(
                    path = %path.display(),
                    moved_to = %aside.display(),
                    "servers.toml is unreadable; set it aside and starting with no servers"
                );
                fs::rename(path, &aside)?;
                Ok(Profiles::default())
            }
        }
    }

    /// Temp file in the same directory, mode 0o600 from the start on Unix, then rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(&File {
            active: self.active,
            servers: self.servers.clone(),
        })?;
        write_atomic(path, text.as_bytes(), true)?;
        Ok(())
    }

    pub fn add(&mut self, name: impl Into<String>, url: &str, token: Token) -> Result<ProfileId> {
        let url = parse_url(url)?;
        let id = ProfileId(GrainId::random());
        self.servers.push(ServerProfile {
            id,
            name: name.into(),
            url,
            token,
        });
        Ok(id)
    }

    pub fn update(
        &mut self,
        id: ProfileId,
        name: impl Into<String>,
        url: &str,
        token: Token,
    ) -> Result<()> {
        let url = parse_url(url)?;
        let s = self
            .servers
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(Error::UnknownProfile(id))?;
        s.name = name.into();
        s.url = url;
        s.token = token;
        Ok(())
    }

    /// Returns whether the profile existed. Removing the active one leaves no active profile.
    pub fn remove(&mut self, id: ProfileId) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.id != id);
        if self.active == Some(id) {
            self.active = None;
        }
        self.servers.len() != before
    }

    /// `None` or the id of an existing profile.
    pub fn set_active(&mut self, id: Option<ProfileId>) -> Result<()> {
        if let Some(id) = id
            && !self.servers.iter().any(|s| s.id == id)
        {
            return Err(Error::UnknownProfile(id));
        }
        self.active = id;
        Ok(())
    }

    pub fn active(&self) -> Option<&ServerProfile> {
        let id = self.active?;
        self.servers.iter().find(|s| s.id == id)
    }
}

fn decode(bytes: &[u8]) -> Option<File> {
    let file: File = toml::from_str(std::str::from_utf8(bytes).ok()?).ok()?;
    file.servers
        .iter()
        .all(|s| matches!(s.url.scheme(), "http" | "https") && s.url.has_host())
        .then_some(file)
}
