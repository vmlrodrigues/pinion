use super::{browser_info, env, Config, Runner, SubCommand, Tag};
use crate::cli::Opt;
use chrono::{DateTime, Utc};

use alfred::{Item, ItemBuilder, Modifier};

mod suggestions;
pub(crate) use suggestions::suggest_post_tags;
use suggestions::{
    authentication_generation, poll_suggestions, Suggestions, SUGGESTION_RETRY_DELAY,
};

const POST_PAGE_VARIABLE: &str = "pinion_post_page";

#[derive(Deserialize, Serialize)]
struct PostPage {
    account: String,
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fallback_browser: Option<String>,
    suggested_tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suggestion_retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suggestion_job: Option<String>,
    #[serde(default)]
    suggestion_authentication_failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suggestion_authentication_generation: Option<DateTime<Utc>>,
}

impl PostPage {
    fn rerun_seconds(&self, suggest: bool) -> Option<u8> {
        if self.url.is_none() && self.fallback_browser.is_none() {
            return Some(5);
        }
        if !suggest || self.suggested_tags.is_some() || self.url.is_none() {
            return None;
        }
        // Round up to the deadline, limited by Alfred's maximum rerun interval.
        let seconds = self
            .suggestion_retry_at
            .and_then(|retry_at| (retry_at - Utc::now()).to_std().ok())
            .map_or(1, |remaining| {
                remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
            });
        Some(u8::try_from(seconds).unwrap_or(5).clamp(1, 5))
    }
}

impl Runner<'_, '_> {
    pub fn list(&mut self, opt: Opt, query_started_at: DateTime<Utc>) {
        let cmd = opt.cmd;
        let query_item = opt.query_as_item;
        match cmd {
            SubCommand::List {
                tags,
                suggest,
                no_existing_page,
                query,
            } => {
                let config = self.config.as_ref().unwrap();
                let show_bookmarked = !no_existing_page && config.page_is_bookmarked;
                let suggest = suggest.unwrap_or(config.suggest_tags);
                let post_filter = env::var_os("pinion_post_filter").is_some();
                let notes = query.as_deref().is_some_and(|query| query.contains(';'));
                let page = if tags && (post_filter || !notes && (show_bookmarked || suggest)) {
                    // Fetch optional suggestions before starting the background check.
                    post_page(config, suggest && !notes)
                } else {
                    None
                };
                let page_rerun = page
                    .as_ref()
                    .filter(|page| config.pinboard_username().ok() == Some(&page.account))
                    .and_then(|page| page.rerun_seconds(suggest && !notes));
                if self.pinboard.as_ref().unwrap().cache_is_available() {
                    self.schedule_sync(query_started_at);
                }
                if let Some(seconds) = page_rerun {
                    self.rerun_seconds =
                        Some(self.rerun_seconds.map_or(seconds, |sync| sync.min(seconds)));
                }
                self.process(tags, show_bookmarked, query_item, query, page.as_ref());
            }
            _ => unreachable!(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process(
        &self,
        tags: bool,
        show_bookmarked: bool,
        query_item: bool,
        q: Option<String>,
        page: Option<&PostPage>,
    ) {
        debug!("Starting in list::process");
        let config = self.config.as_ref().unwrap();
        let pinboard = self.pinboard.as_ref().unwrap();

        let private_pin = (!config.private_new_pin).to_string();
        let toread_pin = config.toread_new_pin.to_string();
        let option_subtitle = if config.private_new_pin {
            "Post the bookmark as PUBLIC."
        } else {
            "Post the bookmark as private."
        };
        let control_subtitle = if config.toread_new_pin {
            "Mark the bookmark as NOT toread."
        } else {
            "Mark the bookmark as TOREAD."
        };

        if tags {
            // Search the tags using the last 'word' in 'q'
            let queries = q.unwrap_or_default();
            let account = config.pinboard_username().unwrap_or_default();
            let post_filter = env::var_os("pinion_post_filter").is_some();
            if let Some(page) = page.filter(|page| post_filter && page.account != account) {
                let status = ItemBuilder::new("The Pinboard account changed")
                    .subtitle("Open `p` again to save to the current account.")
                    .icon_path("erroricon.icns")
                    .valid(false)
                    .into_item();
                let page = serde_json::to_string(page).unwrap_or_default();
                self.write_output_items([status], Some([(POST_PAGE_VARIABLE, page.as_str())]));
                return;
            }
            let rename_source = env::var_os("pinion_rename_source").is_some();
            let rename_context = env::var_os("pinion_rename_editor").is_some().then(|| {
                (
                    env::var("oldtag").unwrap_or_default(),
                    env::var("pinion_rename_account").unwrap_or_default(),
                )
            });
            let page_url = page
                .and_then(|page| page.url.as_deref())
                .unwrap_or_default();
            let fallback_browser = page
                .and_then(|page| page.fallback_browser.as_deref())
                .unwrap_or_default();
            let page_json = page.and_then(|page| serde_json::to_string(page).ok());
            let mut variables = Vec::new();
            if let Some(page) = page_json.as_deref() {
                variables.push((POST_PAGE_VARIABLE, page));
            }
            if let Some((tag, account)) = rename_context.as_ref() {
                variables.extend([
                    ("oldtag", tag.as_str()),
                    ("pinion_rename_account", account.as_str()),
                ]);
            }
            let bind_selection = |item: &mut ItemBuilder<'_>| {
                if post_filter {
                    item.set_variable("pinion_post_account", account.to_owned());
                    item.set_variable("pinion_post_url", page_url.to_owned());
                    item.set_variable("pinion_post_browser", fallback_browser.to_owned());
                    if !fallback_browser.is_empty() {
                        item.set_subtitle("Bookmark the active browser tab on Return.");
                        item.set_subtitle_mod(
                            Modifier::Option,
                            format!("{option_subtitle} Uses the active tab on Return."),
                        );
                        item.set_subtitle_mod(
                            Modifier::Control,
                            format!("{control_subtitle} Uses the active tab on Return."),
                        );
                    } else if page_url.is_empty() {
                        item.set_valid(false);
                        item.set_subtitle("Couldn't identify the browser page. Retrying…");
                    }
                } else if rename_source {
                    item.set_variable("pinion_rename_account", account.to_owned());
                }
            };

            let bookmark_present = (show_bookmarked
                && page
                    .and_then(|page| page.url.as_deref())
                    .is_some_and(|url| pinboard.has_cached_url(url)))
            .then(|| {
                ItemBuilder::new("You already have the bookmark!")
                    .icon_path("bookmark-delete.png")
                    .valid(false)
                    .into_item()
            });

            // Check if user has entered ';' which indicates they are providing a description.
            // So no need to search for tags!
            if !query_item && queries.contains(';') {
                let trim_chars: &[_] = &['\t', ' ', '\\', '\n'];
                let pin_info = queries
                    .splitn(2, ';')
                    .map(|s| s.trim_matches(trim_chars))
                    .collect::<Vec<&str>>();
                debug!("  pin_info: {:?}", pin_info);
                let title = if post_filter && pin_info[0].is_empty() {
                    "Bookmark the page without tags"
                } else {
                    "Hit Return to bookmark the page!"
                };
                let mut item = ItemBuilder::new(title)
                    .icon_path("upload.png")
                    .arg(queries.as_str())
                    .variable("shared", private_pin)
                    .variable("toread", toread_pin)
                    .variable("tags", pin_info[0])
                    .subtitle_mod(Modifier::Option, option_subtitle)
                    .subtitle_mod(Modifier::Control, control_subtitle);
                if !pin_info[1].is_empty() {
                    item = item.variable("description", pin_info[1]);
                }
                bind_selection(&mut item);
                let items = std::iter::once(item.into_item()).chain(bookmark_present);
                self.write_output_items(items, Some(variables));
                return;
            }

            let query_words: Vec<&str> = queries.split_whitespace().collect();
            let popular_tags = page
                .and_then(|page| page.suggested_tags.as_deref())
                .unwrap_or_default()
                .iter()
                .map(|tag| Tag::new(tag.clone(), 0).set_popular())
                .collect::<Vec<_>>();

            let last_query_word_tag;
            let mut alfred_items = Vec::with_capacity(config.tags_to_show as usize + 1);

            let last_query_word = query_words.last().unwrap_or(&"");
            let mut top_items: Vec<Tag> = Vec::with_capacity(2usize);

            let results = if pinboard.cache_is_available() {
                pinboard.search_list_of_tags(last_query_word)
            } else {
                Ok(None)
            };
            match results {
                Err(e) => crate::show_error_alfred(e.to_string()),
                Ok(results) => {
                    let prev_tags = if query_words.len() > 1 {
                        // User has already searched for other tags, we should include those in the
                        // 'autocomplete' field of the AlfredItem.
                        // `query_words` comes from `split_whitespace`, which splits on any Unicode
                        // whitespace, so the separator here is not necessarily an ASCII space —
                        // pasted text often carries U+00A0 or a tab.
                        //
                        // `rfind` reports the *start* byte of that separator, so the slice has to
                        // run to the end of the character. Ending at the start byte lands inside a
                        // multi-byte separator, and `get` then returns None — which silently threw
                        // away every tag the user had already typed.
                        queries
                            .rfind(char::is_whitespace)
                            .and_then(|i| {
                                let sep_len = queries[i..].chars().next().map_or(1, char::len_utf8);
                                queries.get(0..i + sep_len)
                            })
                            .unwrap_or("")
                    } else {
                        ""
                    };

                    // No result means, we couldn't find a tag using the given query
                    // Some result mean we found tags even though the query was empty as
                    // search_list_of_tags returns all tags for empty queries.
                    last_query_word_tag = Tag::new((*last_query_word).to_string(), 0).set_new();
                    let mut filter_idx = None;
                    #[allow(clippy::single_match_else)]
                    let items = match results {
                        Some(i) => {
                            debug!("Found {} tags.", i.len());
                            top_items.insert(0, i[0].clone());
                            // If user input matches a tag, we will show it as first item
                            if let Some(idx) = i.iter().position(|t| &t.0 == last_query_word) {
                                if idx != 0 {
                                    top_items.insert(0, i[idx].clone());
                                    filter_idx = Some(idx - 1); // Substract one since we will be
                                                                // always skipping the firt tag
                                                                // from results (i)
                                    debug!("Found exact tag: {:?}, {}", i[idx], idx);
                                }
                            } else if !last_query_word.is_empty() {
                                // Otherwise we will show the tag with highest frequency matching user
                                // input before popular/suggested tags, unless --query-as-item is set
                                // in which case we create an alfred item with what user has typed
                                top_items.insert(0, last_query_word_tag);
                                if query_item {
                                    filter_idx = None;
                                }
                            }
                            i
                        }
                        None => {
                            debug!("Didn't find any tag for `{}`", last_query_word);
                            filter_idx = None;
                            // An account with no tags at all returns None for any query,
                            // including an empty one. Offering "" as a new tag would show a
                            // blank row, so only offer one when the user actually typed something.
                            if !last_query_word.is_empty() {
                                top_items.insert(0, last_query_word_tag);
                            }
                            vec![]
                        }
                    };
                    let prev_tags_len = prev_tags.len();
                    alfred_items = top_items // Start with top_items, and
                        .iter()
                        .enumerate()
                        .chain(popular_tags.iter().enumerate()) // then add popular_tags
                        // and finally add all tags returned from search, ensuring to remove
                        // items that are in top_results
                        .chain(
                            items.into_iter().skip(1).enumerate().filter(|&(idx, _)| {
                                filter_idx.is_none() || idx != filter_idx.unwrap()
                            }),
                        )
                        .filter(|&(_, tag)| !tag.0.trim().is_empty())
                        // Remove all tags that are already present in user query list
                        .filter(|&(_, tag)| {
                            if query_words.is_empty() {
                                true
                            } else {
                                let upper = query_words.len() - 1;
                                !query_words.as_slice()[0..upper]
                                    .iter()
                                    .any(|q| q == &tag.0.as_str())
                            }
                        })
                        .take(config.tags_to_show as usize)
                        .map(|(_, tag)| {
                            let mut item_args = String::with_capacity(prev_tags_len + tag.0.len());
                            item_args.push_str(prev_tags);
                            item_args.push_str(&tag.0);
                            let mut item = ItemBuilder::new(tag.0.as_str())
                                .subtitle(tag.1.to_string())
                                .autocomplete(item_args.clone())
                                .subtitle_mod(Modifier::Option, option_subtitle)
                                .subtitle_mod(Modifier::Control, control_subtitle)
                                .variable("tags", item_args.clone())
                                .variable("shared", &private_pin)
                                .variable("toread", &toread_pin)
                                .arg(item_args)
                                .valid(true)
                                .icon_path("tag.png");
                            bind_selection(&mut item);
                            item.into_item()
                        })
                        .collect::<Vec<Item>>();
                }
            }
            debug!("alfred_items hast {} entities", alfred_items.len());
            if !query_words.is_empty() && alfred_items.is_empty() {
                let mut item = ItemBuilder::new("Hit Return to bookmark the page!")
                    .icon_path("upload.png")
                    .arg(queries.as_str())
                    .subtitle("You may have entered duplicate tags!")
                    .variable("shared", &private_pin)
                    .variable("toread", &toread_pin)
                    .variable("tags", queries.as_str())
                    .subtitle_mod(Modifier::Option, option_subtitle)
                    .subtitle_mod(Modifier::Control, control_subtitle);
                bind_selection(&mut item);
                alfred_items = vec![item.into_item()];
            }
            if post_filter && alfred_items.is_empty() && self.cache_wait_item.is_none() {
                alfred_items.push(
                    ItemBuilder::new("Type tags to bookmark this page")
                        .subtitle("Type a semicolon to save without tags or add a note.")
                        .valid(false)
                        .icon_path("tag.png")
                        .into_item(),
                );
            }
            alfred_items.extend(bookmark_present);
            self.write_output_items(alfred_items, Some(variables));
        } else {
            if q.is_some() && !q.unwrap().is_empty() {
                warn!(
                    "Ignoring search query, will spit out {} of bookmarks.",
                    config.pins_to_show
                );
            }
            let items = pinboard
                .list_bookmarks()
                .unwrap_or_default()
                .into_iter()
                .take(config.pins_to_show as usize)
                .map(|pin| {
                    ItemBuilder::new(pin.title.as_ref())
                        .subtitle(pin.url.as_ref())
                        .arg(pin.url.as_ref())
                        .into_item()
                })
                .collect::<Vec<Item>>();
            self.write_output_items(items, None::<[(&str, &str); 0]>);
        }
    }
}

fn post_page(config: &Config, suggest: bool) -> Option<PostPage> {
    let account = config.pinboard_username().ok()?;
    let continuation = env::var("apr_execution_counter")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|counter| counter > 1);
    let mut page = continuation
        .then(|| env::var(POST_PAGE_VARIABLE).ok())
        .flatten()
        .and_then(|value| serde_json::from_str::<PostPage>(&value).ok())
        .unwrap_or_else(|| PostPage {
            account: account.to_owned(),
            url: None,
            fallback_browser: None,
            suggested_tags: None,
            suggestion_retry_at: None,
            suggestion_job: None,
            suggestion_authentication_failed: false,
            suggestion_authentication_generation: None,
        });
    if page.account != account {
        return Some(page);
    }
    if page.url.is_none() && page.fallback_browser.is_none() {
        match browser_info::get_passive() {
            Ok(browser_info::BrowserLookup::Tab(tab)) => page.url = Some(tab.url),
            Ok(browser_info::BrowserLookup::NeedsInteraction(browser)) => {
                page.fallback_browser = Some(browser);
            }
            Err(_) => {}
        }
    }
    if page.suggestion_authentication_failed {
        if authentication_generation(config) == page.suggestion_authentication_generation {
            page.suggestion_retry_at = Some(Utc::now() + SUGGESTION_RETRY_DELAY);
            return Some(page);
        }
        page.suggestion_authentication_failed = false;
        page.suggestion_authentication_generation = None;
        page.suggestion_retry_at = None;
    }
    if !suggest
        || page.suggested_tags.is_some()
        || page
            .suggestion_retry_at
            .is_some_and(|retry_at| retry_at > Utc::now())
    {
        return Some(page);
    }
    if let (Ok(cache_dir), Some(url)) = (config.bookmark_cache_dir(), page.url.as_deref()) {
        page.suggestion_retry_at = None;
        match poll_suggestions(&cache_dir, &page.account, url, &mut page.suggestion_job) {
            Ok(Some(Suggestions::Available(tags))) => page.suggested_tags = Some(tags),
            Ok(Some(Suggestions::Deferred(retry_at))) => page.suggestion_retry_at = retry_at,
            Ok(Some(Suggestions::Authentication(generation))) => {
                page.suggestion_authentication_failed = true;
                page.suggestion_authentication_generation = generation;
                page.suggestion_retry_at = Some(Utc::now() + SUGGESTION_RETRY_DELAY);
            }
            Ok(None) => (), // The detached request is still running.
            Err(error) => {
                warn!(
                    "Fetching popular tags: {}",
                    crate::redact_token(&error.to_string())
                );
                page.suggestion_job = None;
                page.suggestion_retry_at = Some(Utc::now() + SUGGESTION_RETRY_DELAY);
            }
        }
    }
    Some(page)
}
