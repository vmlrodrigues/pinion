use super::{anyhow, parse_workflow_version, reqwest, semver, serde_json, url, Result};
#[cfg(test)]
use mockito;
use semver::Version;
use std::cell::RefCell;
use std::time::Duration;
use url::Url;

#[cfg(not(test))]
const GITHUB_API_URL: &str = "https://api.github.com/repos/";
const GITHUB_LATEST_RELEASE_ENDPOINT: &str = "/releases/latest";

#[cfg(test)]
#[allow(deprecated)]
static MOCKITO_URL: &str = mockito::SERVER_URL;
#[cfg(test)]
pub const MOCK_RELEASER_REPO_NAME: &str = "MockZnVja29mZg==fd850fc2e63511e79f720023dfdf24ec";

/// Result of a successful workflow release check.
pub enum ReleaseCheck {
    /// A release with an uploaded workflow file.
    Available {
        version: Version,
        download_url: Url,
        /// The immutable asset identity for releasers that expose one.
        asset_api_url: Option<Url>,
    },
    /// A release exists, but its workflow file could still be uploading.
    MissingAsset,
    /// The server has no release to offer.
    Unavailable,
}

/// An interface for checking with remote servers to identify the latest release for an
/// Alfred workflow.
///
/// This trait has been implemented for [`GithubReleaser`] to check for a newer version of a workflow
/// that's maintained on `github.com`
///
/// [`GithubReleaser`]: struct.GithubReleaser.html
pub trait Releaser: Clone {
    /// Typte that represents semantic compatible identifier of a release.
    type SemVersion: Into<Version>;

    /// Type that represents a url to the latest release resource.
    type DownloadLink: Into<Url>;

    /// Creates a new `Releaser` instance that is identified as `name`
    fn new<S: Into<String>>(name: S) -> Self;

    /// Performs necessary communications to obtain release info in form of
    /// `SemVersion` and `DownloadLink` types.
    ///
    /// Returned tuple consists of semantic version compatible identifier of the release and
    /// a download link/url that can be used to fetch the release.
    ///
    /// Implementors are strongly encouraged to get the meta-data about the latest release without
    /// performing a full download of the workflow.
    ///
    /// # Errors
    /// Method returns `Err(Error)` on file or network error.
    fn fetch_latest_release(&self) -> Result<(Self::SemVersion, Self::DownloadLink)>;

    /// Returns the latest release information that is available from server.
    ///
    /// # Errors
    /// Method returns `Err(Error)` on file or network error.
    fn latest_release(&self) -> Result<(Version, Url)> {
        let (v, url) = self.fetch_latest_release()?;
        Ok((v.into(), url.into()))
    }

    /// Check whether a release and its workflow file are available.
    ///
    /// Transient failures are returned as errors so a previously checked release
    /// can remain available.
    fn check_latest_release(&self) -> Result<ReleaseCheck> {
        let (version, download_url) = self.latest_release()?;
        Ok(ReleaseCheck::Available {
            version,
            download_url,
            asset_api_url: None,
        })
    }

    /// Whether a fresh release check supplies immutable asset identities.
    fn provides_asset_identity(&self) -> bool {
        false
    }
}

/// Whether GitHub confirms that a release or asset is missing.
pub fn is_missing_response(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .map_or(false, |status| {
            status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE
        })
}

/// Struct to handle checking and finding release files from `github.com`
///
/// This implementation of `Releaser` will favor files that end with `alfred3workflow`
/// over `alfredworkflow`. If there are multiple `alfred3workflow`s or `alfredworkflow`s, the first
/// one returned by `github.com` will be used.
///
/// See [`updater::gh()`] for how to use this.
///
/// [`updater::gh()`]: struct.Updater.html#method.gh
#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GithubReleaser {
    repo: String,
    latest_release: RefCell<Option<ReleaseItem>>,
}

// Struct to store information about a single release point.
//
// Each release point may have multiple downloadable assets.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReleaseItem {
    /// name of release that should hold a semver compatible identifier.
    pub tag_name: String,
    assets: Vec<ReleaseAsset>,
}

/// A single downloadable asset.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct ReleaseAsset {
    url: Url,
    state: String,
    browser_download_url: String,
}

impl GithubReleaser {
    fn latest_release_data(&self) -> Result<()> {
        debug!("starting latest_release_data");
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;

        #[cfg(test)]
        let url = format!("{}{}", MOCKITO_URL, GITHUB_LATEST_RELEASE_ENDPOINT);

        #[cfg(not(test))]
        let url = format!(
            "{}{}{}",
            GITHUB_API_URL, self.repo, GITHUB_LATEST_RELEASE_ENDPOINT
        );
        debug!("  url is: {:?}", url);

        client
            .get(&url)
            .send()?
            .error_for_status()
            .map_err(Into::into)
            .and_then(|resp| {
                let latest: ReleaseItem = serde_json::from_reader(resp)?;
                debug!("  release item: {:?}", latest);
                *self.latest_release.borrow_mut() = Some(latest);
                Ok(())
            })
    }

    // Favor the unversioned Alfred workflow extension when several are present.
    fn downloadable_asset(&self) -> Result<Option<(Url, Url)>> {
        self.latest_release
            .borrow()
            .as_ref()
            .ok_or_else(|| {
                anyhow!(
                "no release item available, did you first get version by calling latest_version?",
            )
            })
            .and_then(|r| {
                r.assets
                    .iter()
                    .filter(|asset| {
                        asset.state == "uploaded"
                            && (asset.browser_download_url.ends_with("alfredworkflow")
                                || asset.browser_download_url.ends_with("alfred3workflow")
                                || asset.browser_download_url.ends_with("alfred4workflow"))
                    })
                    .min_by_key(|asset| !asset.browser_download_url.ends_with("alfredworkflow"))
                    .map(|asset| Ok((Url::parse(&asset.browser_download_url)?, asset.url.clone())))
                    .transpose()
            })
    }

    fn latest_version(&self) -> Result<Version> {
        debug!("starting latest_version");
        if self.latest_release.borrow().is_none() {
            self.latest_release_data()?;
        }

        let release = self.latest_release.borrow();
        let tag = &release
            .as_ref()
            .ok_or_else(|| anyhow!("No release information is available."))?
            .tag_name;
        let latest_version = parse_workflow_version(tag)?;
        debug!("  latest version: {:?}", latest_version);
        Ok(latest_version)
    }
}

impl Releaser for GithubReleaser {
    type SemVersion = Version;
    type DownloadLink = Url;

    fn new<S: Into<String>>(repo_name: S) -> GithubReleaser {
        GithubReleaser {
            repo: repo_name.into(),
            latest_release: RefCell::new(None),
        }
    }

    fn provides_asset_identity(&self) -> bool {
        true
    }

    fn fetch_latest_release(&self) -> Result<(Version, Url)> {
        let version = self.latest_version()?;
        let (link, _) = self
            .downloadable_asset()?
            .ok_or_else(|| anyhow!("no usable download url"))?;
        Ok((version, link))
    }

    fn check_latest_release(&self) -> Result<ReleaseCheck> {
        match self.latest_release_data() {
            Err(error) if is_missing_response(&error) => {
                *self.latest_release.borrow_mut() = None;
                return Ok(ReleaseCheck::Unavailable);
            }
            result => result?,
        }
        // Check for an asset before parsing the tag. A release without a
        // workflow file cannot be offered even if its tag is invalid.
        Ok(match self.downloadable_asset()? {
            Some((download_url, asset_api_url)) => ReleaseCheck::Available {
                version: self.latest_version()?,
                download_url,
                asset_api_url: Some(asset_api_url),
            },
            None => ReleaseCheck::MissingAsset,
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use mockito::{mock, Mock};

    #[test]
    fn it_tests_releaser() {
        let _m = setup_mock_server(200);
        let releaser = GithubReleaser::new(MOCK_RELEASER_REPO_NAME);

        assert!(releaser.downloadable_asset().is_err());

        assert!(
            releaser
                .latest_version()
                .expect("couldn't do latest_version")
                > Version::new(0, 11, 0)
        );

        let (url, asset_api_url) = releaser.downloadable_asset().unwrap().unwrap();
        assert_eq!("http://127.0.0.1:1234/releases/download/v0.11.1/alfred-pinboard-rust-v0.11.1.alfredworkflow", url.as_str());
        assert_eq!(
            asset_api_url.as_str(),
            "http://127.0.0.1:1234/releases/assets/6847236"
        );
    }

    #[test]
    fn it_reports_invalid_release_versions() {
        let _release = mock("GET", "/releases/latest")
            .with_status(200)
            .with_body(include_str!("../../tests/latest.json").replace("v0.11.1", "rolling"))
            .create();
        let releaser = GithubReleaser::new(MOCK_RELEASER_REPO_NAME);

        assert!(releaser.fetch_latest_release().is_err());
    }

    pub fn setup_mock_server(status_code: usize) -> Mock {
        mock("GET", "/releases/latest")
            .with_status(status_code)
            .with_body(include_str!("../../tests/latest.json"))
            .create()
    }
}
