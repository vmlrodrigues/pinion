#![allow(clippy::unicode_not_nfc)]
#[allow(clippy::wildcard_imports)]
use super::*;
use env_logger;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::io::{BufReader, BufWriter};

use crate::rmps;
use serde::Serialize;
use tempfile::NamedTempFile;

use self::tag::{Tag, TagFreq};
use super::api::ApiError;
use super::pin::Pin;
use super::CacheRefresh;

const TAGS_CACHE_FN: &str = "tags.cache";
const PINS_CACHE_FN: &str = "pins.cache";
const SNAPSHOT_CACHE_FN: &str = "bookmarks.cache";

const FILE_BUF_SIZE: usize = 4 * 1024 * 1024;

fn tag_counts<'a, 'pin: 'a>(
    pins: impl IntoIterator<Item = &'a Pin<'pin>>,
) -> HashMap<String, usize> {
    let mut urls_by_tag: HashMap<String, HashSet<&str>> = HashMap::new();
    for pin in pins {
        let tags = pin.tags.nfkd().collect::<String>().to_lowercase();
        for tag in tags.split_whitespace() {
            urls_by_tag
                .entry(tag.to_owned())
                .or_default()
                .insert(pin.url.as_ref());
        }
    }
    urls_by_tag
        .into_iter()
        .map(|(tag, urls)| (tag, urls.len()))
        .collect()
}

fn count_tags_for_bookmark(tags: &str, counts: &mut HashMap<String, usize>) {
    for name in normalized_tag_names(tags) {
        *counts.entry(name).or_default() += 1;
    }
}

fn normalized_tag_names(tags: &str) -> Vec<String> {
    let normalized = tags.nfkd().collect::<String>().to_lowercase();
    let mut names: Vec<_> = normalized.split_whitespace().map(str::to_owned).collect();
    names.sort();
    names.dedup();
    names
}

#[derive(Debug)]
pub struct CachedData<'pin> {
    pub pins: Option<Vec<CachedPin<'pin>>>,
    pub tags: Option<Vec<CachedTag>>,
    pub cache_dir: PathBuf,
    pub tags_cache_file: PathBuf,
    pub pins_cache_file: PathBuf,
    skipped_url_tags: Option<HashMap<String, String>>,
    cache_files_valid: bool,
}

// TODO: Add a url_lowered field to CachedPin so we don't have to call
//       .to_lowercase() in pinboard.find_url() every time //
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct CachedPin<'pin> {
    pub pin: Pin<'pin>,
    pub tag_list: Vec<String>,
    pub title_lowered: String,
    pub extended_lowered: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct CachedTag {
    pub tag: Tag,
    pub tag_lowered: String,
}

// The first two fields retain the sequence format used by earlier snapshots.
#[derive(Deserialize)]
struct Snapshot<'pin> {
    pins: Vec<CachedPin<'pin>>,
    tags: Vec<CachedTag>,
    #[serde(default)]
    skipped_url_tags: Option<HashMap<String, String>>,
}

impl<'pin> CachedData<'pin> {
    #[allow(clippy::map_unwrap_or)]
    pub fn new<P: AsRef<Path>>(c_dir: Option<P>) -> Result<Self, Box<dyn std::error::Error>> {
        let _ = env_logger::try_init();
        debug!("new: starting");
        let cached_dir = c_dir.map(|p| p.as_ref().to_path_buf()).unwrap_or_else(|| {
            let mut dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from(""));
            dir.push(".cache");
            dir.push("rusty-pin");
            dir
        });
        debug!("  cached_dir: {:?}", cached_dir);
        let mut data = CachedData::create_cache_dir(cached_dir).map(|c_path| CachedData {
            pins: None,
            tags: None,
            tags_cache_file: c_path.join(TAGS_CACHE_FN),
            pins_cache_file: c_path.join(PINS_CACHE_FN),
            skipped_url_tags: None,
            cache_dir: c_path,
            cache_files_valid: false,
        })?;

        if data.load_cache_data_from_file().is_err() {
            data.cache_files_valid = false;
        }
        Ok(data)
    }

    /// Create an instance for `CachedData` but don't load actual cached files.
    #[allow(clippy::map_unwrap_or)]
    #[allow(dead_code)]
    pub fn init<P: AsRef<Path>>(c_dir: Option<P>) -> Result<Self, Box<dyn std::error::Error>> {
        let _ = env_logger::try_init();
        debug!("init: starting");
        let cached_dir = c_dir.map(|p| p.as_ref().to_path_buf()).unwrap_or_else(|| {
            let mut dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from(""));
            dir.push(".cache");
            dir.push("rusty-pin");
            dir
        });
        debug!("  cached_dir: {:?}", cached_dir);
        let data = CachedData::create_cache_dir(cached_dir).map(|c_path| CachedData {
            pins: None,
            tags: None,
            tags_cache_file: c_path.join(TAGS_CACHE_FN),
            pins_cache_file: c_path.join(PINS_CACHE_FN),
            skipped_url_tags: None,
            cache_dir: c_path,
            cache_files_valid: false,
        })?;
        Ok(data)
    }

    fn create_cache_dir<P: AsRef<Path>>(
        cache_dir: P,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let _ = env_logger::try_init();
        debug!("create_cache_dir: starting");
        fs::create_dir_all(&cache_dir)?;
        debug!(
            "  success create_cache_dir: {:?}",
            cache_dir.as_ref().to_path_buf()
        );
        Ok(cache_dir.as_ref().to_path_buf())
    }
}

impl<'pin> CachedData<'pin> {
    pub fn set_cache_dir<P: AsRef<Path>>(
        &mut self,
        p: &P,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("set_cache_dir: starting");
        self.cache_dir = CachedData::create_cache_dir(p)?;
        self.tags_cache_file = self.cache_dir.join(TAGS_CACHE_FN);
        self.pins_cache_file = self.cache_dir.join(PINS_CACHE_FN);
        self.pins = None;
        self.tags = None;
        self.skipped_url_tags = None;
        self.cache_files_valid = false;
        Ok(())
    }

    pub fn load_cache_data_from_file(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("load_cache_data_from_file: starting");
        let snapshot = match File::open(self.cache_dir.join(SNAPSHOT_CACHE_FN)) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(file) = snapshot {
            let reader = BufReader::with_capacity(FILE_BUF_SIZE, file);
            let snapshot: Snapshot = rmps::from_read(reader)?;
            self.pins = Some(snapshot.pins);
            self.tags = Some(snapshot.tags);
            self.skipped_url_tags = snapshot.skipped_url_tags;
            self.cache_files_valid = true;
            return Ok(());
        }

        // Import caches written by older versions.
        let pins = BufReader::with_capacity(FILE_BUF_SIZE, File::open(&self.pins_cache_file)?);
        let tags = BufReader::with_capacity(FILE_BUF_SIZE, File::open(&self.tags_cache_file)?);
        let pins = rmps::from_read(pins)?;
        let tags = rmps::from_read(tags)?;
        self.pins = Some(pins);
        self.tags = Some(tags);
        self.skipped_url_tags = None;
        self.cache_files_valid = true;
        Ok(())
    }

    pub fn cache_ok(&self) -> bool {
        debug!("cache_ok: starting");
        self.cache_files_valid
    }

    pub fn update_cache(
        &mut self,
        api: &api::Api,
    ) -> Result<CacheRefresh, Box<dyn std::error::Error>> {
        debug!("update_cache: starting");
        let cached_pins = self.pins.iter().flatten().map(|pin| &pin.pin);
        let mut export = api.all_pins(cached_pins)?;
        let mut tags = api.tags_frequency()?;
        self.check_exports(&mut export, &tags, api)?;
        let api::BookmarkExport {
            mut pins,
            retained,
            refresh,
            skipped_url_tags,
            ..
        } = export;
        pins.extend(retained);

        // Keep bookmarks newest first and tags ordered by frequency.
        pins.sort_by_key(|pin| std::cmp::Reverse(pin.time()));
        tags.sort_by(|first, second| second.cmp(first));

        let pins: Vec<CachedPin<'pin>> = pins
            .into_iter()
            .map(|pin| {
                let tags_lowered = pin.tags.nfkd().collect::<String>().to_lowercase();
                let title_lowered = pin.title.nfkd().collect::<String>().to_lowercase();
                let extended_lowered = pin
                    .extended
                    .as_ref()
                    .map(|note| note.nfkd().collect::<String>().to_lowercase());
                CachedPin {
                    tag_list: tags_lowered.split_whitespace().map(str::to_owned).collect(),
                    title_lowered,
                    extended_lowered,
                    pin,
                }
            })
            .collect();
        let tags: Vec<CachedTag> = tags
            .into_iter()
            .map(|tag| CachedTag {
                tag_lowered: tag.0.nfkd().collect::<String>().to_lowercase(),
                tag,
            })
            .collect();

        // Publish both lists together so concurrent readers see one complete snapshot.
        let mut file = NamedTempFile::new_in(&self.cache_dir)?;
        {
            let mut writer = BufWriter::with_capacity(FILE_BUF_SIZE, file.as_file_mut());
            (&pins, &tags, &skipped_url_tags).serialize(&mut Serializer::new(&mut writer))?;
            writer.flush()?;
        }
        file.persist(self.cache_dir.join(SNAPSHOT_CACHE_FN))?;
        self.pins = Some(pins);
        self.tags = Some(tags);
        self.skipped_url_tags = Some(skipped_url_tags);
        self.cache_files_valid = true;
        Ok(refresh)
    }

    fn check_exports(
        &self,
        export: &mut api::BookmarkExport<'_>,
        tags: &[Tag],
        api: &api::Api,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let api::BookmarkExport {
            pins,
            retained,
            refresh,
            unreadable_bookmark_tags,
            skipped_url_tags,
        } = export;
        let disagreement = || {
            ApiError::UnrecognizedResponse(
                "bookmark and tag exports disagree; retry the refresh".into(),
            )
        };
        let used_tags: Vec<_> = tags
            .iter()
            .filter_map(|tag| match tag.1 {
                TagFreq::Used(count) if count > 0 => {
                    Some((tag.0.nfkd().collect::<String>().to_lowercase(), count))
                }
                _ => None,
            })
            .collect();
        let mut received = tag_counts(pins.iter());
        // Each distinct unreadable bookmark can contribute at most once per
        // tag. Where its tags were unreadable too, allow for any tag.
        let mut unknown_tags = 0;
        for tags in unreadable_bookmark_tags.values() {
            if let Some(tags) = tags {
                count_tags_for_bookmark(tags, &mut received);
            } else {
                unknown_tags += 1;
            }
        }

        if used_tags.is_empty() && !received.is_empty() {
            return Err(disagreement().into());
        }
        let used_names: HashSet<_> = used_tags.iter().map(|(name, _)| name.as_str()).collect();
        let cached_tag_names: HashSet<_> = self
            .tags
            .iter()
            .flatten()
            .filter_map(|tag| {
                matches!(tag.tag.1, TagFreq::Used(1..)).then_some(tag.tag_lowered.as_str())
            })
            .collect();
        // A retained bookmark has no current tag evidence when its raw tags
        // are unreadable, or when an unidentified row caused its retention.
        // Keep the old snapshot if publishing the new index would hide one of
        // its known tags. Readable raw tags can still confirm a real removal.
        for pin in retained.iter().filter(|pin| {
            unreadable_bookmark_tags
                .get(pin.url.as_ref())
                .is_none_or(Option::is_none)
        }) {
            if normalized_tag_names(&pin.tags).iter().any(|name| {
                cached_tag_names.contains(name.as_str()) && !used_names.contains(name.as_str())
            }) {
                return Err(disagreement().into());
            }
        }
        // Include tags on identified malformed rows, but don't use the old
        // tags of retained bookmarks as evidence that a tag still exists.
        for name in received
            .keys()
            .filter(|name| !used_names.contains(name.as_str()))
        {
            if cached_tag_names.contains(name.as_str()) {
                return Err(disagreement().into());
            }
            *refresh = CacheRefresh::Partial;
        }

        // Pinboard serves these lists independently, and tag names and counts
        // may disagree (for example, after a rename). Don't evict cached URLs
        // when missing rows could explain a discrepancy.
        let preserved_urls: HashSet<_> = pins
            .iter()
            .chain(retained.iter())
            .map(|pin| pin.url.as_ref())
            .collect();
        let mut omitted = HashSet::new();
        for pin in self.pins.iter().flatten() {
            let names = pin.tag_list.iter().map(String::as_str);
            if !preserved_urls.contains(pin.pin.url.as_ref()) {
                omitted.extend(names.clone());
            }
        }

        // A previously observed unusable URL can explain a count left behind
        // by a real deletion. Newly skipped URLs provide no
        // such evidence for dropping an old bookmark.
        let mut skipped_counts = HashMap::new();
        let mut stable_skipped_counts = HashMap::new();
        for (url, tags) in skipped_url_tags {
            count_tags_for_bookmark(tags, &mut skipped_counts);
            if self
                .skipped_url_tags
                .as_ref()
                .is_some_and(|old| old.contains_key(url))
            {
                count_tags_for_bookmark(tags, &mut stable_skipped_counts);
            }
        }
        // Older snapshots did not retain skipped URLs. Their tag-count surplus
        // can support migration, but confirm omitted bookmarks directly before
        // relying on that surplus to remove a cached URL.
        let legacy_surplus = if self.skipped_url_tags.is_none() {
            let old_counts = tag_counts(self.pins.iter().flatten().map(|pin| &pin.pin));
            self.tags
                .iter()
                .flatten()
                .filter_map(|tag| match tag.tag.1 {
                    TagFreq::Used(count) => count
                        .checked_sub(
                            old_counts
                                .get(&tag.tag_lowered)
                                .copied()
                                .unwrap_or_default(),
                        )
                        .filter(|surplus| *surplus > 0)
                        .map(|surplus| (tag.tag_lowered.as_str(), surplus)),
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        let mut verified_deleted = HashSet::new();
        for (name, count) in used_tags {
            let received_count = received.get(&name).copied().unwrap_or_default();
            if count <= received_count + unknown_tags {
                continue;
            }
            let stable_skipped = stable_skipped_counts
                .get(&name)
                .copied()
                .unwrap_or_default();
            let skipped_count = skipped_counts.get(&name).copied().unwrap_or_default();
            if omitted.contains(name.as_str())
                && count > received_count + unknown_tags + stable_skipped
            {
                let previously_skipped = legacy_surplus
                    .get(name.as_str())
                    .copied()
                    .unwrap_or_default();
                if count > received_count + unknown_tags + skipped_count.min(previously_skipped) {
                    return Err(disagreement().into());
                }
                for pin in self.pins.iter().flatten().filter(|pin| {
                    pin.tag_list.contains(&name) && !preserved_urls.contains(pin.pin.url.as_ref())
                }) {
                    let url = pin.pin.url.as_ref();
                    if verified_deleted.insert(url)
                        && api
                            .find_url(url, super::request_pacing::RequestKind::Background)?
                            .is_some()
                    {
                        return Err(disagreement().into());
                    }
                }
            }
            // A truncated export can omit new rows without dropping old ones.
            // Publish readable changes, but keep retrying at the same watermark
            // whenever the tag endpoint implies missing rows, including a new tag.
            if count > received_count + unknown_tags + skipped_count {
                *refresh = CacheRefresh::Partial;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pinboard::mockito_helper::start_mockito_server;
    use crate::rmps::Deserializer;
    use env_logger;
    use serde::Deserialize;

    #[test]
    fn older_snapshots_and_split_caches_can_confirm_a_deletion_with_skipped_urls() {
        for split_cache in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut cache = CachedData::new(Some(directory.path())).unwrap();
            let api = api::Api::new("test:token");
            let current = json!({
                "href": "https://example.test/current",
                "description": "Current bookmark",
                "tags": "protect",
                "shared": "no",
                "toread": "no",
                "extended": "",
                "time": "2025-01-01T00:00:00Z",
            });
            let mut deleted = current.clone();
            deleted["href"] = json!("https://example.test/deleted");
            let mut skipped = current.clone();
            skipped["href"] = json!("/");
            {
                let rows = json!([current, deleted, skipped]).to_string();
                let _pins = start_mockito_server(r"^/posts/all.*$", 200, rows.as_str());
                let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"protect":3}"#);
                assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
            }
            let pins = cache.pins.as_ref().unwrap();
            let tags = cache.tags.as_ref().unwrap();
            if split_cache {
                fs::write(&cache.pins_cache_file, rmps::to_vec(pins).unwrap()).unwrap();
                fs::write(&cache.tags_cache_file, rmps::to_vec(tags).unwrap()).unwrap();
                fs::remove_file(directory.path().join(SNAPSHOT_CACHE_FN)).unwrap();
            } else {
                fs::write(
                    directory.path().join(SNAPSHOT_CACHE_FN),
                    rmps::to_vec(&(pins, tags)).unwrap(),
                )
                .unwrap();
            }
            let mut reopened = CachedData::new(Some(directory.path())).unwrap();
            assert!(reopened.cache_ok());
            assert!(reopened.skipped_url_tags.is_none());
            let body = json!([current, skipped]).to_string();
            let _pins = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
            let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"protect":2}"#);
            {
                let _lookup = start_mockito_server(r"^/posts/get.*$", 503, "");
                assert!(reopened.update_cache(&api).is_err());
                assert_eq!(2, reopened.pins.as_ref().unwrap().len());
            }
            {
                let found = json!({"posts": [deleted]}).to_string();
                let _lookup = start_mockito_server(r"^/posts/get.*$", 200, found.as_str());
                assert!(reopened.update_cache(&api).is_err());
                assert_eq!(2, reopened.pins.as_ref().unwrap().len());
            }
            {
                let mut malformed = deleted.clone();
                malformed.as_object_mut().unwrap().remove("time");
                let found = json!({"posts": [malformed]}).to_string();
                let _lookup = start_mockito_server(r"^/posts/get.*$", 200, found.as_str());
                assert!(reopened.update_cache(&api).is_err());
                assert_eq!(2, reopened.pins.as_ref().unwrap().len());
            }
            let _lookup = start_mockito_server(r"^/posts/get.*$", 200, r#"{"posts":[]}"#);
            assert_eq!(CacheRefresh::Complete, reopened.update_cache(&api).unwrap());
            assert_eq!(1, reopened.pins.as_ref().unwrap().len());
            let latest = CachedData::new(Some(directory.path())).unwrap();
            assert_eq!(1, latest.pins.as_ref().unwrap().len());
            assert!(latest.skipped_url_tags.unwrap().contains_key("/"));
        }
    }

    #[test]
    fn a_failed_tag_download_preserves_the_bookmark_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mut cache = CachedData::new(Some(directory.path())).unwrap();
        let api = api::Api::new("test:token");
        {
            let _pins = start_mockito_server(
                r"^/posts/all.*$",
                200,
                include_str!("../../tests/all_pins_mockito.json"),
            );
            let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"tag1":1}"#);
            cache.update_cache(&api).unwrap();
        }
        let path = directory.path().join(SNAPSHOT_CACHE_FN);
        let old_snapshot = fs::read(&path).unwrap();

        let _pins = start_mockito_server(r"^/posts/all.*$", 200, "[]");
        let _tags = start_mockito_server(r"^/tags/get.*$", 503, "");
        assert!(cache.update_cache(&api).is_err());
        assert_eq!(fs::read(path).unwrap(), old_snapshot);
        assert!(cache.cache_ok());
        assert!(!cache.pins.as_ref().unwrap().is_empty());
    }

    #[test]
    fn unreadable_tags_cannot_hide_a_retained_bookmarks_known_tag() {
        let directory = tempfile::tempdir().unwrap();
        let mut cache = CachedData::new(Some(directory.path())).unwrap();
        let api = api::Api::new("test:token");
        let saved = json!({
            "href": "https://example.test/saved",
            "description": "Saved bookmark",
            "tags": "Protect former",
            "shared": "no",
            "toread": "no",
            "extended": "",
            "time": "2025-01-01T00:00:00Z",
        });
        let mut other = saved.clone();
        other["href"] = json!("https://example.test/other");
        other["description"] = json!("Other bookmark");
        other["tags"] = json!("backend");
        {
            let body = json!([saved, other]).to_string();
            let _pins = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
            let _tags = start_mockito_server(
                r"^/tags/get.*$",
                200,
                r#"{"protect":1,"former":1,"backend":1}"#,
            );
            assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
        }
        let path = directory.path().join(SNAPSHOT_CACHE_FN);
        let old_snapshot = fs::read(&path).unwrap();
        let mut invalid = saved.clone();
        invalid["description"] = json!(false);
        invalid["tags"] = json!(null);
        let mut unidentified = invalid.clone();
        unidentified.as_object_mut().unwrap().remove("href");
        let mut conflicting = saved.clone();
        conflicting["tags"] = json!("backend");
        for rows in [
            json!([other, invalid]),
            json!([other, unidentified]),
            json!([other, saved, conflicting]),
        ] {
            let body = rows.to_string();
            let _pins = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
            let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"backend":1}"#);
            assert!(cache.update_cache(&api).is_err());
            assert_eq!(old_snapshot, fs::read(&path).unwrap());
            let reopened = CachedData::new(Some(directory.path())).unwrap();
            let names: HashSet<_> = reopened
                .tags
                .as_ref()
                .unwrap()
                .iter()
                .map(|tag| tag.tag_lowered.as_str())
                .collect();
            assert_eq!(HashSet::from(["protect", "former", "backend"]), names);
            assert_eq!(2, reopened.pins.as_ref().unwrap().len());
        }

        // Readable raw tags establish a genuine removal even while another
        // field remains malformed; no old tag is kept alive by its cached row.
        invalid["tags"] = json!("backend");
        {
            let body = json!([other, invalid]).to_string();
            let _pins = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
            let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"backend":2}"#);
            assert_eq!(CacheRefresh::Partial, cache.update_cache(&api).unwrap());
            let tags = cache.tags.as_ref().unwrap();
            assert_eq!(1, tags.len());
            assert_eq!("backend", tags[0].tag_lowered);
        }
        // Once that row is readable, the retained bookmark itself catches up.
        conflicting["description"] = json!("Recovered bookmark");
        let body = json!([other, conflicting]).to_string();
        let _pins = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
        let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"backend":2}"#);
        assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
        assert!(cache
            .pins
            .as_ref()
            .unwrap()
            .iter()
            .all(|pin| pin.tag_list == ["backend"]));
    }

    #[test]
    fn malformed_records_cannot_evict_cached_bookmarks() {
        let directory = tempfile::tempdir().unwrap();
        let mut cache = CachedData::new(Some(directory.path())).unwrap();
        let api = api::Api::new("test:token");
        let bookmark = json!({
            "href": "https://example.test/saved",
            "description": "Saved bookmark",
            "tags": "test",
            "shared": "no",
            "toread": "no",
            "time": "2025-01-01T00:00:00Z",
        });
        let other = json!({
            "href": "https://example.test/other",
            "description": "Another bookmark",
            "tags": "test",
            "shared": "no",
            "toread": "no",
            "time": "2025-01-02T00:00:00Z",
        });
        let mut removed = bookmark.clone();
        removed["href"] = json!("https://example.test/removed");
        removed["description"] = json!("Missing from server");
        let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"test":1}"#);

        let mut malformed = bookmark.clone();
        malformed["description"] = json!(false);
        for (invalid, expected) in [
            (
                malformed.clone(),
                vec!["Another bookmark", "Saved bookmark"],
            ),
            (
                json!({"description": "Missing URL"}),
                vec!["Another bookmark", "Saved bookmark", "Missing from server"],
            ),
        ] {
            {
                let pins = json!([bookmark, removed]).to_string();
                let _mock = start_mockito_server(r"^/posts/all.*$", 200, pins.as_str());
                assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
            }
            let pins = json!([other, invalid]).to_string();
            let _mock = start_mockito_server(r"^/posts/all.*$", 200, pins.as_str());
            assert_eq!(CacheRefresh::Partial, cache.update_cache(&api).unwrap());
            let reopened = CachedData::new(Some(directory.path())).unwrap();
            let pins = reopened.pins.as_ref().unwrap();
            let titles: Vec<_> = pins.iter().map(|pin| pin.pin.title.as_ref()).collect();
            assert_eq!(expected, titles);
        }

        // A valid row resolves the identity of a malformed duplicate.
        let mut corrected = bookmark;
        corrected["description"] = json!("Updated bookmark");
        {
            let pins = json!([other, malformed, corrected]).to_string();
            let _mock = start_mockito_server(r"^/posts/all.*$", 200, pins.as_str());
            assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
            let pins = cache.pins.as_ref().unwrap();
            assert_eq!(2, pins.len());
            assert_eq!("Updated bookmark", pins[1].pin.title);
        }

        // An unrelated malformed row still permits a known deletion.
        let pins = json!([other, {"href": "/", "description": false}]).to_string();
        let _mock = start_mockito_server(r"^/posts/all.*$", 200, pins.as_str());
        assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
        let pins = cache.pins.as_ref().unwrap();
        assert_eq!(1, pins.len());
        assert_eq!("Another bookmark", pins[0].pin.title);
    }

    #[test]
    fn new_malformed_bookmark_leaves_the_published_snapshot_partial() {
        let directory = tempfile::tempdir().unwrap();
        let mut cache = CachedData::new(Some(directory.path())).unwrap();
        let api = api::Api::new("test:token");
        let _tags = start_mockito_server(r"^/tags/get.*$", 200, r#"{"test":1}"#);
        let existing = json!({
            "href": "https://example.test/old",
            "description": "Deleted bookmark",
            "tags": "test",
            "shared": "no",
            "toread": "no",
            "time": "2025-01-01T00:00:00Z",
        });
        {
            let body = json!([existing]).to_string();
            let _mock = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
            assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
        }

        let mut good = existing.clone();
        good["href"] = json!("https://example.test/good");
        good["description"] = json!("New bookmark");
        let mut recovering = existing;
        recovering["href"] = json!("https://example.test/recovering");
        recovering["description"] = json!(false);
        {
            let body = json!([good, recovering]).to_string();
            let _mock = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
            assert_eq!(CacheRefresh::Partial, cache.update_cache(&api).unwrap());
            let published = CachedData::new(Some(directory.path())).unwrap();
            let pins = published.pins.unwrap();
            assert_eq!(1, pins.len());
            assert_eq!("New bookmark", pins[0].pin.title);
        }

        recovering["description"] = json!("Recovered bookmark");
        let body = json!([good, recovering]).to_string();
        let _mock = start_mockito_server(r"^/posts/all.*$", 200, body.as_str());
        assert_eq!(CacheRefresh::Complete, cache.update_cache(&api).unwrap());
        let published = CachedData::new(Some(directory.path())).unwrap();
        let pins = published.pins.unwrap();
        assert_eq!(2, pins.len());
        assert_eq!("Recovered bookmark", pins[1].pin.title);
    }

    #[test]
    fn unicode_normalization_test() {
        let _ = env_logger::try_init();
        debug!("serde_a_cached_pin: starting");
        let mut pin = PinBuilder::new(
            "https://danielkeep.github.io/tlborm/book/README.html",
            "The Little Book of Rust Macros آموزشی",
        )
        .tags("지구 Rust macros")
        .toread("yes")
        .shared("no")
        .description("지구")
        .into_pin();
        pin.time = Utc.ymd(2017, 5, 22).and_hms(17, 46, 54);

        let tag_list = ["지구", "Rust", "macros"]
            .iter()
            .map(|&t| t.chars().nfkd().collect::<String>().to_lowercase())
            .collect::<Vec<String>>();
        let cached_pin = CachedPin {
            pin,
            tag_list,
            title_lowered: "The Little Book of Rust Macros آموزشی"
                .chars()
                .nfkd()
                .collect::<String>()
                .to_lowercase(),
            extended_lowered: Some("지구".nfkd().collect::<String>().to_lowercase()),
        };
        // non-normalized "지구" should not be found in normalized tag list of the pin
        let r = format!(
            "\nquery: {:?}\nextended_norm: {:?}\n",
            "지구".as_bytes(),
            cached_pin.extended_lowered.as_ref().unwrap().as_bytes()
        );
        assert!(
            !cached_pin
                .extended_lowered
                .as_ref()
                .unwrap()
                .as_str()
                .contains("지구"),
            "Normalized and non-normalized tags should not be the same.{}",
            r
        );
        // normalized "지구" should be found in normalized tag list of the pin
        assert!(
            cached_pin.tag_list.contains(&"지구".into()),
            "Normalized and non-normalized tags should not be the same"
        );
        // is_nfk("آموزشی") is true, so we should be able to find it in the normalized cache.
        assert!(
            cached_pin.title_lowered.contains("آموزشی"),
            "Already normalized search queires should be found in our normalized title."
        );
        // Normalizing "آموزشی" should give us the same result as previous assert.
        let normalized_query = "آموزشی".chars().nfkd().collect::<String>();
        assert!(
            cached_pin.title_lowered.contains(&normalized_query),
            "Already normalized search queires should be found in our normalized title."
        );
    }

    #[test]
    fn serde_a_cached_pin() {
        let _ = env_logger::try_init();
        debug!("serde_a_cached_pin: starting");
        let mut pin = PinBuilder::new(
            "https://danielkeep.github.io/tlborm/book/README.html",
            "The Little Book of Rust Macros",
        )
        .tags("Rust macros")
        .toread("yes")
        .shared("no")
        .description("WoW!!!")
        .into_pin();
        pin.time = Utc.ymd(2017, 5, 22).and_hms(17, 46, 54);

        let cached_pin = CachedPin {
            pin,
            tag_list: vec!["rust".into(), "macros".into()],
            title_lowered: "The Little Book of Rust Macros".to_lowercase(),
            extended_lowered: Some("WoW!!!".to_lowercase()),
        };

        let mut buf: Vec<u8> = Vec::new();

        cached_pin
            .serialize(&mut Serializer::new(&mut buf))
            .expect("impossible");
        assert_eq!(185, buf.len());

        let mut de = Deserializer::from_read_ref(&buf);
        let new_cached: CachedPin =
            Deserialize::deserialize(&mut de).expect("Couldn't deserialize a cached pin");

        assert_eq!("The Little Book of Rust Macros", new_cached.pin.title);
        assert_eq!(
            "https://danielkeep.github.io/tlborm/book/README.html",
            new_cached.pin.url.as_ref()
        );
        assert_eq!("yes", new_cached.pin.toread);
        assert_eq!("no", new_cached.pin.shared);
        assert_eq!("WoW!!!", new_cached.pin.extended.unwrap());
        assert_eq!(
            Utc.ymd(2017, 5, 22).and_hms(17, 46, 54),
            new_cached.pin.time
        );
        assert_eq!(
            vec![String::from("rust"), String::from("macros")],
            new_cached.tag_list
        );
    }
}
