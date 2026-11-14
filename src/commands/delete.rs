/// Providing this command with a URL will try to remove the related bookmark from Pinboard.
/// If no URL is provided, this command will fetch browser's tab info and show and Alfred item that
/// can be used for deletion in next step.
///
use super::{browser_info, io, Runner, SubCommand};
use crate::AlfredError;
use alfred::ItemBuilder;
use std::io::Write;

// TODO: right now we accept deleting tag & url at the same time. If user asks to delete a tag
// only, this function will automatically grab browser's url and return an Alfred item containing
// it while deleting the given tag as well. I believe these two options should be made exclusively
// mutual.
impl Runner<'_, '_> {
    pub fn delete(&mut self, cmd: SubCommand) {
        debug!("Starting in delete");
        let SubCommand::Delete {
            url,
            tag,
            expected_account,
            passive_preview,
            ..
        } = cmd
        else {
            unreachable!()
        };
        match self.perform_delete(url, tag, expected_account.as_deref(), passive_preview) {
            Ok(s) if !s.is_empty() => {
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
            Ok(_) => (),
        }
    }

    fn perform_delete(
        &mut self,
        url: Option<String>,
        tag: Option<String>,
        expected_account: Option<&str>,
        passive_preview: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let account = self.config.as_ref().unwrap().pinboard_username()?;
        if expected_account.is_some_and(|selected| selected != account) {
            return Err(AlfredError::DeleteFailed(
                "The Pinboard account changed. Select the bookmark again.".into(),
            )
            .into());
        }
        if let Some(tag) = tag {
            let _mutation = self.lock_bookmark_mutations()?;
            debug!("  tag: {}", tag);
            self.pinboard
                .as_ref()
                .unwrap()
                .delete_tag(tag)
                .map_err(|e| {
                    error!("{}", crate::redact_token(&e.to_string()));
                    AlfredError::DeleteFailed("couldn't delete tag".to_string())
                })?;
            return Ok("Successfully deleted tag.".to_string());
        }
        if let Some(url) = url {
            let _mutation = self.lock_bookmark_mutations()?;
            debug!("  url: {}", url);
            self.pinboard.as_ref().unwrap().delete(&url).map_err(|e| {
                error!("{}", crate::redact_token(&e.to_string()));
                AlfredError::DeleteFailed("couldn't delete url".to_string())
            })?;
            Ok("Successfully deleted bookmark.".to_string())
        } else {
            self.preview(account, passive_preview);
            Ok(String::new())
        }
    }

    fn preview(&self, account: &str, passive: bool) {
        use browser_info::BrowserLookup;

        let result = if passive {
            browser_info::get_passive()
        } else {
            browser_info::get().map(BrowserLookup::Tab)
        };
        let item = match result {
            Ok(BrowserLookup::Tab(tab)) => ItemBuilder::new(tab.title.clone())
                .subtitle(tab.url.clone())
                .arg(tab.url.clone())
                .quicklook_url(tab.url.clone())
                .text_large_type(tab.title)
                .text_copy(tab.url)
                .variable("pinion_delete_account", account)
                .variable("pinion_delete_direct", "0")
                .icon_path("bookmark-delete.png"),
            Ok(BrowserLookup::NeedsInteraction(_)) => {
                ItemBuilder::new("Can't preview deletion from this browser")
                    .subtitle("Find the bookmark with ps, then choose Delete Pinboard Bookmark.")
                    .icon_path("erroricon.icns")
                    .valid(false)
            }
            Err(error) => {
                error!("Couldn't get browser info: {:?}", error);
                ItemBuilder::new("Couldn't get browser's info!")
                    .subtitle(error.to_string())
                    .icon_path("erroricon.icns")
                    .valid(false)
            }
        };
        self.write_output_items([item.into_item()], None::<[(&str, &str); 0]>);
    }
}
