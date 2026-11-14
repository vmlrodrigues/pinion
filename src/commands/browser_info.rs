use crate::AlfredError;
use std::error::Error;
use std::process::Command;

#[derive(Debug)]
pub struct BrowserActiveTabInfo {
    pub url: String,
    pub title: String,
}

const TAB_SEPARATOR: &str = " fd850fc2e63511e79f720023dfdf24ec ";
const FALLBACK_PREFIX: &str = "pinion-browser-fallback:";

pub enum BrowserLookup {
    Tab(BrowserActiveTabInfo),
    NeedsInteraction(String),
}

pub fn get() -> Result<BrowserActiveTabInfo, Box<dyn Error>> {
    active_tab(read(&[])?)
}

pub fn get_passive() -> Result<BrowserLookup, Box<dyn Error>> {
    read(&["passive"])
}

pub fn get_passive_tab() -> Result<BrowserActiveTabInfo, Box<dyn Error>> {
    active_tab(get_passive()?)
}

pub fn get_for_browser(browser: &str) -> Result<BrowserActiveTabInfo, Box<dyn Error>> {
    active_tab(read(&["interactive", browser])?)
}

fn active_tab(result: BrowserLookup) -> Result<BrowserActiveTabInfo, Box<dyn Error>> {
    match result {
        BrowserLookup::Tab(tab) => Ok(tab),
        BrowserLookup::NeedsInteraction(_) => Err("Cannot read the browser tab passively.".into()),
    }
}

fn read(args: &[&str]) -> Result<BrowserLookup, Box<dyn Error>> {
    let output = Command::new("osascript")
        .arg("-s")
        .arg("so")
        .arg("get-current-url.applescript")
        .args(args)
        .output()
        .map_err(|e| AlfredError::OsascriptError(e.to_string()))?;
    if !output.status.success() {
        return Err(AlfredError::OsascriptError(output.status.to_string()).into());
    }
    let osascript_result = String::from_utf8(output.stdout)?;
    debug!("  osascript_result: {}", osascript_result);

    let result = osascript_result.trim_matches(['{', '}', '\n']);
    if let Some(browser) = result.trim_matches('"').strip_prefix(FALLBACK_PREFIX) {
        return Ok(BrowserLookup::NeedsInteraction(browser.to_owned()));
    }
    let mut fields = result
        .split(TAB_SEPARATOR)
        .map(|s| s.trim().trim_matches('"').trim());
    let (Some(url), Some(title), None) = (fields.next(), fields.next(), fields.next()) else {
        return Err("Unexpected response when identifying the browser page.".into());
    };

    // If theTitle is missing use theURL for title as well.
    let tab_info = match (url.is_empty(), title.is_empty()) {
        (true, _) => Err("Cannot get browser's URL".into()),
        (false, true) => Ok(BrowserActiveTabInfo {
            url: url.to_string(),
            title: url.to_string(),
        }),
        _ => Ok(BrowserActiveTabInfo {
            url: url.to_string(),
            title: title.to_string(),
        }),
    };
    debug!("  tab_info: {:?}", tab_info);
    tab_info.map(BrowserLookup::Tab)
}
