use super::{
    anyhow, env, env_logger, parse_workflow_version, DateTime, PathBuf, Receiver, RefCell,
    ReleaseCheck, Releaser, Result, Url, Utc, Version, UPDATE_INTERVAL,
};
use crate::Updater;
use chrono::Duration;
use std::cell::Cell;
use std::cell::Ref;
use std::cell::RefMut;
use std::path::Path;
use std::sync::mpsc;

pub(super) const LATEST_UPDATE_INFO_CACHE_FN_ASYNC: &str = "last_check_status_async.json";
pub(super) const MISSING_ASSET_RETRY: Duration = Duration::minutes(5);

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum ReleaseCache {
    MissingAsset {
        retry_after: DateTime<Utc>,
        checked_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_asset: Option<Url>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_transfer_started_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_version: Option<Version>,
    },
    // Retain the existing JSON shapes for an offered release and a negative check.
    Checked(Option<UpdateInfo>),
}

impl ReleaseCache {
    pub(super) fn missing_asset(
        now: DateTime<Utc>,
        checked_at: Option<DateTime<Utc>>,
        failed_asset: Option<Url>,
    ) -> Self {
        Self::MissingAsset {
            retry_after: now + MISSING_ASSET_RETRY,
            checked_at,
            failed_asset,
            failed_transfer_started_at: None,
            failed_version: None,
        }
    }

    pub(super) fn into_release(self) -> Option<UpdateInfo> {
        match self {
            Self::Checked(release) => release,
            Self::MissingAsset { .. } => None,
        }
    }

    pub(super) fn failed_download(
        selected: UpdateInfo,
        transfer_started_at: DateTime<Utc>,
    ) -> Self {
        Self::MissingAsset {
            retry_after: Utc::now() + MISSING_ASSET_RETRY,
            checked_at: None,
            failed_asset: selected.asset_api_url,
            failed_transfer_started_at: Some(transfer_started_at),
            failed_version: Some(selected.version),
        }
    }
}

// Cached results reuse the time of their original check.
#[derive(Debug)]
pub(super) struct ReleasePayload {
    pub(super) release: Option<UpdateInfo>,
    pub(super) checked_at: Option<DateTime<Utc>>,
}

type ReleasePayloadResult = Result<ReleasePayload>;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct UpdaterState {
    pub(super) last_check: Cell<Option<DateTime<Utc>>>,

    #[serde(default)]
    pub(super) last_successful_check: Cell<Option<DateTime<Utc>>>,

    current_version: Version,

    avail_release: RefCell<Option<UpdateInfo>>,

    #[serde(skip, default = "default_interval")]
    update_interval: i64,

    #[serde(skip)]
    worker_state: RefCell<Option<MPSCState>>,
}

impl UpdaterState {
    pub(super) fn current_version(&self) -> &Version {
        &self.current_version
    }

    pub(super) fn set_version(&mut self, v: Version) {
        self.current_version = v;
    }

    pub(super) fn latest_avail_version(&self) -> Option<Version> {
        self.avail_release
            .borrow()
            .as_ref()
            .map(|ui| ui.version().clone())
    }

    pub(super) fn set_available_release(&self, release: Option<UpdateInfo>) {
        *self.avail_release.borrow_mut() = release;
    }

    pub(super) fn borrow_worker(&self) -> Ref<'_, Option<MPSCState>> {
        self.worker_state.borrow()
    }

    pub(super) fn borrow_worker_mut(&self) -> RefMut<'_, Option<MPSCState>> {
        self.worker_state.borrow_mut()
    }

    pub(super) fn newer_release(&self) -> Option<UpdateInfo> {
        self.avail_release
            .borrow()
            .as_ref()
            .filter(|info| info.is_newer_than(self.current_version()))
            .cloned()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct UpdateInfo {
    // Latest version available from github or releaser
    pub version: Version,

    pub fetched_at: Option<DateTime<Utc>>,

    // Link to use to download the above version
    pub(super) downloadable_url: Url,

    // Older cache files have only the browser URL, which can be reused by replacements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) asset_api_url: Option<Url>,

    // A successful byte transfer is stronger evidence than a metadata-only check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) asset_verified_at: Option<DateTime<Utc>>,
}

impl UpdateInfo {
    pub fn new(v: Version, url: Url) -> Self {
        UpdateInfo {
            version: v,
            fetched_at: None,
            downloadable_url: url,
            asset_api_url: None,
            asset_verified_at: None,
        }
    }

    pub(super) fn version(&self) -> &Version {
        &self.version
    }

    pub(super) fn is_newer_than(&self, current: &Version) -> bool {
        version_precedes(current, self.version())
    }

    pub(super) fn matches_asset(&self, other: &Self) -> bool {
        self.version == other.version
            && match (&self.asset_api_url, &other.asset_api_url) {
                (Some(selected), Some(current)) => selected == current,
                (None, None) => self.downloadable_url == other.downloadable_url,
                _ => false,
            }
    }

    pub(super) fn fetched_at(&self) -> Option<&DateTime<Utc>> {
        self.fetched_at.as_ref()
    }

    pub(super) fn set_fetched_at(&mut self, date_time: DateTime<Utc>) {
        self.fetched_at = Some(date_time);
    }
}

pub(super) fn version_precedes(earlier: &Version, later: &Version) -> bool {
    // Build metadata does not affect semantic version precedence.
    (earlier.major, earlier.minor, earlier.patch, &earlier.pre)
        < (later.major, later.minor, later.patch, &later.pre)
}

#[derive(Debug)]
pub(super) struct MPSCState {
    // First successful call on rx.recv() will cache the results into this field
    recvd_payload: RefCell<Option<ReleasePayloadResult>>,
    // Receiver end of communication channel with worker thread
    rx: RefCell<Option<Receiver<ReleasePayloadResult>>>,
}

impl MPSCState {
    pub(super) fn new(rx: mpsc::Receiver<ReleasePayloadResult>) -> Self {
        MPSCState {
            recvd_payload: RefCell::new(None),
            rx: RefCell::new(Some(rx)),
        }
    }
}

impl<T> Updater<T>
where
    T: Releaser + Send + 'static,
{
    pub(super) fn load_or_new(r: T) -> Result<Self> {
        let _ = env_logger::try_init();
        if let Ok(mut saved_state) = Self::load() {
            // Use the version that workflow reports through environment variable
            // This version takes priority over what we may have saved last time.
            let env_ver = env::workflow_version()
                .map(|v| parse_workflow_version(&v))
                .transpose()?;
            if let Some(v) = env_ver {
                saved_state.current_version = v;
            }
            Ok(Updater {
                state: saved_state,
                releaser: RefCell::new(r),
                expected_bundle_id: None,
                required_executable: None,
            })
        } else {
            let current_version = env::workflow_version()
                .map_or_else(|| Ok(Version::new(0, 0, 0)), |v| parse_workflow_version(&v))?;
            let state = UpdaterState {
                current_version,
                last_check: Cell::new(None),
                last_successful_check: Cell::new(None),
                avail_release: RefCell::new(None),
                worker_state: RefCell::new(None),
                update_interval: UPDATE_INTERVAL,
            };
            Ok(Updater {
                state,
                releaser: RefCell::new(r),
                expected_bundle_id: None,
                required_executable: None,
            })
        }
    }

    pub(super) fn last_check(&self) -> Option<DateTime<Utc>> {
        self.state.last_check.get()
    }

    pub(super) fn set_last_check(&self, t: DateTime<Utc>) {
        self.state.last_check.set(Some(t));
    }

    pub(super) fn update_interval(&self) -> i64 {
        self.state.update_interval
    }

    pub(super) fn set_update_interval(&mut self, t: i64) {
        self.state.update_interval = t;
    }

    fn load() -> Result<UpdaterState> {
        let data_file_path = Self::build_data_fn()?;
        crate::Data::load_from_file(data_file_path)
            .ok_or_else(|| anyhow!("cannot load cached state of updater"))
    }

    // Save updater's state
    pub(super) fn save(&self) -> Result<()> {
        let data_file_path = Self::build_data_fn()?;
        crate::Data::save_to_file(data_file_path, &self.state)
    }

    pub(super) fn release_cache_path() -> Result<PathBuf> {
        Ok(Self::build_data_fn()?.with_file_name(LATEST_UPDATE_INFO_CACHE_FN_ASYNC))
    }

    pub(super) fn release_cache() -> Option<ReleaseCache> {
        Self::release_cache_path()
            .ok()
            .and_then(crate::Data::load_from_file)
    }

    pub(super) fn start_releaser_worker(
        &self,
        tx: mpsc::Sender<ReleasePayloadResult>,
        p: PathBuf,
    ) -> Result<()> {
        use std::thread;

        let releaser = (*self.releaser.borrow()).clone();
        let expected_bundle_id = self.expected_bundle_id.clone();
        let required_executable = self.required_executable.clone();
        let (previous_offer, retrying_missing_asset, failed_asset) = match Self::release_cache() {
            Some(ReleaseCache::MissingAsset { failed_asset, .. }) => (None, true, failed_asset),
            Some(ReleaseCache::Checked(offer)) => (offer, false, None),
            None => (None, false, None),
        };

        thread::Builder::new().spawn(move || {
            debug!("other thread: starting in updater thread");
            let talk_to_mother = || -> Result<()> {
                let mut latest = releaser.check_latest_release()?;
                let mut verified_at = None;
                if let ReleaseCheck::Available {
                    version,
                    asset_api_url: Some(asset),
                    ..
                } = &latest
                {
                    if Some(asset) == failed_asset.as_ref() {
                        let verification_started_at = Utc::now();
                        if super::verify_recovering_asset(
                            asset,
                            version,
                            expected_bundle_id.as_deref(),
                            required_executable.as_deref(),
                        )? {
                            verified_at = Some(verification_started_at);
                        } else {
                            latest = ReleaseCheck::MissingAsset;
                        }
                    }
                }
                // A caller waiting during asset verification can reuse the completed check.
                let checked_at = Utc::now();
                let cache = match latest {
                    ReleaseCheck::Available {
                        version,
                        download_url,
                        asset_api_url,
                    } => {
                        let mut info = UpdateInfo::new(version, download_url);
                        info.asset_api_url = asset_api_url;
                        info.set_fetched_at(checked_at);
                        info.asset_verified_at = verified_at.or_else(|| {
                            previous_offer
                                .as_ref()
                                .filter(|previous| previous.matches_asset(&info))
                                .and_then(|previous| previous.asset_verified_at)
                        });
                        ReleaseCache::Checked(Some(info))
                    }
                    ReleaseCheck::Unavailable if !retrying_missing_asset => {
                        ReleaseCache::Checked(None)
                    }
                    ReleaseCheck::Unavailable | ReleaseCheck::MissingAsset => {
                        ReleaseCache::missing_asset(checked_at, Some(checked_at), failed_asset)
                    }
                };
                crate::Data::save_to_file(&p, &cache)?;
                tx.send(Ok(ReleasePayload {
                    release: cache.into_release(),
                    checked_at: Some(checked_at),
                }))?;
                Ok(())
            };

            let outcome = talk_to_mother();
            debug!("other thread: finished checking releaser status");

            if let Err(error) = outcome {
                tx.send(Err(error))
                    .expect("could not send error from thread");
            }
        })?;
        Ok(())
    }

    // write version of latest avail. release (if any) to a cache file
    pub(super) fn write_last_check_status(
        p: &Path,
        updater_info: &Option<UpdateInfo>,
    ) -> Result<()> {
        crate::Data::save_to_file(p, updater_info)
    }

    // read version of latest avail. release (if any) from a cache file
    pub(super) fn read_last_check_status(p: &Path) -> Result<Option<UpdateInfo>> {
        crate::Data::load_from_file(p).ok_or_else(|| anyhow!("no data in given path"))
    }

    pub(super) fn build_data_fn() -> Result<PathBuf> {
        let workflow_name = env::workflow_name()
            .unwrap_or_else(|| "YouForgotTo/フ:NameYourOwnWork}flowッ".to_string())
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();

        env::workflow_cache()
            .ok_or_else(|| {
                anyhow!("missing env variable for cache dir. forgot to set workflow bundle id?")
            })
            .and_then(|mut data_path| {
                env::workflow_uid()
                    .ok_or_else(|| anyhow!("missing env variable for uid"))
                    .map(|ref uid| {
                        let filename = [uid, "-", workflow_name.as_str(), "-updater.json"].concat();
                        data_path.push(filename);

                        data_path
                    })
            })
    }

    pub(super) fn update_ready_async(&self, try_flag: bool) -> Result<bool> {
        self.state
            .worker_state
            .borrow()
            .as_ref()
            .ok_or_else(|| anyhow!("you need to use init() method first."))
            .and_then(|mpsc| {
                if mpsc.recvd_payload.borrow().is_none() {
                    // No payload received yet, try to talk to worker thread
                    mpsc.rx
                        .borrow()
                        .as_ref()
                        .ok_or_else(|| anyhow!("you need to use init() correctly!"))
                        .and_then(|rx| {
                            let rr = if try_flag {
                                // don't block while trying to receive
                                rx.try_recv().map_err(anyhow::Error::new)
                            } else {
                                // block while waiting to receive
                                rx.recv().map_err(|e| anyhow!(e.to_string()))
                            };
                            rr.and_then(|msg| {
                                let msg_status = msg.map(|payload| {
                                    // received good message, update cache for received payload
                                    self.state.set_available_release(payload.release.clone());
                                    if let Some(checked_at) = payload.checked_at {
                                        if self.last_check().map_or(true, |last| last < checked_at)
                                        {
                                            self.set_last_check(checked_at);
                                        }
                                        let last_success = self.state.last_successful_check.get();
                                        self.state
                                            .last_successful_check
                                            .set(last_success.max(Some(checked_at)));
                                    }
                                    *mpsc.recvd_payload.borrow_mut() = Some(Ok(payload));
                                });
                                // save state regardless of content of msg
                                self.save()?;
                                msg_status?;
                                Ok(())
                            })
                        })?;
                }
                Ok(())
            })?;
        Ok(self
            .state
            .avail_release
            .borrow()
            .as_ref()
            .map_or(false, |release| {
                release.is_newer_than(self.current_version())
            }))
    }

    #[allow(dead_code)]
    #[deprecated(note = "update_ready_sync is deprecated. use init()")]
    pub(super) fn _update_ready_sync(&self) -> Result<bool> {
        // A None value for last_check indicates that workflow is being run for first time.
        // Thus we update last_check to now and just save the updater state without asking
        // Releaser to do a remote call/check for us since we assume that user just downloaded
        // the workflow.
        const LATEST_UPDATE_INFO_CACHE_FN: &str = "last_check_status.json";

        // file for status of last update check
        let p = Self::build_data_fn()?.with_file_name(LATEST_UPDATE_INFO_CACHE_FN);

        // make a network call to see if a newer version is avail.
        // save the result of call to cache file.
        let ask_releaser_for_update = || -> Result<bool> {
            let (v, url) = self.releaser.borrow().latest_release()?;
            let update_avail = *self.current_version() < v;

            let now = Utc::now();
            let payload = {
                let mut info = UpdateInfo::new(v, url);
                info.set_fetched_at(now);
                Some(info)
            };

            self.set_last_check(now);
            Self::write_last_check_status(&p, &payload)?;
            *self.state.avail_release.borrow_mut() = payload;

            self.save()?;
            Ok(update_avail)
        };

        // if first time checking, just update the updater's timestamp, no network call
        if self.last_check().is_none() {
            self.set_last_check(Utc::now());
            self.save()?;
            Ok(false)
        } else if self.due_to_check() {
            // it's time to talk to remote server
            ask_releaser_for_update()
        } else {
            Self::read_last_check_status(&p)
                .map(|last_check_status| {
                    last_check_status.map_or(false, |last_update_info| {
                        *self.current_version() < last_update_info.version
                    })
                    // .unwrap_or(false)
                })
                .or(Ok(false))
        }
    }
}

pub(super) fn default_interval() -> i64 {
    UPDATE_INTERVAL
}
