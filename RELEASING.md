# Releasing Pinion

Use GitHub Actions to build the release. `create_alfred_workflow.sh` is an old
upstream helper with machine-specific paths and is not the release procedure.

## Release readiness

1. Review the changes since the previous release, including new source files.
   Confirm the user-facing changes are covered in the README and changelog.
2. Choose the next semantic version: a minor version for new features, a patch
   version for fixes. Update these three values together, without the `v` prefix:
   - `[package].version` in `Cargo.toml`.
   - Pinion's package entry in `Cargo.lock`.
   - The root `version` in `res/workflow/info.plist`.
3. Move the changelog's unreleased entries into a dated version section, leaving
   an empty `Unreleased` section for the next changes. Update the link definitions
   at the bottom: the new version compares the previous and new tags, and
   `Unreleased` compares the new tag with `HEAD`.
4. Run the local quality gate:

   ```bash
   cargo fmt -p pinion -- --check
   cargo clippy --locked -p pinion --no-deps --tests -- -Dclippy::all -D warnings -Aclippy::pedantic
   cargo test --locked --workspace --lib --bins -- --test-threads=1
   python3 tests/workflow_regression.py
   git diff --check
   ```

   The workflow tests run against local fixtures without using the screen.
   Use a native Alfred smoke test when visible behavior changes. Dismiss the
   overlay and installer afterward and restore any temporary test workflow.

## Publish

1. Commit the reviewed changes and push `master`. Wait for its CI checks to pass.
2. Create an annotated `vX.Y.Z` tag on that tested commit and push that tag.
3. Watch the tag's `alfred-workflow` run. Regression tests must pass before it
   builds both Mac architectures, signs and notarizes the universal binary,
   verifies the workflow archive, and uploads `Pinion.alfredworkflow` to the release.
4. Check the signing and notarization steps in the logs. A build can succeed
   without signing secrets, so a green run alone does not establish notarization.
5. Add release notes describing the user-facing changes and any upgrade steps.
   Keep the downloadable filename `Pinion.alfredworkflow`: the README and updater
   rely on it.

## Verify and close

Download the published asset into a scratch directory and check:

- The manifest identifies `com.victorrodrigues.pinion` at the tagged version.
- The archive passes `pinion verify-workflow-release --tag=vX.Y.Z --bundle=PATH`.
- Its `pinion` executable contains both `arm64` and `x86_64` architectures.
- `codesign --verify --strict --verbose=2 PATH_TO_PINION` succeeds and
  `spctl -a -vv -t open --context context:primary-signature PATH_TO_PINION`
  reports acceptance with a notarized Developer ID.
- GitHub's latest release points to the intended tag and includes the asset.
- Local `HEAD`, `origin/master`, and the release tag identify the same commit,
  and the working tree is clean.

Close the ceremony with the release link, validation results, and any remaining
follow-up work. Celebrate; snacks are optional.
