//! Helper for enabling Alfred workflows to upgrade themselves periodically (Alfred 3+)
//!
//! Using this module, the workflow author can make Alfred check for latest releases
//! ([`try_update_ready()`] or [`update_ready()`]) from a remote server within adjustable intervals
//! (default is 24 hrs).
//!
//! Additionally they can ask Alfred to download the new release to its cache folder for further
//! action: [`download_latest()`].
//!
//! For convenience, an associated method [`Updater::gh()`] is available to check
//! for workflows hosted on `github.com`.
//!
//! However, it's possible to check with other servers as long as the [`Releaser`] trait is
//! implemented for the desired remote service.
//! See [`Updater::new()`] documentation if you are hosting your workflow
//! on a non `github.com` service.
//!
//! ## Notes:
//! - The `github.com` hosted repository should have release items following `github`'s process.
//! This can be done by tagging a commit and then manually building a release where you
//! attach/upload `YourWorkflow.alfredworkflow` to the release page.
//! You can easily create `YourWorkflow.alfredworkflow` file by using the [export feature] of
//! Alfred in its preferences window.
//!
//! - The tag should follow all of the [semantic versioning] rules.
//! The only exception to those rules is that you can prepend your
//! semantic version tag with ASCII letter `v`: `v0.3.1` or `0.3.1`
//!
//! # Note to workflow authors
//! - Depending on network quality, checking if an update is available may take a long time.
//! This module may spawn a worker thread so that the check does not block the main flow of your plugin.
//! However given the limitations of Alfred's plugin architecture, the worker thread cannot outlive
//! your plugin's executable. This means that you either have to wait/block for the worker thread,
//! or if it is taking longer than a desirable time, you will have to abandon it.
//! See the example for more details.
//! - Workflow authors should make sure that _released_ workflow bundles have
//! their version set in [Alfred's preferences window]. However, this module provides
//! [`set_version()`] to set the version during runtime.
//!
//! [`Releaser`]: trait.Releaser.html
//! [`Updater`]: struct.Updater.html
//! [`update_ready()`]: struct.Updater.html#method.update_ready
//! [`try_update_ready()`]: struct.Updater.html#method.try_update_ready
//! [`download_latest()`]: struct.Updater.html#method.download_latest
//! [`Updater::gh()`]: struct.Updater.html#method.gh
//! [`Updater::new()`]: struct.Updater.html#method.new
//! [semantic versioning]: https://semver.org
//! [export feature]: https://www.alfredapp.com/help/workflows/advanced/sharing-workflows/
//! [Alfred's preferences window]: https://www.alfredapp.com/help/workflows/advanced/variables/
//! [`set_version()`]: struct.Updater.html#method.set_version
//! [`set_interval()`]: struct.Updater.html#method.set_interval
//!
//! # Example
//!
//! Create an updater for a workflow hosted on `github.com/spamwax/alfred-pinboard-rs`.
//! By default, it will check for new releases every 24 hours.
//! To change the interval, use [`set_interval()`] method.
//!
//! ```rust,no_run
//! # use anyhow::Result;
//! extern crate alfred;
//!
//! // This crate
//! extern crate alfred_rs;
//!
//! use alfred::{Item, ItemBuilder, json};
//! use alfred_rs::Updater;
//!
//! # use std::io;
//! # fn produce_items_for_user_to_see<'a>() -> Vec<Item<'a>> {
//! #     Vec::new()
//! # }
//! # fn do_some_other_stuff() {}
//! // Our workflow's main 'runner' function
//! fn run<'a>() -> Result<Vec<Item<'a>>> {
//!     let updater = Updater::gh("spamwax/alfred-pinboard-rs")?;
//!
//!     // Start the process for getting latest release info
//!     updater.init().expect("cannot initialize updater");
//!
//!     // We'll do some other work that's related to our workflow:
//!     do_some_other_stuff();
//!     let mut items: Vec<Item> = produce_items_for_user_to_see();
//!
//!     // We can now check if update is ready using two methods:
//!     // 1- Block and wait until we receive results from worker thread
//!     //    It's a good practice to only wait for worker for a limited time so
//!     //    our workflow doesn't become unresponsive (not shown here)
//!     let update_status = updater.update_ready();
//!
//!     // 2- Or without blocking, check if the worker thread sent the results.
//!     //    If the worker thread is still busy, we'll get an `Err`
//!     let update_status = updater.try_update_ready();
//!
//!     if let Ok(is_ready) = update_status { // Comm. with worker was successful
//!         // Check for new update and add an item to 'items'
//!         if is_ready {
//!             let update_item = ItemBuilder::new(
//!                 "New version is available!"
//!             ).into_item();
//!             // Add a new item to previous list of Items
//!             items.push(update_item);
//!         }
//!     } else {
//!         /* worker thread wasn't successful */
//!     }
//!     Ok(items)
//! }
//!
//! fn main() {
//!     // Fetch the items and show them.
//!     if let Ok(ref items) = run() {
//!         json::write_items(io::stdout(), items);
//!     }
//! }
//! ```
//!
//! An *issue* with above example can be when user is on a poor network or server is unresponsive.
//! In this case, the above snippet will try to call server every time workflow is invoked
//! by Alfred until the operation succeeds.

use super::{anyhow, bail, chrono, env_logger, semver, serde_json, url, Result};
use crate::env;
use chrono::prelude::*;
use chrono::Duration;
use reqwest;
use semver::Version;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env as StdEnv;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use tempfile::{Builder as TempFileBuilder, NamedTempFile};
use unicode_normalization::UnicodeNormalization;
use url::Url;
mod imp;
mod releaser;

#[cfg(test)]
mod tests;

/// Default update check interval duration (24 hrs). To change the interval use the
/// [`set_interval()`] method.
///
/// [`set_interval()`]: struct.Updater.html#method.set_interval
pub const UPDATE_INTERVAL: i64 = 24 * 60 * 60;
const DOWNLOAD_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const MAX_WORKFLOW_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PLIST_BYTES: u64 = 8 * 1024 * 1024;
const ASSET_RECOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

use self::imp::ReleaseCache;
pub use self::releaser::{is_missing_response, GithubReleaser, ReleaseCheck, Releaser};

/// Parse the version of a workflow, including the optional Git tag prefix.
///
/// # Errors
/// Returns an error if the version is not valid SemVer.
pub fn parse_workflow_version(version: &str) -> std::result::Result<Version, semver::Error> {
    Version::parse(version.strip_prefix('v').unwrap_or(version))
}

fn github_asset_request(
    client: &reqwest::blocking::Client,
    url: Url,
) -> reqwest::blocking::RequestBuilder {
    client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(
            reqwest::header::USER_AGENT,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        )
}

#[derive(Debug)]
struct InvalidWorkflowBundle(anyhow::Error);

impl std::fmt::Display for InvalidWorkflowBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for InvalidWorkflowBundle {}

/// Whether the asset is confirmed missing or returned bytes that cannot be installed.
pub fn is_unavailable_workflow_asset(error: &anyhow::Error) -> bool {
    is_missing_response(error) || error.is::<InvalidWorkflowBundle>()
}

fn verify_recovering_asset(
    url: &Url,
    version: &Version,
    bundle_id: Option<&str>,
    required_executable: Option<&str>,
) -> Result<bool> {
    let request = github_asset_request(&reqwest::blocking::Client::new(), url.clone())
        .timeout(ASSET_RECOVERY_TIMEOUT);
    let mut download = tempfile::tempfile()?;
    match receive_workflow_bundle(
        request,
        true,
        &mut download,
        bundle_id,
        Some(version),
        required_executable,
    ) {
        Ok(()) => Ok(true),
        Err(error) if is_unavailable_workflow_asset(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn receive_workflow_bundle(
    request: reqwest::blocking::RequestBuilder,
    from_asset_api: bool,
    file: &mut File,
    bundle_id: Option<&str>,
    version: Option<&Version>,
    required_executable: Option<&str>,
) -> Result<()> {
    let mut response = request.send()?.error_for_status()?;
    let returned_metadata = from_asset_api
        && response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| content_type.to_ascii_lowercase().contains("json"));
    if response.status() != reqwest::StatusCode::OK || returned_metadata {
        return Err(InvalidWorkflowBundle(anyhow!(
            "the asset endpoint did not return a workflow bundle"
        ))
        .into());
    }
    {
        let mut writer = BufWriter::with_capacity(0x10_0000, &mut *file);
        let mut buffer = [0; 0x1_0000];
        let mut copied = 0;
        loop {
            // An interrupted 200 response has not supplied installable bytes.
            // Distinguish failed reads from local write errors so it enters
            // asset recovery without blaming GitHub for a full disk.
            let count = response
                .read(&mut buffer)
                .map_err(|error| InvalidWorkflowBundle(anyhow!(error)))?;
            if count == 0 {
                break;
            }
            copied += count as u64;
            if copied > MAX_WORKFLOW_BYTES {
                return Err(InvalidWorkflowBundle(anyhow!("workflow bundle is too large")).into());
            }
            writer.write_all(&buffer[..count])?;
        }
        writer.flush()?;
    }
    file.rewind()?;
    validate_workflow_identity(
        BufReader::new(file),
        bundle_id,
        version,
        required_executable,
    )
    .map_err(InvalidWorkflowBundle)?;
    Ok(())
}

/// Verify the archive and return the workflow's bundle identifier and version.
///
/// # Errors
/// Returns an error for an unreadable ZIP or missing or invalid workflow metadata.
fn validate_workflow_bundle(
    reader: impl Read + Seek,
    required_executable: Option<&str>,
) -> Result<(String, String)> {
    let mut archive = zip::ZipArchive::new(reader)?;
    archive.validate_header_consistency()?;
    let mut destinations = HashMap::new();
    let mut remaining = MAX_WORKFLOW_BYTES;
    let mut manifest = None;
    let mut has_required_executable = required_executable.is_none();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        // The ZIP reader applies Unicode Path overrides even to name_raw(),
        // but macOS's extractor uses the original header name. Refuse that
        // alternate representation so both readers validate the same files.
        if entry
            .extra_data_fields()
            .any(|field| matches!(field, zip::extra_fields::ExtraField::UnicodePath(_)))
        {
            bail!("alternate ZIP filenames are not supported in workflow bundles");
        }
        let filename = entry.name()?;
        let filename = filename.strip_prefix("./").unwrap_or(&filename);
        let path = filename.strip_suffix('/').unwrap_or(filename);
        if entry.enclosed_name().is_none()
            || entry.is_symlink()
            || path.contains('\\')
            || path.chars().any(|character| character.is_ascii_control())
            || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            bail!("invalid path in workflow bundle");
        }
        // macOS also folds Unicode case and normalization when comparing paths.
        let destination: String = path.nfd().flat_map(char::to_uppercase).nfd().collect();
        let is_manifest = destination == "INFO.PLIST";
        if destinations.insert(destination, entry.is_dir()).is_some() {
            bail!("duplicate extracted path in workflow bundle");
        }
        let size = entry.size();
        if size > remaining {
            bail!("workflow bundle is too large");
        }
        remaining -= size;
        if is_manifest && (filename != "info.plist" || !entry.is_file() || size > MAX_PLIST_BYTES) {
            bail!("invalid info.plist entry in workflow bundle");
        }
        if Some(filename) == required_executable {
            has_required_executable = entry.is_file()
                && size > 0
                && entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170_000 == 0o100_000 && mode & 0o100 != 0);
        }
        let mut content = (&mut entry).take(size + 1);
        if is_manifest {
            let mut bytes = Vec::new();
            content.read_to_end(&mut bytes)?;
            if bytes.len() as u64 != size {
                bail!("incorrect info.plist length in workflow bundle");
            }
            manifest = Some(plist::Value::from_reader(Cursor::new(bytes))?);
        } else if io::copy(&mut content, &mut io::sink())? != size {
            bail!("incorrect ZIP entry length in workflow bundle");
        }
    }
    for path in destinations.keys() {
        for (separator, _) in path.match_indices('/') {
            if destinations.get(&path[..separator]) == Some(&false) {
                bail!("a file in the workflow bundle is also used as a directory");
            }
        }
    }
    let info = manifest
        .as_ref()
        .and_then(plist::Value::as_dictionary)
        .ok_or_else(|| anyhow!("workflow bundle has no valid info.plist"))?;
    let get_string = |key: &str| {
        info.get(key)
            .and_then(plist::Value::as_string)
            .filter(|value| !value.trim().is_empty())
    };
    let bundle_id = get_string("bundleid")
        .ok_or_else(|| anyhow!("info.plist has no workflow bundle identifier"))?;
    let version =
        get_string("version").ok_or_else(|| anyhow!("info.plist has no workflow version"))?;
    if get_string("name").is_none()
        || !info
            .get("objects")
            .is_some_and(|value| value.as_array().is_some())
        || !info
            .get("connections")
            .is_some_and(|value| value.as_dictionary().is_some())
    {
        bail!("info.plist does not describe an Alfred workflow");
    }
    if let Some(executable) = required_executable.filter(|_| !has_required_executable) {
        bail!("workflow bundle does not contain a valid {executable} executable");
    }
    Ok((bundle_id.to_owned(), version.to_owned()))
}

/// Check an Alfred archive's identity, version, and required executable, if specified.
///
/// # Errors
/// Returns an error for an invalid bundle, missing executable, or different ID or version.
pub fn validate_selected_workflow_bundle(
    reader: impl Read + Seek,
    expected_bundle_id: &str,
    expected_version: &Version,
    required_executable: Option<&str>,
) -> Result<()> {
    validate_workflow_identity(
        reader,
        Some(expected_bundle_id),
        Some(expected_version),
        required_executable,
    )
}

fn validate_workflow_identity(
    reader: impl Read + Seek,
    expected_bundle_id: Option<&str>,
    expected_version: Option<&Version>,
    required_executable: Option<&str>,
) -> Result<()> {
    let (bundle_id, version) = validate_workflow_bundle(reader, required_executable)?;
    if expected_bundle_id.is_some_and(|expected| bundle_id != expected) {
        bail!("the downloaded bundle has a different workflow identifier");
    }
    if expected_version
        .is_some_and(|expected| parse_workflow_version(&version).ok().as_ref() != Some(expected))
    {
        bail!("the downloaded bundle has a different workflow version");
    }
    Ok(())
}

fn prune_old_downloads(directory: &Path, prefix: &str, selected: &Version) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(version) = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .and_then(|name| name.strip_suffix(".alfredworkflow"))
            .and_then(|name| {
                // Versions can't contain underscores. Older downloads used no suffix.
                let version = name.split_once('_').map_or(name, |(version, _)| version);
                Version::parse(version).ok()
            })
        else {
            continue;
        };
        if imp::version_precedes(selected, &version) || !entry.file_type()?.is_file() {
            continue;
        }
        // An earlier selection may still be opening in Alfred. Keep recent
        // downloads so the other install can read its original bundle.
        if entry
            .metadata()?
            .modified()?
            .elapsed()
            .is_ok_and(|age| age >= DOWNLOAD_RETENTION)
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Struct to check for & download the latest release of workflow from a remote server.
pub struct Updater<T>
where
    T: Releaser,
{
    state: imp::UpdaterState,
    releaser: RefCell<T>,
    expected_bundle_id: Option<String>,
    required_executable: Option<String>,
}

impl Updater<GithubReleaser> {
    /// Create an `Updater` object that will interface with a `github` repository.
    ///
    /// The `repo_name` should be in `user_name/repository_name` form. See the
    /// [module level documentation](./index.html) for full example and description.
    ///
    /// ```rust
    /// # extern crate alfred_rs;
    /// use alfred_rs::Updater;
    ///
    /// # use std::env;
    /// # fn main() {
    /// # env::set_var("alfred_workflow_uid", "abcdef");
    /// # env::set_var("alfred_workflow_data", env::temp_dir());
    /// # env::set_var("alfred_workflow_cache", env::temp_dir());
    /// # env::set_var("alfred_workflow_version", "0.0.0");
    /// let updater = Updater::gh("spamwax/alfred-pinboard-rs").expect("cannot initiate Updater");
    /// # }
    /// ```
    ///
    /// This only creates an `Updater` without performing any network operations.
    /// To check availability of a new release, launch and check for updates by
    /// using [`init()`] and [`update_ready()`] or [`try_update_ready()`] methods.
    ///
    /// To download an available release use [`download_latest()`] method afterwards.
    ///
    /// # Errors
    /// Returns an error if the installed workflow version cannot be parsed.
    ///
    /// [`init()`]: struct.Updater.html#method.init
    /// [`update_ready()`]: struct.Updater.html#method.update_ready
    /// [`try_update_ready()`]: struct.Updater.html#method.try_update_ready
    /// [`download_latest()`]: struct.Updater.html#method.download_latest
    pub fn gh<S>(repo_name: S) -> Result<Self>
    where
        S: Into<String>,
    {
        let releaser = GithubReleaser::new(repo_name);

        Self::load_or_new(releaser)
    }
}

impl<T> Updater<T>
where
    T: Releaser + Send + 'static,
{
    /// Require a recovering asset to carry this workflow's bundle identifier.
    #[must_use]
    pub fn with_expected_bundle_id(mut self, bundle_id: &str) -> Self {
        self.expected_bundle_id = Some(bundle_id.to_owned());
        self
    }

    /// Require a root-level executable in the downloaded or recovered archive.
    #[must_use]
    pub fn with_required_executable(mut self, name: &str) -> Self {
        self.required_executable = Some(name.to_owned());
        self
    }

    /// Create an `Updater` object that will interface with a remote repository for updating operations.
    ///
    /// `repo_name` is an arbitrary tag/identifier associated with the remote repository.
    ///
    /// How the `Updater` interacts with the remote server should be implemented using the [`Releaser`]
    /// trait. This crate provides a default implementation for interacting with
    /// `github.com` repositories, see [`gh()`] and [`GithubReleaser`].
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # extern crate alfred_rs;
    /// # extern crate semver;
    /// # use anyhow::Result;
    /// # extern crate url;
    ///
    /// use url::Url;
    /// use semver::Version;
    ///
    /// use alfred_rs::Updater;
    /// use alfred_rs::updater::Releaser;
    /// # use std::env;
    /// # fn main() {
    ///
    /// #[derive(Clone)]
    /// struct MyPrivateHost {/* inner */};
    ///
    /// // You need to actually implement the trait, following is just a mock.
    /// impl Releaser for MyPrivateHost {
    ///     type SemVersion = Version;
    ///     type DownloadLink = Url;
    ///
    ///     fn new<S: Into<String>>(project_id: S) -> Self {
    ///         MyPrivateHost {}
    ///     }
    ///
    ///     fn fetch_latest_release(&self) -> Result<(Version, Url)> {
    ///         let version = Version::new(1, 0, 12);
    ///         let url = Url::parse("https://ci.remote.cc/release/latest")?;
    ///         Ok((version, url))
    ///     }
    /// }
    ///
    /// let updater: Updater<MyPrivateHost> =
    ///     Updater::new("my_hidden_proj").expect("cannot initiate Updater");
    /// # }
    /// ```
    ///
    /// This only creates an `Updater` without performing any network operations.
    /// To check availability of a new release, launch and check for updates by
    /// using [`init()`] and [`update_ready()`] or [`try_update_ready()`] methods.
    ///
    /// To check availability of a new release use [`update_ready()`] method.
    ///
    /// To download an available release use [`download_latest()`] method afterwards.
    ///
    /// # Errors
    /// Error will happen during calling this method if:
    /// - `Updater` state cannot be read/written during instantiation, or
    /// - The workflow version cannot be parsed as a semantic version compatible identifier.
    ///
    /// [`init()`]: struct.Updater.html#method.init
    /// [`update_ready()`]: struct.Updater.html#method.update_ready
    /// [`try_update_ready()`]: struct.Updater.html#method.try_update_ready
    /// [`download_latest()`]: struct.Updater.html#method.download_latest
    /// [`Releaser`]: trait.Releaser.html
    /// [`GithubReleaser`]: struct.GithubReleaser.html
    /// [`gh()`]: struct.Updater.html#method.gh
    pub fn new<S>(repo_name: S) -> Result<Updater<T>>
    where
        S: Into<String>,
    {
        let releaser = Releaser::new(repo_name);
        Self::load_or_new(releaser)
    }

    /// Initializes `Updater` to fetch latest release information.
    ///
    /// - If it has been more than [`UPDATE_INTERVAL`] seconds (see [`set_interval()`]) since last check,
    /// the method will spawn a worker thread.
    /// In the background, the spawned thread will attempt to make a network call to fetch metadata of releases
    /// *only if* `UPDATE_INTERVAL` seconds has passed since the last network call.
    ///
    /// - All calls, which happen before the `UPDATE_INTERVAL` seconds, will initialize the `Updater`
    /// by using a local cache to report metadata about a release.
    /// Setting the interval to zero forces a remote check, even on the first run.
    ///
    /// For `Updater`s talking to `github.com`, the worker thread will only fetch a small
    /// metadata information to extract the version of the latest release.
    ///
    /// To check on status of worker thread and to get latest release status, use either of
    /// [`update_ready()`] or [`try_update_ready()`] methods.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # extern crate alfred_rs;
    /// # use anyhow::Result;
    /// # use alfred_rs::Updater;
    /// # use std::env;
    /// # fn do_some_other_stuff() {}
    /// # fn test_async() -> Result<()> {
    /// let updater = Updater::gh("spamwax/alfred-pinboard-rs")?;
    ///
    /// let rx = updater.init().expect("Error in starting updater.");
    ///
    /// // We'll do some other work that's related to our workflow while waiting
    /// do_some_other_stuff();
    ///
    /// // We can now check if update is ready using two methods:
    /// // 1- Block and wait until we receive results or errors
    /// let update_status = updater.update_ready();
    ///
    /// // 2- Or without blocking, check if the worker thread sent the results.
    /// //    If the worker thread is still busy, we'll get an `Err`
    /// let update_status = updater.try_update_ready();
    ///
    /// if let Ok(is_ready) = update_status {
    ///     // Everything went ok:
    ///     // No error happened during operation of worker thread
    ///     // and we received release info
    ///     if is_ready {
    ///         // there is an update available.
    ///     }
    /// } else {
    ///     /* either the worker thread wasn't successful or we couldn't get its results */
    /// }
    /// # Ok(())
    /// # }
    /// # fn main() {
    /// # test_async();
    /// # }
    /// ```
    ///
    /// # Errors
    /// Followings can cause the method return an error:
    /// - A worker thread cannot be spawned
    /// - Alfred environment variable error
    /// - File IO error
    ///
    /// [`set_interval()`]: struct.Updater.html#method.set_interval
    /// [`update_ready()`]: struct.Updater.html#method.update_ready
    /// [`try_update_ready()`]: struct.Updater.html#method.try_update_ready
    /// [`UPDATE_INTERVAL`]: constant.UPDATE_INTERVAL.html
    #[allow(clippy::missing_panics_doc)]
    pub fn init(&self) -> Result<()> {
        use std::sync::mpsc;
        let _ = env_logger::try_init();

        debug!("entering init");

        // file for status of last update check
        let p = Self::release_cache_path()?;

        let (tx, rx) = mpsc::channel();

        let force_check = self.update_interval() == 0;
        let cache = Self::release_cache();
        if !force_check
            && self.last_check().is_none()
            && !matches!(cache, Some(ReleaseCache::MissingAsset { .. }))
        {
            self.set_last_check(Utc::now());
            self.save()?;
            // This send is always successful
            tx.send(Ok(imp::ReleasePayload {
                release: None,
                checked_at: None,
            }))
            .unwrap();
            debug!("  last check was set to now()");
        } else if self.is_check_due(cache.as_ref()) {
            // it's time to talk to remote server
            debug!(" calling start_releaser_worker");
            self.start_releaser_worker(tx, p)?;
        } else {
            debug!("  calling read_last_check_status");
            let release = cache
                .and_then(ReleaseCache::into_release)
                .filter(|info| info.is_newer_than(self.current_version()));
            let checked_at = release.as_ref().and_then(|info| info.fetched_at().copied());
            tx.send(Ok(imp::ReleasePayload {
                release,
                checked_at,
            }))
            .unwrap();
        }
        *self.state.borrow_worker_mut() = Some(imp::MPSCState::new(rx));
        debug!("successfully set the state of release worker");
        Ok(())
    }

    /// Checks if a new update is available by waiting for the background thread to finish
    /// fetching release info (blocking).
    ///
    /// In practice, this method will block if it has been more than [`UPDATE_INTERVAL`] seconds
    /// since last check. In any other instance the updater will return the update status
    /// that was cached since last check.
    ///
    /// This method will wait for worker thread (spawned by calling [`init()`]) to deliver release
    /// information from remote server.
    /// Upon successful retrieval, this method will compare release information to the current
    /// vertion of the workflow. The remote repository should tag each release according to semantic
    /// version scheme for this to work.
    ///
    /// You should use this method after calling `init()`, preferably after your workflow is done with other tasks
    /// and now wants to get information about the latest release.
    ///
    /// # Note
    ///
    /// - This method can block while the server responds. Use the non-blocking
    ///   [`try_update_ready()`] if the workflow needs to keep running while it waits.
    /// - On a fresh cache, the default interval skips the first network check. Use
    ///   [`set_interval(0)`][Self::set_interval] before [`init()`] to check immediately.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # extern crate alfred_rs;
    /// # use anyhow::Result;
    /// use alfred_rs::Updater;
    ///
    /// # use std::io;
    /// # fn main() {
    /// let updater =
    ///     Updater::gh("spamwax/alfred-pinboard-rs").expect("cannot initiate Updater");
    /// updater.init().expect("cannot start the worker thread");
    ///
    /// // Perform other workflow related tasks...
    ///
    /// assert_eq!(true, updater.update_ready().expect("cannot get update information"));
    ///
    /// # }
    /// ```
    ///
    /// # Errors
    /// Error will be returned :
    /// - If worker thread has been interrupted
    /// - If [`init()`] method has not been called successfully before this method
    /// - If worker could not communicate with server
    /// - If any file error or Alfred environment variable error happens
    ///
    /// [`init()`]: struct.Updater.html#method.init
    /// [`try_update_ready()`]: struct.Updater.html#method.try_update_ready
    /// [`UPDATE_INTERVAL`]: constant.UPDATE_INTERVAL.html
    pub fn update_ready(&self) -> Result<bool> {
        if self.state.borrow_worker().is_none() {
            bail!("update_ready_sync is deprecated. use init()");
        }
        self.update_ready_async(false)
    }

    /// Try to get release info from background worker and see if a new update is available (non-blocking).
    ///
    /// This method will attempt to receive release information from worker thread
    /// (spawned by calling [`init()`]). Upon successful retrieval, this method will compare
    /// release information to the current vertion of the workflow.
    /// The remote repository should tag each release according to semantic version scheme
    /// for this to work.
    ///
    /// If communication with worker thread is not successful or if the worker thread could not
    /// fetch release information, this method will return an error.
    ///
    /// You should use this method after calling `init()`, preferably after your workflow is done with other tasks
    /// and now wants to get information about the latest release.
    ///
    /// # Note
    ///
    /// - Use the blocking [`update_ready()`] method to wait for the worker thread.
    /// - On a fresh cache, the default interval skips the first network check. Use
    ///   [`set_interval(0)`][Self::set_interval] before [`init()`] to check immediately.
    ///
    /// # Example
    ///
    /// ```no_run
    /// extern crate alfred_rs;
    /// # use anyhow::Result;
    ///
    /// use alfred_rs::Updater;
    ///
    /// # use std::io;
    ///
    /// # fn do_some_other_stuff() {}
    ///
    /// fn main() {
    /// let updater =
    ///     Updater::gh("spamwax/alfred-pinboard-rs").expect("cannot initiate Updater");
    /// updater.init().expect("cannot start the worker thread");
    ///
    /// // Perform other workflow related tasks...
    /// do_some_other_stuff();
    ///
    /// assert_eq!(true, updater.try_update_ready().expect("cannot get update information"));
    ///
    /// // Execution of program will immediately follow to here since this method is non-blocking.
    ///
    /// }
    /// ```
    ///
    /// # Errors
    /// Error will be returned :
    /// - If worker thread is not ready to send information or it has been interrupted
    /// - If [`init()`] method has not been called successfully before this method
    /// - If worker could not communicate with server
    /// - If any file error or Alfred environment variable error happens
    ///
    /// [`init()`]: struct.Updater.html#method.init
    /// [`update_ready()`]: struct.Updater.html#method.update_ready
    pub fn try_update_ready(&self) -> Result<bool> {
        if self.state.borrow_worker().is_none() {
            bail!("update_ready_sync is deprecated. use init()");
        }
        self.update_ready_async(true)
    }

    /// Set workflow's version to `version`.
    ///
    /// Content of `version` needs to follow semantic versioning.
    ///
    /// This method is provided so workflow authors can set the version from within the Rust code.
    ///
    /// # Example
    ///
    /// ```rust
    /// # extern crate alfred_rs;
    /// # use anyhow::Result;
    /// # use alfred_rs::Updater;
    /// # use std::env;
    /// # fn ex_set_version() -> Result<()> {
    /// # env::set_var("alfred_workflow_uid", "abcdef");
    /// # env::set_var("alfred_workflow_data", env::temp_dir());
    /// # env::set_var("alfred_workflow_version", "0.0.0");
    /// let mut updater = Updater::gh("spamwax/alfred-pinboard-rs")?;
    /// updater.set_version("0.23.3");
    /// # Ok(())
    /// # }
    ///
    /// # fn main() {
    /// #     ex_set_version();
    /// # }
    /// ```
    /// An alternative (recommended) way of setting version is through [Alfred's preferences window].
    ///
    /// [Alfred's preferences window]: https://www.alfredapp.com/help/workflows/advanced/variables/
    ///
    /// # Panics
    /// The method will panic if the passed value `version` cannot be parsed as a semantic version compatible string.
    pub fn set_version<S: AsRef<str>>(&mut self, version: S) {
        let v = Version::parse(version.as_ref())
            .expect("version should follow semantic version rules.");
        self.state.set_version(v);

        StdEnv::set_var("alfred_workflow_version", version.as_ref());
    }

    /// Set the interval between checks for a newer release (in seconds)
    ///
    /// [Default value][`UPDATE_INTERVAL`] is 86,400 seconds (24 hrs).
    /// Use zero to force a remote check when calling [`Self::init()`].
    ///
    /// # Example
    /// Set interval to be 7 days
    ///
    /// ```rust
    /// # extern crate alfred_rs;
    /// # use alfred_rs::Updater;
    /// # use std::env;
    /// # fn main() {
    /// # env::set_var("alfred_workflow_uid", "abcdef");
    /// # env::set_var("alfred_workflow_data", env::temp_dir());
    /// # env::set_var("alfred_workflow_cache", env::temp_dir());
    /// # env::set_var("alfred_workflow_version", "0.0.0");
    /// let mut updater =
    ///     Updater::gh("spamwax/alfred-pinboard-rs").expect("cannot initiate Updater");
    /// updater.set_interval(7 * 24 * 60 * 60);
    /// # }
    /// ```
    /// [`UPDATE_INTERVAL`]: constant.UPDATE_INTERVAL.html
    pub fn set_interval(&mut self, tick: i64) {
        self.set_update_interval(tick);
    }

    /// Check if it is time to ask remote server for latest updates.
    ///
    /// It returns `true` if it has been more than [`UPDATE_INTERVAL`] seconds since we last
    /// checked with server (i.e. ran [`update_ready()`]), otherwise returns false.
    ///
    /// [`update_ready()`]: struct.Updater.html#method.update_ready
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # extern crate alfred_rs;
    /// # use anyhow::Result;
    /// # use alfred_rs::Updater;
    /// # fn run() -> Result<()> {
    /// let mut updater = Updater::gh("spamwax/alfred-pinboard-rs")?;
    ///
    /// // Assuming it is has been UPDATE_INTERVAL seconds since last time we ran the
    /// // `update_ready()` and there actually exists a new release:
    /// assert_eq!(true, updater.due_to_check());
    /// # Ok(())
    /// # }
    /// # fn main() {
    /// # run();
    /// # }
    /// ```
    ///
    /// [`UPDATE_INTERVAL`]: constant.UPDATE_INTERVAL.html
    pub fn due_to_check(&self) -> bool {
        self.is_check_due(Self::release_cache().as_ref())
    }

    /// Return whether a completed check since `attempt` has lost its cached result.
    pub fn lost_release_result_since(&self, attempt: DateTime<Utc>) -> bool {
        self.state
            .last_successful_check
            .get()
            .is_some_and(|checked_at| checked_at >= attempt)
            && Self::release_cache().is_none()
    }

    /// Return whether automatic checks are retrying a missing workflow asset.
    pub fn is_retrying_missing_asset(&self) -> bool {
        matches!(
            Self::release_cache(),
            Some(ReleaseCache::MissingAsset { .. })
        )
    }

    fn is_check_due(&self, cache: Option<&ReleaseCache>) -> bool {
        if self.update_interval() == 0 || cache.is_none() {
            return true;
        }
        if let Some(ReleaseCache::MissingAsset { retry_after, .. }) = cache {
            let remaining = *retry_after - Utc::now();
            // A clock rollback cannot extend the short retry window.
            return remaining <= Duration::zero() || remaining > imp::MISSING_ASSET_RETRY;
        }
        if let Some(ReleaseCache::Checked(Some(release))) = cache {
            if release.is_newer_than(self.current_version())
                && release.asset_api_url.is_none()
                && self.releaser.borrow().provides_asset_identity()
            {
                return true;
            }
        }
        self.last_check().map_or(true, |dt| {
            debug!("last check: {}", dt);
            Utc::now().signed_duration_since(dt) > Duration::seconds(self.update_interval())
        })
    }

    /// Download the available release into the workflow's cache directory if it is newer than
    /// the installed version. Call [`Self::update_ready()`] to resolve the current check or
    /// [`Self::load_cached_newer_version()`] to use the previously checked release.
    ///
    /// If the download and save operations are both successful, it returns name of file in which the
    /// downloaded Alfred workflow bundle is saved.
    ///
    /// The downloaded workflow will be saved in dedicated cache folder of the workflow, and it
    /// will be always renamed to `latest_release_WORKFLOW-NAME.alfredworkflow`
    ///
    /// To install the downloaded release, your workflow needs to somehow open the saved file.
    ///
    /// Within shell, it can be installed by issuing something like:
    /// ```bash
    /// open -b com.runningwithcrayons.Alfred latest_release_WORKFLOW-NAME.alfredworkflow
    /// ```
    ///
    /// Or you can add "Run script" object to your workflow and use environment variables set by
    /// Alfred to automatically open the downloaded release:
    /// ```bash
    /// open -b com.runningwithcrayons.Alfred "$alfred_workflow_cache/latest_release_$alfred_workflow_name.alfredworkflow"
    /// ```
    ///
    /// # Note
    ///
    /// The method may take longer than other Alfred-based actions to complete. Workflow authors using this crate
    /// should implement strategies to prevent unpleasant long blocks of user's typical work flow.
    ///
    /// One option to initiate the download and upgrade process is to invoke your executable with a
    /// different argument. The following snippet can be tied to a dedicated Alfred **Hotkey**
    /// or **Script Filter** so that it is only executed when user explicitly asks for it:
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # extern crate alfred;
    /// # extern crate alfred_rs;
    /// # use anyhow::Result;
    /// # use std::io;
    /// use alfred_rs::Updater;
    /// use alfred::{ItemBuilder, json};
    ///
    /// # fn main() {
    /// # let updater =
    /// #    Updater::gh("spamwax/alfred-pinboard-rs").expect("cannot initiate Updater");
    /// # let cmd_line_download_flag = true;
    /// if cmd_line_download_flag && updater.update_ready().unwrap() {
    ///     match updater.download_latest() {
    ///         Ok(downloaded_fn) => {
    ///             json::write_items(io::stdout(), &[
    ///                 ItemBuilder::new("New version of workflow is available!")
    ///                              .subtitle("Click to upgrade!")
    ///                              .arg(downloaded_fn.to_str().unwrap())
    ///                              .variable("update_ready", "yes")
    ///                              .valid(true)
    ///                              .into_item()
    ///             ]);
    ///         },
    ///         Err(e) => {
    ///             // Show an error message to user or log it.
    ///         }
    ///     }
    /// }
    /// #    else {
    /// #    }
    /// # }
    /// ```
    ///
    /// For the above example to automatically work, you then need to connect the output of the script
    /// to an **Open File** action so that Alfred can install/upgrade the new version.
    ///
    /// As suggested in above example, you can add an Alfred variable to the item so that your workflow
    /// can use it for further processing.
    ///
    /// # Errors
    /// This returns an error if there is no known newer release or if the download or save fails.
    ///
    /// [`Releaser`]: trait.Releaser.html
    pub fn download_latest(&self) -> Result<PathBuf> {
        self.download_release(None, None)
    }

    /// Download to the unversioned path after verifying the workflow and release version.
    ///
    /// # Errors
    /// Returns an error when there is no known newer release or the workflow
    /// cannot be downloaded or verified.
    pub fn download_latest_workflow(&self, bundle_id: &str) -> Result<PathBuf> {
        self.download_release(None, Some(bundle_id))
    }

    /// Download the selected version from the loaded release metadata.
    ///
    /// Each selection has its own file, so concurrent downloads can't replace
    /// a bundle already handed off to an installer, even at the same version.
    ///
    /// # Errors
    ///
    /// Returns an error when the loaded release differs from `version`, is
    /// no longer newer than the installed version, or could not be downloaded.
    pub fn download_version(&self, version: &Version) -> Result<PathBuf> {
        self.download_release(Some(version), None)
    }

    /// Download and verify a bundle for a specific workflow before publishing it.
    ///
    /// # Errors
    /// Returns an error if the bundle cannot be downloaded, or its identifier or
    /// version does not match the selection.
    pub fn download_workflow(&self, version: &Version, bundle_id: &str) -> Result<PathBuf> {
        self.download_release(Some(version), Some(bundle_id))
    }

    fn download_release(
        &self,
        selected: Option<&Version>,
        bundle_id: Option<&str>,
    ) -> Result<PathBuf> {
        let release = self
            .state
            .newer_release()
            .ok_or_else(|| anyhow!("no release info for a newer version"))?;
        if let Some(selected) = selected {
            if selected != release.version() {
                bail!("the loaded release differs from the selected version");
            }
        }
        let workflow_name = env::workflow_name()
            .unwrap_or_else(|| "WhyUNoNameYourOwnWorkflow".to_string())
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();
        let prefix = format!("latest_release_{workflow_name}_v");
        let cache_dir = env::workflow_cache().ok_or_else(|| {
            anyhow!("missing env variable for cache dir. forgot to set workflow bundle id?")
        })?;
        let client = reqwest::blocking::Client::new();
        let request = if let Some(asset_api_url) = &release.asset_api_url {
            // GitHub's browser URL can be reused by a replacement. Download
            // the asset by its API identity, following GitHub's CDN redirect.
            github_asset_request(&client, asset_api_url.clone())
        } else {
            client.get(release.downloadable_url.clone())
        };
        let mut download = if let Some(version) = selected {
            TempFileBuilder::new()
                .prefix(&format!("{prefix}{version}_"))
                .suffix(".pending")
                .tempfile_in(&cache_dir)?
        } else {
            NamedTempFile::new_in(&cache_dir)?
        };
        receive_workflow_bundle(
            request,
            release.asset_api_url.is_some(),
            download.as_file_mut(),
            bundle_id,
            bundle_id.map(|_| release.version()),
            self.required_executable.as_deref(),
        )?;
        let path = if let Some(version) = selected {
            // Serialize cleanup so another invocation cannot remove a file
            // between reading its directory entry and checking its age.
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(cache_dir.join("workflow-download.lock"))?;
            lock.lock()?;
            // Keep the bundle from the time it's published, including after slow transfers.
            download
                .as_file()
                .set_modified(std::time::SystemTime::now())?;
            let path = download.path().with_extension("alfredworkflow");
            download.persist_noclobber(&path)?;
            if let Err(error) = prune_old_downloads(&cache_dir, &prefix, version) {
                warn!("Couldn't clean up old workflow downloads: {error}");
            }
            path
        } else {
            let path = cache_dir.join(format!("latest_release_{workflow_name}.alfredworkflow"));
            download.persist(&path)?;
            path
        };
        Ok(path)
    }

    /// Clear the cached offer for this updater's loaded release and schedule a retry.
    ///
    /// The caller must hold the same cache lock used for release checks.
    /// A different asset, or newer verification of the same asset, stays in the cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the missing release status cannot be written.
    pub fn forget_cached_release(&self, transfer_started_at: DateTime<Utc>) -> Result<()> {
        let Some(selected) = self.state.newer_release() else {
            return Ok(());
        };
        let should_forget = match Self::release_cache() {
            Some(ReleaseCache::Checked(Some(cached))) => {
                // An older cache has no identity, so a later check supersedes it.
                cached.matches_asset(&selected)
                    && (selected.asset_api_url.is_some()
                        || cached.fetched_at() == selected.fetched_at())
                    && cached
                        .asset_verified_at
                        .is_none_or(|checked_at| checked_at < transfer_started_at)
            }
            Some(ReleaseCache::MissingAsset {
                checked_at: None,
                failed_asset: Some(asset),
                failed_transfer_started_at: Some(previous),
                failed_version: Some(version),
                ..
            }) => {
                // Keep the most recent failure when transfers overlap. An older
                // successful transfer must not revive a newer failed download.
                Some(&asset) == selected.asset_api_url.as_ref()
                    && version == selected.version
                    && previous < transfer_started_at
            }
            _ => false,
        };
        if should_forget {
            // A failed asset transfer does not establish a release withdrawal.
            let cache = ReleaseCache::failed_download(selected, transfer_started_at);
            crate::Data::save_to_file(Self::release_cache_path()?, &cache)?;
        }
        Ok(())
    }

    /// Record successful byte validation for the same offered asset, including recovery
    /// from an earlier transfer failure. Later failures and release checks take priority.
    ///
    /// The caller must hold the same cache lock used for release checks.
    ///
    /// # Errors
    /// Returns an error if the verification record cannot be written.
    pub fn record_verified_download(&self, transfer_started_at: DateTime<Utc>) -> Result<bool> {
        let Some(selected) = self.state.newer_release() else {
            return Ok(false);
        };
        let mut cached = match Self::release_cache() {
            Some(ReleaseCache::Checked(Some(cached))) => cached,
            Some(ReleaseCache::MissingAsset {
                checked_at: None,
                failed_asset: Some(asset),
                failed_transfer_started_at: Some(failed_at),
                failed_version: Some(version),
                ..
            }) if Some(&asset) == selected.asset_api_url.as_ref()
                && version == selected.version
                && failed_at < transfer_started_at =>
            {
                // This later transfer proves that the same asset recovered.
                // A completed release check carries checked_at and still wins.
                selected.clone()
            }
            _ => return Ok(false),
        };
        if !selected.matches_asset(&cached) {
            return Ok(false);
        }
        if Some(transfer_started_at) > cached.asset_verified_at {
            cached.asset_verified_at = Some(transfer_started_at);
            crate::Data::save_to_file(
                Self::release_cache_path()?,
                &ReleaseCache::Checked(Some(cached)),
            )?;
        }
        Ok(true)
    }

    /// Returns the version for the latest downloadable workflow from [`Releaser`].
    /// `None` is returned if no release info has yet been fetched from server.
    ///
    /// # Note
    /// This method does not perform any network or disk IO. It merely returns the cached
    /// version info based on last successful communication with the remote server.
    /// So it is possible the method will return a version different than server's version if:
    /// - It's been less than [`UPDATE_INTERVAL`] seconds since last check, or
    /// - Worker thread is busy checking and you called this method before it finishes.
    ///
    /// [`Releaser`]: trait.Releaser.html
    /// [`UPDATE_INTERVAL`]: constant.UPDATE_INTERVAL.html
    /// [`update_ready()`]: struct.Updater.html#method.update_ready
    /// [`try_update_ready()`]: struct.Updater.html#method.try_update_ready
    pub fn latest_avail_version(&self) -> Option<Version> {
        self.state.latest_avail_version()
    }

    /// Return the immutable asset API URL for the currently loaded offer.
    pub fn latest_avail_asset_identity(&self) -> Option<Url> {
        self.state.newer_release()?.asset_api_url
    }

    /// Load the latest successfully checked release from disk if it is newer
    /// than the installed workflow, making it available for download.
    pub fn load_cached_newer_version(&self) -> Option<Version> {
        let release = Self::release_cache()
            .and_then(ReleaseCache::into_release)
            .filter(|release| release.is_newer_than(self.current_version()));
        let version = release.as_ref().map(|release| release.version().clone());
        self.state.set_available_release(release);
        version
    }

    /// Reuse an authoritative result published while the caller was waiting.
    ///
    /// The outer option indicates whether the check can be reused. The inner
    /// option contains the newer version, if the check found one.
    pub fn checked_since(&self, started_at: DateTime<Utc>) -> Option<Option<Version>> {
        let checked_at = self.state.last_successful_check.get()?;
        if checked_at < started_at {
            return None;
        }
        let release = match Self::release_cache()? {
            ReleaseCache::Checked(release) => release,
            ReleaseCache::MissingAsset {
                checked_at: result_at,
                ..
            } => {
                return (result_at == Some(checked_at)).then_some(None);
            }
        };
        if let Some(release) = &release {
            if release.fetched_at() != Some(&checked_at) {
                return None;
            }
        }
        Some(
            release
                .filter(|release| release.is_newer_than(self.current_version()))
                .map(|release| release.version().clone()),
        )
    }

    /// Get workflow's current version
    pub fn current_version(&self) -> &Version {
        self.state.current_version()
    }
}
