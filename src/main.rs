// #![cfg_attr(feature = "dev", feature(plugin))]
// #![cfg_attr(feature = "dev", plugin(clippy))]
// #![cfg_attr(
//     feature = "dev",
//     warn(
//         cast_possible_truncation, cast_possible_wrap, cast_precision_loss, cast_sign_loss, mut_mut,
//         non_ascii_literal, result_unwrap_used, shadow_reuse, shadow_same, unicode_not_nfc,
//         wrong_self_convention, wrong_pub_self_convention
//     )
// )]
// #![cfg_attr(feature = "dev", allow(string_extend_chars))]

extern crate chrono;

extern crate dirs;
extern crate semver;
extern crate serde;

#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate env_logger;

#[macro_use]
extern crate log;

extern crate alfred;
extern crate rusty_pin;

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::io;
use std::process;

use clap::Parser;
use rusty_pin::Pinboard;
use thiserror::Error;

mod cli;
mod commands;
mod keychain;
mod workflow_config;

use crate::cli::{Opt, SubCommand};
use crate::workflow_config::Config;

use crate::commands::Runner;
// use commands::{config, delete, list, post, search, update};
use crate::commands::config;

// TODO: Implement Debug for SubCommand in cli.rs so we don't show auth_token in Alfred's debug
//       window.  <09-10-, Hamidme> //
// TODO: Add a command to search pins that have 'toread' enabled. <01-09-21, Hamid> //
// TODO: We need to come up with actual meaningful tests and deploy them to CIs. <23-08-21, Hamid> //
// TODO: parse Alfred preferences and get number of visible items?
// TODO: running ./pinion update from command line panics (starting from
//       fetch_latest_release)
// TODO: Make sure that we don't show any json-like error in macOS's notification (check issue#27)
// TODO: add an option to disable/enable update checks
// TODO: Dont show full JSON errors after alfred's window has closed, just send a notification <01-04-20, hamid>
// TODO: Can we do something about failure of parsing user's bookmarks or when the network times out
// TODO: Try to reduce number of calls to get_browser_info in list.rs <04-04-20, hamid>
// TODO: Separate finding the browser's info into a new separate sud-command so that delete.rs
// does one thing which is deleting and not trying to find the browser's. <07-04-20, hamid>

#[derive(Debug, Error)]
pub enum AlfredError {
    #[error("Corrupted config file. Set API token again!")]
    ConfigFileErr,
    #[error("Missing config file (did you set API token?). Run `pa` to configure Pinboard.")]
    MissingConfigFile,
    #[error("Bad Auth. Token! Set it again with `pa`.")]
    InvalidAuthToken,
    #[error("Cache: {0}")]
    CacheUpdateFailed(String),
    #[error("Post: {0}")]
    Post2PinboardFailed(String),
    #[error("Delete: {0}")]
    DeleteFailed(String),
    #[error("osascript error: {0}")]
    OsascriptError(String),
    #[error("Keychain: {0}")]
    KeychainError(String),
    #[error("Alfred didn't set alfred_workflow_bundleid")]
    MissingBundleId,
    #[error("What did you do?")]
    Other,
}

fn main() {
    /*
         - export alfred_workflow_version=0.11.1
         - export alfred_workflow_data=$HOME/tmp/apr
         - export alfred_workflow_cache=$HOME/tmp/apr/
         - export alfred_workflow_uid=hamid63
         - export alfred_workflow_name="Pinion"
         - export alfred_workflow_bundleid=com.victorrodrigues.pinion
         - export alfred_version=3.6
    */
    // env::set_var("alfred_workflow_data", "/Volumes/Home/hamid/tmp/rust");
    // env::set_var("alfred_workflow_cache", "/Volumes/Home/hamid/tmp/rust");
    // env::set_var("alfred_workflow_uid", "hamid63");
    // env::set_var("alfred_workflow_name", "Pinion");
    // env::set_var("alfred_version", "3.6");
    // env::set_var("RUST_LOG", "rusty_pin=debug,pinion=debug");
    // If user has Alfred's debug panel open, print all debug info
    // by setting RUST_LOG environment variable.
    use env::var_os;
    if alfred::env::is_debug() {
        env::set_var("RUST_LOG", "rusty_pin=debug,alfred_rs=debug,pinion=debug");
    }
    env_logger::init();

    debug!("Parsing input arguments.");
    let opt: Opt = Opt::parse();

    if let SubCommand::VerifyWorkflowRelease { tag, bundle } = &opt.cmd {
        if let Err(error) = commands::verify_workflow_release(tag, bundle) {
            eprintln!("Cannot package release {tag:?}: {error}");
            process::exit(1);
        }
        return;
    }

    debug!("Looking for alfred_workflow_* env. vars");
    let env_flags = (
        var_os("alfred_workflow_version").is_some(),
        var_os("alfred_workflow_data").is_some(),
        var_os("alfred_workflow_cache").is_some(),
        var_os("alfred_workflow_uid").is_some(),
        var_os("alfred_workflow_name").is_some(),
        var_os("alfred_version").is_some(),
    );
    if let (true, true, true, true, true, true) = env_flags {
    } else {
        show_startup_error(
            &opt.cmd,
            "Your workflow is not set up properly. Check alfred_workflow_* env var.",
        );
        return;
    }

    debug!(
        "alfred_workflow_version: {:?}",
        var_os("alfred_workflow_version")
    );
    debug!("alfred_workflow_data: {:?}", var_os("alfred_workflow_data"));
    debug!(
        "alfred_workflow_cache: {:?}",
        var_os("alfred_workflow_cache")
    );
    debug!("alfred_workflow_uid: {:?}", var_os("alfred_workflow_uid"));
    debug!("alfred_workflow_name: {:?}", var_os("alfred_workflow_name"));
    debug!("alfred_version: {:?}", var_os("alfred_version"));

    debug!("Input cli command is {:?}", &opt.cmd);
    if matches!(opt.cmd, SubCommand::CheckWorkflowUpdate) {
        if let Err(error) = commands::check_workflow_update() {
            warn!("Workflow update check: {error}");
        }
        return;
    }
    if let SubCommand::InstallWorkflow { bundle } = &opt.cmd {
        commands::install_workflow(bundle);
        return;
    }
    if let SubCommand::SuggestPostTags { url, account, job } = &opt.cmd {
        if let Err(error) = commands::suggest_post_tags(url, account, job) {
            warn!(
                "Fetching popular tags: {}",
                redact_token(&error.to_string())
            );
        }
        return;
    }
    if let SubCommand::RetitleFilter { title } = &opt.cmd {
        commands::retitle_filter(title);
        return;
    }
    if let SubCommand::RememberBookmark { account, url } = &opt.cmd {
        if let Err(error) = commands::remember_bookmark(account, url) {
            warn!("Couldn't remember recently opened bookmark: {error}");
        }
        return;
    }

    // Separate Config subcommand from rest of cli commands since during config we may run into
    // errors that cannot be fixed.
    if let SubCommand::Config { .. } = opt.cmd {
        config::run(opt.cmd);
    // Otherwise, if user is not configuring, we will abort upon any errors.
    } else {
        let query_started_at = chrono::Utc::now();
        let is_script_filter = var_os("pinion_alfred_filter").is_some()
            && matches!(
                opt.cmd,
                SubCommand::List { .. }
                    | SubCommand::Search {
                        showonlyurl: false,
                        ..
                    }
            );
        let (config, pinboard) = if matches!(opt.cmd, SubCommand::SelfUpdate { .. }) {
            // Software updates only need Alfred's environment and the updater cache.
            (Config::new(), None)
        } else {
            match setup() {
                Ok((config, pinboard)) => (config, Some(pinboard)),
                Err(error) if is_script_filter => {
                    // Software update checks remain available before Pinboard
                    // has been configured or while its cache is unavailable.
                    let runner = Runner {
                        config: Some(Config::new()),
                        pinboard: None,
                        rerun_seconds: None,
                        sync_authentication_failed: false,
                        cache_wait_item: None,
                    };
                    let status = unavailable_cache_item(Some(error.to_string()));
                    runner.write_output_items([status], None::<[(&str, &str); 0]>);
                    return;
                }
                Err(error) => {
                    show_startup_error(&opt.cmd, &error.to_string());
                    return;
                }
            }
        };
        let mut runner = Runner {
            config: Some(config),
            pinboard,
            rerun_seconds: None,
            sync_authentication_failed: false,
            cache_wait_item: None,
        };

        if matches!(opt.cmd, SubCommand::SyncBookmarks) {
            if let Err(error) = runner.sync_cache() {
                warn!("Bookmark sync: {}", redact_token(&error.to_string()));
            }
            return;
        }

        debug!("Workflow Config: {:?}", runner.config);
        if is_script_filter && runner.config.as_ref().unwrap().can_use_json() {
            let status = match runner.ensure_cache_for_filter() {
                Ok(None) => None,
                Ok(Some(wait)) => {
                    runner.rerun_seconds = wait.seconds;
                    Some(unavailable_cache_item(wait.error))
                }
                Err(error) => Some(unavailable_cache_item(Some(error.to_string()))),
            };
            if let Some(status) = status {
                // Posting can use suggestions or typed input, and both tag
                // rename filters can use an explicitly entered name.
                let accepts_input = matches!(opt.cmd, SubCommand::List { tags: true, .. })
                    && (var_os("pinion_post_filter").is_some() || opt.query_as_item);
                if !accepts_input {
                    runner.write_output_items([status], None::<[(&str, &str); 0]>);
                    return;
                }
                runner.cache_wait_item = Some(status);
            }
        } else if matches!(opt.cmd, SubCommand::List { .. } | SubCommand::Search { .. }) {
            if let Err(error) = runner.ensure_cache() {
                show_error_alfred(error.to_string());
                process::exit(1);
            }
        }
        match opt.cmd {
            SubCommand::Update => {
                // Force the download. Asking for `pupdate` explicitly is the only way a
                // user can rebuild a corrupt or truncated cache — deferring to the
                // server's timestamp here would answer "already up-to-date" and leave
                // them stuck.
                runner.update_cache();
            }
            SubCommand::List { .. } => runner.list(opt, query_started_at),
            SubCommand::Search { .. } => {
                runner.schedule_sync(query_started_at);
                runner.search(opt.cmd);
            }
            SubCommand::Post { .. } => {
                runner.post(opt.cmd);
            }
            SubCommand::Delete { .. } => {
                runner.delete(opt.cmd);
            }
            SubCommand::SelfUpdate { .. } => {
                runner.upgrade(&opt.cmd);
            }
            SubCommand::Rename { .. } => {
                runner.rename(&opt.cmd);
            }
            SubCommand::Retitle { .. } => {
                runner.retitle(&opt.cmd);
            }
            SubCommand::RetitleSave { selection } => {
                runner.retitle_selection(&selection);
            }
            // We have already checked for this variant
            SubCommand::Config { .. }
            | SubCommand::SyncBookmarks
            | SubCommand::SuggestPostTags { .. }
            | SubCommand::RetitleFilter { .. }
            | SubCommand::RememberBookmark { .. }
            | SubCommand::VerifyWorkflowRelease { .. }
            | SubCommand::InstallWorkflow { .. }
            | SubCommand::CheckWorkflowUpdate => unreachable!(),
        }
    }
}

fn unavailable_cache_item(error: Option<String>) -> alfred::Item<'static> {
    let item = match error {
        Some(error) => alfred::ItemBuilder::new("Error")
            .subtitle(redact_token(&error))
            .icon_path("erroricon.icns"),
        None => alfred::ItemBuilder::new("Downloading Pinboard bookmarks…"),
    };
    item.valid(false).into_item()
}

fn setup<'a, 'p>() -> Result<(Config, Pinboard<'a, 'p>), Box<dyn std::error::Error>> {
    debug!("Starting in setup");
    let config = Config::setup()?;
    let cache_dir = config.bookmark_cache_dir()?;
    let mut pinboard = Pinboard::open(config.auth_token.clone(), Some(&cache_dir))
        .map_err(|error| AlfredError::CacheUpdateFailed(error.to_string()))?;
    pinboard.enable_request_pacing(cache_dir);
    pinboard.enable_fuzzy_search(config.fuzzy_search);
    pinboard.enable_tag_only_search(config.tag_only_search);
    pinboard.enable_private_new_pin(config.private_new_pin);
    pinboard.enable_toread_new_pin(config.toread_new_pin);

    Ok((config, pinboard))
}

fn write_to_alfred<'a, 'b, I, J>(items: I, supports_json: bool, vars: Option<J>)
where
    I: IntoIterator<Item = alfred::Item<'a>>,
    J: IntoIterator<Item = (&'b str, &'b str)>,
{
    write_alfred_results(items, supports_json, vars, None);
}

fn write_alfred_results<'a, 'b, I, J>(
    items: I,
    supports_json: bool,
    vars: Option<J>,
    rerun_seconds: Option<u8>,
) where
    I: IntoIterator<Item = alfred::Item<'a>>,
    J: IntoIterator<Item = (&'b str, &'b str)>,
{
    let exec_counter = env::var("apr_execution_counter").unwrap_or_else(|_| "1".to_string());
    let output_items = items.into_iter().collect::<Vec<alfred::Item>>();
    let mut variables: HashMap<&str, &str> = HashMap::new();

    variables.insert("apr_execution_counter", exec_counter.as_str());
    if let Some(items) = vars {
        items.into_iter().for_each(|(k, v)| {
            variables.insert(k, v);
        });
    }

    debug!("variables: {:?}", variables);
    // Depending on alfred version use either json or xml output.
    if supports_json {
        let mut output = alfred::json::Builder::with_items(output_items.as_slice())
            .variables(variables)
            .into_json();
        if let Some(items) = output["items"].as_array_mut() {
            for item in items
                .iter_mut()
                .filter(|item| item["variables"]["workflow_update_ready"] == "1")
            {
                // The bookmark filters bind these keys to actions of their own.
                // Serialize combinations here because alfred::Modifier only accepts single keys.
                item["mods"] = serde_json::json!({
                    "alt": { "valid": false },
                    "ctrl": { "valid": false },
                    "cmd": { "valid": false },
                    "alt+ctrl": { "valid": false },
                    "cmd+alt": { "valid": false },
                });
            }
        }
        if let Some(seconds) = rerun_seconds {
            output["rerun"] = serde_json::json!(seconds);
        }
        serde_json::to_writer(io::stdout(), &output).expect("Couldn't write items to Alfred");
    } else {
        alfred::xml::write_items(io::stdout(), &output_items)
            .expect("Couldn't write items to Alfred");
    }
}
/// Remove the Pinboard API token from a string.
///
/// `rusty-pin` sends the token as a URL query parameter and `reqwest` includes
/// the full URL in its error text, so any error may carry the user's
/// credentials. Anything that displays, logs, or persists an error goes through
/// here first.
#[must_use]
pub fn redact_token(s: &str) -> String {
    const KEY: &str = "auth_token=";
    // Everything a percent-encoded Pinboard token can contain. We consume exactly
    // this run and keep whatever follows. Scanning to the next delimiter instead
    // would swallow the rest of the message — a `\n` or `", ` after the token used
    // to take the following words with it.
    fn is_token_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '%' | ':' | '.' | '_' | '~' | '+' | '-')
    }

    // `to_ascii_lowercase` only maps ASCII A-Z, so byte lengths and therefore all
    // indices are preserved and can be used against the original string.
    let haystack = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut at = 0usize;
    while let Some(found) = haystack[at..].find(KEY) {
        let key_end = at + found + KEY.len();
        out.push_str(&s[at..key_end]);
        out.push_str("<redacted>");
        let value_len = s[key_end..]
            .find(|c: char| !is_token_char(c))
            .unwrap_or(s.len() - key_end);
        at = key_end + value_len;
    }
    out.push_str(&s[at..]);
    out
}

fn show_startup_error(cmd: &SubCommand, message: &str) {
    if matches!(
        cmd,
        SubCommand::Retitle {
            editor_context: true,
            ..
        }
    ) {
        commands::retitle_editor_error(message);
        return;
    }
    if matches!(cmd, SubCommand::RetitleFilter { .. }) {
        commands::retitle_filter_error(message);
        return;
    }
    if matches!(
        cmd,
        SubCommand::Rename {
            action_json: true,
            ..
        } | SubCommand::Rename {
            editor_tag: Some(_),
            ..
        }
    ) {
        commands::rename_action_error(message);
        return;
    }
    if matches!(
        cmd,
        SubCommand::Delete {
            action_text: true,
            ..
        } | SubCommand::Post {
            action_text: true,
            ..
        } | SubCommand::Retitle {
            action_text: true,
            ..
        } | SubCommand::RetitleSave { .. }
    ) {
        // Alfred forwards this text to the action's error output.
        print!("Error: {}", redact_token(message));
        return;
    }
    if matches!(
        cmd,
        SubCommand::Delete {
            passive_preview: true,
            ..
        }
    ) {
        let item = unavailable_cache_item(Some(message.to_owned()));
        alfred::json::write_items(io::stdout(), &[item]).expect("Can't write to stdout");
        return;
    }
    if matches!(cmd, SubCommand::Update) {
        // The cache rebuild is an Alfred action that expects a plain text result.
        println!("Error: {}", redact_token(message));
        process::exit(1);
    }
    show_error_alfred(message);
}

fn show_error_alfred<'a, T: Into<Cow<'a, str>>>(s: T) {
    debug!("Starting in show_error_alfred");
    let item = alfred::ItemBuilder::new("Error")
        .subtitle(redact_token(&s.into()))
        .icon_path("erroricon.icns")
        .into_item();
    alfred::json::write_items(io::stdout(), &[item]).expect("Can't write to stdout");
    std::process::exit(1);
}

fn alfred_error_item<'a, T: Into<Cow<'a, str>>>(s: T) -> alfred::Item<'a> {
    debug!("Starting in alfred_error");
    alfred::ItemBuilder::new("Error")
        .subtitle(redact_token(&s.into()))
        .icon_path("erroricon.icns")
        .into_item()
}

#[cfg(test)]
mod tests {
    use super::redact_token;

    #[test]
    fn redacts_token_followed_by_another_parameter() {
        assert_eq!(
            redact_token("https://api.pinboard.in/v1/posts/all?auth_token=user:ABC123&format=json"),
            "https://api.pinboard.in/v1/posts/all?auth_token=<redacted>&format=json"
        );
    }

    #[test]
    fn redacts_token_at_end_of_string() {
        assert_eq!(
            redact_token("failed: https://api.pinboard.in/v1/tags/get?auth_token=user:ABC123"),
            "failed: https://api.pinboard.in/v1/tags/get?auth_token=<redacted>"
        );
    }

    /// reqwest renders errors as `... for url (<url>)`, so the token is closed
    /// by a paren rather than by `&` or end-of-string.
    #[test]
    fn redacts_token_closed_by_paren() {
        assert_eq!(
            redact_token("error sending request for url (https://api.pinboard.in/v1/posts/suggest?auth_token=user:ABC123)"),
            "error sending request for url (https://api.pinboard.in/v1/posts/suggest?auth_token=<redacted>)"
        );
    }

    #[test]
    fn redacts_every_occurrence() {
        assert_eq!(
            redact_token("auth_token=a:1 and auth_token=b:2"),
            "auth_token=<redacted> and auth_token=<redacted>"
        );
    }

    #[test]
    fn leaves_unrelated_text_alone() {
        assert_eq!(
            redact_token("Cannot get browser's URL"),
            "Cannot get browser's URL"
        );
    }

    /// Redaction must remove the token and nothing else. An earlier version
    /// scanned to the next `&`, `)` or space and deleted everything in between,
    /// which quietly ate whole words of the error message.
    #[test]
    fn keeps_the_rest_of_the_message() {
        assert_eq!(
            redact_token("failed:\nURL?auth_token=user:SECRET\nretrying later"),
            "failed:\nURL?auth_token=<redacted>\nretrying later"
        );
        assert_eq!(
            redact_token("https://x?auth_token=user:SECRET#frag more text"),
            "https://x?auth_token=<redacted>#frag more text"
        );
        assert_eq!(
            redact_token(r#"Error { url: "?auth_token=user:SECRET", source: TimedOut }"#),
            r#"Error { url: "?auth_token=<redacted>", source: TimedOut }"#
        );
    }

    #[test]
    fn redacts_regardless_of_key_case() {
        assert_eq!(
            redact_token("?AUTH_TOKEN=user:SECRET&x=1"),
            "?AUTH_TOKEN=<redacted>&x=1"
        );
    }

    #[test]
    fn handles_multibyte_text_around_the_token() {
        assert_eq!(
            redact_token("café ?auth_token=user:SECRET — naïve"),
            "café ?auth_token=<redacted> — naïve"
        );
    }
}
