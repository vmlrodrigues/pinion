use std::borrow::Cow;

use reqwest;
use serde_json;

use chrono::prelude::*;
use url::Url;

use env_logger;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use super::pin::Pin;
use super::request_pacing::{self, RequestKind};
use super::tag::Tag;
use super::CacheRefresh;

use serde::Deserialize;
use thiserror::Error;
// use serde::{Deserialize, Serialize};

#[cfg(not(test))]
const BASE_URL: &str = "https://api.pinboard.in/v1";

#[cfg(test)]
use mockito;
#[cfg(test)]
#[allow(deprecated)]
const BASE_URL: &str = mockito::SERVER_URL;

/// Struct to hold stringify results Pinboard API returns.
/// Sometimes it returns a json key of "result_code" & sometimes just "result"!!!
#[allow(clippy::doc_markdown)]
#[derive(Serialize, Deserialize, Debug)]
struct ApiResult {
    #[serde(default)]
    result_code: String,
    #[serde(default)]
    result: String,
}

impl ApiResult {
    fn ok(self) -> Result<(), Box<dyn std::error::Error>> {
        if self.result_code == "done" || self.result == "done" {
            Ok(())
        } else if !self.result_code.is_empty() {
            Err(self.result_code.into())
        } else {
            Err(self.result.into())
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct UpdateTime {
    #[serde(rename = "update_time")]
    datetime: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Posts {
    posts: Vec<serde_json::Value>,
}

#[derive(Debug)]
pub(super) struct BookmarkExport<'pin> {
    pub(super) pins: Vec<Pin<'pin>>,
    pub(super) retained: Vec<Pin<'pin>>,
    pub(super) refresh: CacheRefresh,
    // One entry per valid URL without a readable row. None means its tags are unknown.
    pub(super) unreadable_bookmark_tags: HashMap<String, Option<String>>,
    // Reliable tag fields from distinct URLs the parser cannot use.
    pub(super) skipped_url_tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Api<'api> {
    auth_token: Cow<'api, str>,
    request_timeout: Option<Duration>,
    pacing_dir: Option<PathBuf>,
}

#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Error)]
pub enum ApiError {
    // #[fail(display = "invalid url: {}", _0)]
    #[error("invalid url: {0}")]
    UrlError(String),
    // #[fail(display = "invalid server response: {}", _0)]
    #[error("invalid server response: {0}")]
    UnrecognizedResponse(String),
    // #[fail(display = "Server couldn't fulfill request: {}", _0)]
    #[error("server couldn't fulfill request: {0}")]
    ServerError(String),
    #[error("server couldn't fulfill request: {}", .0.canonical_reason().unwrap_or("Temporary server error"))]
    TemporaryServerError(reqwest::StatusCode),
    #[error("server couldn't fulfill request: Too Many Requests")]
    RateLimited,
    #[error("server couldn't fulfill request: Too Many Requests")]
    RateLimitedUntil(DateTime<Utc>),
    #[error("server couldn't fulfill request: {}", if *.service_unavailable { "Service Unavailable" } else { "Too Many Requests" })]
    RetryAfter {
        retry_at: DateTime<Utc>,
        full_download: bool,
        service_unavailable: bool,
    },
    #[error("server couldn't fulfill request: Service Unavailable")]
    UnavailableUntil(DateTime<Utc>),
    #[error("Pinboard denied the request. Check the API token with `pa`.")]
    Authentication,
    #[error("Waiting for an available Pinboard request slot.")]
    RequestDeferred,
    // #[fail(display = "network error: {}", _0)]
    #[error("network error: {0}")]
    Network(String),
    // Network(#[from] std::io::Error),
    // #[fail(display = "serde error: {}", _0)]
    #[error("serde error: {0}")]
    SerdeError(String),
}

impl<'api, 'pin> Api<'api> {
    pub fn new<S>(auth_token: S) -> Self
    where
        S: Into<Cow<'api, str>>,
    {
        let _ = env_logger::try_init();
        Api {
            auth_token: auth_token.into(),
            request_timeout: None,
            pacing_dir: None,
        }
    }

    pub(super) fn set_request_timeout(&mut self, timeout: Option<Duration>) {
        self.request_timeout = timeout;
    }

    pub(super) fn set_request_pacing(&mut self, account_cache_dir: PathBuf) {
        self.pacing_dir = Some(account_cache_dir);
    }

    pub(super) fn all_pins<'cached>(
        &self,
        cached_pins: impl Iterator<Item = &'cached Pin<'pin>>,
    ) -> Result<BookmarkExport<'pin>, Box<dyn std::error::Error>>
    where
        'pin: 'cached,
    {
        debug!("all_pins: starting.");
        let res = self.get_api_response(
            [BASE_URL, "/posts/all"].concat(),
            HashMap::new(),
            RequestKind::FullDownload,
        )?;
        debug!("  received all bookmarks");

        let response: serde_json::Value =
            serde_json::from_str(res.as_str()).map_err(|e| ApiError::SerdeError(e.to_string()))?;
        let records = response.as_array().ok_or_else(|| {
            ApiError::UnrecognizedResponse("array of bookmarks expected from server".to_string())
        })?;

        let record_count = records.len();
        let mut pins: Vec<Pin<'pin>> = Vec::with_capacity(record_count);
        let mut pin_indices: HashMap<&str, usize> = HashMap::new();
        let mut conflicting_urls = HashSet::new();
        let mut malformed_urls: HashMap<&str, Option<&str>> = HashMap::new();
        let mut unidentified_record = false;
        for record in records {
            match Pin::deserialize(record) {
                Ok(pin) if has_bookmark_metadata(record) && Url::parse(&pin.url).is_ok() => {
                    let url = record["href"].as_str().expect("parsed bookmark URL");
                    if let Some(&index) = pin_indices.get(url) {
                        if pin != pins[index] {
                            conflicting_urls.insert(url);
                            let tags = (pin.tags == pins[index].tags)
                                .then(|| record["tags"].as_str())
                                .flatten();
                            let previous = malformed_urls.entry(url).or_insert(tags);
                            if *previous != tags {
                                *previous = None;
                            }
                        }
                    } else {
                        pin_indices.insert(url, pins.len());
                        pins.push(pin);
                    }
                }
                _ => match record["href"].as_str() {
                    Some(url) if !url.is_empty() => {
                        let tags = record["tags"].as_str();
                        let previous = malformed_urls.entry(url).or_insert(tags);
                        if *previous != tags {
                            *previous = None;
                        }
                    }
                    _ => unidentified_record = true,
                },
            }
        }
        pins.retain(|pin| !conflicting_urls.contains(pin.url.as_ref()));
        if record_count > 0
            && pins.is_empty()
            && (unidentified_record || malformed_urls.keys().any(|url| Url::parse(url).is_ok()))
        {
            return Err(ApiError::UnrecognizedResponse(
                "no valid bookmarks in a nonempty response from the server".into(),
            )
            .into());
        }
        if pins.len() == record_count {
            info!("parsed all bookmarks. total: {}", pins.len());
            return Ok(BookmarkExport {
                pins,
                retained: Vec::new(),
                refresh: CacheRefresh::Complete,
                unreadable_bookmark_tags: HashMap::new(),
                skipped_url_tags: HashMap::new(),
            });
        }

        info!(
            "couldn't parse {} bookmarks (out of {})",
            record_count - pins.len(),
            record_count
        );

        let received_urls: HashSet<_> = pins.iter().map(|pin| pin.url.as_ref()).collect();
        malformed_urls.retain(|url, _| !received_urls.contains(url));
        // Keep cached versions wherever a malformed row prevents us from
        // confirming deletion. Always prefer a valid row for the same URL.
        let retained: Vec<_> = cached_pins
            .filter(|pin| {
                let url = pin.url.as_ref();
                !received_urls.contains(url)
                    && (unidentified_record || malformed_urls.contains_key(url))
            })
            .cloned()
            .collect();
        // Pinboard sometimes returns unusable URLs. Retry if a real bookmark is
        // missing or the response is too ambiguous to rule one out.
        let mut unreadable_bookmark_tags = HashMap::new();
        let mut skipped_url_tags = HashMap::new();
        for (url, tags) in malformed_urls {
            if Url::parse(url).is_ok() {
                unreadable_bookmark_tags.insert(url.to_owned(), tags.map(str::to_owned));
            } else if let Some(tags) = tags {
                skipped_url_tags.insert(url.to_owned(), tags.to_owned());
            }
        }
        let incomplete =
            unidentified_record || !retained.is_empty() || !unreadable_bookmark_tags.is_empty();
        let refresh = if incomplete {
            CacheRefresh::Partial
        } else {
            CacheRefresh::Complete
        };
        Ok(BookmarkExport {
            pins,
            retained,
            refresh,
            unreadable_bookmark_tags,
            skipped_url_tags,
        })
    }

    pub fn suggest_tags<T: AsRef<str>>(
        &self,
        url: T,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        debug!("suggest_tags: starting.");
        let response = self.get_api_response(
            [BASE_URL, "/posts/suggest"].concat(),
            HashMap::from([("url", url.as_ref())]),
            RequestKind::Optional,
        )?;
        let sections: Vec<HashMap<String, Vec<String>>> = serde_json::from_str(&response)
            .map_err(|error| ApiError::SerdeError(error.to_string()))?;
        if sections.is_empty() {
            return Ok(Vec::new());
        }

        let invalid = || ApiError::UnrecognizedResponse("invalid tag suggestions".into());
        let mut popular = None;
        for mut section in sections {
            if section.is_empty()
                || section
                    .keys()
                    .any(|name| !matches!(name.as_str(), "popular" | "recommended"))
            {
                return Err(invalid().into());
            }
            if let Some(mut tags) = section.remove("popular") {
                // Ignore occasional blank entries in an otherwise usable list.
                let had_entries = !tags.is_empty();
                tags.retain(|tag| !tag.trim().is_empty());
                if had_entries && tags.is_empty() {
                    return Err(invalid().into());
                }
                if popular.replace(tags).is_some() {
                    return Err(invalid().into());
                }
            }
        }
        popular.ok_or_else(|| invalid().into())
    }

    pub(super) fn find_url(
        &self,
        url: &str,
        kind: RequestKind,
    ) -> Result<Option<Pin<'pin>>, Box<dyn std::error::Error>> {
        let response = self.get_api_response(
            [BASE_URL, "/posts/get"].concat(),
            HashMap::from([("url", url)]),
            kind,
        )?;
        let response: Posts = serde_json::from_str(&response)
            .map_err(|error| ApiError::SerdeError(error.to_string()))?;
        let mut posts = response.posts.into_iter();
        let Some(post) = posts.next() else {
            return Ok(None);
        };
        // Only a genuinely empty response confirms absence. The URL-filtered
        // endpoint must return one exact match before any caller can replace or
        // evict the bookmark; malformed or unrelated rows are not a deletion.
        if posts.next().is_some() || post["href"].as_str() != Some(url) {
            return Err(ApiError::UnrecognizedResponse("invalid bookmark lookup".into()).into());
        }
        // The cache model allows missing notes and dates. A replacement needs both fields.
        if !post["extended"].is_string() || !has_bookmark_metadata(&post) {
            return Err(
                ApiError::UnrecognizedResponse("incomplete bookmark metadata".into()).into(),
            );
        }
        let pin = serde_json::from_value(post)
            .map_err(|error| ApiError::SerdeError(error.to_string()))?;
        Ok(Some(pin))
    }

    pub fn add_url(&self, p: Pin) -> Result<(), Box<dyn std::error::Error>> {
        self.save_url(p, None)
    }

    pub(super) fn replace_url(&self, pin: Pin) -> Result<(), Box<dyn std::error::Error>> {
        let timestamp = pin.time.to_rfc3339_opts(SecondsFormat::Secs, true);
        self.save_url(pin, Some(&timestamp))
    }

    fn save_url(&self, p: Pin, timestamp: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        debug!("add_url: starting.");
        let url: &str = &p.url;
        let extended = &p.extended.unwrap_or_default();
        let mut map = HashMap::new();
        debug!(" url: {}", url);

        map.insert("url", url);
        map.insert("description", &p.title);
        map.insert("tags", &p.tags);
        map.insert("toread", &p.toread);
        map.insert("extended", extended);
        map.insert("shared", &p.shared);
        map.insert("replace", "yes");
        if let Some(timestamp) = timestamp {
            map.insert("dt", timestamp);
        }

        debug!("Sending payload to: {}/posts/add\n\t{:?}", BASE_URL, map);
        self.get_api_response(
            [BASE_URL, "/posts/add"].concat(),
            map,
            RequestKind::Ordinary,
        )
        .and_then(|res| {
            serde_json::from_str::<ApiResult>(&res)
                .map_err(|e| From::from(ApiError::UnrecognizedResponse(e.to_string())))
        })
        .and_then(self::ApiResult::ok)
    }

    pub fn tag_rename<T: AsRef<str>>(
        &self,
        old: T,
        new: T,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("tag_rename: starting.");
        let mut map = HashMap::new();
        map.insert("old", old.as_ref());
        map.insert("new", new.as_ref());
        self.get_api_response(
            [BASE_URL, "/tags/rename"].concat(),
            map,
            RequestKind::Ordinary,
        )
        .and_then(|res| {
            serde_json::from_str::<ApiResult>(&res)
                .map_err(|e| From::from(ApiError::UnrecognizedResponse(e.to_string())))
        })
        .and_then(self::ApiResult::ok)
    }

    pub fn tag_delete<T: AsRef<str>>(&self, tag: T) -> Result<(), Box<dyn std::error::Error>> {
        debug!("tag_rename: starting.");
        let mut map = HashMap::new();
        map.insert("tag", tag.as_ref());
        self.get_api_response(
            [BASE_URL, "/tags/delete"].concat(),
            map,
            RequestKind::Ordinary,
        )
        .and_then(|res| {
            serde_json::from_str::<ApiResult>(&res)
                .map_err(|e| From::from(ApiError::UnrecognizedResponse(e.to_string())))
        })
        .and_then(self::ApiResult::ok)
    }

    /// Gets all tags with their usage frequency.
    pub fn tags_frequency(&self) -> Result<Vec<Tag>, Box<dyn std::error::Error>> {
        // Pinboard API returns json narray when user has no tags, otherwise it returns an
        // object/map of tag:frequency!
        debug!("tags_frequency: starting.");
        let res = self.get_api_response(
            [BASE_URL, "/tags/get"].concat(),
            HashMap::new(),
            RequestKind::Background,
        )?;
        // Assuming pinboard is returing String:number style for tag frequency
        debug!("  trying string:usize map");
        let tag_freq = serde_json::from_str::<HashMap<String, usize>>(&res)
            .map(|tagmap| {
                tagmap
                    .into_iter()
                    .map(|(tag, freq)| Tag::new(tag, freq))
                    .collect()
            })
            .map_err(Into::into);
        if tag_freq.is_ok() {
            return tag_freq;
        }
        // Assuming pinboard has returned String:String style for tag frequency since last try didn't work
        debug!("  trying string:string map");
        if let Ok(tagmap) = serde_json::from_str::<HashMap<String, String>>(&res) {
            return tagmap
                .into_iter()
                .map(|(tag, count)| {
                    let frequency = count.parse::<usize>().map_err(|_| {
                        ApiError::UnrecognizedResponse("invalid tag frequency".into())
                    })?;
                    Ok(Tag::new(tag, frequency))
                })
                .collect();
        }
        // If we are here, it most likely means that user's tag list is empty and pinboard is returning
        // an empty vector instead of an object
        debug!("   couldn't get a tag2freq map");
        debug!("  trying to decode non-object empty tag list");
        let raw_tags = serde_json::from_str::<Vec<serde_json::Value>>(&res)?;
        if !raw_tags.is_empty() {
            return Err(ApiError::UnrecognizedResponse(
                "expected a tag map or an empty array".into(),
            )
            .into());
        }
        Ok(vec![])
    }

    pub fn delete<T: AsRef<str>>(&self, url: T) -> Result<(), Box<dyn std::error::Error>> {
        debug!("delete: starting.");
        let mut map = HashMap::new();
        debug!(" url: {}", url.as_ref());
        map.insert("url", url.as_ref());

        self.get_api_response(
            [BASE_URL, "/posts/delete"].concat(),
            map,
            RequestKind::Ordinary,
        )
        .and_then(|res| {
            serde_json::from_str(&res)
                .map_err(|e| From::from(ApiError::UnrecognizedResponse(e.to_string())))
        })
        .and_then(self::ApiResult::ok)
    }

    pub fn recent_update(&self) -> Result<DateTime<Utc>, Box<dyn std::error::Error>> {
        debug!("recent_update: starting.");
        self.get_api_response(
            [BASE_URL, "/posts/update"].concat().as_str(),
            HashMap::new(),
            RequestKind::Background,
        )
        .and_then(|res| {
            serde_json::from_str(&res).map_err(|e| From::from(ApiError::SerdeError(e.to_string())))
        })
        .map(|date: UpdateTime| date.datetime)
    }

    fn add_auth_token<T: AsRef<str>>(&self, url: T) -> Url {
        debug!("add_auth_token: starting.");
        // debug!("  token: `{}`", &self.auth_token);
        Url::parse_with_params(
            url.as_ref(),
            &[("format", "json"), ("auth_token", &self.auth_token)],
        )
        .expect("invalid parameters")
    }

    fn get_api_response<T: AsRef<str>>(
        &self,
        endpoint: T,
        params: HashMap<&str, &str>,
        kind: RequestKind,
    ) -> Result<String, Box<dyn std::error::Error>> {
        debug!("get_api_response: starting.");

        let endpoint_string = endpoint.as_ref().to_string();
        let mut base_url =
            Url::parse(endpoint.as_ref()).map_err(|_| ApiError::UrlError(endpoint_string))?;
        debug!("  url: {:?}", base_url);
        for (k, v) in params {
            base_url.query_pairs_mut().append_pair(k, v);
        }
        let api_url = self.add_auth_token(base_url);

        let client = reqwest::blocking::Client::new();
        let mut request = client.get(api_url);
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        let r = match self.pacing_dir.as_deref() {
            Some(cache_dir) => request_pacing::send(cache_dir, request, kind)?,
            _ => request.send(),
        };

        let mut resp = r.map_err(|error| ApiError::Network(error.to_string()))?;

        if resp.status().is_success() {
            let mut content = String::with_capacity(2 * 1024);
            resp.read_to_string(&mut content)
                .map_err(|error| ApiError::Network(error.to_string()))?;
            debug!(" string from resp ok");
            debug!("   {:?}", content.chars().take(15).collect::<Vec<char>>());
            debug!(" returning from get_api_response");
            Ok(content)
        } else {
            debug!("  response status indicates error");
            debug!("    {:?}", resp.status().as_str());
            debug!("    {:?}", resp.status().canonical_reason(),);
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(ApiError::RateLimited.into());
            }
            if matches!(
                status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) {
                return Err(ApiError::Authentication.into());
            }
            if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
                return Err(ApiError::TemporaryServerError(status).into());
            }
            let e = ApiError::ServerError(
                status
                    .canonical_reason()
                    .map_or_else(|| status.as_u16().to_string(), str::to_owned),
            )
            .into();
            debug!("    ERR: {:?}", e);
            Err(e)
        }
    }
}

fn valid_flag(value: &serde_json::Value) -> bool {
    matches!(value.as_str(), Some("yes" | "no"))
}

fn has_bookmark_metadata(post: &serde_json::Value) -> bool {
    post["time"].is_string() && valid_flag(&post["shared"]) && valid_flag(&post["toread"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::pinboard::mockito_helper::start_mockito_server;
    use crate::pinboard::mockito_helper::MockBodyGenerate;
    use crate::pinboard::pin::PinBuilder;
    use crate::pinboard::tag;

    const TEST_URL: &str = "https://githuуй.com/Здравствуйт?q=13#fragment";

    fn lookup_post() -> serde_json::Value {
        json!({
            "href": TEST_URL,
            "description": "Bookmark title",
            "tags": "tag1 tag2",
            "shared": "no",
            "toread": "yes",
            "extended": "Remote notes",
            "time": "2024-04-05T09:08:07Z"
        })
    }

    #[test]
    fn get_latest_update_time() {
        let _ = env_logger::try_init();
        debug!("get_latest_update_time: starting.");
        let _m = start_mockito_server(
            r"^/posts/update.*$",
            200,
            r#"{"update_time":"2018-02-07T01:54:09Z"}"#,
        );
        let api = Api::new(include_str!("api_token.txt"));
        let r = api.recent_update();
        assert!(r.is_ok());
    }

    #[test]
    fn exact_remote_lookup_rejects_incomplete_metadata() {
        let api = Api::new("test:token");
        let post = lookup_post();

        for field in ["extended", "time", "shared", "toread"] {
            let mut incomplete = post.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            let body = json!({"posts": [incomplete]}).to_string();
            let _mock = start_mockito_server(r"^/posts/get.*$", 200, body.as_str());
            let error = api.find_url(TEST_URL, RequestKind::Ordinary).unwrap_err();
            assert!(error.to_string().contains("incomplete bookmark metadata"));
        }
    }

    #[test]
    fn exact_remote_lookup_rejects_nonstring_notes() {
        let api = Api::new("test:token");
        for notes in [json!(null), json!(false), json!(1), json!([]), json!({})] {
            let mut post = lookup_post();
            post["extended"] = notes;
            let body = json!({"posts": [post]}).to_string();
            let _mock = start_mockito_server(r"^/posts/get.*$", 200, body.as_str());
            let error = api.find_url(TEST_URL, RequestKind::Ordinary).unwrap_err();
            assert!(error.to_string().contains("incomplete bookmark metadata"));
        }
    }

    #[test]
    fn exact_remote_lookup_rejects_malformed_or_ambiguous_rows() {
        let api = Api::new("test:token");
        let post = lookup_post();
        let mut no_url = post.clone();
        no_url.as_object_mut().unwrap().remove("href");
        let mut unrelated = post.clone();
        unrelated["href"] = json!("https://example.test/other");
        for rows in [
            json!([no_url]),
            json!([unrelated]),
            json!([null]),
            json!([post, post]),
            json!([post, no_url]),
        ] {
            let body = json!({"posts": rows}).to_string();
            let _mock = start_mockito_server(r"^/posts/get.*$", 200, body.as_str());
            let error = api.find_url(TEST_URL, RequestKind::Background).unwrap_err();
            assert!(error.to_string().contains("invalid bookmark lookup"));
        }
    }

    #[test]
    fn exact_remote_lookup_distinguishes_empty_and_valid_responses() {
        let api = Api::new("test:token");
        {
            let _mock = start_mockito_server(r"^/posts/get.*$", 200, r#"{"posts":[]}"#);
            assert!(api
                .find_url(TEST_URL, RequestKind::Background)
                .unwrap()
                .is_none());
        }
        for notes in ["Remote notes", ""] {
            let mut post = lookup_post();
            post["extended"] = json!(notes);
            let body = json!({"posts": [post]}).to_string();
            let _mock = start_mockito_server(r"^/posts/get.*$", 200, body.as_str());
            let pin = api
                .find_url(TEST_URL, RequestKind::Ordinary)
                .unwrap()
                .unwrap();
            assert_eq!(pin.url, TEST_URL);
            assert_eq!(pin.extended.as_deref(), Some(notes));
        }
    }

    // This test will always panic if the BASE_URL is not set to some unreachable address causes
    // a network error.
    // To actually test network errors, change the BASE_URL to something like
    // "http://oaeisn13k.com" and remove the should_panic attribute. In that case the test should
    // pass sicne we will encounter a network error.
    #[test]
    #[should_panic(expected = "Expected ApiError::Network")]
    fn network_io_error_test() {
        let _m1 = start_mockito_server(r"^/posts/delete.*$", 429, r#"io error"#);
        let api = Api::new(include_str!("api_token.txt"));
        let r = api.delete("http://google.com/public");
        assert!(r.is_err());
        let err = r.unwrap_err();
        let err = err.downcast_ref::<ApiError>();
        match err.unwrap() {
            ApiError::Network(_) => println!("GOT Network"),
            _ => panic!("Expected ApiError::Network"),
        }
    }

    #[test]
    fn too_many_requests() {
        let _m1 = start_mockito_server(r"^/posts/delete.*$", 429, r#"Back off"#);
        let api = Api::new(include_str!("api_token.txt"));
        let r = api.delete(TEST_URL);
        assert_eq!(
            "server couldn't fulfill request: Too Many Requests",
            r.expect_err("Expected Not Found").to_string()
        );
    }

    #[test]
    fn delete_tag_test() {
        let _ = env_logger::try_init();
        debug!("delete_tag_test: starting.");
        let _m1 = start_mockito_server(r#"^/tags/delete.*$"#, 200, r#"{"result":"done"}"#);
        let api = Api::new(include_str!("api_token.txt"));
        let r = api.tag_delete("DUMMY");
        r.expect("Error in deleting a tag.");

        {
            // Deleting non-existing tag
            // Pinboard returns OK on this operation!!!
            let _m2 = start_mockito_server(
                r"^/tags/delete.+fucking\.way.*$",
                200,
                r#"{"result":"done"}"#,
            );
            api.tag_delete("http://no.fucking.way")
                .expect("pinboard OKs deleting a non-existing tag.");
        }

        {
            // Deleting empty string
            // Pinboard returns OK on this operation!!!
            let _m2 = start_mockito_server(r"^/tags/delete.*$", 200, r#"{"result":"done"}"#);
            api.tag_delete("")
                .expect("pinboard OKs deleting a non-existing tag.");
        }
    }

    #[test]
    fn rename_tag_test() {
        let _ = env_logger::try_init();
        debug!("rename_tag_test: starting");
        let _m1 = start_mockito_server(r#"^/tags/rename.*$"#, 200, r#"{"result":"done"}"#);
        let api = Api::new(include_str!("api_token.txt"));
        let r = api.tag_rename("old_tag", "new_tag");
        r.expect("Error in renaming a tag.");

        // Pinboard apparently can rename null to a new tag!!!
        api.tag_rename("", "iamjesus")
            .expect("Should be able to breath life into abyss");

        {
            // renaming to an empty tag
            let _m2 =
                start_mockito_server(r#"^/tags/rename.*$"#, 200, r#"{"result":"rename to null"}"#);
            let r = api
                .tag_rename("old_tag", "")
                .expect_err("renaming to empty tag should return error");
            assert_eq!("rename to null".to_string(), r.to_string());
        }
    }

    #[test]
    fn delete_api_test() {
        let _ = env_logger::try_init();
        debug!("delete_a_pin: starting.");
        add_a_url();
        let _m1 = start_mockito_server(r#"^/posts/delete.*$"#, 200, r#"{"result_code":"done"}"#);
        let api = Api::new(include_str!("api_token.txt"));
        let r = api.delete(TEST_URL);
        r.expect("Error in deleting a pin.");

        {
            // Deleting non-existing bookmark
            let _m2 = start_mockito_server(
                r"^/posts/delete.+fucking\.way.*$",
                200,
                r#"{"result_code":"item not found"}"#,
            );
            let r = api
                .delete("http://no.fucking.way")
                .expect_err("Deleted non-existing pin");
            assert_eq!("item not found".to_string(), r.to_string());
        }

        {
            // Deleting malformed url
            let _m2 = start_mockito_server(
                r"^/posts/delete.*$",
                200,
                r#"{"result_code":"item not found"}"#,
            );
            let r = api
                .delete(":// bad url/#")
                .expect_err("should not find a malformed url to delete");
            assert_eq!("item not found".to_string(), r.to_string());
        }
    }

    #[test]
    fn add_a_url() {
        let _ = env_logger::try_init();
        debug!("add_a_url: starting.");
        let _m1 = start_mockito_server(r"^/posts/add.*$", 200, r#"{"result_code":"done"}"#);
        let api = Api::new(include_str!("api_token.txt"));
        let p = PinBuilder::new(TEST_URL, "test bookmark/pin")
            .tags("tagestan what")
            .description("russian website!")
            .shared("yes")
            .into_pin();
        let res = api.add_url(p);
        res.expect("Error in adding a pin.");

        {
            // Adding a malformed url
            let _m1 = start_mockito_server(
                r"^/posts/add.+bad_url.*$",
                200,
                r#"{"result_code":"missing url"}"#,
            );
            let p = PinBuilder::new(":// bad_url/#", "test bookmark/pin")
                .tags("tagestan what")
                .description("russian website!")
                .shared("yes")
                .into_pin();
            let r = api
                .add_url(p)
                .expect_err("server should not have accepted malformed url");
            assert_eq!("missing url", r.to_string());
        }
    }

    #[test]
    fn suggest_tags() {
        let _ = env_logger::try_init();
        debug!("suggest_tags: starting.");
        let _m1 = start_mockito_server(
            r"^/posts/suggest.*$",
            200,
            PathBuf::from("tests/suggested_tags_mockito.json"),
        );
        let api = Api::new(include_str!("api_token.txt"));
        let url = "http://blog.com/";
        let res = api.suggest_tags(url);
        assert_eq!(
            vec!["datetime", "library", "rust"],
            res.expect("impossible")
        );
    }

    #[test]
    fn test_tag_freq_str2str() {
        let _ = env_logger::try_init();
        debug!("test_tag_freq: starting.");
        let _m1 = PathBuf::from("tests/all_tags_mockito.json")
            .create_mockito_server(r"^/tags/get.*$", 200);
        let api = Api::new(include_str!("api_token.txt"));
        let res = api.tags_frequency();
        let r = res.unwrap_or_else(|e| panic!("{:?}", e));
        assert_eq!(94, r.len());
    }

    #[test]
    fn test_tag_freq_str2int() {
        let _ = env_logger::try_init();
        debug!("test_tag_freq: starting.");
        let _m1 = PathBuf::from("tests/all_tags_mockito2.json")
            .create_mockito_server(r"^/tags/get.*$", 200);
        let api = Api::new(include_str!("api_token.txt"));
        let res = api.tags_frequency();
        let r = res.unwrap_or_else(|e| panic!("{:?}", e));
        if let Some(tag) = r.iter().find(|&t| t.0 == "آموزشی") {
            match tag.1 {
                tag::TagFreq::Used(5) => {}
                _ => panic!("Expecetd tag freuqency of 5, got {:?}", tag.1),
            }
        } else {
            panic!("Can't find a specific tag in mock list of tag frequencies");
        }
        assert_eq!(94, r.len());
    }

    #[test]
    fn test_tag_freq_empty() {
        let _ = env_logger::try_init();
        debug!("test_tag_freq_empty: starting.");
        {
            let _m1 = "[]".create_mockito_server(r"^/tags/get.*$", 201);
            let api = Api::new(include_str!("api_token.txt"));
            let res = api.tags_frequency();
            let r = res.unwrap_or_else(|e| panic!("{:?}", e));
            assert!(r.is_empty());
        }
        {
            let _m1 = "{}".create_mockito_server(r"^/tags/get.*$", 201);
            let api = Api::new(include_str!("api_token.txt"));
            let res = api.tags_frequency();
            let r = res.unwrap_or_else(|e| panic!("{:?}", e));
            assert!(r.is_empty());
        }
    }

    #[test]
    fn test_all_pins() {
        let _ = env_logger::try_init();
        debug!("test_all_pins: starting.");
        let _m1 = start_mockito_server(
            r"^/posts/all.*$",
            200,
            PathBuf::from("tests/all_pins_mockito.json"),
        );
        let api = Api::new(include_str!("api_token.txt"));
        let BookmarkExport { pins, refresh, .. } = api.all_pins(std::iter::empty()).unwrap();

        assert_eq!(58, pins.len());
        assert_eq!(CacheRefresh::Partial, refresh);
    }

    #[test]
    fn test_all_pins_empty() {
        let _mock = start_mockito_server(r"^/posts/all.*$", 200, "[]");
        let api = Api::new("test:token");

        let BookmarkExport { pins, refresh, .. } = api.all_pins(std::iter::empty()).unwrap();
        assert!(pins.is_empty());
        assert_eq!(CacheRefresh::Complete, refresh);
    }

    #[test]
    fn test_all_pins_rejects_unidentified_or_unreadable_bookmarks() {
        let api = Api::new("test:token");
        for body in [
            r#"[{"tags":"protect","error":"not a bookmark"}]"#,
            r#"[{"href":"https://example.test/malformed","description":false,"tags":"protect","shared":"no","toread":"no"}]"#,
        ] {
            let _mock = start_mockito_server(r"^/posts/all.*$", 200, body);
            let error = api.all_pins(std::iter::empty()).unwrap_err();
            assert!(matches!(
                error.downcast_ref::<ApiError>(),
                Some(ApiError::UnrecognizedResponse(_))
            ));
        }
    }
}
