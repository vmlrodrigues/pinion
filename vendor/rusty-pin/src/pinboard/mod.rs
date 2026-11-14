use std::borrow::Cow;
use std::fs::File;
use std::path::{Path, PathBuf};
use unicode_normalization::{is_nfkd_quick, IsNormalized, UnicodeNormalization};

use rmps::Serializer;
// use serde::Deserialize;

use chrono::prelude::*;
use url::Url;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use env_logger;
use lazy_static::lazy_static;

mod api;
pub use api::ApiError;
mod cached_data;
mod config;

#[cfg(test)]
mod mockito_helper;
#[cfg(test)]
mod tests;

pub mod pin;
mod request_pacing;
pub mod tag;

use self::cached_data::{CachedData, CachedPin};
use self::config::Config;

pub use self::pin::{Pin, PinBuilder};
pub use self::tag::{Tag, TagFreq};

lazy_static! {
    /// Fuzzy matcher used in all search function.
    static ref MATCHER: SkimMatcherV2 = SkimMatcherV2::default().ignore_case();
}

#[derive(Debug)]
pub struct Pinboard<'api, 'pin> {
    api: api::Api<'api>,
    cfg: Config,
    cached_data: CachedData<'pin>,
}

/// Whether the published cache still needs a retry for an incomplete export.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheRefresh {
    /// The export provided enough evidence to accept the snapshot as complete.
    Complete,
    /// Valid bookmarks were cached, but the export may be missing some records.
    Partial,
}

/// # Errors
///
/// Returns error on network failure or file failure
impl<'api, 'pin> Pinboard<'api, 'pin> {
    /// # Errors
    ///
    /// Returns an error if network/server or file I/O error occurs.
    pub fn new<S, P>(
        auth_token: S,
        cached_dir: Option<P>,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        S: Into<Cow<'api, str>>,
        P: AsRef<Path>,
    {
        let mut pinboard = Self::open(auth_token, cached_dir)?;
        if !pinboard.cache_is_available() {
            pinboard.update_cache()?;
        }
        Ok(pinboard)
    }

    /// Open the local cache without contacting Pinboard.
    ///
    /// # Errors
    /// Returns an error if the cache directory cannot be created.
    pub fn open<S, P>(
        auth_token: S,
        cached_dir: Option<P>,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        S: Into<Cow<'api, str>>,
        P: AsRef<Path>,
    {
        Ok(Self {
            api: api::Api::new(auth_token),
            cfg: Config::new(),
            cached_data: CachedData::new(cached_dir)?,
        })
    }

    pub fn cache_is_available(&self) -> bool {
        self.cached_data.cache_ok()
    }

    /// Reload bookmarks and tags published by another process.
    ///
    /// # Errors
    /// Returns an error if no valid copy can be read.
    pub fn reload_cache(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.cached_data.load_cache_data_from_file()
    }

    /// # Errors
    ///
    /// Returns an error on file error, or if cached data is readable.
    pub fn set_cache_dir<P: AsRef<Path>>(
        &mut self,
        p: &P,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("set_cache_dir: starting.");
        self.cached_data.set_cache_dir(p)?;
        self.cached_data.load_cache_data_from_file()
    }

    pub fn enable_tag_only_search(&mut self, v: bool) {
        debug!("enable_tag_only_search: starting.");
        self.cfg.tag_only_search = v;
    }

    pub fn enable_fuzzy_search(&mut self, v: bool) {
        debug!("enable_fuzzy_search: starting.");
        self.cfg.fuzzy_search = v;
    }

    pub fn enable_private_new_pin(&mut self, v: bool) {
        debug!("enable_private_new_pin: starting.");
        self.cfg.private_new_pin = v;
    }

    pub fn enable_toread_new_pin(&mut self, v: bool) {
        debug!("enable_toread_new_pin: starting.");
        self.cfg.toread_new_pin = v;
    }

    /// # Errors
    ///
    /// If the pin being added has an invalid url, an error will be returned.
    pub fn add_pin(&self, p: Pin) -> Result<(), Box<dyn std::error::Error>> {
        debug!("add_pin: starting.");
        let _r = Url::parse(&p.url)?;
        self.api.add_url(p)
    }

    /// Look up the exact bookmark on Pinboard, without using the local cache.
    ///
    /// # Errors
    /// Returns an error if the request fails or the response is invalid.
    pub fn find_remote_url(
        &self,
        url: &str,
    ) -> Result<Option<Pin<'pin>>, Box<dyn std::error::Error>> {
        self.api
            .find_url(url, request_pacing::RequestKind::Ordinary)
    }

    /// Read a bookmark for display without holding the shared request lock during network I/O.
    ///
    /// # Errors
    /// Returns an error if the request fails or the response is invalid.
    pub fn preview_remote_url(
        &self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<Pin<'pin>>, Box<dyn std::error::Error>> {
        let mut api = self.api.clone();
        api.set_request_timeout(Some(timeout));
        api.find_url(url, request_pacing::RequestKind::Background)
    }

    /// Write an updated bookmark back to Pinboard, preserving its saved timestamp.
    ///
    /// # Errors
    /// Returns an error if the URL is invalid or the request fails.
    pub fn replace_pin(&self, pin: Pin) -> Result<(), Box<dyn std::error::Error>> {
        Url::parse(&pin.url)?;
        self.api.replace_url(pin)
    }

    /// # Errors
    ///
    /// Returns an error if network/server error occurs.
    pub fn delete<T: AsRef<str>>(&self, url: T) -> Result<(), Box<dyn std::error::Error>> {
        debug!("delete: starting.");
        self.api.delete(url)
    }

    /// # Errors
    ///
    /// Returns an error if network/server error occurs.
    pub fn is_cache_outdated(
        &self,
        last_update: DateTime<Utc>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        debug!("is_cache_outdated: starting.");
        self.remote_update_time().map(|res| last_update < res)
    }

    /// Return the most recent bookmark update time reported by Pinboard.
    ///
    /// # Errors
    /// Returns an error if the request fails or the response is invalid.
    pub fn remote_update_time(&self) -> Result<DateTime<Utc>, Box<dyn std::error::Error>> {
        self.api.recent_update()
    }

    /// Override the request timeout for this client, or restore the default with `None`.
    pub fn set_request_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.api.set_request_timeout(timeout);
    }

    /// Coordinate ordinary API calls with other clients using the same account directory.
    ///
    /// The caller manages the separate cooldown for full bookmark downloads.
    pub fn enable_request_pacing<P: Into<PathBuf>>(&mut self, account_cache_dir: P) {
        self.api.set_request_pacing(account_cache_dir.into());
    }

    /// Delete a tag
    /// # Errors
    ///
    /// Returns an error if network/server error occurs.
    pub fn delete_tag<T: AsRef<str>>(&self, tag: T) -> Result<(), Box<dyn std::error::Error>> {
        debug!("delete_tag: starting.");
        self.api.tag_delete(tag)
    }

    /// Rename a tag
    /// # Errors
    ///
    /// Returns an error if network error occurs.
    pub fn rename_tag<T: AsRef<str>>(
        &self,
        old: T,
        new: T,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("rename_tag: starting.");
        self.api.tag_rename(old, new)
    }

    /// Update local cache
    /// # Errors
    ///
    /// Returns error if cache cannot be updated due to file errors or serde erros
    pub fn update_cache(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.refresh_cache().map(|_| ())
    }

    /// Update the local bookmark and tag cache and report its completeness.
    ///
    /// # Errors
    /// Returns an error if the download or save fails, or publishing the snapshot
    /// would risk deleting a previously cached bookmark due to malformed data.
    pub fn refresh_cache(&mut self) -> Result<CacheRefresh, Box<dyn std::error::Error>> {
        debug!("update_cache: starting.");
        self.cached_data.update_cache(&self.api)
    }

    /// Returns list of all Tags (tag, frequency)
    #[must_use]
    pub fn list_tag_pairs(&self) -> Option<Vec<&Tag>> {
        debug!("list_tag_pairs: starting.");
        self.cached_data
            .tags
            .as_ref()
            .map(|t| t.iter().map(|d| &d.tag).collect())
    }

    /// Returns list of all bookmarks
    #[must_use]
    pub fn list_bookmarks(&self) -> Option<Vec<&'_ Pin<'_>>> {
        debug!("list_bookmarks: starting.");
        self.cached_data
            .pins
            .as_ref()
            .map(|v| v.iter().map(|p| &p.pin).collect())
    }

    /// Suggest a list of tags based on the provided URL
    /// # Errors
    ///
    /// Returns error if `url` cannot be parsed.
    pub fn popular_tags<T: AsRef<str>>(
        &self,
        url: T,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        debug!("popular_tags: starting.");
        let _r = Url::parse(url.as_ref())?;
        self.api.suggest_tags(url)
    }
}

#[derive(Debug)]
pub enum SearchType {
    TitleOnly,
    TagOnly,
    UrlOnly,
    DescriptionOnly,
    TagTitleOnly,
}

// Search functions
impl<'api, 'pin> Pinboard<'api, 'pin> {
    /// Searches all the fields within bookmarks to filter them.
    /// This function honors `[pinboard::config::Config]` settings for fuzzy search & `tag_only` search.
    /// # Errors
    ///
    /// If cached data is not readable an error is returned.
    pub fn search_items(
        &self,
        query: &str,
    ) -> Result<Option<Vec<&'_ Pin<'_>>>, Box<dyn std::error::Error>> {
        debug!("search_items: starting.");
        let query = if is_nfkd_quick(query.chars()) == IsNormalized::Yes {
            query.into()
        } else {
            query.chars().nfkd().collect::<String>()
        };
        let q = &query.to_lowercase();
        if self.cached_data.cache_ok() {
            let r = self
                .cached_data
                .pins
                .as_ref()
                .map(|p| {
                    p.iter()
                        .filter(|item: &&CachedPin| {
                            if self.cfg.tag_only_search {
                                if self.cfg.fuzzy_search {
                                    item.pin.tag_contains(&query, Some(&MATCHER))
                                } else {
                                    item.pin.tag_contains(q, None)
                                }
                            } else if self.cfg.fuzzy_search {
                                item.pin.contains_fuzzy(&query, &MATCHER)
                            } else {
                                item.pin.contains(q)
                            }
                        })
                        .map(|item| &item.pin)
                        .collect::<Vec<&Pin>>()
                })
                .unwrap_or_default();
            match r.len() {
                0 => Ok(None),
                _ => Ok(Some(r)),
            }
        } else {
            Err("Tags cache data is invalid".into())
            // Err("vailid".into())
        }
    }

    /// Search tags for `query` (uses cached tags).
    /// Returns all tags that _contain_ query.
    /// This function honors [`pinboard::config::Config`] settings for fuzzy search.
    /// # Errors
    ///
    /// If cached data is not readable an error is returned.
    pub fn search_list_of_tags(
        &self,
        query: &str,
    ) -> Result<Option<Vec<&Tag>>, Box<dyn std::error::Error>> {
        debug!("search_list_of_tags: starting.");
        if self.cached_data.cache_ok() {
            let query = if is_nfkd_quick(query.chars()) == IsNormalized::Yes {
                query.into()
            } else {
                query.chars().nfkd().collect::<String>()
            };
            let q = &query.to_lowercase();
            let r = self
                .cached_data
                .tags
                .as_ref()
                .map(|t| {
                    t.iter()
                        .filter(|item| {
                            if self.cfg.fuzzy_search {
                                MATCHER.fuzzy_match(&item.tag.0, &query).is_some()
                            } else {
                                item.tag_lowered.contains(q)
                            }
                        })
                        .map(|ct| &ct.tag)
                        .collect::<Vec<&Tag>>()
                })
                .unwrap_or_default();
            match r.len() {
                0 => Ok(None),
                _ => Ok(Some(r)),
            }
        } else {
            Err("Tags cache data is invalid".into())
        }
    }

    // TODO: find_url should return pins that match `q` barring their fragment //
    // https://github.com/sharkdp/hexyl#preview  and
    // https://github.com/sharkdp/hexyl
    // should be considered identical (?!)

    /// Checks whether the cache contains this exact URL, including path and query case.
    #[must_use]
    pub fn has_cached_url(&self, url: &str) -> bool {
        self.cached_data.cache_ok()
            && self
                .cached_data
                .pins
                .iter()
                .flatten()
                .any(|cached| cached.pin.url == url)
    }

    /// Finds all pins whose URLs match `q` ignoring case.
    ///
    /// `find_url("http://google.com/public")` will match
    /// <http://google.com/public>
    /// but not
    /// <http://google.com/public#fragment>
    ///
    /// # Errors
    ///
    /// If cached data is not readable an error is returned.
    pub fn find_url<S>(
        &'pin self,
        q: S,
    ) -> Result<Option<Vec<&'pin Pin<'pin>>>, Box<dyn std::error::Error>>
    where
        S: AsRef<str>,
    {
        debug!("find_url: starting.");
        if !self.cached_data.cache_ok() {
            return Err("Cache data is invalid.".into());
        }
        let query = &q.as_ref().to_lowercase();
        let results = self
            .cached_data
            .pins
            .as_ref()
            .map(|p: &Vec<CachedPin<'pin>>| {
                p.iter()
                    .filter(|cached_pin: &&CachedPin<'pin>| {
                        cached_pin.pin.url.to_lowercase().as_str() == query
                    })
                    .map(|p| &p.pin)
                    .collect::<Vec<&'pin Pin>>()
            })
            .unwrap_or_default();
        match results.len() {
            0 => Ok(None),
            _ => Ok(Some(results)),
        }
    }

    /// Finds all pins with an exact tag of 'query'
    ///
    /// # Errors
    ///
    /// If cached data is not readable an error is returned.
    pub fn find_tag<S>(
        &'pin self,
        query: S,
    ) -> Result<Option<Vec<&'pin Pin<'pin>>>, Box<dyn std::error::Error>>
    where
        S: AsRef<str>,
    {
        debug!("find_tag: starting.");
        if !self.cached_data.cache_ok() {
            return Err("Cache data is invalid.".into());
        }

        let query = if is_nfkd_quick(query.as_ref().chars()) == IsNormalized::Yes {
            query.as_ref().to_lowercase()
        } else {
            query
                .as_ref()
                .chars()
                .nfkd()
                .collect::<String>()
                .to_lowercase()
        };
        let results = self
            .cached_data
            .pins
            .as_ref()
            .map(|p: &Vec<CachedPin<'pin>>| {
                p.iter()
                    .filter(|cached_pin: &&CachedPin<'pin>| cached_pin.tag_list.contains(&query))
                    .map(|p| &p.pin)
                    .collect::<Vec<&'pin Pin>>()
            })
            .unwrap_or_default();
        match results.len() {
            0 => Ok(None),
            _ => Ok(Some(results)),
        }
    }

    /// Searches the selected `fields` within bookmarks to filter them.
    /// It will return bookmarks that have ALL of search queries provided in 'q' somewhere in the
    /// specified 'fields' of the bookmark.
    /// When searching tags, bookmarks matching more query terms as whole tags come first.
    /// Bookmarks within each group keep their newest-first order.
    /// This function honors [`pinboard::config::Config`] settings for fuzzy search only.
    /// # Errors
    /// If cached data is not readable an error will be returned.
    pub fn search<'b, I, S>(
        &'pin self,
        q: &'b I,
        fields: &[SearchType],
    ) -> Result<Option<Vec<&'pin Pin<'pin>>>, Box<dyn std::error::Error>>
    where
        &'b I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.search_ranked(q, fields, |_| usize::MAX)
    }

    /// Search bookmarks, using a caller's rank within each group of matches.
    /// Lower values come first. Equal ranks keep the cache's newest-first order.
    ///
    /// # Errors
    /// Returns an error if cached data cannot be read.
    #[allow(clippy::too_many_lines)]
    pub fn search_ranked<'b, I, S>(
        &'pin self,
        q: &'b I,
        fields: &[SearchType],
        rank: impl Fn(&str) -> usize,
    ) -> Result<Option<Vec<&'pin Pin<'pin>>>, Box<dyn std::error::Error>>
    where
        &'b I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        debug!("search: starting.");
        if !self.cached_data.cache_ok() {
            return Err("Cache data is invalid.".into());
        }
        // When no field is specified, search everywhere
        let all_fields = vec![
            SearchType::TitleOnly,
            SearchType::TagOnly,
            SearchType::UrlOnly,
            SearchType::DescriptionOnly,
        ];
        let search_fields = if fields.is_empty() {
            all_fields.as_slice()
        } else {
            fields
        };

        // Apply Unicode normalization to user-input search query using 'K'ompatibility and
        // 'D'ecomposition options (nfkd). Alfred seems to use the same.
        let mut normalized_queries = q
            .into_iter()
            .map(|s| s.as_ref().chars().nfkd().collect::<String>().to_lowercase())
            .collect::<Vec<String>>();
        normalized_queries.sort_unstable();
        normalized_queries.dedup();
        #[allow(clippy::if_not_else)]
        let mut results = if !self.cfg.fuzzy_search {
            self.cached_data
                .pins
                .as_ref()
                .map(|p: &Vec<CachedPin<'pin>>| {
                    p.iter()
                        .filter(|cached_pin: &&CachedPin<'pin>| {
                            normalized_queries.iter().all(|query| {
                                search_fields.iter().any(|search_type| match *search_type {
                                    SearchType::TitleOnly => {
                                        cached_pin.title_lowered.contains(query)
                                    }
                                    SearchType::TagOnly => {
                                        cached_pin.tag_list.iter().any(|tag| tag.contains(query))
                                    }
                                    SearchType::UrlOnly => {
                                        cached_pin.pin.url.as_ref().to_lowercase().contains(query)
                                    }
                                    SearchType::DescriptionOnly => {
                                        if let Some(ref extended) = cached_pin.extended_lowered {
                                            extended.contains(query)
                                        } else {
                                            false
                                        }
                                    }
                                    SearchType::TagTitleOnly => {
                                        cached_pin.title_lowered.contains(query)
                                            || cached_pin.tag_list.contains(query)
                                    }
                                })
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            self.cached_data
                .pins
                .as_ref()
                .map(|p| {
                    p.iter()
                        .filter(|cached_pin: &&CachedPin| {
                            normalized_queries.iter().all(|qi| {
                                search_fields.iter().any(|search_type| match *search_type {
                                    SearchType::TitleOnly => MATCHER
                                        .fuzzy_match(&cached_pin.title_lowered, qi.as_ref())
                                        .is_some(),
                                    SearchType::TagOnly => cached_pin
                                        .tag_list
                                        .iter()
                                        .any(|t| MATCHER.fuzzy_match(t, qi.as_ref()).is_some()),
                                    SearchType::UrlOnly => MATCHER
                                        .fuzzy_match(cached_pin.pin.url.as_ref(), qi.as_ref())
                                        .is_some(),
                                    SearchType::DescriptionOnly => {
                                        if let Some(ref extended) = cached_pin.extended_lowered {
                                            MATCHER
                                                .fuzzy_match(extended.as_str(), qi.as_ref())
                                                .is_some()
                                        } else {
                                            false
                                        }
                                    }
                                    SearchType::TagTitleOnly => {
                                        MATCHER
                                            .fuzzy_match(&cached_pin.title_lowered, qi.as_ref())
                                            .is_some()
                                            || cached_pin.tag_list.iter().any(|t| {
                                                MATCHER.fuzzy_match(t, qi.as_ref()).is_some()
                                            })
                                    }
                                })
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };

        let search_tags = search_fields
            .iter()
            .any(|field| matches!(field, SearchType::TagOnly | SearchType::TagTitleOnly));
        results.sort_by_cached_key(|pin| {
            let exact_tags = if search_tags {
                normalized_queries
                    .iter()
                    .filter(|query| pin.tag_list.contains(query))
                    .count()
            } else {
                0
            };
            (std::cmp::Reverse(exact_tags), rank(pin.pin.url.as_ref()))
        });

        match results.len() {
            0 => Ok(None),
            _ => Ok(Some(results.into_iter().map(|pin| &pin.pin).collect())),
        }
    }
}
