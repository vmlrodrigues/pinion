use self::releaser::tests::setup_mock_server;
// #[cfg(not(feature = "ci"))]
use self::releaser::GithubReleaser;
use self::releaser::MOCK_RELEASER_REPO_NAME;
use super::*;
use std::ffi::OsStr;
use std::{thread, time};
use tempfile::Builder;
const VERSION_TEST: &str = "0.10.5";
const VERSION_TEST_NEW: &str = "0.11.1"; // should match what the mock server replies for new version.

fn mock_workflow_bundle() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("info.plist", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>bundleid</key><string>test.alfred.workflow</string>
<key>name</key><string>Test workflow</string>
<key>version</key><string>0.11.1</string>
<key>objects</key><array/><key>connections</key><dict/>
</dict></plist>"#,
    )
    .unwrap();
    zip.finish().unwrap().into_inner()
}

#[test]
fn unicode_path_overrides_cannot_select_a_different_manifest() {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (raw_name, crc, unicode_name, version) in [
        ("info.plist", 0x905b_2b47_u32, "infoé.plist", "1.0.0"),
        ("info~1.plist", 0x8599_9943_u32, "info.plist", "1.1.0"),
    ] {
        let mut unicode_path = vec![1];
        // Info-ZIP's field starts with the CRC-32 of the original header name.
        unicode_path.extend_from_slice(&crc.to_le_bytes());
        unicode_path.extend_from_slice(unicode_name.as_bytes());
        let mut options = zip::write::FullFileOptions::default();
        options
            .add_extra_field(0x7075, unicode_path, false)
            .unwrap();
        zip.start_file(raw_name, options).unwrap();
        let manifest = format!(
            "<plist version=\"1.0\"><dict>\
             <key>bundleid</key><string>test.alfred.workflow</string>\
             <key>name</key><string>Test workflow</string>\
             <key>version</key><string>{version}</string>\
             <key>objects</key><array/><key>connections</key><dict/>\
             </dict></plist>"
        );
        zip.write_all(manifest.as_bytes()).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
    archive.validate_header_consistency().unwrap();
    // The ZIP library sees the selected version at a name that macOS extracts
    // as info~1.plist, while the original info.plist contains the old version.
    assert_eq!(archive.by_index(0).unwrap().name().unwrap(), "infoé.plist");
    assert_eq!(archive.by_index(1).unwrap().name().unwrap(), "info.plist");
    let error = validate_selected_workflow_bundle(
        Cursor::new(bytes),
        "test.alfred.workflow",
        &Version::parse("1.1.0").unwrap(),
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("alternate ZIP filenames"));
}

#[test]
fn utf8_filenames_without_overrides_remain_valid() {
    let mut zip = zip::ZipWriter::new_append(Cursor::new(mock_workflow_bundle())).unwrap();
    zip.start_file("café.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"ordinary UTF-8 filename").unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    validate_selected_workflow_bundle(
        Cursor::new(bytes),
        "test.alfred.workflow",
        &Version::parse(VERSION_TEST_NEW).unwrap(),
        None,
    )
    .unwrap();
}

fn serve_workflow_bundle() -> mockito::Mock {
    mockito::mock("GET", "/releases/assets/6847236")
        .with_header("Content-Type", "application/octet-stream")
        .with_body(mock_workflow_bundle())
        .create()
}

#[test]
fn it_tests_settings_filename() {
    setup_workflow_env_vars(true);
    let updater_state_fn = Updater::<GithubReleaser>::build_data_fn().unwrap();
    assert_eq!(
        "workflow.B0AC54EC-601C-YouForgotTo___Name_Your_Own_Work_flow_-updater.json",
        updater_state_fn.file_name().unwrap().to_str().unwrap()
    );
}

#[test]
fn it_checks_immediately_with_an_empty_cache() {
    setup_workflow_env_vars(true);
    let release = setup_mock_server(200);
    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    updater.set_interval(0);
    updater.init().expect("couldn't init worker");

    assert!(updater.update_ready().expect("couldn't check for update"));
    release.assert();
}

#[test]
fn it_loads_a_checked_newer_version_from_disk() {
    setup_workflow_env_vars(true);
    let response = setup_mock_server(200).expect(1);
    let mut checker = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
    checker.set_interval(0);
    checker.init().unwrap();
    assert!(checker.update_ready().unwrap());

    let updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
    let expected = Version::parse(VERSION_TEST_NEW).unwrap();
    assert_eq!(updater.load_cached_newer_version(), Some(expected));
    let mut cached = serde_json::to_value(updater.state.newer_release().unwrap()).unwrap();
    assert_eq!(
        cached["asset_api_url"],
        "http://127.0.0.1:1234/releases/assets/6847236"
    );
    cached.as_object_mut().unwrap().remove("asset_api_url");
    crate::Data::save_to_file(
        Updater::<GithubReleaser>::release_cache_path().unwrap(),
        &cached,
    )
    .unwrap();
    assert_eq!(
        updater.load_cached_newer_version(),
        Some(Version::parse(VERSION_TEST_NEW).unwrap())
    );
    assert_eq!(updater.state.newer_release().unwrap().asset_api_url, None);
    let legacy_download = mockito::mock(
        "GET",
        "/releases/download/v0.11.1/alfred-pinboard-rust-v0.11.1.alfredworkflow",
    )
    .with_body(mock_workflow_bundle())
    .create();
    let path = updater.download_latest().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), mock_workflow_bundle());
    legacy_download.assert();
    response.assert();
}

#[test]
fn a_missing_release_clears_an_offer_but_a_transient_error_preserves_it() {
    for unavailable in [404, 410, 200] {
        setup_workflow_env_vars(true);
        {
            let _response = setup_mock_server(200);
            let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
            updater.set_interval(0);
            updater.init().unwrap();
            assert!(updater.update_ready().unwrap());
        }
        {
            let _response = setup_mock_server(503);
            let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
            updater.set_interval(0);
            updater.init().unwrap();
            assert!(updater.update_ready().is_err());
            assert!(updater.load_cached_newer_version().is_some());
        }
        {
            let response = mockito::mock("GET", "/releases/latest")
                .with_status(unavailable)
                .with_body(r#"{"tag_name":"v0.11.1","assets":[]}"#)
                .create();
            let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
            updater.set_interval(0);
            updater.init().unwrap();
            assert!(!updater.update_ready().unwrap());
            assert!(updater.latest_avail_version().is_none());
            response.assert();
        }
        let updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
        assert!(updater.load_cached_newer_version().is_none());
        assert!(!updater.due_to_check());
        assert!(updater.download_latest().is_err());
    }
}

#[test]
fn a_missing_asset_can_only_clear_its_own_cached_offer() {
    setup_workflow_env_vars(true);
    let asset = mockito::mock("GET", "/releases/assets/6847236")
        .match_header("range", mockito::Matcher::Missing)
        .with_status(404)
        .expect(4)
        .create();
    let mut checker = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
    checker.set_interval(0);
    {
        let release = mockito::mock("GET", "/releases/latest")
            .with_status(200)
            .with_body(include_str!("../../tests/latest.json"))
            .create();
        checker.init().unwrap();
        assert!(checker.update_ready().unwrap());
        release.assert();
    }
    let downloader = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
    let before_invalidation = downloader.state.last_successful_check.get().unwrap();
    let version = downloader.load_cached_newer_version().unwrap();
    let error = downloader.download_version(&version).unwrap_err();
    assert!(is_missing_response(&error));
    downloader.forget_cached_release(Utc::now()).unwrap();
    assert!(checker.load_cached_newer_version().is_none());
    assert!(checker.latest_avail_version().is_none());
    assert!(checker.download_latest().is_err());
    let after_invalidation = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
    // The asset failure cannot stand in for a GitHub release check, even when
    // the previously successful check finished during the caller's wait.
    assert_eq!(after_invalidation.checked_since(before_invalidation), None);
    assert!(!after_invalidation.due_to_check());

    {
        let release = mockito::mock("GET", "/releases/latest")
            .with_status(200)
            .with_body(include_str!("../../tests/latest.json"))
            .create();
        checker.init().unwrap();
        assert!(!checker.update_ready().unwrap());
        let error = downloader.download_version(&version).unwrap_err();
        assert!(is_missing_response(&error));
        downloader.forget_cached_release(Utc::now()).unwrap();
        assert!(checker.load_cached_newer_version().is_none());
        release.assert();
    }

    let release = mockito::mock("GET", "/releases/latest")
        .with_status(200)
        .with_body(
            include_str!("../../tests/latest.json")
                .replace("v0.11.1", "v0.12.0")
                .replace("6847236", "6847238"),
        )
        .create();
    checker.init().unwrap();
    assert!(checker.update_ready().unwrap());
    let error = downloader.download_version(&version).unwrap_err();
    assert!(is_missing_response(&error));
    downloader.forget_cached_release(Utc::now()).unwrap();
    assert_eq!(
        checker.load_cached_newer_version(),
        Some(Version::new(0, 12, 0))
    );
    asset.assert();
    release.assert();
}

#[test]
fn it_ignores_saved_version_after_an_upgrade_async() {
    // Make sure a freshly upgraded workflow does not use version info from saved state
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);
    first_check_after_installing_workflow();

    {
        // Next check it reports a new version since mock server has a release for us
        let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
        updater.set_interval(0);
        updater.init().expect("couldn't init worker");

        assert!(updater.update_ready().expect("couldn't check for update"));
        assert_eq!(VERSION_TEST, format!("{}", updater.current_version()));
    }

    // Mimic the upgrade process by bumping the version
    StdEnv::set_var("alfred_workflow_version", VERSION_TEST_NEW);
    {
        let updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
        // Updater should pick up new version rather than using saved one
        assert_eq!(VERSION_TEST_NEW, format!("{}", updater.current_version()));
        // The saved URL from the previous version cannot trigger a reinstall.
        assert!(updater.download_latest().is_err());
        assert!(updater.load_cached_newer_version().is_none());
        updater.init().expect("couldn't init worker");
        // No more updates
        assert!(!updater.update_ready().expect("couldn't check for update"));
    }
}

#[test]
fn it_does_not_download_an_equal_or_older_release() {
    for installed in [VERSION_TEST_NEW, "0.12.0"] {
        setup_workflow_env_vars(true);
        let release = setup_mock_server(200).expect(1);
        let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
        updater.set_version(installed);
        updater.set_interval(0);
        updater.init().unwrap();

        assert!(!updater.update_ready().unwrap());
        assert!(updater.download_latest().is_err());
        release.assert();
    }
}

#[test]
fn it_ignores_build_metadata_when_comparing_versions() {
    setup_workflow_env_vars(true);
    let release = mockito::mock("GET", "/releases/latest")
        .with_status(200)
        .with_body(include_str!("../../tests/latest.json").replace("v0.11.1", "v0.11.1+build"))
        .create();
    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).unwrap();
    updater.set_version(VERSION_TEST_NEW);
    updater.set_interval(0);
    updater.init().unwrap();

    assert!(!updater.update_ready().unwrap());
    assert!(updater.download_latest().is_err());
    release.assert();
}

#[test]
fn it_handles_server_error_async() {
    setup_workflow_env_vars(true);
    first_check_after_installing_workflow();
    let response = setup_mock_server(400).expect(1);

    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    // Next check will be immediate
    updater.set_interval(0);
    updater.init().expect("couldn't init worker");
    let error = updater.update_ready().unwrap_err();
    let http_error = error.downcast_ref::<reqwest::Error>().unwrap();
    assert_eq!(http_error.status(), Some(reqwest::StatusCode::BAD_REQUEST));
    response.assert();
}

#[test]
fn it_caches_async_workers_payload() {
    setup_workflow_env_vars(true);

    first_check_after_installing_workflow();
    let _m = setup_mock_server(200);
    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    // Next check will be immediate
    updater.set_interval(0);
    updater.init().expect("couldn't init worker");
    assert!(updater.update_ready().expect("couldn't check for update"),);

    // Consequent calls to update_ready should cache the payload.
    let _m = setup_mock_server(400);
    assert!(updater.update_ready().expect("couldn't check for update"),);
    assert!(updater.update_ready().expect("couldn't check for update"),);
    assert!(updater.update_ready().expect("couldn't check for update"),);

    {
        let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
        // Next check will be immediate
        updater.set_interval(0);
        updater.init().expect("couldn't init worker");
        assert!(updater.update_ready().is_err());
    }
}

#[test]
fn it_get_latest_info_from_releaser() {
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);

    {
        first_check_after_installing_workflow();
        // Blocking
        let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
        // Next check will be immediate
        updater.set_interval(0);
        updater.init().expect("couldn't init worker");

        assert!(updater
            .update_ready()
            .expect("Blocking: couldn't check for update"));
    }
    {
        // Non-blocking
        let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
        // Next check will be immediate
        updater.set_interval(0);
        // Start async worker
        updater.init().expect("couldn't init worker");
        let deadline = time::Instant::now() + time::Duration::from_secs(15);
        loop {
            match updater.try_update_ready() {
                Ok(ready) => {
                    assert!(ready);
                    break;
                }
                Err(error)
                    if error.downcast_ref::<std::sync::mpsc::TryRecvError>()
                        == Some(&std::sync::mpsc::TryRecvError::Empty)
                        && time::Instant::now() < deadline =>
                {
                    thread::sleep(time::Duration::from_millis(10));
                }
                Err(error) => panic!("Non-blocking: couldn't check for update: {}", error),
            }
        }
    }
}

#[allow(clippy::cast_possible_wrap)]
#[test]
fn it_does_one_network_call_per_interval() {
    {
        setup_workflow_env_vars(true);
        let _m = setup_mock_server(200);
        let wait_time = 1;

        first_check_after_installing_workflow();

        let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
        // Next check will be immediate
        updater.set_interval(0);
        updater.init().expect("couldn't init worker");

        // Next update_ready will make a network call
        assert!(updater.update_ready().expect("couldn't check for update"));

        // Increase interval
        updater.set_interval(wait_time as i64);
        assert!(!updater.due_to_check());

        // make mock server return error. This way we can test that no network call was made
        // assuming Updater can read its cache file successfully
        let _m = setup_mock_server(503);
        let t = updater.update_ready();
        assert!(t.is_ok());
        // Make sure we still report update is ready
        assert!(t.unwrap());

        // Now we test that after interval has passed we will make a call
        let two_sec = time::Duration::from_secs(wait_time);
        thread::sleep(two_sec);
        {
            let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
            updater.set_interval(wait_time as i64);
            updater.init().expect("couldn't init worker");
            assert!(updater.due_to_check());

            // Since server is returning error, update_ready() should fail.
            let t = updater.update_ready();
            assert!(t.is_err());
        }
        {
            // Just making sure the next call will go through and return expected results.
            let _m = setup_mock_server(200);
            let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
            // Next check will be immediate
            updater.set_interval(0);
            updater.init().expect("couldn't init worker");
            assert!(updater.due_to_check());

            // Since server is ok, update_ready() should work.
            let t = updater.update_ready();
            assert!(t.is_ok());
            assert!(updater.update_ready().expect("couldn't check for update"));
        }
    }
}

#[test]
fn it_tests_download() {
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);
    let _asset = serve_workflow_bundle();
    first_check_after_installing_workflow();

    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");

    // Next check will be immediate
    updater.set_interval(0);
    // Force current version to be really old.
    updater.set_version("0.0.1");
    updater.init().expect("couldn't init worker");

    // New update is available
    assert!(updater.update_ready().expect("couldn't check for update"));

    let download_fn = updater.download_latest();
    assert!(download_fn.is_ok());
    assert_eq!(
        "latest_release_YouForgotTo___Name_Your_Own_Work_flow_.alfredworkflow",
        download_fn
            .unwrap()
            .file_name()
            .expect("couldn't get download file name")
            .to_str()
            .expect("impossible?!")
    );
}

#[test]
#[should_panic(expected = "no release info")]
fn it_doesnt_download_without_release_info() {
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);
    first_check_after_installing_workflow();

    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    updater.set_interval(864_000);

    assert!(updater.download_latest().is_err());
    updater.download_latest().unwrap();
}

#[test]
fn it_downloads_after_getting_release_info() {
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);
    let _asset = serve_workflow_bundle();
    first_check_after_installing_workflow();

    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    updater.set_interval(0);
    updater.init().expect("couldn't init worker");
    assert!(updater.download_latest().is_err());

    assert!(updater.update_ready().expect("couldn't check for update"));
    assert!(updater.download_latest().is_ok());
}

#[test]
fn it_tests_async_updates_1() {
    //
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);
    first_check_after_installing_workflow();

    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    // Next check will be immediate
    updater.set_interval(0);
    updater.init().expect("couldn't init worker");
    assert!(updater.update_ready().expect("couldn't check for update"));
}

#[test]
fn it_tests_async_updates_2() {
    // This test will only spawn a thread once.
    // Second call will use a cache since it's not due to check.
    setup_workflow_env_vars(true);
    let _m = setup_mock_server(200);
    first_check_after_installing_workflow();

    let mut updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");

    // Next check will spawn a thread. There should be an update avail. from mock server.
    updater.set_interval(0);
    updater.init().expect("couldn't init worker");
    updater.update_ready().expect("chouldn't check for update");

    // make mock server return error. This way we can test that no network call was made
    // assuming Updater can read its cache file successfully
    let _m = setup_mock_server(503);
    // Increase interval
    updater.set_interval(86400);

    assert!(updater.update_ready().expect("couldn't check for update"));
}

pub(super) fn setup_workflow_env_vars(secure_temp_dir: bool) -> PathBuf {
    // Mimic Alfred's environment variables
    let path = if secure_temp_dir {
        Builder::new()
            .prefix("alfred_workflow_test")
            .rand_bytes(5)
            .tempdir()
            .unwrap()
            .into_path()
    } else {
        StdEnv::temp_dir()
    };
    {
        let v: &OsStr = path.as_ref();
        StdEnv::set_var("alfred_workflow_data", v);
        StdEnv::set_var("alfred_workflow_cache", v);
        StdEnv::set_var("alfred_workflow_uid", "workflow.B0AC54EC-601C");
        StdEnv::set_var(
            "alfred_workflow_name",
            "YouForgotTo/フ:Name好YouráOwnسWork}flowッ",
        );
        StdEnv::set_var("alfred_workflow_bundleid", "MY_BUNDLE_ID");
        StdEnv::set_var("alfred_workflow_version", VERSION_TEST);
    }
    path
}

fn first_check_after_installing_workflow() {
    // With the default interval, the first run seeds the cache without a remote check.
    let _m = setup_mock_server(200);

    let updater = Updater::gh(MOCK_RELEASER_REPO_NAME).expect("cannot build Updater");
    assert_eq!(VERSION_TEST, format!("{}", updater.current_version()));

    updater.init().expect("couldn't init worker");

    assert!(!updater.update_ready().expect("couldn't check for update"));
}
