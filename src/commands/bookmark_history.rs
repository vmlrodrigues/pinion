use crate::workflow_config::Config;
use alfred_rs::Data;
use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, OpenOptions, TryLockError};
use std::path::Path;

const HISTORY_FILE: &str = "recent-bookmarks.json";
const MAX_ENTRIES: usize = 1_000;

fn load_urls(dir: &Path) -> Vec<String> {
    Data::read_data_from_disk(&dir.join(HISTORY_FILE)).unwrap_or_default()
}

pub(super) fn ranks(config: &Config) -> HashMap<String, usize> {
    let Ok(account) = config.pinboard_username() else {
        return HashMap::new();
    };
    load_urls(&config.bookmark_history_dir(account))
        .into_iter()
        .enumerate()
        .map(|(index, url)| (url, index))
        .collect()
}

pub(crate) fn remember_bookmark(account: &str, url: &str) -> Result<(), Box<dyn Error>> {
    if account.is_empty() || url.is_empty() {
        return Ok(());
    }
    let dir = Config::new().bookmark_history_dir(account);
    fs::create_dir_all(&dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("recent-bookmarks.lock"))?;
    match lock.try_lock() {
        Ok(()) => (),
        // Recording history is best effort. Opening the URL must stay responsive
        // when another process is updating history.
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    let mut urls = load_urls(&dir);
    urls.retain(|previous| previous != url);
    urls.truncate(MAX_ENTRIES - 1);
    urls.insert(0, url.to_owned());
    Data::write_data_to_disk(dir.join(HISTORY_FILE), &urls)?;
    Ok(())
}
