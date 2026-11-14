use super::{io, PinBuilder, Runner, SubCommand};
use crate::AlfredError;
use std::io::Write;

use super::browser_info;

impl Runner<'_, '_> {
    pub fn post(&mut self, cmd: SubCommand) {
        match self.perform_post(cmd) {
            Ok(s) => {
                io::stdout()
                    .write_all(s.as_bytes())
                    .expect("Couldn't write to stdout");
                if self.config.as_ref().unwrap().auto_update_cache {
                    self.refresh_after_write();
                }
            }
            Err(e) => {
                let msg = ["Error: ", crate::redact_token(&e.to_string()).as_str()].concat();
                io::stdout()
                    .write_all(msg.as_bytes())
                    .expect("Couldn't write to stdout");
            }
        }
    }

    fn perform_post(&mut self, cmd: SubCommand) -> Result<String, Box<dyn std::error::Error>> {
        debug!("Starting in perform_post");
        let SubCommand::Post {
            tags,
            description,
            shared,
            toread,
            expected_url,
            expected_account,
            fallback_browser,
            ..
        } = cmd
        else {
            unreachable!()
        };
        let config = self.config.as_ref().unwrap();
        let account = config.pinboard_username()?;
        if expected_account.is_some_and(|selected| selected != account) {
            return Err(AlfredError::Post2PinboardFailed(
                "The Pinboard account changed. Open `p` again.".into(),
            )
            .into());
        }
        let fallback_browser = fallback_browser
            .as_deref()
            .filter(|browser| !browser.is_empty());
        let lookup = match (expected_url.as_deref(), fallback_browser) {
            (Some(""), Some(browser)) => browser_info::get_for_browser(browser),
            (Some(""), None) => {
                return Err(AlfredError::Post2PinboardFailed(
                    "Couldn't identify the browser page. Open `p` again.".into(),
                )
                .into());
            }
            (Some(_), _) => browser_info::get_passive_tab(),
            (None, _) => browser_info::get(),
        };
        let browser_tab_info = lookup.map_err(|e| {
            error!("{}", crate::redact_token(&e.to_string()));
            AlfredError::Post2PinboardFailed("cannot get browser's info".to_string())
        })?;
        if expected_url
            .is_some_and(|selected| !selected.is_empty() && selected != browser_tab_info.url)
        {
            return Err(AlfredError::Post2PinboardFailed(
                "The browser page changed. Open `p` again.".into(),
            )
            .into());
        }
        let shared = shared.unwrap_or(!config.private_new_pin);
        let toread = toread.unwrap_or(config.toread_new_pin);
        let mut pin_builder = PinBuilder::new(&browser_tab_info.url, &browser_tab_info.title)
            .tags(tags.join(" "))
            .shared(if shared { "yes" } else { "no" })
            .toread(if toread { "yes" } else { "no" });
        if let Some(desc) = description {
            pin_builder = pin_builder.description(desc);
        }

        let _mutation = self.lock_bookmark_mutations()?;
        self.pinboard
            .as_mut()
            .unwrap()
            .add_pin(pin_builder.into_pin())
            .map_err(|e| {
                error!("{}", crate::redact_token(&e.to_string()));
                AlfredError::Post2PinboardFailed("Could not post to Pinboard.".to_string())
            })?;
        Ok(format!("Successfully posted: {}\n", browser_tab_info.title))
    }
}
