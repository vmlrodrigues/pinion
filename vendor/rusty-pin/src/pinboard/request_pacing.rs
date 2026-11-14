use chrono::{DateTime, TimeDelta, Utc};
use reqwest::blocking::{RequestBuilder, Response};
use reqwest::{header::RETRY_AFTER, StatusCode};
use std::error::Error;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;
use tempfile::NamedTempFile;

use super::ApiError;

const ORDINARY_REQUEST_INTERVAL: TimeDelta = TimeDelta::seconds(3);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestKind {
    Ordinary,
    FullDownload,
    Background,
    Optional,
}

#[derive(Deserialize, Serialize)]
struct RequestWindow {
    recorded_at: DateTime<Utc>,
    available_at: DateTime<Utc>,
    retry_after: Option<DateTime<Utc>>,
    download_retry_after: Option<DateTime<Utc>>,
    service_unavailable_until: Option<DateTime<Utc>>,
}

pub(super) fn send(
    cache_dir: &Path,
    request: RequestBuilder,
    kind: RequestKind,
) -> Result<Result<Response, reqwest::Error>, Box<dyn Error>> {
    let full_download = kind == RequestKind::FullDownload;
    let path = cache_dir.join("pinboard-requests.json");
    let (lock, prior_window) = loop {
        let lock = acquire_window_lock(cache_dir, kind)?;
        let now = Utc::now();
        let Some(window) = read_window(&path, now)? else {
            break (lock, None);
        };
        let rate_limited_until = if full_download {
            window.retry_after.max(window.download_retry_after)
        } else {
            window.retry_after
        };
        let maintenance_until = window.service_unavailable_until;
        if let Some(retry_at) = rate_limited_until
            .max(maintenance_until)
            .filter(|time| *time > now)
        {
            return Err(ApiError::RetryAfter {
                retry_at,
                full_download,
                service_unavailable: maintenance_until == Some(retry_at),
            }
            .into());
        }
        let remaining = (window.available_at - now).min(ORDINARY_REQUEST_INTERVAL);
        if full_download || remaining <= TimeDelta::zero() {
            break (lock, Some(window));
        }
        if kind == RequestKind::Optional {
            return Err(ApiError::RequestDeferred.into());
        }
        // Let in-flight exports record a Retry-After while we wait for a slot.
        drop(lock);
        std::thread::sleep(remaining.to_std()?);
    };
    if full_download {
        // The caller already enforces the longer bookmark download cooldown.
        // A download must never hold the lock while it waits for its response.
        drop(lock);
        return send_without_window_lock(cache_dir, &path, request, kind);
    }

    // Reserve before calling the network so interrupting a script filter does
    // not free a slot for a request that may have already reached Pinboard.
    let mut window = prior_window.unwrap_or_default();
    window.reserve_slot();
    write_window(&path, &window)?;
    if matches!(kind, RequestKind::Background | RequestKind::Optional) {
        // Background checks and suggestions reserve an ordinary slot without
        // making foreground requests wait for their response headers.
        drop(lock);
        return send_without_window_lock(cache_dir, &path, request, kind);
    }
    let response = request.send();
    let backoff = response.as_ref().ok().and_then(retry_after);
    // Space requests from receipt of the headers as connection time is variable.
    // The body is read outside this lock.
    window.reserve_slot();
    if let Some((retry_at, status)) = backoff {
        window.record_backoff(retry_at, status, kind);
    }
    extend_reservation(write_window(&path, &window));
    drop(lock);
    response_with_backoff(response, backoff)
}

fn send_without_window_lock(
    cache_dir: &Path,
    path: &Path,
    request: RequestBuilder,
    kind: RequestKind,
) -> Result<Result<Response, reqwest::Error>, Box<dyn Error>> {
    let response = request.send();
    let backoff = response.as_ref().ok().and_then(retry_after);
    if let Some((retry_at, status)) = backoff {
        extend_reservation(record_backoff(cache_dir, path, retry_at, status, kind));
    }
    response_with_backoff(response, backoff)
}

fn response_with_backoff(
    response: Result<Response, reqwest::Error>,
    backoff: Option<(DateTime<Utc>, StatusCode)>,
) -> Result<Result<Response, reqwest::Error>, Box<dyn Error>> {
    if let Some((retry_at, status)) = backoff {
        return Err(if status == StatusCode::SERVICE_UNAVAILABLE {
            ApiError::UnavailableUntil(retry_at)
        } else {
            ApiError::RateLimitedUntil(retry_at)
        }
        .into());
    }
    Ok(response)
}

impl Default for RequestWindow {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            recorded_at: now,
            available_at: now,
            retry_after: None,
            download_retry_after: None,
            service_unavailable_until: None,
        }
    }
}

impl RequestWindow {
    fn reserve_slot(&mut self) {
        self.recorded_at = Utc::now();
        self.available_at = self.recorded_at + ORDINARY_REQUEST_INTERVAL;
    }

    fn record_backoff(&mut self, retry_at: DateTime<Utc>, status: StatusCode, kind: RequestKind) {
        let deadline = if status == StatusCode::SERVICE_UNAVAILABLE {
            &mut self.service_unavailable_until
        } else if kind == RequestKind::FullDownload {
            &mut self.download_retry_after
        } else {
            &mut self.retry_after
        };
        *deadline = Some(deadline.map_or(retry_at, |prior| prior.max(retry_at)));
    }
}

fn extend_reservation(result: Result<(), Box<dyn Error>>) {
    if let Err(error) = result {
        // The request may have already changed a bookmark. Report its actual
        // result even if extending the reservation fails.
        warn!("Couldn't extend the Pinboard request reservation: {error}");
    }
}

fn open_window_lock(cache_dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache_dir.join("pinboard-requests.lock"))
}

fn acquire_window_lock(cache_dir: &Path, kind: RequestKind) -> Result<File, Box<dyn Error>> {
    if kind != RequestKind::Optional {
        return Ok(lock_window(cache_dir)?);
    }
    let lock = open_window_lock(cache_dir)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(ApiError::RequestDeferred.into()),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn lock_window(cache_dir: &Path) -> io::Result<File> {
    let lock = open_window_lock(cache_dir)?;
    lock.lock()?;
    Ok(lock)
}

fn read_window(path: &Path, now: DateTime<Utc>) -> Result<Option<RequestWindow>, Box<dyn Error>> {
    match File::open(path) {
        Ok(file) => match serde_json::from_reader::<_, RequestWindow>(file) {
            Ok(window) => Ok((now >= window.recorded_at).then_some(window)),
            Err(error) if !error.is_io() => Ok(None),
            Err(error) => Err(error.into()),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_window(path: &Path, window: &RequestWindow) -> Result<(), Box<dyn Error>> {
    let mut file = NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    serde_json::to_writer(file.as_file_mut(), window)?;
    file.persist(path)?;
    Ok(())
}

fn record_backoff(
    cache_dir: &Path,
    path: &Path,
    retry_at: DateTime<Utc>,
    status: StatusCode,
    kind: RequestKind,
) -> Result<(), Box<dyn Error>> {
    let _lock = lock_window(cache_dir)?;
    let mut window = read_window(path, Utc::now())?.unwrap_or_default();
    window.record_backoff(retry_at, status, kind);
    write_window(path, &window)
}

fn retry_after(response: &Response) -> Option<(DateTime<Utc>, StatusCode)> {
    let status = response.status();
    if !matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        return None;
    }
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    let now = Utc::now();
    let deadline = if let Ok(seconds) = value.parse::<u64>() {
        let delay = TimeDelta::try_seconds(i64::try_from(seconds).ok()?)?;
        now.checked_add_signed(delay)
    } else {
        DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| date.with_timezone(&Utc))
    };
    deadline
        .filter(|deadline| *deadline > now)
        .map(|deadline| (deadline, status))
}
