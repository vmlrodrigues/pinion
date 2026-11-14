use std::fs::{File, OpenOptions};
use std::io;
use std::{env, process};

use crate::cli::SubCommand;
use crate::workflow_config::Config;

use rusty_pin::{PinBuilder, Pinboard, Tag};

pub mod config;
mod delete;
mod list;
mod post;
mod rename;
mod retitle;
mod search;
mod update;
mod upgrade;

mod bookmark_history;
mod browser_info;

pub(crate) use bookmark_history::remember_bookmark;
pub(crate) use list::suggest_post_tags;
pub(crate) use rename::action_error as rename_action_error;
pub(crate) use retitle::{retitle_editor_error, retitle_filter, retitle_filter_error};
pub(crate) use upgrade::{check_workflow_update, install_workflow, verify_workflow_release};

pub(super) struct Runner<'api, 'pin> {
    pub config: Option<Config>,
    pub pinboard: Option<Pinboard<'api, 'pin>>,
    pub rerun_seconds: Option<u8>,
    pub sync_authentication_failed: bool,
    pub cache_wait_item: Option<alfred::Item<'static>>,
}

impl Runner<'_, '_> {
    fn lock_bookmark_mutations(&self) -> Result<File, Box<dyn std::error::Error>> {
        let cache_dir = self.config.as_ref().unwrap().bookmark_cache_dir()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache_dir.join("pinboard-mutations.lock"))?;
        lock.lock()?;
        Ok(lock)
    }

    pub(super) fn write_output_items<'a, 'b, I, J>(&'b self, items: I, vars: Option<J>)
    where
        I: IntoIterator<Item = alfred::Item<'a>>,
        J: IntoIterator<Item = (&'b str, &'b str)>,
    {
        debug!("Starting in write_output_items");
        let mut output_items = items.into_iter().collect::<Vec<alfred::Item>>();
        output_items.extend(self.cache_wait_item.clone());
        let mut rerun = self.rerun_seconds;
        if self.sync_authentication_failed {
            output_items.push(
                alfred::ItemBuilder::new("Pinboard sync paused")
                    .subtitle(rusty_pin::pinboard::ApiError::Authentication.to_string())
                    .icon_path("erroricon.icns")
                    .valid(false)
                    .into_item(),
            );
        }

        match self.automatic_upgrade_item() {
            Ok((item, update_rerun)) => {
                output_items.extend(item);
                if let Some(seconds) = update_rerun {
                    rerun = Some(rerun.map_or(seconds, |current| current.min(seconds)));
                }
            }
            Err(e) => error!("Error checking for workflow updates: {:?}", e),
        }

        let config = self.config.as_ref().unwrap();
        let json_format = config.can_use_json();
        let variables = config
            .pinboard_username()
            .ok()
            .map(|username| ("username", username))
            .into_iter()
            .chain(vars.into_iter().flatten());
        crate::write_alfred_results(output_items, json_format, Some(variables), rerun);
    }
}
