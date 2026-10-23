//! The platform-neutral side of the photos screen: what the library's authorization means
//! for the person, which resources of an asset get uploaded under which names (spec §5.3,
//! a Live Photo's video goes under the photo's base name), and the streamed hash that
//! reads an original without holding it in memory.

/// The app's photo library authorization, as `PHAuthorizationStatus` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// The person has not been asked yet.
    NotDetermined,
    /// Access is blocked by a policy the person cannot change (parental controls, MDM).
    Restricted,
    /// The person refused access.
    Denied,
    /// Full access.
    Authorized,
    /// Access to the photos the person selected only.
    Limited,
}

impl Auth {
    /// What the status means, in Japanese, for the photos screen.
    pub fn describe(self) -> &'static str {
        match self {
            Auth::NotDetermined => "未確認（「許可を求める」を押してください）",
            Auth::Restricted => "制限されています（設定アプリで許可してください）",
            Auth::Denied => "拒否されています（設定アプリで許可してください）",
            Auth::Authorized => "すべての写真を許可",
            Auth::Limited => "選択した写真のみ許可",
        }
    }

    /// A short name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Auth::NotDetermined => "not_determined",
            Auth::Restricted => "restricted",
            Auth::Denied => "denied",
            Auth::Authorized => "authorized",
            Auth::Limited => "limited",
        }
    }
}

/// What a resource of an asset holds, from `PHAssetResourceType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    /// The original photo.
    Photo,
    /// The original video.
    Video,
    /// The original video of a Live Photo.
    PairedVideo,
    /// The current (possibly edited) photo.
    FullSizePhoto,
    /// An alternate original, such as the JPEG of a RAW+JPEG pair.
    AlternatePhoto,
    /// Any other resource type, by its raw value.
    Other(i64),
}

impl ResourceKind {
    /// The kind for a raw `PHAssetResourceType` value.
    pub fn from_raw(raw: i64) -> Self {
        match raw {
            1 => ResourceKind::Photo,
            2 => ResourceKind::Video,
            4 => ResourceKind::AlternatePhoto,
            5 => ResourceKind::FullSizePhoto,
            9 => ResourceKind::PairedVideo,
            other => ResourceKind::Other(other),
        }
    }
}

/// One resource of an asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceInfo {
    pub kind: ResourceKind,
    pub original_filename: String,
    /// The uniform type identifier, such as `public.heic`.
    pub uti: String,
}

/// An asset of the photo library, with its resources in the order PhotoKit lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetInfo {
    pub local_id: String,
    /// The creation date, as text, if the library has one.
    pub created: Option<String>,
    pub is_live: bool,
    pub resources: Vec<ResourceInfo>,
}

/// The resources to upload for an asset, each with the name it is sent under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadPlan {
    pub primary: (ResourceInfo, String),
    pub paired_video: Option<(ResourceInfo, String)>,
}

/// Which resources of `asset` to send: the original photo or video (never the edited
/// rendition) and, for a Live Photo, its paired video named after the photo's base name
/// with the video's extension. `None` if the asset has no original photo or video.
pub fn plan_upload(asset: &AssetInfo) -> Option<UploadPlan> {
    let primary = asset
        .resources
        .iter()
        .find(|r| r.kind == ResourceKind::Photo)
        .or_else(|| {
            asset
                .resources
                .iter()
                .find(|r| r.kind == ResourceKind::Video)
        })?;
    let paired_video = if primary.kind == ResourceKind::Photo {
        asset
            .resources
            .iter()
            .find(|r| r.kind == ResourceKind::PairedVideo)
            .map(|video| {
                let name = match extension(&video.original_filename) {
                    Some(ext) => format!("{}.{ext}", base_name(&primary.original_filename)),
                    None => base_name(&primary.original_filename).to_owned(),
                };
                (video.clone(), name)
            })
    } else {
        None
    };
    Some(UploadPlan {
        primary: (primary.clone(), primary.original_filename.clone()),
        paired_video,
    })
}

/// `name` without its extension (the part from the last dot, unless the dot starts the
/// name).
fn base_name(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// The extension of `name`, without the dot.
fn extension(name: &str) -> Option<&str> {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => Some(&name[i + 1..]),
        _ => None,
    }
}

/// A blake3 hash fed one chunk at a time, counting the bytes it saw.
#[derive(Default)]
pub struct StreamHash {
    hasher: blake3::Hasher,
    bytes: u64,
}

impl StreamHash {
    /// A hash of nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the next `chunk` of the data.
    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
    }

    /// The number of bytes seen so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The byte count and the hash as `blake3:<hex>`.
    pub fn finish(self) -> (u64, String) {
        (self.bytes, format!("blake3:{}", self.hasher.finalize()))
    }
}

/// What reading one original reports, from whichever thread PhotoKit calls back on.
#[derive(Clone, Debug, PartialEq)]
pub enum FetchEvent {
    /// The bytes read so far.
    Progress(u64),
    /// The read ended.
    Done(FetchResult),
}

/// How reading one original ended.
#[derive(Clone, Debug, PartialEq)]
pub struct FetchResult {
    pub bytes: u64,
    /// `blake3:<hex>` of the bytes read.
    pub hash: String,
    pub secs: f64,
    /// Why the read failed, if it did; `bytes` and `hash` then cover what arrived.
    pub error: Option<String>,
}

impl FetchResult {
    /// One line for the report.
    pub fn summary(&self) -> String {
        let mut line = format!(
            "{} bytes, {:.2} s, {:.1} MB/s, {}",
            self.bytes,
            self.secs,
            mb_per_s(self.bytes, self.secs),
            self.hash,
        );
        if let Some(error) = &self.error {
            line.push_str(&format!(", error: {error}"));
        }
        line
    }
}

/// Throughput in megabytes (10^6 bytes) per second; 0 when no time passed.
pub fn mb_per_s(bytes: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        bytes as f64 / 1_000_000.0 / secs
    } else {
        0.0
    }
}

/// One line for the report about `asset`: its date, whether it is live, its resources
/// and the names [`plan_upload`] sends them under.
pub fn sample_line(asset: &AssetInfo) -> String {
    let resources: Vec<String> = asset
        .resources
        .iter()
        .map(|r| format!("{:?} {} ({})", r.kind, r.original_filename, r.uti))
        .collect();
    let plan = match plan_upload(asset) {
        Some(UploadPlan {
            primary,
            paired_video: Some(video),
        }) => format!("{} + {}", primary.1, video.1),
        Some(UploadPlan { primary, .. }) => primary.1,
        None => "nothing to send".to_owned(),
    };
    format!(
        "{}{}: {} -> {}",
        asset.created.as_deref().unwrap_or("no date"),
        if asset.is_live { " live" } else { "" },
        resources.join(", "),
        plan,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(kind: ResourceKind, name: &str) -> ResourceInfo {
        ResourceInfo {
            kind,
            original_filename: name.to_owned(),
            uti: String::new(),
        }
    }

    fn asset(resources: Vec<ResourceInfo>) -> AssetInfo {
        AssetInfo {
            local_id: "id".to_owned(),
            created: None,
            is_live: false,
            resources,
        }
    }

    fn names(plan: &UploadPlan) -> (&str, Option<&str>) {
        (
            plan.primary.1.as_str(),
            plan.paired_video.as_ref().map(|(_, name)| name.as_str()),
        )
    }

    #[test]
    fn live_photo_pairs_video_under_the_photo_base_name() {
        let live = asset(vec![
            resource(ResourceKind::Photo, "IMG_0001.HEIC"),
            resource(ResourceKind::PairedVideo, "IMG_0001.MOV"),
        ]);
        let plan = plan_upload(&live).unwrap();
        assert_eq!(names(&plan), ("IMG_0001.HEIC", Some("IMG_0001.MOV")));
        assert_eq!(plan.paired_video.unwrap().0.kind, ResourceKind::PairedVideo);

        let renamed = asset(vec![
            resource(ResourceKind::Photo, "IMG_0001.HEIC"),
            resource(ResourceKind::PairedVideo, "FullSizeRender.mov"),
        ]);
        let plan = plan_upload(&renamed).unwrap();
        assert_eq!(names(&plan), ("IMG_0001.HEIC", Some("IMG_0001.mov")));
    }

    #[test]
    fn edited_photo_still_sends_the_original() {
        let edited = asset(vec![
            resource(ResourceKind::FullSizePhoto, "FullSizeRender.jpg"),
            resource(ResourceKind::Photo, "IMG_0002.JPG"),
        ]);
        let plan = plan_upload(&edited).unwrap();
        assert_eq!(plan.primary.0.kind, ResourceKind::Photo);
        assert_eq!(names(&plan), ("IMG_0002.JPG", None));
    }

    #[test]
    fn video_asset_has_no_pair_and_unknown_asset_is_none() {
        let video = asset(vec![resource(ResourceKind::Video, "IMG_0003.MOV")]);
        let plan = plan_upload(&video).unwrap();
        assert_eq!(plan.primary.0.kind, ResourceKind::Video);
        assert_eq!(names(&plan), ("IMG_0003.MOV", None));

        let unknown = asset(vec![
            resource(ResourceKind::Other(7), "Adjustments.plist"),
            resource(ResourceKind::FullSizePhoto, "FullSizeRender.jpg"),
        ]);
        assert_eq!(plan_upload(&unknown), None);
        assert_eq!(plan_upload(&asset(Vec::new())), None);
    }

    #[test]
    fn resource_kinds_follow_the_photokit_raw_values() {
        assert_eq!(ResourceKind::from_raw(1), ResourceKind::Photo);
        assert_eq!(ResourceKind::from_raw(2), ResourceKind::Video);
        assert_eq!(ResourceKind::from_raw(4), ResourceKind::AlternatePhoto);
        assert_eq!(ResourceKind::from_raw(5), ResourceKind::FullSizePhoto);
        assert_eq!(ResourceKind::from_raw(9), ResourceKind::PairedVideo);
        assert_eq!(ResourceKind::from_raw(19), ResourceKind::Other(19));
    }

    #[test]
    fn stream_hash_equals_one_shot_hash() {
        let data: Vec<u8> = (0..1024 * 1024u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let expected = format!("blake3:{}", blake3::hash(&data));
        let ragged = [1, 7, 4096, 3, 65_537, 100, 12_345];

        let mut by_byte = StreamHash::new();
        data.chunks(1).for_each(|c| by_byte.update(c));
        let mut by_page = StreamHash::new();
        data.chunks(4096).for_each(|c| by_page.update(c));
        let mut by_ragged = StreamHash::new();
        let mut rest = data.as_slice();
        for size in ragged.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (chunk, tail) = rest.split_at((*size).min(rest.len()));
            by_ragged.update(chunk);
            rest = tail;
        }

        for hash in [by_byte, by_page, by_ragged] {
            assert_eq!(hash.finish(), (data.len() as u64, expected.clone()));
        }
    }

    #[test]
    fn sample_line_names_the_planned_uploads() {
        let mut live = asset(vec![
            resource(ResourceKind::Photo, "IMG_0001.HEIC"),
            resource(ResourceKind::PairedVideo, "FullSizeRender.mov"),
        ]);
        live.is_live = true;
        let line = sample_line(&live);
        assert!(line.starts_with("no date live: "), "{line}");
        assert!(line.ends_with("-> IMG_0001.HEIC + IMG_0001.mov"), "{line}");
    }

    #[test]
    fn throughput_is_zero_without_elapsed_time() {
        assert_eq!(mb_per_s(5_000_000, 2.0), 2.5);
        assert_eq!(mb_per_s(5_000_000, 0.0), 0.0);
    }

    #[test]
    fn denied_and_restricted_explain_how_to_allow() {
        for auth in [Auth::Denied, Auth::Restricted] {
            assert!(auth.describe().contains("設定アプリで許可してください"));
        }
        for auth in [Auth::NotDetermined, Auth::Authorized, Auth::Limited] {
            assert!(!auth.describe().contains("設定アプリ"));
        }
    }
}
