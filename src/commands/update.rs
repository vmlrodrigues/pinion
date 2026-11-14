use super::{io, Config, Runner};
use alfred_rs::data::Data;
use chrono::{DateTime, TimeDelta, Utc};
use rusty_pin::pinboard::{ApiError, CacheRefresh};
use std::error::Error;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const SYNC_STATE_FILE: &str = "bookmark-sync.json";
const LOCAL_WRITE_MARKER: &str = "bookmark-sync.dirty";
const AUTHENTICATION_RESET_MARKER: &str = "bookmark-sync.reauthenticated";
const SYNC_INTERVAL: TimeDelta = TimeDelta::minutes(1);
const DOWNLOAD_INTERVAL: TimeDelta = TimeDelta::minutes(5);
const FULL_REFRESH_INTERVAL: TimeDelta = TimeDelta::hours(1);
const SYNC_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const SYNC_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const WATERMARK_TIMEOUT: Duration = Duration::from_secs(1);
const PENDING_TIMEOUT: TimeDelta = TimeDelta::seconds(30);
const ACTIVE_RERUN: u8 = 1;
const DEFERRED_RERUN: u8 = 5;

pub(crate) struct CacheWait {
    pub(crate) seconds: Option<u8>,
    pub(crate) error: Option<String>,
}

impl CacheWait {
    fn loading() -> Self {
        Self {
            seconds: Some(ACTIVE_RERUN),
            error: None,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct SyncState {
    last_check: Option<DateTime<Utc>>,
    last_download: Option<DateTime<Utc>>,
    last_download_attempt: Option<DateTime<Utc>>,
    remote_update: Option<DateTime<Utc>>,
    pending: bool,
    pending_since: Option<DateTime<Utc>>,
    deferred_update: Option<DateTime<Utc>>,
    deferred_reconcile: bool,
    retry_update_check: bool,
    throttle_minutes: i64,
    retry_after: Option<RetryAfter>,
    authentication_failed: bool,
}

#[derive(Deserialize, Serialize)]
struct RetryAfter {
    recorded_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
    #[serde(default)]
    service_unavailable: bool,
}

impl SyncState {
    fn load(cache_dir: &Path) -> Self {
        Data::read_data_from_disk(&cache_dir.join(SYNC_STATE_FILE)).unwrap_or_default()
    }

    fn save(&self, cache_dir: &Path) -> Result<(), Box<dyn Error>> {
        Ok(Data::write_data_to_disk(
            cache_dir.join(SYNC_STATE_FILE),
            self,
        )?)
    }

    fn finish_attempt<T>(
        &mut self,
        cache_dir: &Path,
        request_token: &str,
        result: Result<T, Box<dyn Error>>,
    ) -> Result<T, Box<dyn Error>> {
        let now = Utc::now();
        self.pending = false;
        self.pending_since = None;
        let api_error = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ApiError>());
        self.authentication_failed = matches!(api_error, Some(ApiError::Authentication));
        // A queued worker may have read its credentials before `pa` replaced them.
        if self.authentication_failed
            && crate::keychain::get().is_some_and(|token| token != request_token)
        {
            self.resume_after_authentication();
            self.retry_update_check = true;
            self.save(cache_dir)?;
            return result;
        }
        match api_error {
            Some(ApiError::RetryAfter {
                retry_at,
                service_unavailable,
                ..
            }) if *retry_at > now => {
                self.set_retry_after(now, *retry_at, *service_unavailable);
            }
            Some(ApiError::RateLimitedUntil(retry_at) | ApiError::UnavailableUntil(retry_at))
                if *retry_at > now =>
            {
                self.throttle_minutes = 0;
                self.set_retry_after(
                    now,
                    *retry_at,
                    matches!(api_error, Some(ApiError::UnavailableUntil(_))),
                );
            }
            Some(
                ApiError::RateLimited
                | ApiError::RateLimitedUntil(_)
                | ApiError::RetryAfter {
                    service_unavailable: false,
                    ..
                },
            ) => {
                self.last_check = Some(now);
                self.throttle_minutes = (self.throttle_minutes.clamp(0, 60) * 2).clamp(5, 60);
                self.retry_after = None;
            }
            _ => {
                self.last_check = Some(now);
                self.throttle_minutes = 0;
                self.retry_after = None;
            }
        }
        if result.is_ok() || self.authentication_failed {
            self.retry_update_check = false;
        }
        self.save(cache_dir)?;
        // A successful `pa` can land after our Keychain comparison. Read its
        // marker after publishing this attempt so it can still clear the pause.
        absorb_authentication_reset(self, cache_dir)?;
        result
    }

    fn is_recent(&self, now: DateTime<Utc>, interval: TimeDelta) -> bool {
        self.last_check
            .is_some_and(|last_check| (TimeDelta::zero()..interval).contains(&(now - last_check)))
    }

    fn resume_after_authentication(&mut self) {
        self.authentication_failed = false;
        self.last_check = None;
        // Retry with the replacement credentials while respecting any successful download.
        self.last_download_attempt = None;
    }

    fn is_pending(&self, now: DateTime<Utc>, interval: TimeDelta) -> bool {
        self.pending
            && self
                .pending_since
                .or(self.last_check)
                .is_some_and(|started| (TimeDelta::zero()..interval).contains(&(now - started)))
    }

    fn active_retry_after(&self, now: DateTime<Utc>) -> Option<&RetryAfter> {
        self.retry_after
            .as_ref()
            .filter(|retry| (retry.recorded_at..retry.deadline).contains(&now))
    }

    fn set_retry_after(
        &mut self,
        recorded_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
        service_unavailable: bool,
    ) {
        self.retry_after = Some(RetryAfter {
            recorded_at,
            deadline,
            service_unavailable,
        });
    }

    fn throttle_interval(&self) -> TimeDelta {
        TimeDelta::minutes(self.throttle_minutes.clamp(0, 60))
    }

    fn defer_download(&mut self, remote_update: Option<DateTime<Utc>>) {
        self.deferred_update = remote_update.or(self.deferred_update);
        self.deferred_reconcile = self.deferred_update.is_none();
    }

    fn check_download_allowed(
        &self,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(retry) = self.active_retry_after(now) {
            return Err(ApiError::RetryAfter {
                retry_at: retry.deadline,
                full_download: true,
                service_unavailable: retry.service_unavailable,
            }
            .into());
        }
        if self.is_recent(now, self.throttle_interval()) {
            return Err(ApiError::RateLimited.into());
        }
        if !force && self.authentication_failed {
            return Err(ApiError::Authentication.into());
        }
        if !force && self.download_on_cooldown(now) {
            return Err(
                "Bookmark cache unavailable. Try again in a few minutes or run pu to retry now."
                    .into(),
            );
        }
        Ok(())
    }

    fn download_on_cooldown(&self, now: DateTime<Utc>) -> bool {
        [self.last_download_attempt, self.last_download]
            .into_iter()
            .flatten()
            .any(|timestamp| (TimeDelta::zero()..DOWNLOAD_INTERVAL).contains(&(now - timestamp)))
    }
}

pub(super) fn last_cache_update(config: &Config) -> Option<DateTime<Utc>> {
    config
        .bookmark_cache_dir()
        .ok()
        .and_then(|cache_dir| SyncState::load(&cache_dir).last_download)
}

pub(super) fn request_authentication_reset(config: &Config) -> Result<(), Box<dyn Error>> {
    let cache_dir = config.bookmark_cache_dir()?;
    if !cache_dir.try_exists()? {
        return Ok(());
    }
    // Record the reset before checking the refresh lock. A slow download must
    // not delay pa's confirmation after the Keychain has accepted the token.
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache_dir.join(AUTHENTICATION_RESET_MARKER))?;
    let lock = open_refresh_lock(&cache_dir)?;
    match lock.try_lock() {
        Ok(()) => absorb_authentication_reset(&mut SyncState::load(&cache_dir), &cache_dir),
        Err(TryLockError::WouldBlock) => Ok(()),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

impl Runner<'_, '_> {
    pub fn update_cache(&mut self) {
        match self.update() {
            Ok(message) => print!("{message}"),
            Err(error) => print!("Error: {}", crate::redact_token(&error.to_string())),
        }
        io::stdout().flush().expect("Couldn't write to stdout");
    }

    pub(super) fn refresh_after_write(&self) {
        if let Err(error) = self.defer_after_write() {
            warn!("Bookmark sync: {}", crate::redact_token(&error.to_string()));
        }
    }

    fn defer_after_write(&self) -> Result<(), Box<dyn Error>> {
        let cache_dir = self.config.as_ref().unwrap().bookmark_cache_dir()?;
        // Leave this marker even if a slow download owns the refresh lock.
        // A refresh that has already started may miss the just-written bookmark.
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache_dir.join(LOCAL_WRITE_MARKER))?;
        Self::start_sync_if_due(&cache_dir)?;
        Ok(())
    }

    pub(crate) fn ensure_cache(&mut self) -> Result<(), Box<dyn Error>> {
        if self.pinboard.as_ref().unwrap().cache_is_available() {
            return Ok(());
        }

        let cache_dir = self.config.as_ref().unwrap().bookmark_cache_dir()?;
        let lock = open_refresh_lock(&cache_dir)?;
        lock.lock()?;
        // Another invocation may have populated the cache while we waited.
        if self.pinboard.as_mut().unwrap().reload_cache().is_err() {
            let mut state = SyncState::load(&cache_dir);
            absorb_authentication_reset(&mut state, &cache_dir)?;
            state.check_download_allowed(Utc::now(), false)?;
            let result = self.download_initial_cache(&mut state, &cache_dir);
            state.finish_attempt(
                &cache_dir,
                &self.config.as_ref().unwrap().auth_token,
                result,
            )?;
        }
        Ok(())
    }

    /// A Script Filter can be replaced while the user types. Leave its first
    /// download to the detached worker and ask Alfred to render again.
    pub(crate) fn ensure_cache_for_filter(&mut self) -> Result<Option<CacheWait>, Box<dyn Error>> {
        let pinboard = self.pinboard.as_mut().unwrap();
        if pinboard.cache_is_available() {
            return Ok(None);
        }
        let cache_dir = self.config.as_ref().unwrap().bookmark_cache_dir()?;
        let lock = open_refresh_lock(&cache_dir)?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(Some(CacheWait::loading())),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        if pinboard.reload_cache().is_ok() {
            return Ok(None);
        }

        let mut state = SyncState::load(&cache_dir);
        absorb_authentication_reset(&mut state, &cache_dir)?;
        let now = Utc::now();
        if !state.is_pending(now, PENDING_TIMEOUT) {
            if let Err(error) = state.check_download_allowed(now, false) {
                let authentication_failed = matches!(
                    error.downcast_ref::<ApiError>(),
                    Some(ApiError::Authentication)
                );
                return Ok(Some(CacheWait {
                    seconds: (!authentication_failed).then_some(DEFERRED_RERUN),
                    error: Some(error.to_string()),
                }));
            }
            queue_sync(&mut state, &cache_dir, now)?;
        }
        Ok(Some(CacheWait::loading()))
    }

    pub(crate) fn schedule_sync(&mut self, query_started_at: DateTime<Utc>) {
        let config = self.config.as_ref().unwrap();
        if !config.auto_update_cache {
            return;
        }
        let Ok(cache_dir) = config.bookmark_cache_dir() else {
            return;
        };
        match Self::start_sync_if_due(&cache_dir) {
            Ok(rerun) => self.rerun_seconds = rerun,
            Err(error)
                if matches!(
                    error.downcast_ref::<ApiError>(),
                    Some(ApiError::Authentication)
                ) =>
            {
                self.sync_authentication_failed = true;
            }
            Err(error) => warn!("Bookmark sync: {}", crate::redact_token(&error.to_string())),
        }
        // The worker may have published the cache while this search was loading it.
        if SyncState::load(&cache_dir)
            .last_download
            .is_some_and(|last_download| last_download >= query_started_at)
        {
            if let Err(error) = self.pinboard.as_mut().unwrap().reload_cache() {
                warn!(
                    "Bookmark cache: {}",
                    crate::redact_token(&error.to_string())
                );
            }
        }
    }

    fn start_sync_if_due(cache_dir: &Path) -> Result<Option<u8>, Box<dyn Error>> {
        let lock = open_refresh_lock(cache_dir)?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(Some(ACTIVE_RERUN)),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }

        let mut state = SyncState::load(cache_dir);
        absorb_authentication_reset(&mut state, cache_dir)?;
        if state.authentication_failed {
            return Err(ApiError::Authentication.into());
        }
        let local_write = absorb_local_write(&mut state, cache_dir)?;
        let now = Utc::now();
        let throttle = state.throttle_interval();
        if state.is_pending(now, PENDING_TIMEOUT) {
            return Ok(Some(ACTIVE_RERUN));
        }
        if state.active_retry_after(now).is_some() {
            return Ok(Some(DEFERRED_RERUN));
        }
        if state.deferred_update.is_some() || state.deferred_reconcile {
            // Alfred accepts reruns up to five seconds. Keep the open search
            // eligible to download what we've already observed.
            let interval = if state.deferred_reconcile && !local_write {
                SYNC_INTERVAL.max(throttle)
            } else {
                throttle
            };
            if state.download_on_cooldown(now) || state.is_recent(now, interval) {
                return Ok(Some(DEFERRED_RERUN));
            }
        } else if state.is_pending(now, SYNC_INTERVAL)
            || state.is_recent(now, SYNC_INTERVAL.max(throttle))
        {
            // Keep polling after an interrupted worker or a failed timestamp
            // check, even when there is no known update to download yet.
            return Ok((state.pending || state.retry_update_check).then_some(DEFERRED_RERUN));
        }

        queue_sync(&mut state, cache_dir, now)?;
        Ok(Some(ACTIVE_RERUN))
    }

    pub(crate) fn sync_cache(&mut self) -> Result<(), Box<dyn Error>> {
        let cache_dir = self.config.as_ref().unwrap().bookmark_cache_dir()?;
        let lock = open_refresh_lock(&cache_dir)?;
        lock.lock()?;
        let mut state = SyncState::load(&cache_dir);
        absorb_authentication_reset(&mut state, &cache_dir)?;
        if !state.pending {
            return Ok(());
        }
        absorb_local_write(&mut state, &cache_dir)?;

        let pinboard = self.pinboard.as_mut().unwrap();
        if !pinboard.cache_is_available() {
            let _ = pinboard.reload_cache();
        }
        let result = if !pinboard.cache_is_available() {
            self.download_initial_cache(&mut state, &cache_dir)
        } else if self.config.as_ref().unwrap().auto_update_cache {
            self.sync_from_pinboard(&mut state, &cache_dir)
        } else {
            state.deferred_update = None;
            state.deferred_reconcile = false;
            Ok(())
        };
        state.finish_attempt(
            &cache_dir,
            &self.config.as_ref().unwrap().auth_token,
            result,
        )
    }

    fn sync_from_pinboard(
        &mut self,
        state: &mut SyncState,
        cache_dir: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let now = Utc::now();
        // Reconcile periodically when multiple server changes share one timestamp.
        let reconcile = state.deferred_reconcile
            || state.last_download.is_none_or(|last_download| {
                !(TimeDelta::zero()..FULL_REFRESH_INTERVAL).contains(&(now - last_download))
            });
        let remote_update = if let Some(observed) = state.deferred_update {
            Some(observed)
        } else {
            let probe = if reconcile {
                self.remote_update_time_for_download(SYNC_CHECK_TIMEOUT)
            } else {
                self.probe_remote_update(SYNC_CHECK_TIMEOUT).map(Some)
            };
            probe.inspect_err(|_| state.retry_update_check = true)?
        };
        if !reconcile && state.deferred_update.is_none() && state.remote_update == remote_update {
            return Ok(());
        }
        if state.download_on_cooldown(Utc::now()) {
            state.defer_download(remote_update);
            return Ok(());
        }
        self.pinboard
            .as_mut()
            .unwrap()
            .set_request_timeout(Some(SYNC_DOWNLOAD_TIMEOUT));
        // Record the server timestamp seen before the download so a later edit
        // still triggers the next refresh.
        self.download_cache(state, cache_dir, remote_update)
            .map(|_| ())
    }

    fn update(&mut self) -> Result<&'static str, Box<dyn Error>> {
        let cache_dir = self.config.as_ref().unwrap().bookmark_cache_dir()?;
        let lock = open_refresh_lock(&cache_dir)?;
        lock.lock()?;
        let mut state = SyncState::load(&cache_dir);
        absorb_authentication_reset(&mut state, &cache_dir)?;
        state.check_download_allowed(Utc::now(), true)?;
        absorb_local_write(&mut state, &cache_dir)?;
        let auto_update_cache = self.config.as_ref().unwrap().auto_update_cache;
        let result = self
            .remote_update_time_for_download(WATERMARK_TIMEOUT)
            .inspect_err(|_| state.retry_update_check = true)
            .and_then(|remote_update| {
                self.pinboard
                    .as_mut()
                    .unwrap()
                    .set_request_timeout(Some(SYNC_DOWNLOAD_TIMEOUT));
                self.download_cache(&mut state, &cache_dir, remote_update)
            })
            .map(|refresh| match refresh {
                CacheRefresh::Complete => "Updated cache files!",
                CacheRefresh::Partial if auto_update_cache => {
                    "Updated the cache with the readable bookmarks. Some will be retried."
                }
                CacheRefresh::Partial => {
                    "Updated the cache with the readable bookmarks. Run pu again to retry the rest."
                }
            });
        state.finish_attempt(
            &cache_dir,
            &self.config.as_ref().unwrap().auth_token,
            result,
        )
    }

    fn download_initial_cache(
        &mut self,
        state: &mut SyncState,
        cache_dir: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let remote_update = self.remote_update_time_for_download(WATERMARK_TIMEOUT)?;
        let pinboard = self.pinboard.as_mut().unwrap();
        pinboard.set_request_timeout(Some(SYNC_DOWNLOAD_TIMEOUT));
        let result = self.download_cache(state, cache_dir, remote_update);
        self.pinboard.as_mut().unwrap().set_request_timeout(None);
        result.map(|_| ())
    }

    fn probe_remote_update(&mut self, timeout: Duration) -> Result<DateTime<Utc>, Box<dyn Error>> {
        let pinboard = self.pinboard.as_mut().unwrap();
        pinboard.set_request_timeout(Some(timeout));
        let result = pinboard.remote_update_time();
        pinboard.set_request_timeout(None);
        result
    }

    fn remote_update_time_for_download(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<DateTime<Utc>>, Box<dyn Error>> {
        match self.probe_remote_update(timeout) {
            Ok(timestamp) => Ok(Some(timestamp)),
            Err(error)
                if matches!(
                    error.downcast_ref::<ApiError>(),
                    Some(
                        ApiError::RateLimited
                            | ApiError::RateLimitedUntil(_)
                            | ApiError::UnavailableUntil(_)
                            | ApiError::RetryAfter { .. }
                            | ApiError::Authentication
                    )
                ) =>
            {
                Err(error)
            }
            // The full-download endpoint can be healthy when this check fails.
            Err(_) => Ok(None),
        }
    }

    fn download_cache(
        &mut self,
        state: &mut SyncState,
        cache_dir: &Path,
        remote_update: Option<DateTime<Utc>>,
    ) -> Result<CacheRefresh, Box<dyn Error>> {
        // The download budget applies even if a later request fails or this
        // process is interrupted before it can publish the snapshot.
        state.defer_download(remote_update);
        let previous_attempt = state.last_download_attempt.replace(Utc::now());
        state.save(cache_dir)?;
        let pinboard = self.pinboard.as_mut().unwrap();
        // A different process may have refreshed the cache before we obtained
        // the lock. Validate the response against its most recent publication.
        let _ = pinboard.reload_cache();
        let refresh = pinboard.refresh_cache().inspect_err(|error| {
            if matches!(
                error.downcast_ref::<ApiError>(),
                Some(ApiError::RetryAfter {
                    full_download: true,
                    ..
                })
            ) {
                state.last_download_attempt = previous_attempt;
            }
        })?;
        state.last_download = Some(Utc::now());
        if refresh == CacheRefresh::Complete {
            state.remote_update = remote_update;
            state.deferred_update = None;
            state.deferred_reconcile = false;
        }
        Ok(refresh)
    }
}

fn absorb_authentication_reset(
    state: &mut SyncState,
    cache_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let marker = cache_dir.join(AUTHENTICATION_RESET_MARKER);
    if marker.try_exists()? {
        if state.authentication_failed {
            state.resume_after_authentication();
            state.retry_update_check = true;
            state.save(cache_dir)?;
        }
        fs::remove_file(marker)?;
    }
    Ok(())
}

fn absorb_local_write(state: &mut SyncState, cache_dir: &Path) -> Result<bool, Box<dyn Error>> {
    let marker = cache_dir.join(LOCAL_WRITE_MARKER);
    if !marker.try_exists()? {
        return Ok(false);
    }
    // Under the refresh lock, save the intent before consuming the marker.
    // A write that shares this marker completed before any ensuing download.
    state.defer_download(None);
    state.save(cache_dir)?;
    fs::remove_file(marker)?;
    Ok(true)
}

fn open_refresh_lock(cache_dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache_dir.join("bookmark-sync.lock"))
}

fn queue_sync(
    state: &mut SyncState,
    cache_dir: &Path,
    now: DateTime<Utc>,
) -> Result<(), Box<dyn Error>> {
    // Reserve the worker before spawning so another keystroke doesn't start a second one.
    state.pending_since = Some(now);
    state.pending = true;
    state.save(cache_dir)?;
    if let Err(error) = spawn_sync() {
        state.pending = false;
        state.pending_since = None;
        state.save(cache_dir)?;
        return Err(error.into());
    }
    Ok(())
}

fn spawn_sync() -> io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("sync-bookmarks")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()?;
    Ok(())
}
