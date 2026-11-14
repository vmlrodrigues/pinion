use crate::workflow_config::Config;
use alfred_rs::Data;
use chrono::{DateTime, TimeDelta, Utc};
use rusty_pin::{pinboard::ApiError, Pinboard};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::{env, thread, time};

const SUGGESTION_TIMEOUT: time::Duration = time::Duration::from_secs(5);
const SUGGESTION_JOB_LIFETIME: TimeDelta = TimeDelta::seconds(15);
const SUGGESTION_RESULT_RETENTION: TimeDelta = TimeDelta::minutes(1);
pub(super) const SUGGESTION_RETRY_DELAY: TimeDelta = TimeDelta::seconds(5);

#[derive(Deserialize, Serialize)]
pub(super) enum Suggestions {
    Available(Vec<String>),
    Deferred(Option<DateTime<Utc>>),
    Authentication(Option<DateTime<Utc>>),
}

pub(super) fn authentication_generation(config: &Config) -> Option<DateTime<Utc>> {
    // pa rewrites settings after saving credentials. This lets an open session
    // wait for that change without storing the token or repeating denied requests.
    fs::metadata(
        config
            .data_dir()
            .join(crate::workflow_config::CONFIG_FILE_NAME),
    )
    .and_then(|metadata| metadata.modified())
    .ok()
    .map(DateTime::<Utc>::from)
}

pub(super) fn poll_suggestions(
    cache_dir: &Path,
    account: &str,
    url: &str,
    pending_job: &mut Option<String>,
) -> Result<Option<Suggestions>, Box<dyn Error>> {
    let directory = cache_dir.join("post-tag-suggestions");
    if let Some(job) = pending_job.as_deref() {
        let age = job_age(job).ok_or("Invalid tag suggestion request")?;
        let result = Data::read_data_from_disk(&directory.join(job)).ok();
        if result.is_some() {
            *pending_job = None;
        } else if !(TimeDelta::zero()..SUGGESTION_JOB_LIFETIME).contains(&age) {
            return Err("Tag suggestion lookup did not complete".into());
        }
        return Ok(result);
    }

    fs::create_dir_all(&directory)?;
    for entry in fs::read_dir(&directory)?.flatten() {
        if entry
            .file_name()
            .to_str()
            .and_then(job_age)
            .is_some_and(|age| age > SUGGESTION_RESULT_RETENTION)
        {
            let _ = fs::remove_file(entry.path());
        }
    }

    let job = format!("{}-{}", Utc::now().timestamp_millis(), process::id());
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("suggest-post-tags")
        .arg(format!("--url={url}"))
        .arg(format!("--account={account}"))
        .arg(format!("--job={job}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut worker = command.spawn()?;
    let path = directory.join(&job);
    let started = time::Instant::now();
    // Preserve the quick first result, but let the child survive a filter rerun
    // or cancellation so a later response can carry its actual Retry-After.
    while started.elapsed() < time::Duration::from_secs(1) {
        if let Ok(result) = Data::read_data_from_disk(&path) {
            return Ok(Some(result));
        }
        if worker.try_wait()?.is_some() {
            return Ok(Some(Data::read_data_from_disk(&path)?));
        }
        thread::sleep(time::Duration::from_millis(20));
    }
    *pending_job = Some(job);
    Ok(None)
}

fn job_age(job: &str) -> Option<TimeDelta> {
    let (started_at, pid) = job.split_once('-')?;
    pid.parse::<u32>().ok()?;
    let started_at = DateTime::from_timestamp_millis(started_at.parse().ok()?)?;
    Some(Utc::now() - started_at)
}

pub(crate) fn suggest_post_tags(url: &str, account: &str, job: &str) -> Result<(), Box<dyn Error>> {
    let config = Config::setup()?;
    if config.pinboard_username()? != account || job_age(job).is_none() {
        return Ok(());
    }
    let cache_dir = config.bookmark_cache_dir()?;
    let generation = authentication_generation(&config);
    let mut pinboard = Pinboard::open(config.auth_token.clone(), Some(&cache_dir))?;
    pinboard.enable_request_pacing(&cache_dir);
    pinboard.set_request_timeout(Some(SUGGESTION_TIMEOUT));
    let suggestions = match pinboard.popular_tags(url) {
        Ok(tags) => Suggestions::Available(tags),
        Err(error) => match error.downcast_ref::<ApiError>() {
            Some(ApiError::RequestDeferred) => Suggestions::Deferred(None),
            Some(ApiError::Authentication) => {
                if crate::keychain::get().is_some_and(|token| token != config.auth_token) {
                    Suggestions::Deferred(None)
                } else {
                    Suggestions::Authentication(generation)
                }
            }
            Some(
                ApiError::RetryAfter { retry_at, .. }
                | ApiError::RateLimitedUntil(retry_at)
                | ApiError::UnavailableUntil(retry_at),
            ) => {
                let now = Utc::now();
                let deadline = if *retry_at > now {
                    *retry_at
                } else {
                    now + SUGGESTION_RETRY_DELAY
                };
                Suggestions::Deferred(Some(deadline))
            }
            Some(
                ApiError::RateLimited
                | ApiError::TemporaryServerError(_)
                | ApiError::Network(_)
                | ApiError::SerdeError(_)
                | ApiError::UnrecognizedResponse(_),
            ) => Suggestions::Deferred(Some(Utc::now() + SUGGESTION_RETRY_DELAY)),
            _ => {
                warn!(
                    "Fetching popular tags: {}",
                    crate::redact_token(&error.to_string())
                );
                Suggestions::Available(Vec::new())
            }
        },
    };
    let directory = cache_dir.join("post-tag-suggestions");
    Ok(Data::write_data_to_disk(directory.join(job), &suggestions)?)
}
