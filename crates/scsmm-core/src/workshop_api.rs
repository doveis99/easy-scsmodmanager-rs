//! Steam Workshop public metadata: titles and preview images.
//!
//! Many workshop mods carry no `display_name` (the real name lives in the
//! Steam metadata) and no icon. Steam exposes both, unauthenticated, at
//!
//! ```text
//! POST https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/
//!   itemcount=N  publishedfileids[0]=<id>  publishedfileids[1]=<id> ...
//! ```
//!
//! The only thing sent is the list of workshop ids. Responses are cached
//! on disk ([`WorkshopCache`]) so each id is asked for once and then
//! refreshed rarely.
//!
//! The HTTP half is behind the `net` cargo feature; parsing and the cache
//! work without it.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::manifest::sniff_image_mime;
use crate::{format_err, Result};

pub const DETAILS_ENDPOINT: &str = "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/";
/// Steam tolerates more, but smaller batches recover faster on failure.
pub const BATCH_LIMIT: usize = 50;
/// Cached metadata older than this is fetched again (titles get renamed).
pub const MAX_AGE_SECS: u64 = 14 * 24 * 60 * 60;
/// Workshop previews are well under this; anything larger is not an icon.
pub const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkshopMeta {
    pub title: Option<String>,
    pub preview_url: Option<String>,
    /// Unix seconds of the item's last update on Steam; 0 when unknown.
    #[serde(default)]
    pub time_updated: u64,
    /// Unix seconds when this entry was fetched.
    #[serde(default)]
    pub fetched_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Parse a `GetPublishedFileDetails` response body.
///
/// Ids Steam does not know or that are private (`result != 1`) are simply
/// absent. Only `https://` preview URLs are kept.
pub fn parse_details(body: &str) -> Result<HashMap<String, WorkshopMeta>> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format_err(format!("workshop response: {e}")))?;
    let details = payload["response"]["publishedfiledetails"].as_array().cloned().unwrap_or_default();
    let text = |v: &serde_json::Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let fetched_at = now_secs();
    let mut out = HashMap::new();
    for entry in details {
        // ids arrive as strings, but tolerate a bare number
        let id = match &entry["publishedfileid"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if !is_workshop_id(&id) || entry["result"].as_i64() != Some(1) {
            continue;
        }
        out.insert(
            id,
            WorkshopMeta {
                title: text(&entry["title"]),
                preview_url: text(&entry["preview_url"]).filter(|u| u.starts_with("https://")),
                time_updated: entry["time_updated"].as_u64().unwrap_or(0),
                fetched_at,
            },
        );
    }
    Ok(out)
}

pub fn is_workshop_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
}

/// On-disk cache: `meta.json` plus one preview image file per id.
pub struct WorkshopCache {
    dir: PathBuf,
    entries: HashMap<String, WorkshopMeta>,
}

impl WorkshopCache {
    /// Open (or start) the cache in `dir`. A missing or corrupt index just
    /// means an empty cache - everything in it can be fetched again.
    pub fn open(dir: &Path) -> Self {
        let entries =
            fs::read(dir.join("meta.json")).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default();
        Self { dir: dir.to_path_buf(), entries }
    }

    pub fn get(&self, id: &str) -> Option<&WorkshopMeta> {
        self.entries.get(id)
    }

    pub fn title(&self, id: &str) -> Option<&str> {
        self.entries.get(id)?.title.as_deref()
    }

    /// Ids with no entry yet, or one older than [`MAX_AGE_SECS`].
    pub fn missing_or_stale<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let now = now_secs();
        let mut out: Vec<String> = Vec::new();
        for id in ids {
            let fresh = self.entries.get(id).is_some_and(|e| now.saturating_sub(e.fetched_at) < MAX_AGE_SECS);
            if is_workshop_id(id) && !fresh && !out.iter().any(|o| o == id) {
                out.push(id.to_string());
            }
        }
        out
    }

    /// Store fetched entries. Ids that were asked for but not returned are
    /// remembered as empty entries, so a private or deleted item is not
    /// asked for again on every start.
    pub fn store(&mut self, asked: &[String], found: HashMap<String, WorkshopMeta>) -> Result<()> {
        let fetched_at = now_secs();
        for id in asked {
            self.entries.entry(id.clone()).or_default().fetched_at = fetched_at;
        }
        for (id, meta) in found {
            // a changed preview URL invalidates the stored image
            let url_changed = self
                .entries
                .get(&id)
                .is_some_and(|old| old.preview_url.is_some() && old.preview_url != meta.preview_url);
            if url_changed {
                let _ = fs::remove_file(self.preview_path(&id));
            }
            self.entries.insert(id, meta);
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec(&self.entries).map_err(|e| format_err(e.to_string()))?;
        let tmp = self.dir.join("meta.json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, self.dir.join("meta.json"))?;
        Ok(())
    }

    fn preview_path(&self, id: &str) -> PathBuf {
        self.dir.join("previews").join(format!("{id}.img"))
    }

    pub fn has_preview(&self, id: &str) -> bool {
        is_workshop_id(id) && self.preview_path(id).is_file()
    }

    pub fn read_preview(&self, id: &str) -> Option<Vec<u8>> {
        if !is_workshop_id(id) {
            return None;
        }
        fs::read(self.preview_path(id)).ok()
    }

    /// Keep downloaded preview bytes - only if they really are an image.
    pub fn write_preview(&self, id: &str, bytes: &[u8]) -> Result<()> {
        if !is_workshop_id(id) {
            return Err(format_err("not a workshop id"));
        }
        if sniff_image_mime(bytes).is_none() {
            return Err(format_err("preview is not a recognised image"));
        }
        let path = self.preview_path(id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(feature = "net")]
mod net {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    /// A client with a generic user agent and a 20 s timeout.
    pub fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent("easy-scsmodmanager-rs").build()
    }

    #[derive(Debug, Default)]
    pub struct FetchOutcome {
        pub found: HashMap<String, WorkshopMeta>,
        /// Ids whose batch Steam actually answered. Only these may be
        /// remembered as "asked": an id from a failed batch must be tried
        /// again, not cached as unknown.
        pub answered: Vec<String>,
        /// Error of the last failed batch, if any.
        pub error: Option<String>,
    }

    /// Fetch metadata for the given workshop ids, in batches. A failed
    /// batch is skipped and reported in `error`.
    pub fn fetch_metadata(agent: &ureq::Agent, ids: &[String]) -> FetchOutcome {
        let mut out = FetchOutcome::default();
        for batch in ids.chunks(BATCH_LIMIT) {
            match fetch_batch(agent, batch) {
                Ok(found) => {
                    out.found.extend(found);
                    out.answered.extend(batch.iter().cloned());
                }
                Err(e) => out.error = Some(e.to_string()),
            }
        }
        out
    }

    fn fetch_batch(agent: &ureq::Agent, ids: &[String]) -> Result<HashMap<String, WorkshopMeta>> {
        let count = ids.len().to_string();
        let keys: Vec<String> = (0..ids.len()).map(|i| format!("publishedfileids[{i}]")).collect();
        let mut form: Vec<(&str, &str)> = vec![("itemcount", count.as_str())];
        form.extend(keys.iter().zip(ids).map(|(k, v)| (k.as_str(), v.as_str())));
        let body = agent
            .post(DETAILS_ENDPOINT)
            .send_form(&form)
            .map_err(|e| format_err(format!("workshop request failed: {e}")))?
            .into_string()?;
        parse_details(&body)
    }

    /// Download one preview image. Only `https://` URLs are followed and
    /// the body is capped at [`MAX_PREVIEW_BYTES`].
    pub fn fetch_preview(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
        if !url.starts_with("https://") {
            return Err(format_err("refusing a non-https preview url"));
        }
        let response = agent.get(url).call().map_err(|e| format_err(format!("preview request failed: {e}")))?;
        let mut bytes = Vec::new();
        response.into_reader().take(MAX_PREVIEW_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PREVIEW_BYTES {
            return Err(format_err("preview image is too large"));
        }
        Ok(bytes)
    }
}

#[cfg(feature = "net")]
pub use net::{agent, fetch_metadata, fetch_preview, FetchOutcome};

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"response":{"result":1,"resultcount":3,"publishedfiledetails":[
        {"publishedfileid":"978025490","result":1,"title":"  Nice Mod  ","description":"long",
         "preview_url":"https://images.steamusercontent.com/ugc/1/ABC/","time_updated":1700000000,"file_size":"12345"},
        {"publishedfileid":"111","result":9},
        {"publishedfileid":"222","result":1,"title":"","preview_url":"http://insecure.example/x.jpg"},
        {"publishedfileid":"../etc","result":1,"title":"evil"}
    ]}}"#;
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 1];

    #[test]
    fn parses_details() {
        let found = parse_details(BODY).unwrap();
        assert_eq!(found.len(), 2);
        let m = &found["978025490"];
        assert_eq!(m.title.as_deref(), Some("Nice Mod"));
        assert_eq!(m.preview_url.as_deref(), Some("https://images.steamusercontent.com/ugc/1/ABC/"));
        assert_eq!(m.time_updated, 1_700_000_000);
        // empty title and a non-https preview are dropped, the entry stays
        assert_eq!(found["222"].title, None);
        assert_eq!(found["222"].preview_url, None);
        assert!(parse_details("not json").is_err());
        assert!(parse_details("{}").unwrap().is_empty());
    }

    #[test]
    fn cache_round_trip_and_staleness() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("workshop");
        let mut cache = WorkshopCache::open(&dir);
        let ids = ["978025490", "111", "978025490", "not-an-id"];
        let asked = cache.missing_or_stale(ids);
        assert_eq!(asked, ["978025490", "111"]);

        cache.store(&asked, parse_details(BODY).unwrap()).unwrap();
        assert_eq!(cache.title("978025490"), Some("Nice Mod"));
        // "111" was asked for but not returned: remembered, not asked again
        assert_eq!(cache.title("111"), None);
        assert!(cache.missing_or_stale(ids).is_empty());

        // survives a reopen; an old entry is due again
        let mut reopened = WorkshopCache::open(&dir);
        assert_eq!(reopened.title("978025490"), Some("Nice Mod"));
        reopened.entries.get_mut("111").unwrap().fetched_at = 1;
        assert_eq!(reopened.missing_or_stale(ids), ["111"]);
    }

    #[test]
    fn previews_must_be_images_with_safe_names() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = WorkshopCache::open(tmp.path());
        assert!(!cache.has_preview("978025490"));
        cache.write_preview("978025490", JPEG).unwrap();
        assert!(cache.has_preview("978025490"));
        assert_eq!(cache.read_preview("978025490").as_deref(), Some(JPEG));
        assert!(cache.write_preview("978025490", b"<html>").is_err());
        assert!(cache.write_preview("../x", JPEG).is_err());
        assert_eq!(cache.read_preview("../x"), None);
    }

    #[test]
    fn a_changed_preview_url_drops_the_stored_image() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = WorkshopCache::open(tmp.path());
        let id = "978025490".to_string();
        cache.store(std::slice::from_ref(&id), parse_details(BODY).unwrap()).unwrap();
        cache.write_preview(&id, JPEG).unwrap();
        let mut updated = parse_details(BODY).unwrap();
        updated.get_mut(&id).unwrap().preview_url = Some("https://images.example/new".into());
        cache.store(std::slice::from_ref(&id), updated).unwrap();
        assert!(!cache.has_preview(&id));
    }
}
