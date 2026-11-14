use super::{io, process, Runner, SubCommand};
use alfred_rs::updater::{
    is_missing_response, is_unavailable_workflow_asset, parse_workflow_version,
    validate_selected_workflow_bundle, GithubReleaser,
};
use alfred_rs::{Data, Updater};
use chrono::{DateTime, TimeDelta, Utc};
use semver::Version;
use std::error::Error;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const UPDATE_ATTEMPT_FILE: &str = "workflow-update-attempt.json";
const UPDATE_RETRY_INTERVAL: TimeDelta = TimeDelta::minutes(5);
const UPDATE_LAUNCH_GRACE: TimeDelta = TimeDelta::seconds(2);
const UPDATE_RETRY_RERUN: u8 = 5;
const UPDATE_ASSET_VARIABLE: &str = "workflow_update_asset";
const UPDATE_VERSION_VARIABLE: &str = "workflow_update_version";
const OFFER_CHANGED: &str = "The offered version changed or is unavailable. Run pcheck again.";
const PINION_BUNDLE_ID: &str = "com.victorrodrigues.pinion";
const PINION_EXECUTABLE: &str = "pinion";

pub(crate) fn verify_workflow_release(tag: &str, bundle: &Path) -> Result<(), Box<dyn Error>> {
    let version = parse_workflow_version(tag)
        .map_err(|error| format!("The tag is not a semantic version: {error}"))?;
    Ok(validate_selected_workflow_bundle(
        io::BufReader::new(File::open(bundle)?),
        PINION_BUNDLE_ID,
        &version,
        Some(PINION_EXECUTABLE),
    )?)
}

impl Runner<'_, '_> {
    pub fn upgrade(&self, cmd: &SubCommand) {
        debug!("Starting in upgrade");
        match *cmd {
            SubCommand::SelfUpdate {
                check,
                download,
                ref expected_version,
            } => {
                if check && download {
                    eprintln!("Cannot check and download at the same time.");
                    process::exit(1);
                }
                if check {
                    let json_format = self.config.as_ref().unwrap().can_use_json();
                    let item = match check_for_upgrade() {
                        Ok(Some(item)) => item,
                        Ok(None) => update_status("No newer version of Pinion is available."),
                        Err(error) => {
                            debug!("Error in fetching update status: {error:?}");
                            update_status("Couldn't check for Pinion updates.")
                        }
                    };
                    crate::write_to_alfred([item], json_format, None::<Vec<(&str, &str)>>);
                } else if download {
                    let expected_asset = std::env::var(UPDATE_ASSET_VARIABLE).ok();
                    download_upgrade(expected_version.as_ref(), expected_asset.as_deref());
                }
            }
            _ => unreachable!(),
        }
    }

    pub(super) fn automatic_upgrade_item(
        &self,
    ) -> Result<(Option<alfred::Item<'static>>, Option<u8>), Box<dyn Error>> {
        let updater = workflow_updater()?;
        let rerun = if updater.due_to_check() {
            schedule_update_check()?
        } else {
            None
        };
        updater.load_cached_newer_version();
        let item = upgrade_item(&updater);
        let rerun = rerun.or_else(|| {
            (item.is_none() && updater.is_retrying_missing_asset()).then_some(UPDATE_RETRY_RERUN)
        });
        Ok((item, rerun))
    }
}

pub(crate) fn check_workflow_update() -> Result<(), Box<dyn Error>> {
    let lock = update_lock()?;
    lock.lock()?;
    let mut updater = workflow_updater()?;
    if updater.due_to_check() {
        updater.set_interval(0);
        updater.init()?;
        updater.update_ready()?;
    }
    Ok(())
}

fn schedule_update_check() -> Result<Option<u8>, Box<dyn Error>> {
    let lock = update_lock()?;
    match lock.try_lock() {
        Ok(()) => (),
        Err(TryLockError::WouldBlock) => return Ok(Some(1)),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    // The worker may have completed after the caller checked the cached state.
    let updater = workflow_updater()?;
    if !updater.due_to_check() {
        return Ok(None);
    }
    let now = Utc::now();
    if let Some(attempt) = Data::load_from_file::<_, DateTime<Utc>>(UPDATE_ATTEMPT_FILE) {
        let elapsed = now - attempt;
        if (TimeDelta::zero()..UPDATE_RETRY_INTERVAL).contains(&elapsed)
            && !updater.lost_release_result_since(attempt)
        {
            // Poll promptly during launch, then keep the open search eligible
            // for a retry if the check failed and is still due.
            return Ok(Some(if elapsed < UPDATE_LAUNCH_GRACE {
                1
            } else {
                UPDATE_RETRY_RERUN
            }));
        }
    }

    Data::save_to_file(UPDATE_ATTEMPT_FILE, &now)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("check-workflow-update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()?;
    Ok(Some(1))
}

fn workflow_updater() -> Result<Updater<GithubReleaser>, Box<dyn Error>> {
    Ok(Updater::gh("vmlrodrigues/pinion")?
        .with_expected_bundle_id(PINION_BUNDLE_ID)
        .with_required_executable(PINION_EXECUTABLE))
}

fn update_lock() -> io::Result<File> {
    let cache_dir = alfred::env::workflow_cache()
        .ok_or_else(|| io::Error::other("Missing Alfred workflow cache directory."))?;
    std::fs::create_dir_all(&cache_dir)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache_dir.join("workflow-update.lock"))
}

pub(crate) fn install_workflow(bundle: &Path) {
    match open_selected_workflow(bundle) {
        Ok(()) => println!("Opening Alfred to install..."),
        Err(error) => {
            debug!("Couldn't open the selected workflow: {error:?}");
            println!("Error: {}", crate::redact_token(&error.to_string()));
        }
    }
}

fn open_selected_workflow(bundle: &Path) -> Result<(), Box<dyn Error>> {
    let cache_dir =
        alfred::env::workflow_cache().ok_or("Alfred's workflow cache is unavailable.")?;
    if bundle.parent() != Some(cache_dir.as_path()) || !bundle.is_file() {
        return Err("The downloaded workflow is unavailable. Run pcheck again.".into());
    }
    let lock = update_lock()?;
    lock.lock()?;
    let updater = workflow_updater()?;
    let selected_asset = std::env::var(UPDATE_ASSET_VARIABLE).map_err(|_| OFFER_CHANGED)?;
    let selected_version = std::env::var(UPDATE_VERSION_VARIABLE)
        .ok()
        .and_then(|version| Version::parse(&version).ok())
        .ok_or(OFFER_CHANGED)?;
    if updater.load_cached_newer_version().as_ref() != Some(&selected_version)
        || updater
            .latest_avail_asset_identity()
            .as_ref()
            .map(|url| url.as_str())
            != Some(selected_asset.as_str())
    {
        return Err(OFFER_CHANGED.into());
    }
    validate_selected_workflow_bundle(
        io::BufReader::new(File::open(bundle)?),
        PINION_BUNDLE_ID,
        &selected_version,
        Some(PINION_EXECUTABLE),
    )?;
    let opened = Command::new("open")
        .args(["-b", "com.runningwithcrayons.Alfred"])
        .arg(bundle)
        .stdout(Stdio::null())
        .status()?;
    if !opened.success() {
        return Err("Couldn't install the downloaded workflow.".into());
    }
    Ok(())
}

fn check_for_upgrade() -> Result<Option<alfred::Item<'static>>, Box<dyn Error>> {
    let Some(updater) = updater_with_offer(false)? else {
        return Ok(None);
    };
    Ok(upgrade_item(&updater))
}

fn updater_with_offer(
    allow_cached: bool,
) -> Result<Option<Updater<GithubReleaser>>, Box<dyn Error>> {
    let started_at = Utc::now();
    let lock = update_lock()?;
    match lock.try_lock() {
        Ok(()) => (),
        Err(TryLockError::WouldBlock) => {
            debug!("Waiting for the active workflow update check");
            lock.lock()?;
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let mut updater = workflow_updater()?;
    if allow_cached && updater.load_cached_newer_version().is_some() {
        return Ok(Some(updater));
    }
    if updater.checked_since(started_at).is_none() {
        updater.set_interval(0);
        updater.init()?;
        updater.update_ready()?;
    }
    Ok(updater
        .load_cached_newer_version()
        .is_some()
        .then_some(updater))
}

fn download_upgrade(expected: Option<&Version>, expected_asset: Option<&str>) {
    // Alfred offers must still match a current GitHub check when selected.
    let updater = match updater_with_offer(expected.is_none()) {
        Ok(Some(updater)) => updater,
        Ok(None) if expected.is_some() => download_error(OFFER_CHANGED),
        Ok(None) => download_error("No newer version of Pinion is available."),
        Err(error) => {
            debug!("Error in fetching update status: {error:?}");
            download_error("Couldn't check for Pinion updates.");
        }
    };
    let transfer_started_at = Utc::now();
    let filename = if let Some(expected) = expected {
        if updater.latest_avail_version().as_ref() != Some(expected)
            || expected_asset.is_some_and(|asset| {
                updater
                    .latest_avail_asset_identity()
                    .as_ref()
                    .map(|url| url.as_str())
                    != Some(asset)
            })
        {
            download_error(OFFER_CHANGED);
        }
        updater.download_workflow(expected, PINION_BUNDLE_ID)
    } else {
        updater.download_latest_workflow(PINION_BUNDLE_ID)
    };
    let filename = filename.unwrap_or_else(|error| {
        debug!("Download error: {error:?}");
        if is_unavailable_workflow_asset(&error) {
            let forget_offer = || -> Result<(), Box<dyn Error>> {
                let lock = update_lock()?;
                lock.lock()?;
                Ok(updater.forget_cached_release(transfer_started_at)?)
            };
            if let Err(error) = forget_offer() {
                debug!("Couldn't clear the cached update: {error:?}");
            }
            if is_missing_response(&error) {
                download_error(OFFER_CHANGED);
            }
        }
        download_error("Couldn't download the latest workflow.")
    });
    // Another check can change the offer while the asset is downloading.
    let record_verification = || -> Result<_, Box<dyn Error>> {
        let lock = update_lock()?;
        lock.lock()?;
        Ok((lock, updater.record_verified_download(transfer_started_at)))
    };
    let _offer_lock = match record_verification() {
        Ok((lock, Ok(true))) => Some(lock),
        Ok((_, Ok(false))) if expected.is_some() => {
            if let Err(error) = std::fs::remove_file(&filename) {
                debug!("Couldn't delete the withdrawn workflow download: {error:?}");
            }
            download_error(OFFER_CHANGED);
        }
        Ok((lock, Err(error))) => {
            // Failure to persist a timestamp should not discard a verified download.
            debug!("Couldn't record the verified workflow download: {error:?}");
            Some(lock)
        }
        Err(error) if expected.is_some() => {
            debug!("Error validating the downloaded workflow: {error:?}");
            download_error("Couldn't check for Pinion updates.")
        }
        // The direct CLI can download using a cached offer without a recheck.
        Ok((_, Ok(false))) => None,
        Err(error) => {
            debug!("Couldn't record the verified workflow download: {error:?}");
            None
        }
    };
    let path = filename
        .to_str()
        .unwrap_or_else(|| download_error("The downloaded workflow's path is invalid."));
    write!(io::stdout(), "Download Successful: {path}").expect("Couldn't write to output!");
}

fn upgrade_item(updater: &Updater<GithubReleaser>) -> Option<alfred::Item<'static>> {
    let available = updater.latest_avail_version()?;
    let asset = updater.latest_avail_asset_identity()?;
    let installed = updater.current_version();
    Some(
        alfred::ItemBuilder::new("New Version Is Available for Pinion! 🎉")
            .subtitle(format!(
                "Click to download & upgrade {installed} ⟶ {available}"
            ))
            .icon_path("auto_update.png")
            .variable("workflow_update_ready", "1")
            .variable(UPDATE_ASSET_VARIABLE, asset.to_string())
            .variable(UPDATE_VERSION_VARIABLE, available.to_string())
            .arg(available.to_string())
            .into_item(),
    )
}

fn update_status(title: &'static str) -> alfred::Item<'static> {
    alfred::ItemBuilder::new(title)
        .icon_path("auto_update.png")
        .valid(false)
        .into_item()
}

fn download_error(message: &str) -> ! {
    println!("Error: {message}");
    process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::update_status;

    #[test]
    fn update_status_cannot_be_selected() {
        let mut result = Vec::new();
        let item = update_status("No newer version of Pinion is available.");
        alfred::json::write_items(&mut result, &[item]).unwrap();

        let result: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let item = &result["items"][0];
        assert_eq!(item["valid"], false);
        assert!(item.get("arg").is_none());
    }
}
