use super::{io, process, Runner, SubCommand};
use chrono::Utc;
use rusty_pin::Pin;
use std::env;
use std::io::Write;
use std::time::Duration;

const EDIT_LIFETIME_SECONDS: i64 = 600;
const TITLE_PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);
const REOPEN_EDITOR: &str = "Choose Rename Pinboard Title again";

enum BookmarkLookup {
    Preview,
    BeforeSave,
}

#[derive(Deserialize, Serialize)]
struct EditSelection {
    url: String,
    title: String,
    expected_title: String,
    expected_account: String,
    started_at: i64,
}

fn session_is_active(started_at: i64) -> bool {
    Utc::now()
        .timestamp()
        .checked_sub(started_at)
        .is_some_and(|age| (0..=EDIT_LIFETIME_SECONDS).contains(&age))
}

impl Runner<'_, '_> {
    pub fn retitle(&mut self, cmd: &SubCommand) {
        let SubCommand::Retitle {
            url,
            editor_context,
            expected_account,
            expected_title,
            title,
            ..
        } = cmd
        else {
            unreachable!()
        };
        if *editor_context {
            if let Err(error) = self
                .check_expected_account(expected_account.as_deref())
                .and_then(|()| self.open_title_editor(url))
            {
                retitle_editor_error(&error);
            }
            return;
        }

        let title = title.join(" ");
        let title = title.trim();
        if title.is_empty() {
            match self
                .check_expected_account(expected_account.as_deref())
                .and_then(|()| self.lookup_bookmark(url, BookmarkLookup::Preview))
            {
                Ok(bookmark) => retitle_output(&bookmark.title),
                Err(error) => {
                    error!("{}", crate::redact_token(&error));
                    process::exit(1);
                }
            }
        } else {
            self.report_save(
                url,
                title,
                expected_account.as_deref(),
                expected_title.as_deref(),
                true,
            );
        }
    }

    pub(crate) fn retitle_selection(&mut self, selection: &str) {
        // The payload may contain escaped characters, including quotes and newlines.
        let Some(edit) = serde_json::from_str::<EditSelection>(selection)
            .ok()
            .filter(EditSelection::is_valid)
        else {
            retitle_output(&format!(
                "Error: Rename expired or invalid. {REOPEN_EDITOR}."
            ));
            return;
        };
        self.report_save(
            &edit.url,
            edit.title.trim(),
            Some(&edit.expected_account),
            Some(&edit.expected_title),
            false,
        );
    }

    fn open_title_editor(&self, url: &str) -> Result<(), String> {
        let account = self
            .config
            .as_ref()
            .unwrap()
            .pinboard_username()
            .map_err(|error| error.to_string())?;
        let bookmark = self.lookup_bookmark(url, BookmarkLookup::Preview)?;
        write_editor_context(url, &bookmark.title, account, "");
        Ok(())
    }

    fn lookup_bookmark(&self, url: &str, lookup: BookmarkLookup) -> Result<Pin<'_>, String> {
        if url.trim().is_empty() {
            return Err("Need a URL.".into());
        }
        let pinboard = self.pinboard.as_ref().unwrap();
        let bookmark = match lookup {
            BookmarkLookup::Preview => pinboard.preview_remote_url(url, TITLE_PREVIEW_TIMEOUT),
            BookmarkLookup::BeforeSave => pinboard.find_remote_url(url),
        };
        bookmark
            .map_err(|error| format!("Couldn't look up the bookmark: {error}"))?
            .ok_or_else(|| "That bookmark no longer exists on Pinboard.".to_owned())
    }

    fn check_expected_account(&self, expected_account: Option<&str>) -> Result<(), String> {
        let config = self.config.as_ref().unwrap();
        if expected_account
            .is_some_and(|expected| config.pinboard_username().ok() != Some(expected))
        {
            return Err(format!("The Pinboard account changed. {REOPEN_EDITOR}."));
        }
        Ok(())
    }

    fn report_save(
        &self,
        url: &str,
        title: &str,
        expected_account: Option<&str>,
        expected_title: Option<&str>,
        exit_on_error: bool,
    ) {
        match self.save_title(url, title, expected_account, expected_title) {
            Ok(true) => {
                retitle_output(&format!("Renamed to: {title}"));
                if self.config.as_ref().unwrap().auto_update_cache {
                    self.refresh_after_write();
                }
            }
            Ok(false) => retitle_output("Title unchanged."),
            Err(error) => {
                retitle_output(&format!("Error: {}", crate::redact_token(&error)));
                if exit_on_error {
                    process::exit(1);
                }
            }
        }
    }

    fn save_title(
        &self,
        url: &str,
        title: &str,
        expected_account: Option<&str>,
        expected_title: Option<&str>,
    ) -> Result<bool, String> {
        self.check_expected_account(expected_account)?;
        // Pinboard replaces the whole record. Keep this lookup and replacement
        // together so another local Pinion command cannot edit it in between.
        let _mutation = self
            .lock_bookmark_mutations()
            .map_err(|error| format!("Couldn't lock the bookmark for editing: {error}"))?;
        let mut bookmark = self.lookup_bookmark(url, BookmarkLookup::BeforeSave)?;
        if title == bookmark.title {
            return Ok(false);
        }
        if expected_title.is_some_and(|expected| expected != bookmark.title) {
            return Err(format!("The title changed on Pinboard. {REOPEN_EDITOR}."));
        }
        bookmark.title = title.to_owned().into();
        self.pinboard
            .as_ref()
            .unwrap()
            .replace_pin(bookmark)
            .map_err(|error| format!("Couldn't rename the bookmark: {error}"))?;
        Ok(true)
    }
}

impl EditSelection {
    fn is_valid(&self) -> bool {
        session_is_active(self.started_at)
            && !self.url.trim().is_empty()
            && !self.title.trim().is_empty()
            && !self.expected_account.is_empty()
            && ![
                &self.url,
                &self.title,
                &self.expected_title,
                &self.expected_account,
            ]
            .iter()
            .any(|field| field.contains('\0'))
    }
}

pub(crate) fn retitle_filter(title: &str) {
    let error = env::var("retitle_error").unwrap_or_default();
    if !error.is_empty() {
        retitle_filter_error(&error);
        return;
    }
    let url = env::var("retitle_url").unwrap_or_default();
    let current = env::var("retitle_current_title").unwrap_or_default();
    let account = env::var("retitle_account").unwrap_or_default();
    let started_at = env::var("retitle_started_at")
        .ok()
        .and_then(|value| value.parse::<i64>().ok());
    let title = title.trim();

    if url.is_empty() || account.is_empty() {
        retitle_filter_error(REOPEN_EDITOR);
        return;
    }
    let Some(started_at) = started_at.filter(|&start| session_is_active(start)) else {
        write_editor_item("Rename expired", REOPEN_EDITOR, None);
        return;
    };
    if title.is_empty() {
        write_editor_item(
            "Type the new title…",
            &format!("Current title: {current}"),
            None,
        );
        return;
    }
    if title == current.trim() {
        write_editor_item(title, "Unchanged — edit the text to rename it", None);
        return;
    }
    let host = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .filter(|host| !host.is_empty())
        .unwrap_or(&url);
    let subtitle = format!("Replaces “{current}” on {host}");
    let selection = EditSelection {
        url,
        title: title.to_owned(),
        expected_title: current,
        expected_account: account,
        started_at,
    };
    write_editor_item(title, &subtitle, Some(&selection));
}

pub(crate) fn retitle_editor_error(message: &str) {
    write_editor_context("", "", "", &crate::redact_token(message));
}

fn write_editor_context(url: &str, title: &str, account: &str, error: &str) {
    let context = serde_json::json!({"alfredworkflow": {
        "arg": title,
        "variables": {
            "retitle_url": url,
            "retitle_current_title": title,
            "retitle_account": account,
            "retitle_started_at": Utc::now().timestamp().to_string(),
            "retitle_error": error,
        }
    }});
    println!("{context}");
}

pub(crate) fn retitle_filter_error(message: &str) {
    write_editor_item(
        "Couldn't look up the bookmark on Pinboard",
        &crate::redact_token(message),
        None,
    );
}

fn write_editor_item(title: &str, subtitle: &str, selection: Option<&EditSelection>) {
    let mut item = serde_json::json!({
        "title": title,
        "subtitle": subtitle,
        "valid": selection.is_some(),
        "icon": {"path": "bookmarks.png"},
    });
    if let Some(selection) = selection {
        let argument = serde_json::to_string(selection).expect("Title selection is serializable");
        item["arg"] = argument.into();
    }
    serde_json::to_writer(io::stdout(), &serde_json::json!({"items": [item]}))
        .expect("Couldn't write items to Alfred");
}

fn retitle_output(message: &str) {
    io::stdout()
        .write_all(message.as_bytes())
        .expect("Couldn't write to stdout");
}
