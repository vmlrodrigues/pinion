"""Run the Alfred commands against local Pinboard and GitHub fixtures."""

from collections import Counter
from datetime import datetime, timedelta, timezone
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
import json
import os
from pathlib import Path
import plistlib
import re
import select
import shlex
import shutil
import signal
from struct import pack
import subprocess
import tempfile
from threading import Event, Thread
import time
import unittest
from urllib.parse import parse_qs, urlparse
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo
from zlib import crc32


REPO = Path(__file__).resolve().parent.parent
TOKEN = "test:fixture"
ROTATED_TOKEN = "test:rotated-fixture"
SECOND_TOKEN = "second:fixture"
OLDER = "2026-01-01T00:00:00Z"
NEWER = "2026-01-02T00:00:00Z"
RELEASE_PATH = "/repos/vmlrodrigues/pinion/releases/latest"
ASSET_ENDPOINT = "/repos/vmlrodrigues/pinion/releases/assets/"
ASSET_PATH = ASSET_ENDPOINT + "1"
ASSET_BROWSER_PATH = "/Pinion.alfredworkflow"
RETITLE_UID = "B17E1000-"


def bookmark(url, title, tags, date, *, shared="no", toread="no", notes=""):
    return dict(href=url, description=title, tags=tags, time=date,
                shared=shared, toread=toread, extended=notes)


def add_fixture_executable(bundle):
    program = ZipInfo("pinion")
    program.create_system = 3
    program.external_attr = 0o100755 << 16
    bundle.writestr(program, b"#!/bin/sh\nexit 0\n")


def workflow_bundle(version, marker="fixture release asset", *, info=None,
                    bundle_id="com.victorrodrigues.pinion", executable=True):
    if info is None:
        info = plistlib.dumps({"bundleid": bundle_id, "name": "Pinion",
                               "version": version, "objects": [], "connections": {}})
    archive = BytesIO()
    with ZipFile(archive, "w", ZIP_DEFLATED) as bundle:
        if info:
            bundle.writestr("info.plist", info)
        if executable:
            add_fixture_executable(bundle)
        bundle.writestr("marker.txt", marker)
    return archive.getvalue()


def bundle_marker(path):
    with ZipFile(path) as bundle:
        return bundle.read("marker.txt").decode()


def pack_legacy_cache(value):
    if value is None:
        return b"\xc0"
    if isinstance(value, str):
        encoded = value.encode()
        return b"\xd9" + bytes([len(encoded)]) + encoded
    if isinstance(value, int):
        return bytes([value])
    if isinstance(value, dict):
        return bytes([0x80 + len(value)]) + b"".join(
            pack_legacy_cache(key) + pack_legacy_cache(item) for key, item in value.items())
    return bytes([0x90 + len(value)]) + b"".join(map(pack_legacy_cache, value))


class MockServices(ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), MockHandler)
        self.address = f"http://127.0.0.1:{self.server_port}"
        self.reset()

    def reset(self):
        self.paths = Counter()
        self.pins = []
        self.export_pins = None
        self.lookup_posts = None
        self.second_pins = []
        self.second_status = {}
        self.saves = []
        self.deletes = []
        self.renames = []
        self.timestamp = OLDER
        self.status = {}
        self.revoke_original_token = False
        self.tags = None
        self.release_tag = "v1.0.0"
        self.release_has_workflow = True
        self.block_update = False
        self.download_delay = 0
        self.update_started = Event()
        self.resume_update = Event()
        self.block_download = False
        self.download_started = Event()
        self.resume_download = Event()
        self.block_tags = False
        self.tags_started = Event()
        self.resume_tags = Event()
        self.block_lookup = False
        self.block_lookup_headers = False
        self.lookup_started = Event()
        self.resume_lookup = Event()
        self.bookmark_added = Event()
        self.block_release = False
        self.release_started = Event()
        self.resume_release = Event()
        self.block_asset = False
        self.asset_id = 1
        self.asset_body = "fixture release asset"
        self.asset_headers = []
        self.asset_content_type = "application/octet-stream"
        self.asset_started = Event()
        self.resume_asset = Event()
        self.ordinary_request_interval = 0
        self.last_ordinary_request = {}
        self.throttled_requests = []
        self.retry_after = {}
        self.popular_tags = {}
        self.raw_suggestions = None
        self.block_suggestions_for = None
        self.suggestion_started = Event()
        self.resume_suggestion = Event()
        self.truncate_suggestions = False


class MockHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        query = {
            key: values[0]
            for key, values in parse_qs(parsed.query, keep_blank_values=True).items()
        }
        self.server.paths[path] += 1
        token = query.get("auth_token")
        if path.startswith("/v1/") and (
            token not in (TOKEN, ROTATED_TOKEN, SECOND_TOKEN)
            or (token == TOKEN and self.server.revoke_original_token)
        ):
            self.send_error(401)
            return
        second_account = token == SECOND_TOKEN
        pins = self.server.second_pins if second_account else self.server.pins
        statuses = self.server.second_status if second_account else self.server.status
        if path.startswith("/v1/") and path != "/v1/posts/all":
            now = time.monotonic()
            last = self.server.last_ordinary_request.get(token, 0)
            if now - last < self.server.ordinary_request_interval:
                self.server.throttled_requests.append(path)
                self.send_error(429)
                return
            self.server.last_ordinary_request[token] = now
        if path == "/v1/posts/update" and self.server.block_update and not second_account:
            self.server.update_started.set()
            self.server.resume_update.wait(10)
        if path == "/v1/posts/all" and self.server.block_download:
            self.server.download_started.set()
            self.server.resume_download.wait(10)
        if path == "/v1/tags/get" and self.server.block_tags:
            self.server.tags_started.set()
            self.server.resume_tags.wait(12)
        if path == RELEASE_PATH and self.server.block_release:
            self.server.release_started.set()
            self.server.resume_release.wait(10)
        asset_request = path == ASSET_ENDPOINT + str(self.server.asset_id)
        asset_body = self.server.asset_body
        if asset_request:
            self.server.asset_headers.append((self.headers.get("Accept"),
                                              self.headers.get("X-GitHub-Api-Version")))
        asset_version = self.server.release_tag.removeprefix("v")
        if asset_request and self.server.block_asset:
            self.server.asset_started.set()
            self.server.resume_asset.wait(10)
        if path == "/v1/posts/suggest" and query["url"] == self.server.block_suggestions_for:
            self.server.suggestion_started.set()
            self.server.resume_suggestion.wait(10)
        if path == "/v1/posts/suggest" and self.server.truncate_suggestions:
            self.send_response(200)
            self.send_header("Content-Length", "100")
            self.end_headers()
            self.wfile.write(b"[")
            return
        if path == "/v1/posts/suggest" and self.server.raw_suggestions is not None:
            body = self.server.raw_suggestions
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if status := statuses.get(path):
            if status in (429, 503) and path in self.server.retry_after:
                self.send_response(status)
                self.send_header("Retry-After", self.server.retry_after[path])
                self.end_headers()
                return
            self.send_error(status)
            return

        if path == "/v1/posts/update":
            response = {"update_time": self.server.timestamp}
        elif path == "/v1/posts/all":
            time.sleep(self.server.download_delay)
            response = (self.server.export_pins if not second_account
                        and self.server.export_pins is not None else pins)
        elif path == "/v1/tags/get":
            response = None if second_account else self.server.tags
            if response is None:
                response = dict(Counter(tag for pin in pins for tag in pin["tags"].split()))
        elif path == "/v1/posts/get":
            posts = (self.server.lookup_posts if not second_account
                     and self.server.lookup_posts is not None else
                     [pin for pin in pins if pin.get("href") == query["url"]])
            response = {"posts": posts}
        elif path == "/v1/posts/add":
            self.server.saves.append(query)
            self.server.bookmark_added.set()
            response = {"result_code": "done"}
        elif path == "/v1/posts/delete":
            self.server.deletes.append(query)
            pins[:] = [pin for pin in pins if pin["href"] != query["url"]]
            response = {"result_code": "done"}
        elif path == "/v1/tags/rename":
            self.server.renames.append(query)
            for pin in pins:
                pin["tags"] = " ".join(query["new"] if tag == query["old"] else tag
                                       for tag in pin["tags"].split())
            response = {"result": "done"}
        elif path == "/v1/posts/suggest":
            url = query["url"]
            response = [{"popular": self.server.popular_tags.get(url, ["suggested"])}]
        elif path == RELEASE_PATH:
            assets = [{
                "url": self.server.address + ASSET_ENDPOINT + str(self.server.asset_id),
                "id": self.server.asset_id,
                "name": "Pinion.alfredworkflow",
                "state": "uploaded",
                "browser_download_url": self.server.address + ASSET_BROWSER_PATH,
            }] if self.server.release_has_workflow else []
            response = {
                "tag_name": self.server.release_tag,
                "assets": assets,
            }
        elif asset_request:
            response = asset_body if isinstance(asset_body, bytes) else workflow_bundle(
                asset_version, asset_body,
            )
        else:
            self.send_error(404)
            return

        if path == "/v1/posts/get" and self.server.block_lookup_headers:
            self.server.lookup_started.set()
            self.server.resume_lookup.wait(10)
        self.send_response(200)
        self.send_header("Content-Type", self.server.asset_content_type if asset_request
                         else "application/json")
        self.end_headers()
        body = response if asset_request else json.dumps(response).encode()
        if path == "/v1/posts/get" and self.server.block_lookup:
            self.server.lookup_started.set()
            self.server.resume_lookup.wait(10)
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            # A request may time out before a deliberately delayed fixture responds.
            pass

    def log_message(self, *_):
        pass


def build_fixture(root, address, *, fast_pacing=True):
    """Redirect both APIs in a scratch copy, keeping release binaries on the real hosts."""
    for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
        shutil.copyfile(REPO / name, root / name)
    for directory in ("src", "vendor"):
        shutil.copytree(REPO / directory, root / directory,
                        ignore=shutil.ignore_patterns("target"))
    replacements = [
        ("vendor/rusty-pin/src/pinboard/api.rs", "https://api.pinboard.in/v1", address + "/v1"),
        ("vendor/alfred-rs/src/updater/releaser.rs", "https://api.github.com/repos/", address + "/repos/"),
    ]
    if fast_pacing:
        replacements.append(("vendor/rusty-pin/src/pinboard/request_pacing.rs",
                             "TimeDelta::seconds(3)", "TimeDelta::milliseconds(80)"))
    for file, old, new in replacements:
        path = root / file
        contents = path.read_text()
        if contents.count(old) != 1:
            raise ValueError(f"Expected one production value in {file}")
        path.write_text(contents.replace(old, new))
    target = root / "target"
    subprocess.run(["cargo", "build", "--locked", "-p", "pinion", "--target-dir", str(target)],
                   cwd=root, check=True)
    return target / "debug" / "pinion"


class WorkflowRegression(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = MockServices()
        cls.addClassCleanup(cls.server.server_close)
        worker = Thread(target=cls.server.serve_forever, daemon=True)
        worker.start()
        cls.addClassCleanup(cls.server.shutdown)
        source = tempfile.TemporaryDirectory(prefix="pinion-regression-source-")
        cls.addClassCleanup(source.cleanup)
        cls.binary = build_fixture(Path(source.name), cls.server.address)
        workflow = plistlib.loads((REPO / "res/workflow/info.plist").read_bytes())
        cls.connections = workflow["connections"]
        cls.objects = {obj["uid"]: obj.get("config", {}) for obj in workflow["objects"]}
        cls.object_types = {obj["uid"]: obj["type"] for obj in workflow["objects"]}

    def setUp(self):
        self.server.reset()
        temp = tempfile.TemporaryDirectory(prefix="pinion-regression-alfred-")
        self.addCleanup(temp.cleanup)
        self.workflow = Path(temp.name)
        (self.workflow / "pinion").symlink_to(self.binary)
        browser = self.workflow / "osascript"
        browser.write_text("#!/bin/sh\nexit 1\n")
        browser.chmod(0o700)
        security = self.workflow / "security"
        security.write_text('''#!/bin/sh
case "$1" in
    find-generic-password) cat "$PINION_FIXTURE_KEYCHAIN" ;;
    add-generic-password)
        while [ "$1" != -w ] && [ "$#" -gt 1 ]; do shift; done
        printf '%s\\n' "$2" > "$PINION_FIXTURE_KEYCHAIN" ;;
    *) exit 1 ;;
esac
''')
        security.chmod(0o700)
        self.cache = self.workflow / "cache"
        self.cache.mkdir()
        (self.cache / "last_check_status_async.json").write_text("null")
        self.updater_file = self.cache / "pinion-regression-Pinion-updater.json"
        self.updater_file.write_text(json.dumps({
            "last_check": datetime.now(timezone.utc).isoformat(),
            "current_version": "1.0.0",
            "avail_release": None,
        }))
        self.data = self.workflow / "data"
        self.account_cache = self.cache / "pinboard-accounts" / TOKEN.split(":", 1)[0].encode().hex()
        self.sync_file = self.account_cache / "bookmark-sync.json"
        self.env = {
            **os.environ,
            "PATH": f"{self.workflow}{os.pathsep}{os.environ['PATH']}",
            "alfred_version": "5.0.0",
            "alfred_workflow_bundleid": "invalid.example.pinion-regression",
            "alfred_workflow_cache": str(self.cache),
            "alfred_workflow_data": str(self.data),
            "alfred_workflow_uid": "pinion-regression",
            "alfred_workflow_name": "Pinion",
            "alfred_workflow_version": "1.0.0",
            "auto_update_cache": "1", "fuzzy_search": "1", "tag_only_search": "0",
            "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1",
            "RUST_LOG": "off",
            "PINION_FIXTURE_KEYCHAIN": str(self.workflow / "keychain-token"),
        }
        self.env.pop("alfred_debug", None)
        self.env.pop("pinion_alfred_filter", None)

    def run_command(self, args, *, env=None, timeout=10, check=True):
        return subprocess.run(args, cwd=self.workflow, env=env or self.env,
                              capture_output=True, text=True, timeout=timeout, check=check)

    def uid(self, keyword_or_uid):
        if keyword_or_uid in self.objects:
            return keyword_or_uid
        return next(uid for uid, obj in self.objects.items() if obj.get("keyword") == keyword_or_uid)

    def keyword_script(self, keyword, query):
        return self.objects[self.uid(keyword)]["script"].replace("{query}", shlex.quote(query))

    def run_keyword(self, keyword, query="", *, timeout=10, check=True, variables=None):
        return self.run_filter(keyword, query, timeout=timeout, check=check, variables=variables)

    def run_filter(self, keyword_or_uid, query="", *, timeout=10, check=True, variables=None):
        return json.loads(self.run_command(["bash", "-c", self.keyword_script(keyword_or_uid, query),
                                            keyword_or_uid, query],
                                           env={**self.env, **(variables or {})},
                                           timeout=timeout, check=check).stdout)

    def run_object(self, uid, arg, variables=None):
        script = self.objects[uid]["script"]
        return self.run_command(["bash", "-c", script, uid, arg], env={**self.env, **(variables or {})})

    def select_result(self, source, result, item, *, modifiers=0, include_outputs=False):
        keys = {0: "", 262144: "ctrl", 524288: "alt", 786432: "alt+ctrl",
                1048576: "cmd", 1572864: "cmd+alt"}
        variant = item.get("mods", {}).get(keys[modifiers], {})
        self.assertTrue(variant.get("valid", item.get("valid", True)), item)
        variables = {**result.get("variables", {}),
                     **variant.get("variables", item.get("variables", {}))}
        return self.follow_connection(source, variant.get("arg", item.get("arg", "")),
                                      variables, modifiers=modifiers, include_outputs=include_outputs)

    def follow_connection(self, source, arg, variables=None, *, modifiers=0,
                          include_outputs=False):
        targets = self.connection_targets(source, arg, variables, modifiers=modifiers,
                                          include_outputs=include_outputs)
        self.assertEqual(len(targets), 1, targets)
        return targets[0]

    def connection_targets(self, source, arg, variables=None, *, modifiers=0,
                           include_outputs=False):
        """Follow the plist's utilities to the next action, filter, or requested output."""
        def resolve(uid, argument, env, keys=0, output=None):
            for edge in self.connections[uid]:
                if edge["modifiers"] == keys and edge.get("sourceoutputuid") == output:
                    yield from resolve_destination(edge["destinationuid"], argument, env)

        def resolve_destination(dest, argument, env):
            def expand(template):
                return re.sub(r"\{(query|var:([^}]+))\}",
                              lambda match: argument if match[1] == "query"
                              else env.get(match[2], ""), template)

            obj = self.objects[dest]
            kind = self.object_types[dest]
            if kind == "alfred.workflow.utility.argument":
                extra = {key: expand(value) for key, value in obj.get("variables", {}).items()}
                next_arg = argument if obj["passthroughargument"] else expand(obj["argument"])
                yield from resolve(dest, next_arg, {**env, **extra})
            elif kind == "alfred.workflow.utility.conditional":
                output = None
                for condition in obj["conditions"]:
                    value = expand(condition["inputstring"])
                    pattern = condition["matchstring"]
                    mode = condition["matchmode"]
                    self.assertIn(mode, (0, 1, 4))
                    if mode == 4:
                        flags = 0 if condition["matchcasesensitive"] else re.IGNORECASE
                        matches = re.search(pattern, value, flags) is not None
                    else:
                        if not condition["matchcasesensitive"]:
                            value, pattern = value.casefold(), pattern.casefold()
                        matches = (value == pattern) == (mode == 0)
                    if matches:
                        output = condition["uid"]
                        break
                yield from resolve(dest, argument, env, output=output)
            elif kind == "alfred.workflow.utility.junction":
                yield from resolve(dest, argument, env)
            elif kind == "alfred.workflow.utility.filter":
                self.assertEqual(obj["matchmode"], 2)
                flags = 0 if obj["matchcasesensitive"] else re.IGNORECASE
                if re.search(obj["matchstring"], expand(obj["inputstring"]), flags):
                    yield from resolve(dest, argument, env)
            elif include_outputs or not kind.startswith("alfred.workflow.output."):
                self.assertFalse(kind.startswith("alfred.workflow.utility."), kind)
                yield dest, argument, env

        return list(resolve(self.uid(source), arg, variables or {}, modifiers))

    def follow_workflow_output(self, source, output, variables=None):
        message = json.loads(output)["alfredworkflow"]
        return self.follow_connection(source, message["arg"],
                                      {**(variables or {}), **message.get("variables", {})})

    def configure(self, token=TOKEN):
        self.run_command(["./pinion", "config", "--authorization", token])

    def show_browser_page(self, url, title="Fixture page"):
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            + "printf '%s\\n' " + shlex.quote(f'{{"{url} fd850fc2e63511e79f720023dfdf24ec {title}"}}')
            + "\n"
        )
        browser.chmod(0o700)

    def open_bookmark_result(self, source, result, row):
        action = self.select_result(source, result, row)
        recorded = self.run_object(*action)
        target, argument, _ = self.follow_connection(action[0], recorded.stdout, action[2])
        self.assertEqual(self.object_types[target], "alfred.workflow.action.openurl")
        self.assertEqual(argument, row["arg"])

    def test_post_notifications_distinguish_the_status_from_the_page_title(self):
        url = "https://example.test/errors-as-titles"
        title = "Error: This is the page title"
        self.server.pins = [bookmark(url, "Previous title", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Previous title"])
        self.env.update(auto_update_cache="0", suggest_tags="0")
        self.show_browser_page(url, title)
        result = self.run_keyword("p", "protect")
        row = next(item for item in result["items"] if item.get("arg") == "protect")

        def notices(action, output):
            notifications = []
            for uid, arg, variables in self.connection_targets(action[0], output, action[2],
                                                                include_outputs=True):
                formatted = self.run_object(uid, arg, variables).stdout.strip()
                if not formatted:
                    continue
                for target, message, _ in self.connection_targets(uid, formatted, variables,
                                                                   include_outputs=True):
                    if self.object_types[target] == "alfred.workflow.output.notification":
                        notifications.append((self.objects[target]["title"], message))
            return notifications

        for modifier, notify in ((0, "0"), (262144, "1"), (524288, "0"), (786432, "1")):
            with self.subTest(modifier=modifier):
                self.env["notify_on_post"] = notify
                action = self.select_result("p", result, row, modifiers=modifier)
                output = self.run_object(*action).stdout
                self.assertEqual(self.server.saves[-1]["description"], title)
                expected = [("👍", f"Posted: {title}")] if notify == "1" else []
                self.assertEqual(notices(action, output), expected)

        self.env["notify_on_post"] = "0"
        self.server.status["/v1/posts/add"] = 500
        action = self.select_result("p", result, row)
        failed = self.run_object(*action).stdout
        self.assertEqual(notices(action, failed), [("Error", "Post: Could not post to Pinboard.")])

    def test_post_notes_keep_the_bookmark_indicator_and_the_save_action(self):
        url = "https://example.test/already-bookmarked"
        self.server.pins = [bookmark(url, "Saved title", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Saved title"])
        self.env["suggest_tags"] = "0"
        self.show_browser_page(url)
        original = self.run_keyword("p", "protect")
        notes = self.run_keyword("p", "protect; revised notes", variables=original["variables"])
        indicator = next(item for item in notes["items"] if item["title"] == "You already have the bookmark!")
        self.assertFalse(indicator["valid"])
        row = next(item for item in notes["items"] if item.get("arg") == "protect; revised notes")
        self.assertIn("Successfully posted", self.run_object(*self.select_result("p", notes, row)).stdout)
        self.assertEqual(self.server.saves[-1]["tags"], "protect")
        self.assertEqual(self.server.saves[-1]["extended"], "revised notes")

    def test_release_packager_checks_the_manifest_before_the_alfred_install_path(self):
        source = self.workflow / "release-source"
        source.mkdir()
        tracked = subprocess.check_output(["git", "ls-files", "-z", "res/workflow"], cwd=REPO)
        for name in os.fsdecode(tracked).split("\0")[:-1]:
            destination = source / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPO / name, destination)
        shutil.copyfile(REPO / "Cargo.lock", source / "Cargo.lock")
        staged_manifest = source / "res/workflow/info.plist"
        manifest = plistlib.loads(staged_manifest.read_bytes())
        for arch in ("aarch64", "x86_64"):
            output = source / "target" / f"{arch}-apple-darwin" / "release" / "pinion"
            output.parent.mkdir(parents=True)
            output.symlink_to(self.binary)
        for name, body in (("strip", "exit 0"), ("lipo", 'cp "$4" "$3"')):
            program = self.workflow / name
            program.write_text(f"#!/bin/sh\n{body}\n")
            program.chmod(0o700)
        archive = source / "Pinion.alfredworkflow"
        packager = REPO / ".github/workflows/script.sh"
        git_dir = subprocess.check_output(["git", "rev-parse", "--absolute-git-dir"],
                                          cwd=REPO, text=True).strip()

        def package(tag, version, bundle_id="com.victorrodrigues.pinion"):
            staged_manifest.write_bytes(plistlib.dumps({**manifest, "version": version,
                                                       "bundleid": bundle_id}))
            env = {key: value for key, value in self.env.items() if not key.startswith("alfred_")}
            env.update({"GIT_DIR": git_dir, "GIT_WORK_TREE": str(source),
                   "GITHUB_WORKSPACE": str(source), "RELEASE_COMMIT": "true", "GITHUB_REF_NAME": tag,
                   "MACOS_SIGN_IDENTITY": "", "TMPDIR": str(self.workflow)})
            return subprocess.run(["bash", str(packager)], cwd=source, env=env, text=True,
                                  capture_output=True, timeout=15)

        for tag, version, bundle_id, error in (
            ("v1.1.0", "1.0.0", "com.victorrodrigues.pinion", "different workflow version"),
            ("vlatest", "latest", "com.victorrodrigues.pinion", "is not a semantic version"),
            ("v18446744073709551616.0.0", "18446744073709551616.0.0", "com.victorrodrigues.pinion",
             "is not a semantic version"),
            ("v1.1.0", "1.1.0", "example.other", "different workflow identifier"),
        ):
            with self.subTest(tag=tag, version=version, bundle_id=bundle_id):
                failed = package(tag, version, bundle_id)
                self.assertNotEqual(failed.returncode, 0, failed.stdout)
                self.assertIn(error, failed.stderr)
                self.assertFalse(archive.exists())
        success = package("v1.1.0", "1.1.0")
        self.assertEqual(success.returncode, 0, success.stderr)
        self.server.release_tag = "v1.1.0"
        self.server.asset_body = archive.read_bytes()
        found = self.run_keyword("pcheck")
        action = self.select_result("pcheck", found, found["items"][0])
        downloaded = self.run_object(*action).stdout
        install = self.follow_connection(action[0], downloaded, action[2])
        open_program = self.workflow / "open"
        open_program.write_text('#!/bin/sh\ntest -f "$3"\n')
        open_program.chmod(0o700)
        self.assertIn("Opening Alfred to install", self.run_object(*install).stdout)

    def test_p_defers_browser_fallback_until_save_and_checks_the_account(self):
        self.configure()
        self.search()
        self.env.update(page_is_bookmarked="0", suggest_tags="0", auto_update_cache="0")
        self.env["FIXTURE_BROWSER"] = "Firefox.app"
        self.env["FIXTURE_BROWSER_LOG"] = str(self.workflow / "browser.log")
        browser = self.workflow / "osascript"
        browser.write_text("""#!/bin/sh
case "${4-}" in
  passive)
    printf 'passive\\n' >> "$FIXTURE_BROWSER_LOG"
    printf '{"pinion-browser-fallback:%s"}\\n' "$FIXTURE_BROWSER" ;;
  interactive)
    printf 'interactive\\n' >> "$FIXTURE_BROWSER_LOG"
    [ "$5" = "$FIXTURE_BROWSER" ] || exit 1
    printf '%s\\n' '{"https://example.test/fallback fd850fc2e63511e79f720023dfdf24ec Fallback page"}' ;;
  *) printf 'unexpected\\n' >> "$FIXTURE_BROWSER_LOG"; exit 1 ;;
esac
""")
        browser.chmod(0o700)
        log = Path(self.env["FIXTURE_BROWSER_LOG"])
        initial = self.run_filter("p")
        self.assertEqual(log.read_text().splitlines(), ["passive"])
        self.assertEqual(self.server.paths["/v1/posts/suggest"], 0)

        post = self.run_filter("p", "protect; A note", variables=initial["variables"])
        row = next(row for row in post["items"] if row.get("arg") == "protect; A note")
        self.assertEqual(log.read_text().splitlines(), ["passive"])
        save = self.select_result("p", post, row, modifiers=524288)

        self.configure(SECOND_TOKEN)
        self.assertIn("The Pinboard account changed", self.run_object(*save).stdout)
        self.assertEqual(log.read_text().splitlines(), ["passive"])
        self.configure()
        self.env["FIXTURE_BROWSER"] = "qutebrowser.app"
        self.assertIn("Error: ", self.run_object(*save).stdout)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        self.env["FIXTURE_BROWSER"] = "Firefox.app"
        self.assertIn("Successfully posted", self.run_object(*save).stdout)
        self.assertEqual(log.read_text().splitlines(), ["passive", "interactive", "interactive"])
        self.assertEqual(self.server.saves[0]["url"], "https://example.test/fallback")
        self.assertEqual(self.server.saves[0]["extended"], "A note")
        self.assertEqual(self.server.saves[0]["shared"], "yes")

    def test_p_indicator_distinguishes_path_and_query_case(self):
        saved_url = "https://example.test/ABC?key=Value"
        self.server.pins = [bookmark(saved_url, "Original", "protect", OLDER)]
        self.configure()
        self.search()
        self.env.update(page_is_bookmarked="1", suggest_tags="0", auto_update_cache="0")
        notice = "You already have the bookmark!"
        for url in ("https://example.test/abc?key=Value",
                    "https://example.test/ABC?key=value"):
            with self.subTest(url=url):
                self.show_browser_page(url)
                view = self.run_filter("p")
                self.assertNotIn(notice, [row["title"] for row in view["items"]])
        self.show_browser_page(saved_url)
        view = self.run_filter("p")
        indicator = next(row for row in view["items"] if row["title"] == notice)
        self.assertFalse(indicator["valid"])

    def test_ps_treats_leading_hyphens_and_option_names_as_search_terms(self):
        url = "https://example.test/tagged"
        self.server.pins = [
            bookmark(url, "Literal terms", "-protect -t --title *", OLDER),
            bookmark("https://example.test/title", "Routine protect plan", "other", NEWER),
        ]
        self.env.update(auto_update_cache="0", fuzzy_search="0")
        self.configure()
        self.search()

        for query in ("-protect", "-t", "--title", "-protect -t --title *"):
            with self.subTest(query=query):
                results = self.run_filter("ps", query)
                self.assertEqual([item.get("arg") for item in results["items"]], [url])
                self.open_bookmark_result("ps", results, results["items"][0])

        direct = self.run_command(["./pinion", "search", "-t", "-U", "--", "-protect"])
        self.assertEqual(direct.stdout.splitlines(), [url])

    def test_recent_opens_rank_bookmarks_within_their_search_match_group(self):
        old = "https://example.test/dashboard?tab=surveillance&view=close"
        new = "https://example.test/newer"
        fuzzy = "https://example.test/weaker"
        self.server.pins = [bookmark(old, "Dashboard Meridian Close Surveillance", "protect", OLDER),
                            bookmark(new, "Newer exact match", "protect", NEWER),
                            bookmark(fuzzy, "Protect overview", "other", NEWER)]
        self.server.second_pins = list(self.server.pins)
        self.configure()
        self.assertEqual(len(self.search()), 3)
        self.env["auto_update_cache"] = "0"

        def urls(result):
            return [row["arg"] for row in result["items"]]

        result = self.run_keyword("ps", "protect")
        self.assertEqual(urls(result), [new, old, fuzzy])
        # A modifier that copies the bookmark does not count as opening it.
        row = next(row for row in result["items"] if row.get("arg") == old)
        copy, _, _ = self.select_result("ps", result, row, modifiers=1048576, include_outputs=True)
        self.assertEqual(self.object_types[copy], "alfred.workflow.output.clipboard")
        self.assertEqual(urls(self.run_keyword("ps", "protect")), [new, old, fuzzy])

        self.open_bookmark_result("ps", result, row)
        result = self.run_keyword("ps", "protect")
        self.assertEqual(urls(result), [old, new, fuzzy])
        self.assertEqual(urls(self.run_keyword("ps", "PROTECT")), [old, new, fuzzy])
        limited = self.run_keyword("ps", "protect", variables={"pins_to_show": "1"})
        self.assertEqual(urls(limited), [old])
        self.open_bookmark_result("ps", result, result["items"][-1])
        self.assertEqual(urls(self.run_keyword("ps", "protect")), [old, new, fuzzy])

        tags = self.run_keyword("pt", "protect")
        tag = next(row for row in tags["items"] if row.get("arg") == "protect")
        source, query, context = self.select_result("pt", tags, tag)
        exact = self.run_filter(source, query, variables=context)
        self.assertEqual(urls(exact), [old, new])
        self.open_bookmark_result(source, exact, exact["items"][1])
        self.assertEqual(urls(self.run_keyword("ps", "protect")), [new, old, fuzzy])

        # History survives a Pinboard cache rebuild and doesn't leak between accounts.
        old_row = next(row for row in exact["items"] if row.get("arg") == old)
        self.open_bookmark_result(source, exact, old_row)
        self.assertIn("Updated cache files", self.run_object(*self.follow_connection("pu", "")).stdout)
        self.assertEqual(urls(self.run_keyword("ps", "protect")), [old, new, fuzzy])
        self.configure(SECOND_TOKEN)
        self.assertIn("Updated cache files", self.run_object(*self.follow_connection("pu", "")).stdout)
        self.assertEqual(urls(self.run_keyword("ps", "protect")), [new, old, fuzzy])
        self.configure()
        self.assertEqual(urls(self.run_keyword("ps", "protect")), [old, new, fuzzy])

        # A local history write failure must not prevent opening the bookmark.
        unavailable = self.data / "unavailable"
        unavailable.write_text("")
        self.open_bookmark_result(
            "ps", {"variables": {"alfred_workflow_data": str(unavailable)}}, old_row,
        )

    def test_ps_orders_multiple_exact_tags_before_recent_partial_matches(self):
        full = bookmark("https://example.test/both", "Both exact tags", "protect cameras", NEWER)
        old_full = bookmark("https://example.test/old-both", "Older exact tags", "protect cameras", OLDER)
        partial = bookmark("https://example.test/partial", "C-a-m-e-r-a-s notes", "protect operations", OLDER)
        fuzzy = bookmark("https://example.test/fuzzy", "P-r-o-t-e-c-t c-a-m-e-r-a-s", "operations", OLDER)
        self.server.pins = [full, old_full, partial, fuzzy]
        self.configure()
        self.search()
        self.env["auto_update_cache"] = "0"
        for opened in (partial, fuzzy, old_full):
            view = self.run_keyword("ps", "protect cameras")
            row = next(row for row in view["items"] if row.get("arg") == opened["href"])
            self.open_bookmark_result("ps", view, row)

        for query in ("protect cameras", "PROTECT protect CAMERAS", "cameras protect"):
            with self.subTest(query=query):
                view = self.run_keyword("ps", query)
                self.assertEqual([row["arg"] for row in view["items"]],
                                 [old_full["href"], full["href"], partial["href"], fuzzy["href"]])
                limited = self.run_keyword("ps", query, variables={"pins_to_show": "1"})
                self.assertEqual(limited["items"][0]["arg"], old_full["href"])

    def test_ps_and_pt_remove_duplicate_urls_before_limiting_old_cache_results(self):
        first = bookmark("https://example.test/new", "First exact match", "protect", NEWER)
        duplicate = bookmark("https://example.test/duplicate", "Second exact match", "protect", OLDER)
        self.server.pins = [first, duplicate, dict(duplicate)]
        self.server.tags = {"protect": 2}
        self.env.update(pins_to_show="2", auto_update_cache="0")
        self.configure()
        self.assertEqual(self.search(), ["First exact match", "Second exact match"])

        # Recreate the MessagePack cache written before duplicate rows were removed.
        def cached(row):
            pin = [row[key] for key in ("href", "description", "tags", "shared", "toread", "extended", "time")]
            return [pin, ["protect"], row["description"].lower(), row["extended"].lower()]

        pins = [cached(row) for row in self.server.pins]
        tags = [[["protect", {"Used": 2}], "protect"]]
        snapshot = self.account_cache / "bookmarks.cache"
        for storage in ("snapshot", "legacy"):
            with self.subTest(storage=storage):
                if storage == "snapshot":
                    snapshot.write_bytes(pack_legacy_cache([pins, tags]))
                else:
                    snapshot.unlink()
                    (self.account_cache / "pins.cache").write_bytes(pack_legacy_cache(pins))
                    (self.account_cache / "tags.cache").write_bytes(pack_legacy_cache(tags))
                original = self.run_keyword("ps", "protect")
                selected = next(row for row in original["items"] if row.get("arg") == duplicate["href"])
                self.open_bookmark_result("ps", original, selected)
                expected = [duplicate["href"], first["href"]]
                self.assertEqual([row["arg"] for row in self.run_keyword("ps", "protect")["items"]], expected)
                view = self.run_keyword("pt", "protect")
                source, query, context = self.select_result("pt", view, view["items"][0])
                exact = self.run_filter(source, query, variables=context)
                self.assertEqual([row["arg"] for row in exact["items"]], expected)

    def test_open_bookmark_stays_responsive_while_history_is_locked(self):
        url = "https://example.test/history-lock?view=one&tab=two"
        self.server.pins = [bookmark(url, "History lock bookmark", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["History lock bookmark"])
        result = self.run_keyword("ps", "protect")
        row = result["items"][0]
        history = self.data / "pinboard-accounts" / TOKEN.split(":", 1)[0].encode().hex()
        history.mkdir(parents=True, exist_ok=True)
        with (history / "recent-bookmarks.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            action = self.select_result("ps", result, row)
            completed = self.run_command(
                ["bash", "-c", self.objects[action[0]]["script"], action[0], action[1]],
                env={**self.env, **action[2]}, timeout=2,
            )
            target, argument, _ = self.follow_connection(action[0], completed.stdout, action[2])
            self.assertEqual(self.object_types[target], "alfred.workflow.action.openurl")
            self.assertEqual(argument, url)
        self.open_bookmark_result("ps", result, row)
        self.assertEqual(json.loads((history / "recent-bookmarks.json").read_text()), [url])

    def test_ps_retries_an_export_missing_a_bookmark_with_a_previously_unseen_tag(self):
        existing = bookmark("https://example.test/cached", "Existing bookmark", "protect", OLDER)
        remote = bookmark("https://example.test/brandnew", "New cross-device bookmark", "brandnew", NEWER)
        self.server.pins = [existing]
        self.configure()
        self.assertEqual(self.search(), ["Existing bookmark"])
        self.server.tags = {"protect": 1, "brandnew": 1}
        self.server.timestamp = NEWER
        self.age_sync_state()
        initial = self.run_keyword("ps", "brandnew")
        self.assertEqual(initial["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("ps", "brandnew", variables=initial["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual(waiting["items"][0]["title"], "No bookmarks found!")
        self.server.pins.append(remote)
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=0)
        resumed = self.run_keyword("ps", "brandnew", variables=waiting["variables"])
        self.assertEqual(resumed["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        found = self.run_keyword("ps", "brandnew", variables=resumed["variables"])
        self.assertNotIn("rerun", found)
        self.assertEqual([row["title"] for row in found["items"]], ["New cross-device bookmark"])
        self.open_bookmark_result("ps", found, found["items"][0])

    def test_pt_retries_when_a_new_bookmarks_tag_has_not_reached_the_tag_index(self):
        existing = bookmark("https://example.test/cached", "Existing bookmark", "protect", OLDER)
        remote = bookmark("https://example.test/brandnew", "New cross-device bookmark", "brandnew", NEWER)
        self.server.pins = [existing]
        self.configure()
        self.assertEqual(self.search(), ["Existing bookmark"])
        self.server.pins.append(remote)
        self.server.tags = {"protect": 1}
        self.server.timestamp = NEWER
        self.age_sync_state()
        first = self.run_keyword("pt")
        self.assertEqual(first["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("pt", variables=first["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertNotIn("brandnew", {row.get("arg") for row in waiting["items"]})
        self.server.tags["brandnew"] = 1
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=0)
        resumed = self.run_keyword("pt", variables=waiting["variables"])
        self.assertEqual(resumed["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        tags = self.run_keyword("pt", variables=resumed["variables"])
        self.assertNotIn("rerun", tags)
        selected = next(row for row in tags["items"] if row.get("arg") == "brandnew")
        child, query, context = self.select_result("pt", tags, selected)
        found = self.run_filter(child, query, variables=context)
        self.assertEqual([row["title"] for row in found["items"]], ["New cross-device bookmark"])
        self.open_bookmark_result(child, found, found["items"][0])

    def test_sync_accepts_real_deletions_with_a_known_permanently_skipped_url(self):
        deleted = bookmark("https://example.test/deleted", "Deleted bookmark", "protect", OLDER)
        current = bookmark("https://example.test/current", "Current bookmark", "protect", OLDER)
        skipped = bookmark("/", "Permanently unusable URL", "protect", OLDER)
        self.server.pins = [deleted, current, skipped]
        self.configure()
        self.assertEqual(set(self.search()), {"Deleted bookmark", "Current bookmark"})
        self.server.pins = [{**current, "description": "Updated current bookmark"}, skipped]
        self.server.timestamp = NEWER
        self.age_sync_state()
        checking = self.run_keyword("ps", "protect")
        self.assertEqual(checking["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertIsNone(state["deferred_update"])
        found = self.run_keyword("ps", "protect", variables=checking["variables"])
        self.assertNotIn("rerun", found)
        self.assertEqual([row["title"] for row in found["items"]], ["Updated current bookmark"])
        self.assertIn("Updated cache files", self.run_object(*self.follow_connection("pu", "")).stdout)

        # Changes to the skipped row's tags still explain the current counts.
        self.server.pins = [{**current, "tags": "newtag"}, {**skipped, "tags": "newtag"}]
        self.server.timestamp = "2026-01-03T00:00:00Z"
        self.age_sync_state()
        self.assertEqual(self.run_keyword("pt")["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], self.server.timestamp)
        self.server.pins = [{**skipped, "tags": "newtag"}]
        self.server.timestamp = "2026-01-04T00:00:00Z"
        self.age_sync_state()
        self.assertEqual(self.run_keyword("ps", "newtag")["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], self.server.timestamp)
        self.assertIsNone(state["deferred_update"])
        self.assertEqual(self.search(), ["No bookmarks found!"])

    def test_conflicting_duplicate_bookmarks_keep_the_cached_value_until_recovery(self):
        kept = bookmark("https://example.test/cached", "Cached bookmark", "protect", OLDER)
        other = bookmark("https://example.test/other", "Other bookmark", "protect", NEWER)
        self.server.pins = [kept, other]
        self.configure()
        self.assertEqual(self.search(), ["Other bookmark", "Cached bookmark"])
        self.server.pins = [other, kept, {**kept, "description": "Conflicting title"}]
        self.server.tags = {"protect": 2}
        self.server.timestamp = NEWER
        self.age_sync_state()
        initial = self.run_keyword("ps", "protect")
        self.assertEqual(initial["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        waiting = self.run_keyword("ps", "protect", variables=initial["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([row["title"] for row in waiting["items"]], ["Other bookmark", "Cached bookmark"])
        self.server.pins = [other, {**kept, "description": "Recovered title"}]
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect", variables=waiting["variables"])["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Other bookmark", "Recovered title"])


    def test_open_p_retries_a_transient_browser_lookup_without_suggestions(self):
        url = "https://example.test/browser-recovery"
        self.server.pins = [bookmark(url, "Browser recovery", "protect", OLDER)]
        self.configure()
        self.search()
        self.env.update(auto_update_cache="0", suggest_tags="0", page_is_bookmarked="0")
        browser = self.workflow / "osascript"
        log = self.workflow / "browser-attempts"
        self.env["PINION_BROWSER_ATTEMPTS"] = str(log)

        for query, tags, notes in (("protect", "protect", ""), ("; retained note", "", "retained note")):
            with self.subTest(query=query):
                browser.write_text('#!/bin/sh\nprintf "passive\\n" >> "$PINION_BROWSER_ATTEMPTS"\nprintf "unexpected response\\n"\n')
                browser.chmod(0o700)
                count = len(log.read_text().splitlines()) if log.exists() else 0
                failed = self.run_keyword("p", query)
                save = next(row for row in failed["items"] if row.get("arg") == query)
                self.assertFalse(save["valid"])
                self.assertEqual(failed["rerun"], 5)
                self.assertEqual(len(log.read_text().splitlines()), count + 1)

                waiting = self.run_keyword("p", query, variables=failed["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual(len(log.read_text().splitlines()), count + 2)
                self.assertEqual(self.server.paths["/v1/posts/suggest"], 0)
                self.show_browser_page(url)
                ready = self.run_keyword("p", query, variables=waiting["variables"])
                save = next(row for row in ready["items"] if row.get("arg") == query)
                self.assertTrue(save.get("valid", True))
                self.assertNotIn("rerun", ready)
                saved = self.run_object(*self.select_result("p", ready, save))
                self.assertIn("Successfully posted", saved.stdout)
                self.assertEqual(self.server.saves[-1]["url"], url)
                self.assertEqual(self.server.saves[-1]["tags"], tags)
                self.assertEqual(self.server.saves[-1]["extended"], notes)

    def test_open_p_recovers_denied_suggestions_after_same_account_pa(self):
        endpoint = "/v1/posts/suggest"
        url = "https://example.test/suggestion-credentials"
        self.server.pins = [bookmark(url, "Suggestion credentials", "protect", OLDER)]
        self.server.popular_tags[url] = ["after-pa"]
        self.configure()
        self.search()
        self.env.update(auto_update_cache="0", suggest_tags="1", page_is_bookmarked="0")
        self.show_browser_page(url)

        for status in (401, 403):
            with self.subTest(status=status):
                self.wait_for_request_slot()
                self.server.status[endpoint] = status
                count = self.server.paths[endpoint]
                denied = self.run_keyword("p", "protect")
                self.assertEqual(denied["rerun"], 5)
                session = json.loads(denied["variables"]["pinion_post_page"])
                self.assertIsNone(session["suggested_tags"])
                self.assertTrue(session["suggestion_authentication_failed"])
                self.assertEqual(self.server.paths[endpoint], count + 1)
                # Repeated timer reruns inspect configuration locally and make no
                # further denied request while the credentials stay unchanged.
                for _ in range(2):
                    session["suggestion_retry_at"] = OLDER
                    variables = {**denied["variables"], "pinion_post_page": json.dumps(session)}
                    waiting = self.run_keyword("p", "protect", variables=variables)
                    self.assertEqual(waiting["rerun"], 5)
                    self.assertEqual(self.server.paths[endpoint], count + 1)
                del self.server.status[endpoint]
                token = ROTATED_TOKEN if status == 401 else TOKEN
                saved_token = self.run_object(*self.follow_connection("pa", token))
                self.assertIn("Successfully set user credentials.", saved_token.stdout)
                self.wait_for_request_slot()
                ready = self.run_keyword("p", "protect", variables=waiting["variables"])
                self.assertNotIn("rerun", ready)
                self.assertIn("after-pa", [row["title"] for row in ready["items"]])
                self.assertEqual(self.server.paths[endpoint], count + 2)
                session = json.loads(ready["variables"]["pinion_post_page"])
                self.assertFalse(session["suggestion_authentication_failed"])
                row = next(row for row in ready["items"] if row["title"] == "after-pa")
                posted = self.run_object(*self.select_result("p", ready, row))
                self.assertIn("Successfully posted", posted.stdout)
                self.assertEqual(self.server.saves[-1]["auth_token"], token)

        # A real successful empty response remains complete across pa.
        self.wait_for_request_slot()
        self.server.raw_suggestions = b"[]"
        empty = self.run_keyword("p", "protect")
        count = self.server.paths[endpoint]
        self.assertNotIn("rerun", empty)
        self.server.raw_suggestions = None
        self.run_object(*self.follow_connection("pa", ROTATED_TOKEN))
        reused = self.run_keyword("p", "protect", variables=empty["variables"])
        self.assertNotIn("rerun", reused)
        self.assertEqual(self.server.paths[endpoint], count)
        self.assertEqual(json.loads(reused["variables"]["pinion_post_page"])["suggested_tags"], [])

    def test_open_p_ignores_an_old_suggestion_auth_failure_after_pa(self):
        from unittest.mock import patch

        endpoint = "/v1/posts/suggest"
        url = "https://example.test/suggestion-token-race"
        self.server.pins = [bookmark(url, "Suggestion token race", "protect", OLDER)]
        self.server.popular_tags[url] = ["current-credentials"]
        self.configure()
        self.search()
        self.env.update(auto_update_cache="0", suggest_tags="1", page_is_bookmarked="0")
        self.show_browser_page(url)
        self.wait_for_request_slot()
        self.server.status[endpoint] = 401
        started, resume = Event(), Event()
        original = MockHandler.send_error

        def hold_old_denial(handler, code, *args, **kwargs):
            if code == 401 and urlparse(handler.path).path == endpoint:
                started.set()
                resume.wait(8)
            return original(handler, code, *args, **kwargs)

        with patch.object(MockHandler, "send_error", hold_old_denial):
            try:
                running = self.run_keyword("p", "protect")
                self.assertTrue(started.is_set())
                self.assertEqual(running["rerun"], 1)
                changed = self.run_object(*self.follow_connection("pa", ROTATED_TOKEN))
                self.assertIn("Successfully set user credentials.", changed.stdout)
                del self.server.status[endpoint]
            finally:
                resume.set()
            variables = running["variables"]
            deadline = time.monotonic() + 4
            while time.monotonic() < deadline:
                ready = self.run_keyword("p", "protect", variables=variables)
                variables = ready["variables"]
                if any(row["title"] == "current-credentials" for row in ready["items"]):
                    break
                self.assertGreater(ready.get("rerun", 0), 0)
                time.sleep(.05)
            else:
                self.fail("The old denial prevented the open p session from retrying with pa's token")
        self.assertNotIn("rerun", ready)
        self.assertEqual(self.server.paths[endpoint], 2)

    def test_update_rejects_control_characters_in_extracted_filenames(self):
        def ambiguous_archive(character, shadow):
            old_info = plistlib.dumps({"bundleid": "com.victorrodrigues.pinion", "name": "Pinion",
                                      "version": "1.0.0", "objects": [], "connections": {}})
            info = plistlib.dumps({"bundleid": "com.victorrodrigues.pinion", "name": "Pinion",
                                  "version": "1.1.0", "objects": [], "connections": {}})
            data = BytesIO()
            with ZipFile(data, "w", ZIP_DEFLATED) as archive:
                # macOS unzip strips ASCII controls from paths, including before
                # a later canonical entry. The validator must reject either name.
                name = f"info.pli{character}st" if shadow == "manifest" else f"pin{character}ion"
                archive.writestr(name, old_info if shadow == "manifest" else b"invalid program")
                archive.writestr("info.plist", info)
                add_fixture_executable(archive)
            return data.getvalue()

        self.server.release_tag = "v1.1.0"
        self.server.asset_body = workflow_bundle("1.1.0")
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        approved = self.run_object(*download).stdout
        self.assertTrue(approved.startswith("Download Successful: "), approved)
        bundle = Path(approved.removeprefix("Download Successful: "))
        installer = self.follow_connection(download[0], approved, download[2])
        opened = self.workflow / "opened"
        open_program = self.workflow / "open"
        open_program.write_text('#!/bin/sh\nprintf "%s" "$3" > opened\n')
        open_program.chmod(0o700)
        for character in ("\x01", "\t", "\n", "\r", "\x1b", "\x7f"):
            for shadow in ("manifest", "executable"):
                with self.subTest(character=repr(character), shadow=shadow):
                    body = ambiguous_archive(character, shadow)
                    bundle.write_bytes(body)
                    rejected = self.run_object(*installer).stdout
                    self.assertIn("invalid path in workflow bundle", rejected)
                    self.assertFalse(opened.exists())
                    packaging = self.run_command(
                        ["./pinion", "verify-workflow-release", "--tag=v1.1.0", f"--bundle={bundle}"],
                        check=False,
                    )
                    self.assertNotEqual(packaging.returncode, 0)
                    self.server.asset_body = body
                    rejected = self.run_object(*download).stdout
                    self.assertTrue(rejected.startswith("Error: "), rejected)
                    self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                    self.server.asset_body = workflow_bundle("1.1.0")
                    self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
        bundle.write_bytes(workflow_bundle("1.1.0"))
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text(), str(bundle))

    def test_update_rejects_unicode_path_overrides_before_installation(self):
        def overridden_entry(raw_name, effective_name):
            entry = ZipInfo(raw_name)
            payload = b"\x01" + pack("<I", crc32(raw_name.encode())) + effective_name.encode()
            entry.extra = pack("<HH", 0x7075, len(payload)) + payload
            return entry

        def ambiguous_bundle(shadow):
            data = BytesIO()
            with ZipFile(data, "w", ZIP_DEFLATED) as archive:
                if shadow == "manifest":
                    for raw_name, effective_name, version in (
                        ("info.plist", "infoé.plist", "1.0.0"),
                        ("info~1.plist", "info.plist", "1.1.0"),
                    ):
                        info = plistlib.dumps({"bundleid": "com.victorrodrigues.pinion",
                                               "name": "Pinion", "version": version,
                                               "objects": [], "connections": {}})
                        archive.writestr(overridden_entry(raw_name, effective_name), info)
                    add_fixture_executable(archive)
                else:
                    with ZipFile(BytesIO(workflow_bundle("1.1.0"))) as healthy:
                        for entry in healthy.infolist():
                            if shadow == "executable" and entry.filename == "pinion":
                                continue
                            archive.writestr(entry, healthy.read(entry.filename))
                    if shadow == "executable":
                        for raw_name, effective_name, body in (
                            ("pinion", "other", b"invalid program"),
                            ("pinion~1", "pinion", b"#!/bin/sh\nexit 0\n"),
                        ):
                            entry = overridden_entry(raw_name, effective_name)
                            entry.create_system = 3
                            entry.external_attr = 0o100755 << 16
                            archive.writestr(entry, body)
                    else:
                        archive.writestr(overridden_entry("resource", "renamed-resource"), b"resource")
            return data.getvalue()

        self.server.release_tag = "v1.1.0"
        self.server.asset_body = workflow_bundle("1.1.0")
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        approved = self.run_object(*download).stdout
        self.assertTrue(approved.startswith("Download Successful: "), approved)
        bundle = Path(approved.removeprefix("Download Successful: "))
        installer = self.follow_connection(download[0], approved, download[2])
        opened = self.workflow / "opened"
        program = self.workflow / "open"
        program.write_text('#!/bin/sh\nprintf "%s" "$3" > opened\n')
        program.chmod(0o700)
        for shadow in ("manifest", "executable", "resource"):
            with self.subTest(shadow=shadow):
                body = ambiguous_bundle(shadow)
                bundle.write_bytes(body)
                rejected = self.run_object(*installer).stdout
                self.assertTrue(rejected.startswith("Error: "), rejected)
                self.assertFalse(opened.exists())
                packaging = self.run_command(
                    ["./pinion", "verify-workflow-release", "--tag=v1.1.0", f"--bundle={bundle}"],
                    check=False,
                )
                self.assertNotEqual(packaging.returncode, 0)
                self.server.asset_body = body
                rejected = self.run_object(*download).stdout
                self.assertTrue(rejected.startswith("Error: "), rejected)
                notice, _, _ = self.follow_connection(download[0], rejected, download[2],
                                                      include_outputs=True)
                self.assertEqual(self.object_types[notice], "alfred.workflow.output.notification")
                self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                self.server.asset_body = workflow_bundle("1.1.0")
                self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
        bundle.write_bytes(workflow_bundle("1.1.0"))
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text(), str(bundle))

    def test_incomplete_http_asset_is_hidden_until_complete_bytes_recover(self):
        from unittest.mock import patch

        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        original = MockHandler.do_GET
        cache = self.cache / "last_check_status_async.json"

        def incomplete(handler):
            if urlparse(handler.path).path != ASSET_PATH:
                return original(handler)
            handler.server.paths[ASSET_PATH] += 1
            body = workflow_bundle("1.1.0")
            handler.send_response(200)
            handler.send_header("Content-Type", "application/octet-stream")
            handler.send_header("Content-Length", str(len(body)))
            handler.end_headers()
            handler.wfile.write(body[:len(body) // 2])

        with patch.object(MockHandler, "do_GET", incomplete):
            rejected = self.run_object(*download).stdout
            self.assertTrue(rejected.startswith("Error: "), rejected)
            self.assertIn("failed_asset", json.loads(cache.read_text()))
            self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
            waiting = self.run_keyword("ps", "protect")
            self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
            self.assertEqual(waiting["rerun"], 5)
            self.assertFalse(list(self.cache.glob("*.alfredworkflow")))
        self.server.asset_body = workflow_bundle("1.1.0")
        recovered = self.run_keyword("pcheck")
        self.assertEqual(recovered["items"][0]["arg"], "1.1.0")
        output = self.run_object(*self.select_result("pcheck", recovered, recovered["items"][0])).stdout
        self.assertTrue(output.startswith("Download Successful: "), output)

    def test_later_selected_success_recovers_after_an_earlier_direct_transfer_failure(self):
        from itertools import count
        from unittest.mock import patch

        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        download = self.select_result("pcheck", result, result["items"][0])
        original = MockHandler.do_GET
        requests = count()
        first, second, release_first, release_second = Event(), Event(), Event(), Event()

        def overlap(handler):
            if urlparse(handler.path).path != ASSET_PATH:
                return original(handler)
            number = next(requests)
            if number == 0:
                first.set()
                release_first.wait(10)
                handler.send_error(404)
                return
            if number == 1:
                second.set()
                release_second.wait(10)
            return original(handler)

        with patch.object(MockHandler, "do_GET", overlap):
            with subprocess.Popen(["./pinion", "self", "-d"], cwd=self.workflow, env=self.env,
                                  stdout=subprocess.PIPE, text=True) as older:
                self.assertTrue(first.wait(3))
                with subprocess.Popen(
                    ["bash", "-c", self.objects[download[0]]["script"], download[0], download[1]],
                    cwd=self.workflow, env={**self.env, **download[2]}, stdout=subprocess.PIPE,
                    text=True,
                ) as newer:
                    try:
                        self.assertTrue(second.wait(3))
                        release_first.set()
                        self.assertIn("Run pcheck again", older.communicate(timeout=5)[0])
                        failed = json.loads((self.cache / "last_check_status_async.json").read_text())
                        self.assertIsNone(failed["checked_at"])
                        self.assertEqual(failed["failed_version"], "1.1.0")
                        release_second.set()
                        approved = newer.communicate(timeout=5)[0]
                        self.assertTrue(approved.startswith("Download Successful: "), approved)
                    finally:
                        release_first.set()
                        release_second.set()
        cached = json.loads((self.cache / "last_check_status_async.json").read_text())
        self.assertEqual(cached["version"], "1.1.0")
        self.assertIn("asset_verified_at", cached)
        self.assertNotIn("failed_asset", cached)
        self.assertTrue(Path(approved.removeprefix("Download Successful: ")).is_file())
        installer = self.follow_connection(download[0], approved, download[2])
        opened = self.workflow / "opened"
        open_program = self.workflow / "open"
        open_program.write_text('#!/bin/sh\nprintf "%s" "$3" > opened\n')
        open_program.chmod(0o700)
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text(), approved.removeprefix("Download Successful: "))

    def test_older_selected_success_cannot_revive_a_newer_direct_transfer_failure(self):
        from itertools import count
        from unittest.mock import patch

        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        download = self.select_result("pcheck", result, result["items"][0])
        original = MockHandler.do_GET
        requests = count()
        first, release_first = Event(), Event()

        def overlap(handler):
            if urlparse(handler.path).path != ASSET_PATH:
                return original(handler)
            if next(requests) == 0:
                first.set()
                release_first.wait(10)
                return original(handler)
            handler.send_error(404)

        with patch.object(MockHandler, "do_GET", overlap):
            with subprocess.Popen(
                ["bash", "-c", self.objects[download[0]]["script"], download[0], download[1]],
                cwd=self.workflow, env={**self.env, **download[2]}, stdout=subprocess.PIPE,
                text=True,
            ) as older:
                try:
                    self.assertTrue(first.wait(3))
                    failure = self.run_command(["./pinion", "self", "-d"], check=False).stdout
                    self.assertIn("Run pcheck again", failure)
                    release_first.set()
                    rejected = older.communicate(timeout=5)[0]
                    self.assertIn("Run pcheck again", rejected)
                finally:
                    release_first.set()
        self.assertIn("failed_asset", json.loads((self.cache / "last_check_status_async.json").read_text()))
        self.assertFalse(list(self.cache.glob("*.alfredworkflow")))

    def test_newer_selected_transfer_keeps_authoritative_withdrawal_after_an_older_failure(self):
        from itertools import count
        from unittest.mock import patch

        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        download = self.select_result("pcheck", result, result["items"][0])
        original = MockHandler.do_GET
        requests = count()
        first, second, release_first, release_second = Event(), Event(), Event(), Event()

        def overlap(handler):
            if urlparse(handler.path).path != ASSET_PATH:
                return original(handler)
            number = next(requests)
            if number == 0:
                first.set()
                release_first.wait(10)
                handler.send_error(404)
                return
            if number == 1:
                second.set()
                release_second.wait(10)
            return original(handler)

        with patch.object(MockHandler, "do_GET", overlap):
            with subprocess.Popen(["./pinion", "self", "-d"], cwd=self.workflow, env=self.env,
                                  stdout=subprocess.PIPE, text=True) as older:
                self.assertTrue(first.wait(3))
                with subprocess.Popen(
                    ["bash", "-c", self.objects[download[0]]["script"], download[0], download[1]],
                    cwd=self.workflow, env={**self.env, **download[2]}, stdout=subprocess.PIPE,
                    text=True,
                ) as newer:
                    try:
                        self.assertTrue(second.wait(3))
                        release_first.set()
                        self.assertIn("Run pcheck again", older.communicate(timeout=5)[0])
                        self.server.status[RELEASE_PATH] = 404
                        self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                        cache = json.loads((self.cache / "last_check_status_async.json").read_text())
                        self.assertIsNotNone(cache["checked_at"])
                        release_second.set()
                        self.assertIn("Run pcheck again", newer.communicate(timeout=5)[0])
                    finally:
                        release_first.set()
                        release_second.set()
        cache = json.loads((self.cache / "last_check_status_async.json").read_text())
        self.assertIsNotNone(cache["checked_at"])
        self.assertFalse(list(self.cache.glob("*.alfredworkflow")))

    def test_concurrent_missing_transfers_remember_the_newest_failure(self):
        from itertools import count
        from unittest.mock import patch

        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        download = self.select_result("pcheck", result, result["items"][0])
        original = MockHandler.do_GET
        requests = count()
        started = [Event() for _ in range(3)]
        release = [Event() for _ in range(3)]
        cache_path = self.cache / "last_check_status_async.json"

        def overlap(handler):
            if urlparse(handler.path).path != ASSET_PATH:
                return original(handler)
            number = next(requests)
            started[number].set()
            release[number].wait(10)
            if number == 1:
                return original(handler)
            handler.send_error(404)

        with patch.object(MockHandler, "do_GET", overlap):
            with subprocess.Popen(["./pinion", "self", "-d"], cwd=self.workflow, env=self.env,
                                  stdout=subprocess.PIPE, text=True) as first:
                self.assertTrue(started[0].wait(3))
                with subprocess.Popen(
                    ["bash", "-c", self.objects[download[0]]["script"], download[0], download[1]],
                    cwd=self.workflow, env={**self.env, **download[2]}, stdout=subprocess.PIPE,
                    text=True,
                ) as selected:
                    self.assertTrue(started[1].wait(3))
                    with subprocess.Popen(["./pinion", "self", "-d"], cwd=self.workflow, env=self.env,
                                          stdout=subprocess.PIPE, text=True) as last:
                        try:
                            self.assertTrue(started[2].wait(3))
                            release[0].set()
                            self.assertIn("Run pcheck again", first.communicate(timeout=5)[0])
                            older_failure = json.loads(cache_path.read_text())["failed_transfer_started_at"]
                            release[2].set()
                            self.assertIn("Run pcheck again", last.communicate(timeout=5)[0])
                            self.assertGreater(json.loads(cache_path.read_text())["failed_transfer_started_at"],
                                               older_failure)
                            release[1].set()
                            self.assertIn("Run pcheck again", selected.communicate(timeout=5)[0])
                        finally:
                            for signal in release:
                                signal.set()
        self.assertIn("failed_asset", json.loads(cache_path.read_text()))
        self.assertFalse(list(self.cache.glob("*.alfredworkflow")))

    def test_older_caches_confirm_deletion_with_skipped_url_counts_without_losing_saves(self):
        deleted = bookmark("https://example.test/deleted", "Deleted bookmark", "protect", OLDER)
        current = bookmark("https://example.test/current", "Current bookmark", "protect", OLDER)
        skipped = bookmark("/", "Permanently unusable URL", "protect", OLDER)
        self.configure()

        def cached(row):
            pin = [row[key] for key in ("href", "description", "tags", "shared", "toread", "extended", "time")]
            return [pin, ["protect"], row["description"].lower(), row["extended"].lower()]

        for storage in ("snapshot", "legacy"):
            with self.subTest(storage=storage):
                self.server.pins = [deleted, current, skipped]
                self.server.timestamp = OLDER
                self.assertIn("Updated cache files", self.run_object(*self.follow_connection("pu", "")).stdout)
                pins = [cached(deleted), cached(current)]
                tags = [[["protect", {"Used": 3}], "protect"]]
                snapshot = self.account_cache / "bookmarks.cache"
                if storage == "snapshot":
                    snapshot.write_bytes(pack_legacy_cache([pins, tags]))
                else:
                    snapshot.unlink()
                    (self.account_cache / "pins.cache").write_bytes(pack_legacy_cache(pins))
                    (self.account_cache / "tags.cache").write_bytes(pack_legacy_cache(tags))
                self.server.pins = [{**current, "description": "Updated current bookmark"}, skipped]
                self.server.timestamp = NEWER
                self.server.status["/v1/posts/get"] = 503
                self.age_sync_state()
                first = self.run_keyword("ps", "protect")
                self.assertEqual(first["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], OLDER)
                self.assertEqual(state["deferred_update"], NEWER)
                waiting = self.run_keyword("ps", "protect", variables=first["variables"])
                self.assertEqual({row["title"] for row in waiting["items"]},
                                 {"Deleted bookmark", "Current bookmark"})
                self.assertEqual(waiting["rerun"], 5)
                self.show_browser_page(current["href"], "Current bookmark")
                self.env["suggest_tags"] = "0"
                choices = self.run_keyword("p", "protect; Saved during failed migration")
                chosen = next(row for row in choices["items"] if row.get("arg") == "protect; Saved during failed migration")
                self.assertIn("Successfully posted", self.run_object(*self.select_result("p", choices, chosen)).stdout)
                self.assertEqual(self.server.saves[-1]["extended"], "Saved during failed migration")
                del self.server.status["/v1/posts/get"]
                self.age_sync_state(minutes=0)
                resumed = self.run_keyword("ps", "protect", variables=waiting["variables"])
                self.assertEqual(resumed["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], NEWER)
                self.assertIsNone(state["deferred_update"])
                found = self.run_keyword("ps", "protect", variables=resumed["variables"])
                self.assertEqual([row["title"] for row in found["items"]], ["Updated current bookmark"])
                self.assertNotIn("rerun", found)
                self.open_bookmark_result("ps", found, found["items"][0])


    def test_legacy_migration_retries_malformed_deletion_lookups_before_evicting(self):
        old = bookmark("https://example.test/old", "Still-live bookmark", "protect", OLDER)
        current = bookmark("https://example.test/current", "Current bookmark", "protect", OLDER)
        skipped = bookmark("/", "Permanently unusable URL", "protect", OLDER)
        self.configure()

        def cached(row):
            pin = [row[key] for key in ("href", "description", "tags", "shared", "toread", "extended", "time")]
            return [pin, ["protect"], row["description"].lower(), row["extended"].lower()]

        for storage in ("snapshot", "legacy"):
            with self.subTest(storage=storage):
                self.server.pins = [old, current, skipped]
                self.server.export_pins = None
                self.server.lookup_posts = None
                self.server.tags = None
                self.server.timestamp = OLDER
                self.assertIn("Updated cache files", self.run_object(*self.follow_connection("pu", "")).stdout)
                pins = [cached(old), cached(current)]
                tags = [[["protect", {"Used": 3}], "protect"]]
                snapshot = self.account_cache / "bookmarks.cache"
                if storage == "snapshot":
                    snapshot.write_bytes(pack_legacy_cache([pins, tags]))
                else:
                    snapshot.unlink()
                    (self.account_cache / "pins.cache").write_bytes(pack_legacy_cache(pins))
                    (self.account_cache / "tags.cache").write_bytes(pack_legacy_cache(tags))

                self.server.export_pins = [current, skipped]
                self.server.tags = {"protect": 2}
                self.server.timestamp = NEWER
                variables = {}
                for posts in (
                    [{key: value for key, value in old.items() if key != "href"}],
                    [{**old, "href": "https://example.test/unrelated"}],
                    [old, {"description": "Incomplete unrelated row"}],
                    [old, old],
                ):
                    with self.subTest(posts=posts):
                        self.server.lookup_posts = posts
                        self.age_sync_state()
                        first = self.run_keyword("ps", "protect", variables=variables)
                        self.assertEqual(first["rerun"], 1)
                        state = self.wait_for_sync()
                        self.assertEqual(state["remote_update"], OLDER)
                        self.assertEqual(state["deferred_update"], NEWER)
                        waiting = self.run_keyword("ps", "protect", variables=first["variables"])
                        self.assertEqual(waiting["rerun"], 5)
                        self.assertEqual({row["title"] for row in waiting["items"]},
                                         {"Still-live bookmark", "Current bookmark"})
                        variables = waiting["variables"]
                self.assertIn(old, self.server.pins)

                # A valid live lookup still rejects the short export.
                self.server.lookup_posts = None
                self.age_sync_state(minutes=0)
                self.assertEqual(self.run_keyword("ps", "protect", variables=variables)["rerun"], 1)
                self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)

                # A genuinely empty lookup permits a real deletion at the same timestamp.
                self.server.pins = [current, skipped]
                self.age_sync_state(minutes=0)
                resumed = self.run_keyword("ps", "protect", variables=variables)
                self.assertEqual(resumed["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], NEWER)
                self.assertIsNone(state["deferred_update"])
                found = self.run_keyword("ps", "protect", variables=resumed["variables"])
                self.assertNotIn("rerun", found)
                self.assertEqual([row["title"] for row in found["items"]], ["Current bookmark"])
                self.open_bookmark_result("ps", found, found["items"][0])

    def test_pu_reports_startup_failures_through_its_error_output(self):
        def check_failure(keyword, message):
            update = self.follow_connection(keyword, "")
            result = self.run_object(*update)
            self.assertTrue(result.stdout.startswith("Error: "), result.stdout)
            self.assertIn(message, result.stdout)
            output, argument, _ = self.follow_connection(
                update[0], result.stdout, update[2], include_outputs=True,
            )
            self.assertEqual(self.object_types[output], "alfred.workflow.output.largetype")
            self.assertEqual(argument, result.stdout)

        check_failure("pu", "did you set API token?")
        self.data.mkdir(exist_ok=True)
        (self.data / "settings.json").write_text("invalid JSON")
        check_failure("pupdate", "Corrupted config file. Set API token again!")
        self.assertEqual(self.server.paths["/v1/posts/all"], 0)

    def test_delete_preview_cannot_act_on_a_new_account(self):
        url = "https://example.test/shared"
        self.server.pins = [bookmark(url, "First account", "protect", OLDER)]
        self.server.second_pins = [bookmark(url, "Second account", "protect", OLDER)]
        self.show_browser_page(url)
        self.configure()

        preview = self.run_filter("pind")
        row = next(row for row in preview["items"] if row.get("arg") == url)
        missing_context = {key: value for key, value in row.items() if key != "variables"}
        denied = self.run_object(*self.select_result("pind", preview, missing_context))
        self.assertIn("The Pinboard account changed", denied.stdout)
        self.configure(SECOND_TOKEN)
        denied = self.run_object(*self.select_result("pind", preview, row))
        self.assertIn("The Pinboard account changed", denied.stdout)
        self.assertEqual(self.server.paths["/v1/posts/delete"], 0)
        self.assertEqual(len(self.server.second_pins), 1)

        current = self.run_filter("pind")
        row = next(row for row in current["items"] if row.get("arg") == url)
        allowed = self.run_object(*self.select_result("pind", current, row))
        self.assertIn("Successfully deleted bookmark", allowed.stdout)
        self.assertEqual(self.server.deletes[0]["auth_token"], SECOND_TOKEN)
        self.assertEqual(len(self.server.pins), 1)
        self.assertFalse(self.server.second_pins)

        self.configure()
        delete_action = "E269A06C-1525-47CC-9700-2E6CD335FA63"
        direct = self.run_object(*self.follow_connection(delete_action, url))
        self.assertIn("Successfully deleted bookmark", direct.stdout)
        self.assertEqual(self.server.deletes[-1]["auth_token"], TOKEN)

    def test_pind_uses_passive_lookup_and_offers_a_url_based_alternative(self):
        url = "https://example.test/selected"
        self.server.pins = [bookmark(url, "Selected bookmark", "protect", OLDER)]
        self.configure()
        self.search()
        log = self.workflow / "browser-lookup.log"
        self.env["FIXTURE_BROWSER_LOG"] = str(log)
        browser = self.workflow / "osascript"
        browser.write_text("""#!/bin/sh
printf '%s\\n' "${4-interactive}" >> "$FIXTURE_BROWSER_LOG"
if [ "$4" = passive ]; then
    printf '{"pinion-browser-fallback:Firefox.app"}\\n'
else
    printf '{"https://example.test/unrelated fd850fc2e63511e79f720023dfdf24ec Wrong bookmark"}\\n'
fi
""")
        browser.chmod(0o700)

        view = self.run_filter("pind")
        row = view["items"][0]
        self.assertEqual(log.read_text().splitlines(), ["passive"])
        self.assertFalse(row["valid"])
        self.assertNotIn("arg", row)
        self.assertIn("ps", row["subtitle"])
        self.assertIn("Delete Pinboard Bookmark", row["subtitle"])
        self.assertEqual(self.server.paths["/v1/posts/delete"], 0)

        search = self.run_filter("ps", "protect")
        bookmark_row = next(row for row in search["items"] if row.get("arg") == url)
        delete_action = "E269A06C-1525-47CC-9700-2E6CD335FA63"
        deleted = self.run_object(*self.follow_connection(delete_action, bookmark_row["arg"]))
        self.assertIn("Successfully deleted bookmark", deleted.stdout)
        self.assertEqual(log.read_text().splitlines(), ["passive"])
        self.assertEqual(self.server.deletes[0]["url"], url)

    def test_delete_actions_route_startup_failures_to_alfred_error_output(self):
        url = "https://example.test/selected"
        self.server.pins = [bookmark(url, "Selected bookmark", "protect", OLDER)]
        self.show_browser_page(url)
        self.configure()

        preview = self.run_filter("pind")
        row = next(row for row in preview["items"] if row.get("arg") == url)
        selected = self.select_result("pind", preview, row)
        universal = self.follow_connection("E269A06C-1525-47CC-9700-2E6CD335FA63", url)
        settings = self.data / "settings.json"
        active_settings = settings.read_bytes()

        def check_failure(route, failure):
            result = self.run_object(*route)
            self.assertTrue(result.stdout.startswith("Error: "), result.stdout)
            self.assertIn(failure, result.stdout)
            output, text, _ = self.follow_connection(
                route[0], result.stdout, route[2], include_outputs=True,
            )
            self.assertEqual(self.object_types[output], "alfred.workflow.output.largetype")
            self.assertEqual(text, result.stdout)

        settings.unlink()
        check_failure(selected, "did you set API token?")
        settings.write_bytes(active_settings)
        Path(self.env["PINION_FIXTURE_KEYCHAIN"]).unlink()
        check_failure(universal, "Set it again with `pa`")

        preview = self.run_filter("pind")
        self.assertEqual(preview["items"][0]["title"], "Error")
        self.assertFalse(preview["items"][0]["valid"])
        self.assertEqual(self.server.paths["/v1/posts/delete"], 0)

    def test_tag_rename_keeps_its_account_across_both_filters(self):
        url = "https://example.test/shared-tag"
        self.server.pins = [bookmark(url, "First account", "protect", OLDER)]
        self.server.second_pins = [bookmark(url, "Second account", "protect", OLDER)]
        self.configure()
        self.search()
        original = self.run_filter("pr", "protect")
        row = next(row for row in original["items"] if row.get("arg") == "protect")
        start = self.select_result("pr", original, row)

        def open_rename(entry):
            output = self.run_object(*entry)
            return self.follow_workflow_output(entry[0], output.stdout, entry[2])

        def check_failure(entry, explanation):
            output = json.loads(self.run_object(*entry).stdout)["alfredworkflow"]
            self.assertEqual(output["variables"]["success"], "false")
            destination, message, _ = self.follow_connection(
                entry[0], output["arg"], {**entry[2], **output["variables"]},
                include_outputs=True,
            )
            self.assertEqual(self.object_types[destination], "alfred.workflow.output.largetype")
            self.assertIn(explanation, message)

        def select_new_tag(editor):
            choices = self.run_filter(editor[0], "renamed", variables=editor[2])
            row = next(row for row in choices["items"] if row.get("arg") == "renamed")
            return self.select_result(editor[0], choices, row)

        editor = open_rename(start)
        final = select_new_tag(editor)
        self.configure(SECOND_TOKEN)
        for action in (start, final):
            check_failure(action, "The Pinboard account changed")
        self.assertEqual(self.server.paths["/v1/tags/rename"], 0)
        self.assertEqual(self.server.second_pins[0]["tags"], "protect")

        universal_action = "AB0E9CB1-D5D1-483E-B88C-DAC90C702C41"
        fresh = self.follow_connection(universal_action, "protect")
        saved = json.loads(self.run_object(*select_new_tag(open_rename(fresh))).stdout)
        self.assertEqual(saved["alfredworkflow"]["variables"]["success"], "true")
        self.assertEqual(self.server.renames[0]["auth_token"], SECOND_TOKEN)
        self.assertEqual(self.server.pins[0]["tags"], "protect")
        self.assertEqual(self.server.second_pins[0]["tags"], "renamed")

    def test_tag_rename_editor_open_shows_startup_errors_for_both_alfred_entries(self):
        universal = self.follow_connection("AB0E9CB1-D5D1-483E-B88C-DAC90C702C41", "protect")

        def check_error(entry):
            opened = self.run_object(*entry)
            result = json.loads(opened.stdout)["alfredworkflow"]
            self.assertEqual(result["variables"]["success"], "false")
            destination, text, _ = self.follow_connection(
                entry[0], result["arg"], {**entry[2], **result["variables"]},
                include_outputs=True,
            )
            self.assertEqual(self.object_types[destination], "alfred.workflow.output.largetype")
            self.assertIn("did you set API token?", text)

        check_error(universal)

        self.server.pins = [bookmark("https://example.test/tagged", "Tagged", "protect", OLDER)]
        self.configure()
        self.search()
        view = self.run_filter("pr", "protect")
        selected = next(row for row in view["items"] if row.get("arg") == "protect")
        entry = self.select_result("pr", view, selected)
        (self.data / "settings.json").unlink()
        check_error(entry)
        self.assertEqual(self.server.paths["/v1/tags/rename"], 0)

    def test_alfred_tag_rename_saves_without_python_and_reports_startup_errors(self):
        source, target = '-old"tag', '-new"tag'
        url = "https://example.test/tagged"
        self.server.pins = [bookmark(url, "Tagged bookmark", source, OLDER)]
        self.configure()
        self.search()
        self.env["auto_update_cache"] = "0"

        results = self.run_filter("pr", source)
        row = next(row for row in results["items"] if row.get("arg") == source)
        start = self.select_result("pr", results, row)
        opened = self.run_object(*start)
        detail, _, context = self.follow_workflow_output(start[0], opened.stdout, start[2])
        results = self.run_filter(detail, target, variables=context)
        row = next(row for row in results["items"] if row.get("arg") == target)
        save, argument, variables = self.select_result(detail, results, row)

        # Run the actual save script with no Python or Xcode shims on PATH.
        # The test Keychain helper still needs cat to read the fixture token.
        (self.workflow / "cat").symlink_to("/bin/cat")
        env = {**self.env, **variables, "PATH": str(self.workflow)}

        def save_tag():
            output = self.run_command(["/bin/bash", "-c", self.objects[save]["script"], save, argument],
                                      env=env).stdout
            message = json.loads(output)["alfredworkflow"]
            destination, text, _ = self.follow_connection(
                save, message["arg"], {**variables, **message["variables"]}, include_outputs=True,
            )
            return message, self.object_types[destination], text

        settings = self.data / "settings.json"
        valid_settings = settings.read_bytes()
        settings.unlink()
        denied, destination, text = save_tag()
        self.assertEqual(denied["variables"]["success"], "false")
        self.assertEqual(destination, "alfred.workflow.output.largetype")
        self.assertIn("did you set API token?", text)
        self.assertEqual(self.server.paths["/v1/tags/rename"], 0)

        settings.write_bytes(valid_settings)
        saved, destination, text = save_tag()
        self.assertEqual(saved["variables"]["success"], "true")
        self.assertEqual(destination, "alfred.workflow.output.notification")
        self.assertEqual(text, "Successfully renamed tag.")
        self.assertEqual(self.server.pins[0]["tags"], target)

        direct = self.run_command(["./pinion", "rename", "--", target, source])
        self.assertEqual(direct.stdout, "Successfully renamed tag.")
        direct = self.run_command(["./pinion", "rename", "--", "", target], check=False)
        self.assertEqual(direct.returncode, 1)
        self.assertEqual(direct.stdout, "Error: Enter two tags.")

    def test_post_startup_failures_reach_the_error_notification_for_each_modifier(self):
        url = "https://example.test/browser-page"
        self.server.pins = [bookmark(url, "Browser page", "protect", OLDER)]
        self.configure()
        self.show_browser_page(url)
        self.search()
        results = self.run_filter("p", "protect")
        row = next(row for row in results["items"] if row.get("arg") == "protect")
        token_file = Path(self.env["PINION_FIXTURE_KEYCHAIN"])
        blocked_dir = self.workflow / "blocked-cache-parent"
        blocked_dir.write_text("Not a directory")

        for settings, expected in (
            ({"alfred_workflow_cache": str(blocked_dir / "cache")}, "Cache:"),
            ({"PINION_FIXTURE_KEYCHAIN": str(token_file.with_name("no-token"))},
             "Set it again with `pa`"),
        ):
            for modifiers in (0, 262144, 524288, 786432):
                with self.subTest(settings=settings, modifiers=modifiers):
                    uid, query, variables = self.select_result("p", results, row, modifiers=modifiers)
                    result = self.run_object(uid, query, {**variables, **settings})
                    self.assertTrue(result.stdout.startswith("Error: "), result.stdout)
                    self.assertNotIn("panicked", result.stderr)
                    feedback = self.follow_connection(uid, result.stdout, variables)
                    message = self.run_object(*feedback).stdout
                    self.assertIn(expected, message)
                    notice, text, _ = self.follow_connection(feedback[0], message, feedback[2],
                                                             include_outputs=True)
                    self.assertEqual(self.object_types[notice], "alfred.workflow.output.notification")
                    self.assertEqual(self.objects[notice]["title"], "Error")
                    self.assertEqual(text, message)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        token_file.unlink()
        direct = self.run_command(["./pinion", "post"], check=False)
        self.assertEqual(direct.returncode, 1)
        self.assertEqual(json.loads(direct.stdout)["items"][0]["title"], "Error")

    def test_post_suggestions_save_only_the_selected_browser_page_and_account(self):
        page_a = "https://example.test/page-a"
        page_b = "https://example.test/page-b"
        self.server.pins = [bookmark(page_b, "Existing B", "keep-me", OLDER)]
        self.server.popular_tags[page_a] = ["a-only"]
        self.configure()
        self.search()
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.show_browser_page(page_a)
        self.wait_for_request_slot()

        view = self.run_filter("p", "test")
        deadline = time.monotonic() + 4
        while not any(item["title"] == "a-only" for item in view["items"]):
            self.assertGreater(view.get("rerun", 0), 0)
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)
            view = self.run_filter("p", "test", variables=view["variables"])
        row = next(row for row in view["items"] if row["title"] == "a-only")
        save = self.select_result("p", view, row)
        self.show_browser_page(page_b)
        denied = self.run_object(*save)
        self.assertIn("The browser page changed", denied.stdout)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        self.show_browser_page(page_a)
        self.configure(SECOND_TOKEN)
        denied = self.run_object(*save)
        self.assertIn("The Pinboard account changed", denied.stdout)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)
        stale_rerun = self.run_filter("p", "test", variables=view["variables"])
        notice = next(row for row in stale_rerun["items"] if "account changed" in row["title"])
        self.assertFalse(notice["valid"])
        self.assertFalse(any(row.get("valid", True) for row in stale_rerun["items"]))
        fresh = self.run_filter("p", "test; fresh session")
        row = next(row for row in fresh["items"] if row.get("arg") == "test; fresh session")
        self.assertTrue(row.get("valid", True))
        self.assertEqual(row["variables"]["pinion_post_account"], SECOND_TOKEN.split(":", 1)[0])

        self.configure()
        allowed = self.run_object(*save)
        self.assertIn("Successfully posted", allowed.stdout)
        self.assertEqual(self.server.saves[0]["url"], page_a)
        self.assertEqual(self.server.saves[0]["tags"], "a-only")

    def test_post_waits_for_an_identifiable_page_before_offering_save(self):
        url = "https://example.test/page"
        self.server.pins = [bookmark(url, "Browser page", "protect", OLDER)]
        self.configure()
        self.search()
        self.env.update(page_is_bookmarked="0", suggest_tags="0", auto_update_cache="0")
        browser = self.workflow / "osascript"
        browser.write_text("#!/bin/sh\nprintf 'unexpected response\\n'\n")
        browser.chmod(0o700)

        view = self.run_filter("p", "; note")
        row = next(row for row in view["items"] if row["arg"] == "; note")
        self.assertFalse(row["valid"])
        self.assertIn("Couldn't identify the browser page", row["subtitle"])

        self.show_browser_page(url)
        ready = self.run_filter("p", "; note", variables=view["variables"])
        row = next(row for row in ready["items"] if row["arg"] == "; note")
        saved = self.run_object(*self.select_result("p", ready, row))
        self.assertIn("Successfully posted", saved.stdout)
        self.assertEqual(self.server.saves[0]["url"], url)
        self.assertEqual(self.server.saves[0]["tags"], "")
        self.assertEqual(self.server.saves[0]["extended"], "note")

    def test_leading_hyphens_pass_through_tag_search_rename_and_all_post_routes(self):
        url = "https://example.test/hyphen"
        self.server.pins = [bookmark(url, "Hyphen result", "-original", OLDER)]
        self.configure()
        self.search()
        self.env.update(page_is_bookmarked="0", suggest_tags="0", auto_update_cache="0")
        self.show_browser_page(url)

        view = self.run_filter("pt", "-original")
        row = next(row for row in view["items"] if row.get("arg") == "-original")
        detail, query, context = self.select_result("pt", view, row)
        matches = self.run_filter(detail, query, variables=context)
        self.assertEqual([row["title"] for row in matches["items"]], ["Hyphen result"])

        view = self.run_filter("pr", "-original")
        row = next(row for row in view["items"] if row.get("arg") == "-original")
        start = self.select_result("pr", view, row)
        output = self.run_object(*start)
        detail, _, context = self.follow_workflow_output(start[0], output.stdout, start[2])
        edit = self.run_filter(detail, "-renamed", variables=context)
        row = next(row for row in edit["items"] if row.get("arg") == "-renamed")
        saved = json.loads(self.run_object(*self.select_result(detail, edit, row)).stdout)
        self.assertEqual(saved["alfredworkflow"]["variables"]["success"], "true")
        self.assertEqual(self.server.pins[0]["tags"], "-renamed")

        for modifiers, shared, toread in ((0, "no", "no"), (262144, "no", "yes"),
                                          (524288, "yes", "no"), (786432, "yes", "yes")):
            with self.subTest(modifiers=modifiers):
                view = self.run_filter("p", "-new-tag; -leading note")
                row = next(row for row in view["items"] if row.get("arg") == "-new-tag; -leading note")
                output = self.run_object(*self.select_result("p", view, row, modifiers=modifiers))
                self.assertIn("Successfully posted", output.stdout)
                saved = self.server.saves[-1]
                self.assertEqual((saved["tags"], saved["extended"], saved["shared"], saved["toread"]),
                                 ("-new-tag", "-leading note", shared, toread))

    def test_bare_p_omits_blank_rows_and_saves_only_a_selected_tag(self):
        url = "https://example.test/existing"
        self.server.pins = [bookmark(url, "Existing", "protect", OLDER, notes="Keep this note")]
        self.server.popular_tags[url] = ["", "  ", "recommended"]
        self.env.update(page_is_bookmarked="1", suggest_tags="1", auto_update_cache="0")
        self.configure()
        self.assertEqual(self.search(), ["Existing"])
        self.show_browser_page(url, "Existing")
        self.wait_for_request_slot()

        result = self.run_keyword("p")
        deadline = time.monotonic() + 3
        while not any(row.get("arg") == "recommended" for row in result["items"]):
            self.assertGreater(result.get("rerun", 0), 0, result)
            self.assertLess(time.monotonic(), deadline, "The recommendation never arrived")
            result = self.run_keyword("p", variables=result["variables"])
        choices = [row for row in result["items"] if row.get("valid", True)]
        self.assertEqual([row["arg"] for row in choices], ["protect", "recommended"])
        self.assertTrue(all(row["title"].strip() for row in choices))
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)
        self.assertEqual(self.server.pins[0]["extended"], "Keep this note")

        action = self.select_result("p", result, choices[1])
        self.assertIn("Successfully posted", self.run_object(*action).stdout)
        self.assertEqual([save["tags"] for save in self.server.saves], ["recommended"])

    def test_bare_p_on_empty_account_requires_explicit_untagged_save(self):
        url = "https://example.test/new-page"
        self.env.update(page_is_bookmarked="0", suggest_tags="0", auto_update_cache="0")
        self.configure()
        self.assertEqual(self.search(), ["No bookmarks found!"])
        self.show_browser_page(url)

        result = self.run_keyword("p")
        [hint] = result["items"]
        self.assertIn("Type tags", hint["title"])
        self.assertFalse(hint["valid"])
        with self.assertRaises(AssertionError):
            self.select_result("p", result, hint)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        saveable = self.run_keyword("p", "; explicit no-tag save", variables=result["variables"])
        [row] = saveable["items"]
        self.assertIn("without tags", row["title"])
        action = self.select_result("p", saveable, row)
        self.assertIn("Successfully posted", self.run_object(*action).stdout)
        self.assertEqual([(save["url"], save["tags"], save["extended"]) for save in self.server.saves],
                         [(url, "", "explicit no-tag save")])

    def test_pinboard_paces_ordinary_requests_on_first_download(self):
        self.server.ordinary_request_interval = 0.06
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()

        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)
        self.assertEqual(self.server.paths["/v1/tags/get"], 1)
        self.assertFalse(self.server.throttled_requests)

    def test_pinboard_paces_rename_lookup_and_save(self):
        self.server.ordinary_request_interval = 0.06
        url = "https://example.test/retitle"
        self.server.pins = [bookmark(url, "Original", "protect", OLDER)]
        self.configure()
        self.env["auto_update_cache"] = "0"
        start, edit, save = [next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}{n}"))
                             for n in ("0002", "0003", "0004")]
        opened = json.loads(self.run_object(start, url).stdout)["alfredworkflow"]
        self.assertEqual(opened["arg"], "Original")
        row = json.loads(self.run_object(edit, "- Renamed", opened["variables"]).stdout)["items"][0]
        self.assertTrue(row["valid"])
        self.assertIn("Renamed to: - Renamed", self.run_object(save, row["arg"]).stdout)
        self.assertEqual(self.server.saves[0]["description"], "- Renamed")
        self.assertFalse(self.server.throttled_requests)

    def test_pinboard_paces_post_after_background_check(self):
        self.server.ordinary_request_interval = 0.06
        self.server.pins = [bookmark("https://example.test/other", "Other page", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Other page"])

        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' '{\"https://example.test/page fd850fc2e63511e79f720023dfdf24ec Page\"}'\n"
        )
        browser.chmod(0o700)
        self.env.update(page_is_bookmarked="0", suggest_tags="1")
        self.age_sync_state(age_download=False)
        self.wait_for_request_slot()
        self.server.block_update = True
        try:
            result = self.run_keyword("p")
            self.assertIn("suggested", [item["title"] for item in result["items"]])
            self.assertTrue(self.server.update_started.wait(3))
        finally:
            self.server.resume_update.set()

        post = self.run_command(["./pinion", "post", "--tags", "protect"])
        self.assertIn("Successfully posted", post.stdout)
        self.assertEqual(self.server.saves[0]["url"], "https://example.test/page")
        self.assertFalse(self.server.throttled_requests)
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

    def test_pinboard_retry_after_carries_across_commands(self):
        url = "https://example.test/retitle"
        self.server.pins = [bookmark(url, "Original", "protect", OLDER)]
        self.configure()
        self.server.status["/v1/posts/get"] = 429
        self.server.retry_after["/v1/posts/get"] = "60"
        self.run_command(["./pinion", "retitle", "--url", url], check=False)
        self.assertEqual(self.server.paths["/v1/posts/get"], 1)

        self.server.status.clear()
        self.assertNotEqual(self.run_command(["./pinion", "retitle", "--url", url],
                                             check=False).returncode, 0)
        self.assertEqual(self.server.paths["/v1/posts/get"], 1)
        self.assertIn("Too Many Requests", self.run_command(["./pinion", "update"]).stdout)
        self.assertEqual(self.server.paths["/v1/posts/all"], 0)

        window = self.account_cache / "pinboard-requests.json"
        window.write_text("invalid json")
        self.run_command(["./pinion", "retitle", "--url", url])
        self.assertEqual(self.server.paths["/v1/posts/get"], 2)
        # Bookmark sync also recorded that deadline when pu was refused.
        state = json.loads(self.sync_file.read_text())
        state["retry_after"]["deadline"] = OLDER
        self.sync_file.write_text(json.dumps(state))

        self.server.status["/v1/posts/all"] = 429
        self.server.retry_after["/v1/posts/all"] = "60"
        self.age_sync_state(minutes=61, age_download=False)
        self.assertIn("Too Many Requests", self.run_command(["./pinion", "update"]).stdout)
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)
        self.run_command(["./pinion", "retitle", "--url", url])
        self.assertEqual(self.server.paths["/v1/posts/get"], 3)
        self.server.status.clear()
        self.age_sync_state(minutes=61, age_download=False)
        self.assertIn("Too Many Requests", self.run_command(["./pinion", "update"]).stdout)
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

    def test_sync_resumes_when_pinboard_retry_after_expires(self):
        for endpoint in ("/v1/posts/get", "/v1/posts/all"):
            with self.subTest(endpoint=endpoint):
                self.configure()
                self.server.pins = [
                    bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
                ]
                self.server.timestamp = OLDER
                self.assertIn("Updated cache files", self.run_command(["./pinion", "update"]).stdout)
                self.assertEqual(self.search(), ["Dashboard"])

                # A request outside the sync worker sets a pacing deadline the
                # worker has not observed. Move the pending screen to just before it.
                self.server.status[endpoint] = 429
                self.server.retry_after[endpoint] = "600"
                if endpoint == "/v1/posts/all":
                    # Replicate a separate throttled full download without letting
                    # it also update this invocation's bookmark sync state.
                    before = self.sync_file.read_text()
                    self.assertIn("Too Many Requests", self.run_command(["./pinion", "update"]).stdout)
                    self.sync_file.write_text(before)
                else:
                    url = self.server.pins[0]["href"]
                    lookup = self.run_command(["./pinion", "retitle", "--url", url], check=False)
                    self.assertNotEqual(lookup.returncode, 0)
                self.server.status.clear()
                self.age_sync_state()
                previous_attempt = datetime.fromisoformat(
                    json.loads(self.sync_file.read_text())["last_download_attempt"]
                )
                downloads = self.server.paths["/v1/posts/all"]
                checks = self.server.paths["/v1/posts/update"]
                window_file = self.account_cache / "pinboard-requests.json"
                window = json.loads(window_file.read_text())
                window_key = "download_retry_after" if endpoint == "/v1/posts/all" else "retry_after"
                self.assertIsNotNone(window[window_key])
                self.server.pins.append(bookmark("https://example.test/new", "Remote", "protect", NEWER))
                self.server.timestamp = NEWER

                started = self.run_keyword("ps", "protect")
                self.assertEqual(started["rerun"], 1)
                blocked = self.wait_for_sync()
                self.assertEqual(blocked["throttle_minutes"], 0)
                self.assertEqual(datetime.fromisoformat(blocked["last_download_attempt"]), previous_attempt)
                self.assertEqual(blocked["retry_after"]["deadline"], window[window_key])
                self.assertEqual(self.server.paths["/v1/posts/all"], downloads)
                if endpoint == "/v1/posts/get":
                    self.assertEqual(self.server.paths["/v1/posts/update"], checks)

                waiting = self.run_keyword("ps", "protect", variables=started["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
                self.assertIn("Too Many Requests", self.run_command(["./pinion", "update"]).stdout)
                self.assertEqual(self.server.paths["/v1/posts/all"], downloads)

                now = datetime.now(timezone.utc)
                expired = (now - timedelta(seconds=1)).isoformat()
                blocked["retry_after"] = {"recorded_at": OLDER, "deadline": expired}
                self.sync_file.write_text(json.dumps(blocked))
                window = json.loads(window_file.read_text())
                window[window_key] = expired
                window_file.write_text(json.dumps(window))

                if endpoint == "/v1/posts/all":
                    self.assertEqual(self.run_keyword("ps", "protect", variables=waiting["variables"])["rerun"], 1)
                    self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
                else:
                    self.assertIn("Updated cache files", self.run_command(["./pinion", "update"]).stdout)
                self.assertEqual(self.search(), ["Remote", "Dashboard"])
                self.assertEqual(self.server.paths["/v1/posts/all"], downloads + 1)

    def test_post_filter_rechecks_page_membership_after_sync(self):
        page = "https://example.test/browser-page"
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{page} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
        )
        browser.chmod(0o700)
        self.env.update(page_is_bookmarked="1", suggest_tags="0")
        existing = bookmark("https://example.test/old", "Other page", "before", OLDER)
        added = bookmark(page, "Browser page", "after", NEWER)
        self.server.pins = [existing]
        self.configure()
        loading = self.run_keyword("p")
        self.assertGreater(loading.get("rerun", 0), 0)
        self.assertFalse(loading["items"][0]["valid"])
        self.wait_for_sync()
        self.age_sync_state()
        self.server.pins = [added]
        self.server.timestamp = NEWER
        self.server.block_update = True
        notice = "You already have the bookmark!"

        try:
            initial = self.run_keyword("p", "a", variables=loading["variables"])
            self.assertNotIn(notice, [item["title"] for item in initial["items"]])
            self.assertGreater(initial.get("rerun", 0), 0)
            self.assertTrue(self.server.update_started.wait(3))
        finally:
            self.server.resume_update.set()
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)

        # Alfred supplies the previous result's session variables on a rerun.
        # The saved tab URL avoids calling the browser again.
        browser.write_text("#!/bin/sh\nexit 1\n")
        rerun = self.run_keyword("p", "a", variables=initial["variables"])
        titles = [item["title"] for item in rerun["items"]]
        self.assertEqual(titles[-1], notice)
        self.assertFalse(rerun["items"][-1]["valid"])
        self.assertNotIn("arg", rerun["items"][-1])
        self.assertTrue(rerun["items"][0].get("valid", True))
        with self.assertRaises(AssertionError):
            self.select_result("p", rerun, rerun["items"][-1])
        self.assertIn("after", titles)
        self.assertNotIn("before", titles)

    def test_post_filter_keeps_popular_tags_in_the_current_page_session(self):
        page_a = "https://example.test/page-a"
        page_b = "https://example.test/page-b"
        self.server.popular_tags = {page_a: ["page-a-only"], page_b: ["page-b-only"]}
        self.server.pins = [bookmark(page_a, "Page A", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Page A"])
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")

        def show_page(url):
            browser = self.workflow / "osascript"
            browser.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' '{{\"{url} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
            )
            browser.chmod(0o700)

        def titles(result):
            return [item["title"] for item in result["items"]]

        show_page(page_a)
        self.wait_for_request_slot()
        initial = self.run_keyword("p")
        self.assertIn("page-a-only", titles(initial))
        typed = self.run_keyword("p", "protect", variables=initial["variables"])
        self.assertIn("page-a-only", titles(typed))
        self.assertEqual(self.server.paths["/v1/posts/suggest"], 1)

        show_page(page_b)
        self.wait_for_request_slot()
        self.server.block_suggestions_for = page_b
        try:
            # Model a first lookup that runs past the filter's response budget.
            initial = self.run_keyword("p", timeout=3)
            self.assertTrue(self.server.suggestion_started.is_set())
            self.assertNotIn("page-a-only", titles(initial))
            self.assertEqual(initial["rerun"], 1)
            typed = self.run_keyword("p", "protect", variables=initial["variables"])
            self.assertNotIn("page-a-only", titles(typed))
            self.assertNotIn("page-b-only", titles(typed))
            self.assertEqual(self.server.paths["/v1/posts/suggest"], 2)
            # Typing notes hides suggestions but keeps the same in-flight
            # request available if the user removes those notes.
            description = self.run_keyword("p", "protect; some notes", variables=typed["variables"])
            self.assertNotIn("rerun", description)
            self.assertEqual(self.server.paths["/v1/posts/suggest"], 2)
        finally:
            self.server.resume_suggestion.set()

        variables = description["variables"]
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            edited = self.run_keyword("p", "protect", variables=variables)
            variables = edited["variables"]
            if "page-b-only" in titles(edited):
                break
            time.sleep(0.02)
        else:
            self.fail("The p filter did not show the completed suggestions")
        self.assertNotIn("page-a-only", titles(edited))
        self.assertNotIn("rerun", edited)
        self.assertEqual(self.server.paths["/v1/posts/suggest"], 2)

    def test_post_filter_reruns_when_suggestions_wait_for_request_slot(self):
        page = "https://example.test/browser-page"
        self.server.pins = [bookmark(page, "Browser page", "local", OLDER)]
        self.server.popular_tags[page] = ["from-pinboard"]
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{page} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
        )
        browser.chmod(0o700)
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.configure()

        self.run_keyword("ps", "local")
        self.wait_for_sync()
        request_window = self.account_cache / "pinboard-requests.json"
        window = json.loads(request_window.read_text())
        # Restore the production window after the fixture's fast cache bootstrap.
        now = datetime.now(timezone.utc)
        window.update(recorded_at=now.isoformat(), available_at=(now + timedelta(seconds=3)).isoformat())
        request_window.write_text(json.dumps(window))

        def titles(result):
            return [item["title"] for item in result["items"]]

        reserved = self.run_keyword("p", timeout=2)
        self.assertIn("local", titles(reserved))
        self.assertNotIn("from-pinboard", titles(reserved))
        self.assertEqual(reserved["rerun"], 1)
        self.assertEqual(self.server.paths["/v1/posts/suggest"], 0)

        # Simulate another request awaiting headers even after the slot expires.
        window["available_at"] = now.isoformat()
        request_window.write_text(json.dumps(window))
        with (self.account_cache / "pinboard-requests.lock").open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            busy = self.run_keyword("p", timeout=2, variables=reserved["variables"])
            self.assertIn("local", titles(busy))
            self.assertEqual(busy["rerun"], 1)
            self.assertEqual(self.server.paths["/v1/posts/suggest"], 0)

        # The deferred lookup keeps its page and requires no keystroke to retry.
        browser.write_text("#!/bin/sh\nexit 1\n")
        ready = self.run_keyword("p", variables=busy["variables"])
        self.assertIn("from-pinboard", titles(ready))
        self.assertNotIn("rerun", ready)
        reused = self.run_keyword("p", "local", variables=ready["variables"])
        self.assertIn("from-pinboard", titles(reused))
        self.assertEqual(self.server.paths["/v1/posts/suggest"], 1)

    def test_post_filter_retries_suggestions_after_pinboard_retry_after(self):
        page = "https://example.test/browser-page"
        suggestions = "/v1/posts/suggest"
        self.server.pins = [bookmark(page, "Browser page", "local", OLDER)]
        self.server.popular_tags[page] = ["from-pinboard"]
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.configure()
        self.run_keyword("ps", "local")
        self.wait_for_sync()

        browser = self.workflow / "osascript"
        request_window = self.account_cache / "pinboard-requests.json"

        for endpoint in ("/v1/posts/get", suggestions):
            with self.subTest(endpoint=endpoint):
                browser.write_text(
                    "#!/bin/sh\n"
                    f"printf '%s\\n' '{{\"{page} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
                )
                browser.chmod(0o700)
                self.wait_for_request_slot()
                self.server.status[endpoint] = 429
                self.server.retry_after[endpoint] = "30"
                if endpoint != suggestions:
                    lookup = self.run_command(["./pinion", "retitle", "--url", page], check=False)
                    self.assertNotEqual(lookup.returncode, 0)

                count = self.server.paths[suggestions] + int(endpoint == suggestions)
                first = self.run_keyword("p", "local")
                self.assertEqual(first["rerun"], 5)
                self.assertIn("local", [item["title"] for item in first["items"]])
                self.assertNotIn("from-pinboard", [item["title"] for item in first["items"]])
                self.assertEqual(self.server.paths[suggestions], count)

                browser.write_text("#!/bin/sh\nexit 1\n")
                # Another operation can own the pacing lock while we are waiting.
                with (self.account_cache / "pinboard-requests.lock").open("r+") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX)
                    waiting = self.run_keyword("p", "local", variables=first["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual(self.server.paths[suggestions], count)

                # Advance the shared and session deadlines without waiting in CI.
                window = json.loads(request_window.read_text())
                variables = waiting["variables"]
                session = json.loads(variables["pinion_post_page"])
                self.assertEqual(session["suggestion_retry_at"], window["retry_after"])
                window.update(retry_after=OLDER, available_at=OLDER)
                request_window.write_text(json.dumps(window))
                session["suggestion_retry_at"] = OLDER
                variables["pinion_post_page"] = json.dumps(session)
                del self.server.status[endpoint]

                ready = self.run_keyword("p", "local", variables=variables)
                self.assertIn("from-pinboard", [item["title"] for item in ready["items"]])
                self.assertNotIn("rerun", ready)
                reused = self.run_keyword("p", "local", variables=ready["variables"])
                self.assertIn("from-pinboard", [item["title"] for item in reused["items"]])
                self.assertEqual(self.server.paths[suggestions], count + 1)

    def test_post_filter_retries_rate_limited_suggestions_with_a_missing_or_expired_deadline(self):
        endpoint = "/v1/posts/suggest"
        page = "https://example.test/browser-page"
        self.server.pins = [bookmark(page, "Browser page", "local", OLDER)]
        self.server.popular_tags[page] = ["from-pinboard"]
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{page} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
        )
        browser.chmod(0o700)
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.configure()
        self.run_keyword("ps", "local")
        self.wait_for_sync()

        for retry_after in (None, "0", "Wed, 01 Jan 2020 00:00:00 GMT"):
            with self.subTest(retry_after=retry_after):
                self.wait_for_request_slot()
                self.server.status[endpoint] = 429
                if retry_after is not None:
                    self.server.retry_after[endpoint] = retry_after
                count = self.server.paths[endpoint]
                first = self.run_keyword("p", "local")
                self.assertEqual(first["rerun"], 5)
                self.assertEqual(self.server.paths[endpoint], count + 1)
                self.assertNotIn("from-pinboard", [row["title"] for row in first["items"]])

                del self.server.status[endpoint]
                self.server.retry_after.pop(endpoint, None)
                waiting = self.run_keyword("p", "local", variables=first["variables"])
                self.assertEqual(self.server.paths[endpoint], count + 1)
                self.assertIn("rerun", waiting)
                variables = waiting["variables"]
                session = json.loads(variables["pinion_post_page"])
                session["suggestion_retry_at"] = OLDER
                variables["pinion_post_page"] = json.dumps(session)
                self.wait_for_request_slot()

                ready = self.run_keyword("p", "local", variables=variables)
                self.assertIn("from-pinboard", [row["title"] for row in ready["items"]])
                self.assertNotIn("rerun", ready)
                self.assertEqual(self.server.paths[endpoint], count + 2)

    def test_open_post_filter_recovers_from_temporary_suggestion_failures(self):
        page = "https://example.test/browser-page"
        endpoint = "/v1/posts/suggest"
        self.server.pins = [bookmark(page, "Browser page", "local", OLDER)]
        self.server.popular_tags[page] = ["recovered-suggestion"]
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.configure()
        self.show_browser_page(page, "Browser page")
        self.run_keyword("ps", "local")
        self.wait_for_sync()

        for status in (503, 502, "truncated body"):
            with self.subTest(status=status):
                self.wait_for_request_slot()
                if status == "truncated body":
                    self.server.truncate_suggestions = True
                else:
                    self.server.status[endpoint] = status
                count = self.server.paths[endpoint]
                failed = self.run_keyword("p", "local")
                self.assertEqual(failed["rerun"], 5)
                self.assertIn("local", [row["title"] for row in failed["items"]])
                self.assertNotIn("recovered-suggestion", [row["title"] for row in failed["items"]])
                self.assertEqual(self.server.paths[endpoint], count + 1)

                self.server.status.pop(endpoint, None)
                self.server.truncate_suggestions = False
                waiting = self.run_keyword("p", "local", variables=failed["variables"])
                self.assertEqual(self.server.paths[endpoint], count + 1)
                self.assertGreater(waiting.get("rerun", 0), 0)
                variables = waiting["variables"]
                session = json.loads(variables["pinion_post_page"])
                session["suggestion_retry_at"] = OLDER
                variables["pinion_post_page"] = json.dumps(session)
                self.wait_for_request_slot()

                recovered = self.run_keyword("p", "local", variables=variables)
                self.assertNotIn("rerun", recovered)
                item = next(row for row in recovered["items"] if row["title"] == "recovered-suggestion")
                posted = self.run_object(*self.select_result("p", recovered, item))
                self.assertEqual(self.server.paths[endpoint], count + 2)
                self.assertIn("Successfully posted", posted.stdout)
                self.assertEqual(self.server.saves[-1]["url"], page)
                self.assertEqual(self.server.saves[-1]["tags"], "recovered-suggestion")

    def test_open_post_filter_recovers_when_rate_limit_headers_arrive_after_the_response_budget(self):
        endpoint = "/v1/posts/suggest"
        page = "https://example.test/browser-page"
        self.server.pins = [bookmark(page, "Browser page", "local", OLDER)]
        self.server.popular_tags[page] = ["from-pinboard"]
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{page} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
        )
        browser.chmod(0o700)
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.configure()
        self.run_keyword("ps", "local")
        self.wait_for_sync()
        self.wait_for_request_slot()

        self.server.status[endpoint] = 429
        self.server.retry_after[endpoint] = "30"
        self.server.block_suggestions_for = page
        try:
            first = self.run_keyword("p", "local", timeout=3)
            self.assertTrue(self.server.suggestion_started.is_set())
            self.assertIn("local", [row["title"] for row in first["items"]])
            self.assertNotIn("from-pinboard", [row["title"] for row in first["items"]])
            self.assertEqual(first["rerun"], 1)
            waiting = self.run_keyword("p", "local", variables=first["variables"])
            self.assertEqual(waiting["rerun"], 1)
            self.assertEqual(self.server.paths[endpoint], 1)
        finally:
            self.server.resume_suggestion.set()
        variables = waiting["variables"]
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            result = self.run_keyword("p", "local", variables=variables)
            variables = result["variables"]
            session = json.loads(variables["pinion_post_page"])
            if session.get("suggestion_retry_at"):
                break
            time.sleep(0.02)
        else:
            self.fail("The open p filter did not receive the late Retry-After")
        self.assertEqual(result["rerun"], 5)
        self.assertEqual(self.server.paths[endpoint], 1)
        window_file = self.account_cache / "pinboard-requests.json"
        window = json.loads(window_file.read_text())
        self.assertEqual(session["suggestion_retry_at"], window["retry_after"])
        window.update(retry_after=OLDER, available_at=OLDER)
        window_file.write_text(json.dumps(window))
        session["suggestion_retry_at"] = OLDER
        variables["pinion_post_page"] = json.dumps(session)
        del self.server.status[endpoint]

        browser.write_text("#!/bin/sh\nexit 1\n")
        ready = self.run_keyword("p", "local", variables=variables)
        row = next(row for row in ready["items"] if row["title"] == "from-pinboard")
        self.assertNotIn("rerun", ready)
        self.assertEqual(self.server.paths[endpoint], 2)

        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{page} fd850fc2e63511e79f720023dfdf24ec Browser page\"}}'\n"
        )
        saved = self.run_object(*self.select_result("p", ready, row))
        self.assertIn("Successfully posted", saved.stdout)
        self.assertEqual(self.server.saves[-1]["url"], page)
        self.assertEqual(self.server.saves[-1]["tags"], "from-pinboard")

    def wait_for_request_slot(self):
        window = json.loads((self.account_cache / "pinboard-requests.json").read_text())
        available_at = datetime.fromisoformat(window["available_at"])
        remaining = (available_at - datetime.now(timezone.utc)).total_seconds()
        if remaining > 0:
            time.sleep(remaining)

    def search(self, *, timeout=10):
        cold = not (self.account_cache / "bookmarks.cache").exists()
        result = self.run_keyword("ps", "protect", timeout=timeout)
        if cold and result.get("rerun"):
            self.wait_for_sync()
            result = self.run_keyword("ps", "protect", timeout=timeout)
        return [item["title"] for item in result["items"]]

    def age_sync_state(self, *, minutes=None, age_download=True):
        state = json.loads(self.sync_file.read_text())
        state["last_check"] = (OLDER if minutes is None else
                               (datetime.now(timezone.utc) - timedelta(minutes=minutes)).isoformat())
        if age_download:
            aged = (datetime.now(timezone.utc) - timedelta(minutes=6)).isoformat()
            state["last_download"] = aged
            state["last_download_attempt"] = aged
        self.sync_file.write_text(json.dumps(state))

    def wait_for_sync(self, sync_file=None):
        sync_file = sync_file or self.sync_file
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            state = json.loads(sync_file.read_text())
            if not state.get("pending", False):
                return state
            time.sleep(0.02)
        self.fail("Bookmark sync worker did not finish")

    def wait_for_update_check(self):
        deadline = time.monotonic() + 5
        with (self.cache / "workflow-update.lock").open("r+") as lock:
            while time.monotonic() < deadline:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return
                except BlockingIOError:
                    time.sleep(0.02)
        self.fail("GitHub update worker did not finish")

    def test_cached_search_stays_responsive_and_imports_remote_bookmarks(self):
        fuzzy = bookmark("https://example.test/p-r-o-t-e-c-t", "Fuzzy URL", "", NEWER)
        exact = bookmark("https://example.test/dashboard", "Exact tag", "protect", OLDER)
        self.server.pins = [fuzzy]
        self.configure()
        self.assertEqual(self.search(), ["Fuzzy URL"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)
        self.assertEqual(self.server.paths["/v1/tags/get"], 1)
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)

        self.server.pins = [fuzzy, exact]
        self.server.timestamp = NEWER
        self.assertEqual(self.search(), ["Fuzzy URL"])
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)
        self.age_sync_state()
        self.server.block_update = True
        try:
            cached = self.run_keyword("ps", "protect", timeout=3)
            self.assertEqual([item["title"] for item in cached["items"]], ["Fuzzy URL"])
            self.assertGreater(cached.get("rerun", 0), 0)
            self.assertTrue(self.server.update_started.wait(3))
            self.run_command(["./pinion", "config", "--bookmark-numbers", "7"])
        finally:
            self.server.resume_update.set()
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Exact tag", "Fuzzy URL"])
        settings = json.loads((self.data / "settings.json").read_text())["settings"]
        self.assertEqual(settings["pins_to_show"], 7)

        self.server.block_update = False
        self.age_sync_state()
        self.assertEqual(self.search(), ["Exact tag", "Fuzzy URL"])
        self.wait_for_sync()
        self.assertEqual(self.server.paths["/v1/posts/all"], 2)

    def test_rate_limit_pauses_automatic_sync(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.status = {"/v1/posts/update": 429}
        self.age_sync_state()
        self.assertEqual(self.search(), ["Dashboard"])
        self.wait_for_sync()
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=2)
        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        self.server.status = {}
        self.age_sync_state(minutes=6)
        self.assertEqual(self.search(), ["Dashboard"])
        self.wait_for_sync()
        self.assertEqual(self.server.paths["/v1/posts/update"], checks + 1)

    def test_account_switch_isolates_existing_cache_and_inflight_worker(self):
        old = bookmark("https://example.test/old", "First account bookmark", "protect", OLDER)
        new = bookmark("https://example.test/new", "New first account bookmark", "protect", NEWER)
        second = bookmark("https://example.test/second", "Second account bookmark", "protect", NEWER)
        self.server.pins = [old]
        self.server.second_pins = [second]
        self.configure()
        self.assertEqual(self.search(), [old["description"]])
        for malformed in ("second:", ":fixture"):
            rejected = self.run_command(["./pinion", "config", "--authorization", malformed],
                                        check=False)
            self.assertNotEqual(rejected.returncode, 0)
        self.assertEqual(self.search(), [old["description"]])

        # Older Pinion caches have no account identifier. Never adopt them for a new account.
        shutil.copyfile(self.account_cache / "bookmarks.cache", self.cache / "bookmarks.cache")
        self.server.pins.append(new)
        self.server.timestamp = NEWER
        self.server.block_update = True
        self.age_sync_state()
        try:
            self.assertEqual(self.search(), [old["description"]])
            self.assertTrue(self.server.update_started.wait(3))
            self.configure(SECOND_TOKEN)
            self.server.second_status = {"/v1/posts/all": 503}
            result = self.run_keyword("ps", "protect", check=False, timeout=3)
            self.assertGreater(result.get("rerun", 0), 0)
            second_cache = self.cache / "pinboard-accounts" / SECOND_TOKEN.split(":", 1)[0].encode().hex()
            self.wait_for_sync(second_cache / "bookmark-sync.json")
            result = self.run_keyword("ps", "protect", check=False, timeout=3)
            self.assertEqual([item["title"] for item in result["items"]], ["Error"])
            self.server.second_status.clear()
            self.assertIn("Updated cache files", self.run_command(["./pinion", "update"]).stdout)
            self.assertEqual(self.search(), [second["description"]])
        finally:
            self.server.resume_update.set()

        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        result = self.run_keyword("ps", "protect")
        self.assertEqual(result["variables"]["username"], "second")
        self.assertEqual([item["title"] for item in result["items"]], [second["description"]])
        self.configure()
        self.assertEqual(self.search(), [new["description"], old["description"]])

    def test_slow_bookmark_download_completes_in_background(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER))
        self.server.timestamp = NEWER
        self.server.download_delay = 5.5
        self.age_sync_state()

        cached = self.run_keyword("ps", "protect", timeout=3)
        self.assertEqual([item["title"] for item in cached["items"]], ["Dashboard"])
        self.assertGreater(cached.get("rerun", 0), 0)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark", "Dashboard"])

    def test_failed_tag_download_respects_bookmark_download_cooldown(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.timestamp = NEWER
        self.server.status = {"/v1/tags/get": 503}
        self.age_sync_state()

        self.search()
        failed = self.wait_for_sync()
        self.assertEqual(failed["remote_update"], OLDER)
        self.assertEqual(self.server.paths["/v1/posts/all"], 2)
        self.age_sync_state(minutes=2, age_download=False)
        self.server.status.clear()
        self.assertEqual(self.search(), ["Dashboard"])
        self.wait_for_sync()
        self.assertEqual(self.server.paths["/v1/posts/all"], 2)

        self.age_sync_state()
        self.search()
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.server.paths["/v1/posts/all"], 3)

    def test_failed_initial_download_respects_cooldown_and_pu_can_retry(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.server.status = {"/v1/tags/get": 503}
        self.configure()

        self.assertGreater(self.run_keyword("ps", "protect").get("rerun", 0), 0)
        self.wait_for_sync()
        for _ in range(2):
            result = self.run_keyword("ps", "protect", check=False)
            self.assertEqual([item["title"] for item in result["items"]], ["Error"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)
        self.server.status.clear()
        self.assertIn("Updated cache files", self.run_command(["./pinion", "update"]).stdout)
        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 2)

    def test_post_and_tag_rename_work_after_a_failed_cold_cache_download(self):
        self.assertEqual(self.objects[self.uid("p")]["argumenttype"], 1,
                         "Alfred must run the filter before the user enters a tag")
        self.configure()
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.server.block_tags = True
        self.server.status["/v1/tags/get"] = 503
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{url} fd850fc2e63511e79f720023dfdf24ec Dashboard\"}}'\n"
        )
        browser.chmod(0o700)
        self.env["suggest_tags"] = "1"
        self.server.popular_tags[url] = ["popular-test-tag"]

        try:
            self.assertGreater(self.run_keyword("ps", "protect").get("rerun", 0), 0)
            self.assertTrue(self.server.tags_started.wait(3))
            typed = self.run_keyword("p", "protect")
            self.assertTrue(any(item.get("arg") == "protect" and item.get("valid", True)
                                for item in typed["items"]))
            choices = self.run_keyword("p", "; save while tags are stalled")
            save = next(item for item in choices["items"] if item.get("arg") == "; save while tags are stalled")
            post = self.run_object(*self.select_result("p", choices, save))
            self.assertIn("Successfully posted", post.stdout)
            self.assertEqual(self.server.saves[0]["extended"], "save while tags are stalled")
        finally:
            self.server.resume_tags.set()
        self.wait_for_sync()
        self.assertFalse((self.account_cache / "bookmarks.cache").exists())
        self.server.status.clear()

        suggestions = self.run_keyword("p")
        deadline = time.monotonic() + 5
        while not any(item["title"] == "popular-test-tag" for item in suggestions["items"]):
            self.assertGreater(suggestions.get("rerun", 0), 0)
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.10)
            suggestions = self.run_keyword("p", variables=suggestions["variables"])
        save = next(item for item in suggestions["items"] if item["title"] == "popular-test-tag")
        post = self.run_object(*self.select_result("p", suggestions, save))
        self.assertIn("Successfully posted", post.stdout)
        self.assertEqual(self.server.saves[1]["url"], url)
        self.assertEqual(self.server.saves[1]["tags"], "popular-test-tag")

        tags = self.run_keyword("pr", "protect")
        original = next(item for item in tags["items"] if item.get("arg") == "protect")
        start = self.select_result("pr", tags, original)
        opened = self.run_object(*start)
        editor, _, context = self.follow_workflow_output(start[0], opened.stdout, start[2])
        choices = self.run_filter(editor, "renamed", variables=context)
        new_tag = next(item for item in choices["items"] if item.get("arg") == "renamed")
        renamed = self.run_object(*self.select_result(editor, choices, new_tag))
        self.assertEqual(json.loads(renamed.stdout)["alfredworkflow"]["arg"],
                         "Successfully renamed tag.")
        self.assertEqual(self.server.paths["/v1/posts/add"], 2)
        self.assertEqual(self.server.paths["/v1/tags/rename"], 1)
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

    def test_cold_auth_failure_keeps_the_open_search_eligible_for_github_retry(self):
        self.configure()
        self.updater_file.unlink()
        self.server.status.update({"/v1/posts/update": 401, RELEASE_PATH: 503})

        self.server.block_release = True
        try:
            loading = self.run_keyword("ps", "protect")
            self.assertTrue(self.server.release_started.wait(3))
            self.assertTrue(self.wait_for_sync()["authentication_failed"])
        finally:
            self.server.resume_release.set()
        self.wait_for_update_check()
        self.assertEqual(self.server.paths[RELEASE_PATH], 1)

        waiting = self.run_keyword("ps", "protect", variables=loading["variables"])
        self.assertTrue(any("`pa`" in item.get("subtitle", "") for item in waiting["items"]))
        self.assertGreater(waiting.get("rerun", 0), 0)
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)
        self.assertEqual(self.server.paths[RELEASE_PATH], 1)

        self.server.status.pop(RELEASE_PATH)
        self.server.release_tag = "v1.1.0"
        (self.cache / "workflow-update-attempt.json").write_text(json.dumps(OLDER))
        self.server.release_started.clear()
        self.server.resume_release.clear()
        try:
            retried = self.run_keyword("ps", "protect", variables=waiting["variables"])
            self.assertGreater(retried.get("rerun", 0), 0)
            self.assertTrue(self.server.release_started.wait(3))
        finally:
            self.server.resume_release.set()
        self.wait_for_update_check()
        ready = self.run_keyword("ps", "protect", variables=retried["variables"])
        self.assertEqual(self.server.paths[RELEASE_PATH], 2)
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)
        self.assertNotIn("rerun", ready)
        offer = next(item for item in ready["items"] if item.get("arg") == "1.1.0")
        self.assertEqual(self.select_result("ps", ready, offer)[0],
                         self.follow_connection("pcheck", offer["arg"])[0])

    def test_search_still_offers_updates_before_pinboard_is_configured(self):
        self.check_search_offers_updates_without_pinboard()

    def test_search_still_offers_updates_with_an_unusable_pinboard_cache(self):
        self.configure()
        (self.account_cache / "bookmark-sync.lock").mkdir(parents=True)
        self.check_search_offers_updates_without_pinboard()

    def check_search_offers_updates_without_pinboard(self):
        self.updater_file.unlink()
        self.server.release_tag = "v1.1.0"
        self.server.block_release = True
        try:
            waiting = self.run_keyword("ps", "protect")
            self.assertGreater(waiting.get("rerun", 0), 0)
            self.assertTrue(self.server.release_started.wait(3))
        finally:
            self.server.resume_release.set()
        self.wait_for_update_check()

        ready = self.run_keyword("ps", "protect", variables=waiting["variables"])
        self.assertEqual(ready["items"][0]["title"], "Error")
        self.assertFalse(ready["items"][0]["valid"])
        offer = next(item for item in ready["items"] if item.get("arg") == "1.1.0")
        self.assertEqual(self.select_result("ps", ready, offer)[0],
                         self.follow_connection("pcheck", offer["arg"])[0])
        self.assertNotIn("/v1/posts/update", self.server.paths)

    def test_pu_rebuilds_cold_cache_when_tags_take_more_than_three_seconds(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.server.block_tags = True

        def release_tags_after_deadline():
            if self.server.tags_started.wait(4):
                self.server.resume_tags.wait(3.25)
                self.server.resume_tags.set()

        timer = Thread(target=release_tags_after_deadline)
        timer.start()
        try:
            update = next(uid for uid, obj in self.objects.items()
                          if "ret=$(./pinion update)" in obj.get("script", ""))
            result = self.run_object(update, "")
            self.assertIn("Updated cache files", result.stdout)
        finally:
            self.server.resume_tags.set()
            timer.join()
        self.assertTrue(self.server.tags_started.is_set())
        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)
        self.assertEqual(self.server.paths["/v1/tags/get"], 1)

    def test_open_first_search_recovers_after_the_download_cooldown(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.server.status["/v1/tags/get"] = 503
        self.assertGreater(self.run_keyword("ps", "protect")["rerun"], 0)
        self.wait_for_sync()
        waiting = self.run_keyword("ps", "protect")
        self.assertEqual(waiting["rerun"], 5)
        self.assertFalse(waiting["items"][0]["valid"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

        self.server.status.clear()
        self.age_sync_state()
        self.assertGreater(self.run_keyword("ps", "protect", variables=waiting["variables"])["rerun"], 0)
        self.wait_for_sync()
        self.assertEqual(self.search(), ["Dashboard"])

    def test_stalled_background_tags_do_not_block_post_until_the_download_timeout(self):
        self.configure()
        self.server.ordinary_request_interval = 0.06
        self.server.pins = [
            bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        ]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins = [
            bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER)
        ]
        self.server.timestamp = NEWER
        self.age_sync_state()
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' '{\"https://example.test/post fd850fc2e63511e79f720023dfdf24ec Page\"}'\n"
        )
        browser.chmod(0o700)

        self.server.block_tags = True
        self.server.status["/v1/tags/get"] = 503
        try:
            self.run_keyword("ps", "protect")
            self.assertTrue(self.server.tags_started.wait(3))
            post = self.run_command(["./pinion", "post", "--tags", "protect"], timeout=2)
            self.assertIn("Successfully posted:", post.stdout)
            self.assertEqual(self.server.saves[0]["url"], "https://example.test/post")
            self.assertFalse(self.server.throttled_requests)
        finally:
            self.server.resume_tags.set()
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        self.assertEqual(self.search(), ["Dashboard"])

    def test_queued_post_respects_retry_after_from_background_tags(self):
        self.configure()
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.wait_for_request_slot()
        source = tempfile.TemporaryDirectory(prefix="pinion-production-pacing-")
        self.addCleanup(source.cleanup)
        binary = build_fixture(Path(source.name), self.server.address, fast_pacing=False)
        command = self.workflow / "pinion"
        command.unlink()
        command.symlink_to(binary)
        self.age_sync_state()

        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{url} fd850fc2e63511e79f720023dfdf24ec Dashboard\"}}'\n"
        )
        browser.chmod(0o700)
        self.server.timestamp = NEWER
        self.server.block_tags = True
        self.server.status["/v1/tags/get"] = 429
        self.server.retry_after["/v1/tags/get"] = "60"
        mutation = self.account_cache / "pinboard-mutations.lock"
        mutation.touch()

        try:
            self.run_keyword("ps", "protect")
            self.assertTrue(self.server.tags_started.wait(8))
            with subprocess.Popen(
                ["./pinion", "post", "-t", "protect"], cwd=self.workflow, env=self.env,
                stdout=subprocess.PIPE, text=True,
            ) as post:
                # Wait for the post to start its mutation within the tag request's slot.
                deadline = time.monotonic() + 2
                with mutation.open("r+") as lock:
                    while time.monotonic() < deadline:
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            fcntl.flock(lock, fcntl.LOCK_UN)
                        except BlockingIOError:
                            break
                        time.sleep(0.01)
                    else:
                        self.fail("The post did not start within the request slot")
                self.assertFalse(self.server.bookmark_added.wait(0.1))
                self.server.resume_tags.set()
                self.assertTrue(self.wait_for_sync()["deferred_update"])
                self.assertIn("Error:", post.communicate(timeout=6)[0])
        finally:
            self.server.resume_tags.set()
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

    def test_successful_post_leaves_a_refresh_pending_while_download_is_busy(self):
        self.configure()
        browser = self.workflow / "osascript"
        new_url = "https://example.test/new"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{new_url} fd850fc2e63511e79f720023dfdf24ec New bookmark\"}}'\n"
        )
        browser.chmod(0o700)
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.timestamp = NEWER
        self.server.block_download = True
        self.age_sync_state()
        try:
            self.run_keyword("ps", "protect")
            self.assertTrue(self.server.download_started.wait(3))
            post = self.run_command(["./pinion", "post", "-t", "protect"], timeout=2)
            self.assertIn("Successfully posted:", post.stdout)
            self.assertEqual([saved["url"] for saved in self.server.saves], [new_url])
        finally:
            self.server.resume_download.set()
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)

        self.server.pins.append(bookmark(new_url, "New bookmark", "protect", NEWER))
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 5)
        self.assertEqual(self.server.paths["/v1/posts/all"], 2)
        self.age_sync_state()
        self.run_keyword("ps", "protect")
        self.wait_for_sync()
        self.assertEqual(self.search(), ["New bookmark", "Dashboard"])

    def test_first_cache_download_survives_cancelled_filter(self):
        self.assert_download_survives_cancelled_filter(self.keyword_script("ps", "protect"))

    def test_first_cache_download_survives_cancelled_tag_rename(self):
        self.assert_download_survives_cancelled_filter(self.keyword_script("pr", "protect"),
                                                       accepts_typed_tag=True)

    def test_first_cache_download_survives_cancelled_tag_rename_edit(self):
        edit = self.objects["9CB57748-F1F2-4665-A7E0-55B546C50C77"]["script"]
        self.assert_download_survives_cancelled_filter(edit, accepts_typed_tag=True)

    def test_cancelled_delete_preview_leaves_first_download_available(self):
        self.configure()
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        browser = self.workflow / "osascript"
        browser.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' '{{\"{url} fd850fc2e63511e79f720023dfdf24ec Dashboard\"}}'\n"
        )
        browser.chmod(0o700)

        self.server.block_download = True
        script = self.keyword_script("pind", "") + "\nprintf '\\n'\nread -r hold"
        process = subprocess.Popen(["bash", "-c", script], cwd=self.workflow, env=self.env,
                                   start_new_session=True, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready, _, _ = select.select([process.stdout], [], [], 3)
            self.assertTrue(ready, "The delete preview waited for a full download")
            preview = json.loads(process.stdout.readline())
            self.assertEqual(preview["items"][0]["arg"], url)
            self.assertFalse(self.server.download_started.is_set())
            self.assertFalse(self.sync_file.exists())
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
            process.communicate(timeout=3)
            self.server.resume_download.set()

        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

    def assert_download_survives_cancelled_filter(self, script, *, accepts_typed_tag=False):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.server.block_download = True
        # Keep the filter's process group alive after it emits JSON, so a
        # cancellation can kill it while the network request is still pending.
        script += "\nprintf '\\n'\nread -r hold"
        process = subprocess.Popen(["bash", "-c", script, "alfred-filter", "protect"], cwd=self.workflow,
                                   env=self.env, start_new_session=True, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            self.assertTrue(self.server.download_started.wait(3))
            ready, _, _ = select.select([process.stdout], [], [], 3)
            self.assertTrue(ready, "The initial filter withheld its loading result")
            result = json.loads(process.stdout.readline())
            self.assertGreater(result.get("rerun", 0), 0)
            loading = next(item for item in result["items"]
                           if item["title"] == "Downloading Pinboard bookmarks…")
            self.assertFalse(loading["valid"])
            if accepts_typed_tag:
                self.assertTrue(any(item.get("arg") == "protect" and item.get("valid", True)
                                    for item in result["items"]))
            os.killpg(process.pid, signal.SIGTERM)
            process.communicate(timeout=3)
            self.assertEqual(process.returncode, -signal.SIGTERM)
        finally:
            self.server.resume_download.set()
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate(timeout=3)
        self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)
        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

    def test_cold_and_manual_refresh_honor_live_rate_limit(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.server.status = {"/v1/posts/update": 429}

        self.assertGreater(self.run_keyword("ps", "protect").get("rerun", 0), 0)
        cold = self.wait_for_sync()
        self.assertGreater(cold["throttle_minutes"], 0)
        self.assertEqual(self.server.paths["/v1/posts/all"], 0)
        result = self.run_keyword("ps", "protect", check=False)
        self.assertEqual([item["title"] for item in result["items"]], ["Error"])
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)

        before_retry = self.run_command(["./pinion", "update"], check=False)
        self.assertIn("Error:", before_retry.stdout)
        self.assertEqual(self.server.paths["/v1/posts/update"], 1)

        self.age_sync_state(minutes=6, age_download=False)
        manual = self.run_command(["./pinion", "update"], check=False)
        self.assertIn("Error:", manual.stdout)
        self.assertEqual(self.server.paths["/v1/posts/update"], 2)
        self.assertEqual(self.server.paths["/v1/posts/all"], 0)
        self.assertGreater(self.wait_for_sync()["throttle_minutes"], 0)

        self.server.status.clear()
        self.age_sync_state(minutes=61, age_download=False)
        self.assertIn("Updated cache files", self.run_command(["./pinion", "update"]).stdout)
        self.assertEqual(self.search(), ["Dashboard"])
        self.assertEqual(self.wait_for_sync()["throttle_minutes"], 0)

    def test_direct_search_initializes_cache_before_printing_results(self):
        self.configure()
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        result = self.run_command(["./pinion", "search", "protect"])
        self.assertEqual([item["title"] for item in json.loads(result.stdout)["items"]],
                         ["Dashboard"])
        urls = self.run_command(["./pinion", "search", "-U", "protect"])
        self.assertEqual(urls.stdout.splitlines(), [url])
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

    def test_open_search_retries_after_sync_worker_exits_during_setup(self):
        self.configure()
        self.server.pins = [
            bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        ]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote", "protect", NEWER))
        self.server.timestamp = NEWER
        self.age_sync_state()

        security = self.workflow / "security"
        original = security.read_text()
        security.write_text('''#!/bin/sh
case "$(/bin/ps -p "$PPID" -o command=)" in
    *sync-bookmarks*) touch "$PINION_FIXTURE_KEYCHAIN.failed"; exit 1 ;;
esac
''' + original)
        try:
            checks = self.server.paths["/v1/posts/update"]
            started = self.run_keyword("ps", "protect")
            self.assertEqual(started["rerun"], 1)
            marker = self.workflow / "keychain-token.failed"
            deadline = time.monotonic() + 3
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(marker.exists(), "The sync worker did not hit the Keychain failure")
        finally:
            security.write_text(original)

        now = datetime.now(timezone.utc)
        state = json.loads(self.sync_file.read_text())
        self.assertTrue(state["pending"])
        state["pending_since"] = (now - timedelta(seconds=35)).isoformat()
        self.sync_file.write_text(json.dumps(state))
        waiting = self.run_keyword("ps", "protect", variables=started["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)

        state["pending_since"] = (now - timedelta(seconds=61)).isoformat()
        self.sync_file.write_text(json.dumps(state))
        retried = self.run_keyword("ps", "protect", variables=waiting["variables"])
        self.assertEqual(retried["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        refreshed = self.run_keyword("ps", "protect", variables=retried["variables"])
        self.assertEqual([item["title"] for item in refreshed["items"]], ["Remote", "Dashboard"])
        self.assertNotIn("rerun", refreshed)

    def test_open_search_retries_a_failed_timestamp_check(self):
        original = bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        new = bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER)
        self.server.pins = [original]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(new)
        self.server.timestamp = NEWER
        self.age_sync_state()
        variables = {}
        for status, block_minutes, wait_minutes in ((503, 0, 2), (429, 2, 6)):
            with self.subTest(status=status):
                self.server.status["/v1/posts/update"] = status
                started = self.run_keyword("ps", "protect", variables=variables)
                self.assertEqual(started["rerun"], 1)
                self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)
                checks = self.server.paths["/v1/posts/update"]
                self.age_sync_state(minutes=block_minutes, age_download=False)
                waiting = self.run_keyword("ps", "protect", variables=started["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
                self.assertEqual(self.server.paths["/v1/posts/update"], checks)
                self.assertEqual(self.server.paths["/v1/posts/all"], 1)
                self.age_sync_state(minutes=wait_minutes, age_download=False)
                variables = waiting["variables"]

        self.server.status.clear()
        started = self.run_keyword("ps", "protect", variables=variables)
        self.assertEqual(started["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        refreshed = self.run_keyword("ps", "protect", variables=started["variables"])
        self.assertNotIn("rerun", refreshed)
        self.assertEqual([item["title"] for item in refreshed["items"]],
                         ["Remote bookmark", "Dashboard"])

        self.age_sync_state()
        self.server.status["/v1/posts/update"] = 401
        self.run_keyword("ps", "protect")
        self.wait_for_sync()
        denied = self.run_keyword("ps", "protect")
        self.assertNotIn("rerun", denied)

    def test_deferred_download_pauses_on_auth_failure_and_resumes_after_pa(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote", "protect", NEWER))
        self.server.timestamp = NEWER

        self.age_sync_state(age_download=False)
        started = self.run_keyword("ps", "protect")
        self.assertEqual(self.wait_for_sync()["deferred_update"], NEWER)
        self.age_sync_state()
        self.server.status["/v1/posts/all"] = 401
        waiting = self.run_keyword("ps", "protect", variables=started["variables"])
        self.assertTrue(self.wait_for_sync()["authentication_failed"])

        self.age_sync_state(minutes=6, age_download=False)
        calls = self.server.paths.copy()
        denied = self.run_keyword("ps", "protect", variables=waiting["variables"])
        self.assertNotIn("rerun", denied)
        self.assertEqual([item["title"] for item in denied["items"]],
                         ["Dashboard", "Pinboard sync paused"])
        self.assertFalse(denied["items"][-1]["valid"])
        self.assertIn("`pa`", denied["items"][-1]["subtitle"])
        self.assertEqual(self.server.paths, calls)

        self.server.status.clear()
        self.configure()
        restarted = self.run_keyword("ps", "protect")
        self.assertGreater(restarted.get("rerun", 0), 0)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote", "Dashboard"])

    def test_pa_confirms_before_a_healthy_refresh_finishes(self):
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.configure()
        self.server.block_tags = True
        try:
            self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
            self.assertTrue(self.server.tags_started.wait(4))
            pa, token, variables = self.follow_connection("pa", ROTATED_TOKEN)
            result = self.run_command(["bash", "-c", self.objects[pa]["script"], pa, token],
                                      env={**self.env, **variables}, timeout=2)
            self.assertEqual(result.stdout.strip(), "Successfully set user credentials.")
            self.assertEqual((self.workflow / "keychain-token").read_text().strip(), ROTATED_TOKEN)
            self.assertTrue(json.loads(self.sync_file.read_text())["pending"])
            self.assertTrue(any(
                "Token saved to your Keychain" in self.objects[edge["destinationuid"]].get("text", "")
                for edge in self.connections[self.connections[pa][0]["destinationuid"]]
            ))
        finally:
            self.server.resume_tags.set()
        self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)

        update, arg, variables = self.follow_connection(pa, result.stdout.strip(), variables)
        result = self.run_object(update, arg, variables)
        self.assertIn("Updated cache files", result.stdout)
        self.assertEqual(self.search(), ["Dashboard"])

        # Saving an invalid replacement must not let a fresh 401 bypass the pause.
        pa, token, variables = self.follow_connection("pa", "test:invalid-fixture")
        result = self.run_object(pa, token, variables)
        self.assertEqual(result.stdout.strip(), "Successfully set user credentials.")
        update, arg, variables = self.follow_connection(pa, result.stdout.strip(), variables)
        checks = self.server.paths["/v1/posts/update"]
        self.assertIn("Pinboard denied the request", self.run_object(update, arg, variables).stdout)
        denied = self.run_keyword("ps", "protect")
        self.assertEqual([item["title"] for item in denied["items"]],
                         ["Dashboard", "Pinboard sync paused"])
        self.assertNotIn("rerun", denied)
        self.assertEqual(self.server.paths["/v1/posts/update"], checks + 1)

    def test_pa_clears_an_auth_failure_published_after_the_token_was_saved(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote", "protect", NEWER))
        self.server.timestamp = NEWER
        self.age_sync_state(age_download=False)
        self.run_keyword("ps", "protect")
        self.assertEqual(self.wait_for_sync()["deferred_update"], NEWER)
        self.age_sync_state()

        # Let the worker read the old token during setup. Hold the second
        # Keychain lookup after it has captured the same token, so even the
        # stale-token comparison cannot see the replacement.
        security = self.workflow / "security"
        security.write_text('''#!/bin/sh
if [ "$1" = find-generic-password ]; then
    case "$(/bin/ps -p "$PPID" -o command=)" in
        *sync-bookmarks*)
            if mkdir "$PINION_FIXTURE_KEYCHAIN.worker" 2>/dev/null; then
                exec cat "$PINION_FIXTURE_KEYCHAIN"
            elif [ ! -e "$PINION_FIXTURE_KEYCHAIN.resume" ]; then
                captured=$(cat "$PINION_FIXTURE_KEYCHAIN")
                touch "$PINION_FIXTURE_KEYCHAIN.comparing"
                attempts=0
                while [ ! -e "$PINION_FIXTURE_KEYCHAIN.resume" ] && [ "$attempts" -lt 1000 ]; do
                    sleep 0.01
                    attempts=$((attempts + 1))
                done
                printf '%s\n' "$captured"
                exit 0
            fi ;;
    esac
fi
''' + security.read_text())
        self.server.status["/v1/posts/all"] = 401
        try:
            started = self.run_keyword("ps", "protect")
            comparing = self.workflow / "keychain-token.comparing"
            deadline = time.monotonic() + 3
            while not comparing.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(comparing.exists(), "The failed worker did not reach its Keychain comparison")
            pa, token, variables = self.follow_connection("pa", ROTATED_TOKEN)
            result = self.run_command(["bash", "-c", self.objects[pa]["script"], pa, token],
                                      env={**self.env, **variables}, timeout=2)
            self.assertEqual(result.stdout.strip(), "Successfully set user credentials.")
            del self.server.status["/v1/posts/all"]
            self.server.revoke_original_token = True
        finally:
            (self.workflow / "keychain-token.resume").touch()

        self.wait_for_sync()
        with (self.account_cache / "bookmark-sync.lock").open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
        # A search may rerun before the workflow's forced update starts.
        # The recorded authorization must still prevent a stale pause.
        restarted = self.run_keyword("ps", "protect", variables=started["variables"])
        self.assertNotIn("Pinboard sync paused", [item["title"] for item in restarted["items"]])
        self.assertEqual(restarted.get("rerun"), 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        refreshed = self.run_keyword("ps", "protect", variables=restarted["variables"])
        self.assertEqual([item["title"] for item in refreshed["items"]], ["Remote", "Dashboard"])

    def test_old_sync_worker_cannot_pause_sync_after_pa_rotates_token(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote", "protect", NEWER))
        self.server.timestamp = NEWER
        self.age_sync_state(age_download=False)
        self.run_keyword("ps", "protect")
        self.assertEqual(self.wait_for_sync()["deferred_update"], NEWER)
        self.age_sync_state()

        # Delay the worker after it captures the old token and before it takes
        # the refresh lock. Later Keychain reads must return the current token.
        security = self.workflow / "security"
        original = security.read_text()
        security.write_text('''#!/bin/sh
if [ "$1" = find-generic-password ]; then
    case "$(/bin/ps -p "$PPID" -o command=)" in
        *sync-bookmarks*)
            captured=$(cat "$PINION_FIXTURE_KEYCHAIN")
            if mkdir "$PINION_FIXTURE_KEYCHAIN.captured" 2>/dev/null; then
                attempts=0
                while [ ! -e "$PINION_FIXTURE_KEYCHAIN.resume" ] && [ "$attempts" -lt 1000 ]; do
                    sleep 0.01
                    attempts=$((attempts + 1))
                done
                printf '%s\\n' "$captured"
                exit 0
            fi ;;
    esac
fi
''' + original)
        calls = self.server.paths["/v1/posts/all"]
        try:
            started = self.run_keyword("ps", "protect")
            self.assertEqual(started["rerun"], 1)
            captured = self.workflow / "keychain-token.captured"
            deadline = time.monotonic() + 3
            while not captured.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(captured.exists(), "The worker did not capture the original token")
            self.configure(ROTATED_TOKEN)
            self.server.revoke_original_token = True
        finally:
            (self.workflow / "keychain-token.resume").touch()

        stale_attempt = self.wait_for_sync()
        self.assertEqual(self.server.paths["/v1/posts/all"], calls + 1)
        self.assertFalse(stale_attempt["authentication_failed"])
        self.assertEqual(stale_attempt["deferred_update"], NEWER)

        restarted = self.run_keyword("ps", "protect", variables=started["variables"])
        self.assertEqual(restarted["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        refreshed = self.run_keyword("ps", "protect", variables=restarted["variables"])
        self.assertEqual([item["title"] for item in refreshed["items"]], ["Remote", "Dashboard"])
        self.assertNotIn("rerun", refreshed)
        self.assertEqual(self.server.paths["/v1/posts/all"], calls + 2)

    def test_open_search_reruns_until_deferred_remote_change_is_imported(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER))
        self.server.timestamp = NEWER
        self.age_sync_state(age_download=False)

        started = self.run_keyword("ps", "protect")
        self.assertGreater(started.get("rerun", 0), 0)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("ps", "protect")
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual(self.server.paths["/v1/posts/all"], 1)

        # Advance the last download without aging the short check. A rerun must
        # pick up the already detected change as soon as the cooldown is over.
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=0)
        rerun = self.run_keyword("ps", "protect")
        self.assertGreater(rerun.get("rerun", 0), 0)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        refreshed = self.run_keyword("ps", "protect")
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        self.assertNotIn("rerun", refreshed)
        self.assertEqual([item["title"] for item in refreshed["items"]], ["Remote bookmark", "Dashboard"])

    def test_open_search_retries_a_deferred_download_after_pinboard_recovers(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER))
        self.server.timestamp = NEWER
        self.age_sync_state(age_download=False)
        self.assertGreater(self.run_keyword("ps", "protect").get("rerun", 0), 0)
        self.assertEqual(self.wait_for_sync()["deferred_update"], NEWER)

        self.server.status["/v1/tags/get"] = 503
        self.age_sync_state(minutes=0)
        self.assertGreater(self.run_keyword("ps", "protect").get("rerun", 0), 0)
        failed = self.wait_for_sync()
        self.assertEqual(failed["remote_update"], OLDER)
        self.assertEqual(failed["deferred_update"], NEWER)
        downloads = self.server.paths["/v1/posts/all"]

        self.server.status.clear()
        waiting = self.run_keyword("ps", "protect")
        self.assertGreater(waiting.get("rerun", 0), 0)
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/all"], downloads)
        self.age_sync_state(minutes=0)
        self.assertGreater(self.run_keyword("ps", "protect").get("rerun", 0), 0)
        refreshed = self.wait_for_sync()
        self.assertEqual(refreshed["remote_update"], NEWER)
        self.assertIsNone(refreshed["deferred_update"])
        self.assertEqual(self.search(), ["Remote bookmark", "Dashboard"])

    def test_hourly_reconcile_uses_healthy_download_endpoints(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER))
        self.server.timestamp = NEWER

        def make_reconcile_due():
            self.age_sync_state()
            state = json.loads(self.sync_file.read_text())
            state["last_download"] = OLDER
            self.sync_file.write_text(json.dumps(state))

        for denied in (401, 403, 429):
            with self.subTest(status=denied):
                make_reconcile_due()
                self.server.status["/v1/posts/update"] = denied
                self.run_keyword("ps", "protect")
                blocked = self.wait_for_sync()
                self.assertEqual(blocked["remote_update"], OLDER)
                self.assertEqual(self.server.paths["/v1/posts/all"], 1)
                if denied in (401, 403):
                    self.configure()
        make_reconcile_due()
        self.server.status["/v1/posts/update"] = 520
        self.server.status["/v1/tags/get"] = 503
        self.run_keyword("ps", "protect")
        failed = self.wait_for_sync()
        self.assertEqual(failed["remote_update"], OLDER)
        self.assertTrue(failed["deferred_reconcile"])
        waiting = self.run_keyword("ps", "protect")
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 2)

        self.age_sync_state()
        del self.server.status["/v1/tags/get"]
        self.run_keyword("ps", "protect")
        state = self.wait_for_sync()
        self.assertIsNone(state["remote_update"])
        self.assertFalse(state["deferred_reconcile"])
        self.assertEqual(self.server.paths["/v1/posts/all"], 3)
        self.assertEqual(self.search(), ["Remote bookmark", "Dashboard"])

    def test_open_search_retries_a_failed_manual_download(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote bookmark", "protect", NEWER))
        self.server.timestamp = NEWER
        self.server.status["/v1/tags/get"] = 503
        update = self.run_command(["./pinion", "update"])
        self.assertIn("Error:", update.stdout)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("ps", "protect")
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])

        self.server.status.clear()
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=0)
        self.run_keyword("ps", "protect")
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        self.assertEqual(self.search(), ["Remote bookmark", "Dashboard"])

    def test_open_search_retries_after_a_manual_refresh_timestamp_rate_limit(self):
        endpoint = "/v1/posts/update"
        self.configure()
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/remote", "Remote", "protect", NEWER))
        self.server.timestamp = NEWER
        self.server.status[endpoint] = 429
        update = self.follow_connection("pu", "")
        self.assertIn("Too Many Requests", self.run_object(*update).stdout)

        calls = self.server.paths.copy()
        pending = self.wait_for_sync()
        self.assertGreater(pending["throttle_minutes"], 0)
        self.assertTrue(pending["retry_update_check"])
        waiting = self.run_keyword("ps", "protect")
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
        self.assertEqual(self.server.paths, calls)

        self.server.status.clear()
        self.age_sync_state()
        restarted = self.run_keyword("ps", "protect", variables=waiting["variables"])
        self.assertEqual(restarted["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote", "Dashboard"])

    def test_pinboard_maintenance_deadline_pauses_refresh_post_and_suggestions(self):
        for endpoint in ("/v1/posts/update", "/v1/posts/all", "/v1/posts/suggest"):
            with self.subTest(endpoint=endpoint):
                self.assert_pinboard_maintenance_deadline(endpoint)

    def assert_pinboard_maintenance_deadline(self, endpoint):
        self.configure()
        url = "https://example.test/dashboard"
        self.show_browser_page(url)
        self.env.update(page_is_bookmarked="0", suggest_tags="1")
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.server.popular_tags[url] = ["maintenance-tested"]
        self.server.timestamp = OLDER
        self.assertEqual(self.search(), ["Dashboard"])
        self.wait_for_request_slot()
        self.server.status[endpoint] = 503
        self.server.retry_after[endpoint] = "600"
        update = self.follow_connection("pu", "")
        if endpoint == "/v1/posts/suggest":
            suggestions = self.run_keyword("p", "protect")
        else:
            before_download = self.server.paths["/v1/posts/all"]
            self.assertIn("Service Unavailable", self.run_object(*update).stdout)
            self.assertEqual(self.server.paths["/v1/posts/all"],
                             before_download + int(endpoint == "/v1/posts/all"))
            suggestions = self.run_keyword("p", "protect")

        self.assertEqual(suggestions["rerun"], 5)
        session = json.loads(suggestions["variables"]["pinion_post_page"])
        window_file = self.account_cache / "pinboard-requests.json"
        window = json.loads(window_file.read_text())
        self.assertEqual(session["suggestion_retry_at"], window["service_unavailable_until"])

        self.server.status.pop(endpoint)
        parked = self.server.paths.copy()
        row = next(row for row in suggestions["items"] if row.get("arg") == "protect")
        self.assertIn("Error:", self.run_object(*self.select_result("p", suggestions, row)).stdout)
        self.assertEqual(self.server.paths, parked)
        self.assertIn("Service Unavailable", self.run_object(*update).stdout)
        self.assertEqual(self.server.paths, parked)
        resumed_search = self.run_keyword("ps", "protect")
        self.assertEqual(resumed_search["rerun"], 5)
        self.assertEqual([row["title"] for row in resumed_search["items"]], ["Dashboard"])

        window["service_unavailable_until"] = OLDER
        window["available_at"] = OLDER
        window_file.write_text(json.dumps(window))
        state = self.wait_for_sync()
        state["retry_after"]["deadline"] = OLDER
        self.sync_file.write_text(json.dumps(state))
        variables = suggestions["variables"]
        session["suggestion_retry_at"] = OLDER
        variables["pinion_post_page"] = json.dumps(session)
        self.env["auto_update_cache"] = "0"
        ready = self.run_keyword("p", "protect", variables=variables)
        row = next(row for row in ready["items"] if row["title"] == "maintenance-tested")
        self.assertNotIn("rerun", ready)
        saved = self.run_object(*self.select_result("p", ready, row))
        self.assertIn("Successfully posted", saved.stdout)
        self.assertEqual(self.server.saves[-1]["tags"], "maintenance-tested")
        self.assertIn("Updated cache files", self.run_object(*update).stdout)
        self.env["auto_update_cache"] = "1"

    def test_new_malformed_bookmark_does_not_advance_watermark(self):
        self.configure()
        self.server.pins = [bookmark("https://example.test/old", "Old bookmark", "protect", OLDER)]
        self.assertEqual(self.search(), ["Old bookmark"])
        new_pin = bookmark("https://example.test/good", "New bookmark", "protect", OLDER)
        recovered = bookmark("https://example.test/bad", "Recovered bookmark", "protect", NEWER)
        self.server.pins = [new_pin, {**recovered, "description": False}]
        self.server.timestamp = NEWER
        self.age_sync_state()
        self.run_keyword("ps", "protect")
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        self.assertEqual(self.search(), ["New bookmark"])

        self.server.pins = [new_pin, recovered]
        self.age_sync_state(minutes=0)
        self.run_keyword("ps", "protect")
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertIsNone(state["deferred_update"])
        self.assertEqual(self.search(), ["Recovered bookmark", "New bookmark"])

    def test_malformed_cached_bookmark_survives_mixed_response_and_can_retry(self):
        other = bookmark("https://example.test/other", "Other", "protect", OLDER)
        dashboard = bookmark("https://example.test/dashboard", "Dashboard", "protect", NEWER)
        remote = bookmark("https://example.test/remote", "Remote bookmark", "protect", OLDER)
        self.server.pins = [other, dashboard]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard", "Other"])

        self.server.pins = [remote, {**dashboard, "description": False}]
        self.server.timestamp = NEWER
        self.age_sync_state()
        self.run_keyword("ps", "protect")
        partial = self.wait_for_sync()
        self.assertEqual(partial["remote_update"], OLDER)
        self.assertEqual(partial["deferred_update"], NEWER)
        self.assertEqual(self.search(), ["Dashboard", "Remote bookmark"])

        self.server.pins = [remote, {**dashboard, "description": "Updated dashboard"}]
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=0)
        self.run_keyword("ps", "protect")
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        self.assertEqual(self.search(), ["Updated dashboard", "Remote bookmark"])

    def test_pu_imports_valid_changes_when_a_malformed_bookmark_has_no_url(self):
        cached = bookmark("https://example.test/cached", "Cached bookmark", "protect", OLDER)
        remote = bookmark("https://example.test/remote", "Remote bookmark", "protect", NEWER)
        self.server.pins = [cached]
        self.configure()
        self.assertEqual(self.search(), ["Cached bookmark"])

        self.server.pins = [remote, {"description": "Missing URL", "tags": "protect"}]
        self.server.timestamp = NEWER
        result = self.run_command(["./pinion", "update"])
        self.assertIn("Some will be retried", result.stdout)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark", "Cached bookmark"])

        self.server.pins = [remote]
        result = self.run_command(["./pinion", "update"])
        self.assertIn("Updated cache files", result.stdout)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark"])

    def test_partial_pu_explains_manual_retry_when_automatic_updates_are_disabled(self):
        self.env["auto_update_cache"] = "0"
        self.configure()
        good = bookmark("https://example.test/good", "Readable bookmark", "protect", OLDER)
        bad = bookmark("https://example.test/bad", "Recovered bookmark", "protect", NEWER)
        self.server.pins = [good, {**bad, "description": False}]
        self.server.timestamp = NEWER
        update = self.follow_connection("pu", "")

        partial = self.run_object(*update).stdout
        self.assertIn("Run pu again to retry the rest", partial)
        self.assertNotIn("Some will be retried", partial)
        visible = self.run_keyword("ps", "protect")
        self.assertNotIn("rerun", visible)
        self.assertEqual([item["title"] for item in visible["items"]], ["Readable bookmark"])

        self.server.pins = [good, bad]
        self.assertIn("Updated cache files", self.run_object(*update).stdout)
        self.assertEqual(self.search(), ["Recovered bookmark", "Readable bookmark"])

    def test_inconsistent_pinboard_refresh_preserves_search_and_tag_results(self):
        dashboard = bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        remote = bookmark("https://example.test/remote", "Remote bookmark", "protect", NEWER)
        self.server.pins = [dashboard]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])

        for export, tags in (([remote], {"result_code": "temporarily unavailable"}),
                             ([], {"protect": 1})):
            with self.subTest(export=export, tags=tags):
                self.server.pins = export
                self.server.tags = tags
                self.server.timestamp = NEWER
                self.age_sync_state()
                checking = self.run_keyword("ps", "protect")
                self.assertEqual(checking["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], OLDER)
                self.assertEqual(state["deferred_update"], NEWER)
                waiting = self.run_keyword("ps", "protect", variables=checking["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                tag_results = self.run_keyword("pt")
                self.assertIn("protect", [row["title"] for row in tag_results["items"]])
                self.assertNotIn("result_code", [row["title"] for row in tag_results["items"]])

        self.server.pins = [remote]
        self.server.tags = None
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark"])

        # A valid empty account must still be able to remove the last bookmark.
        self.server.pins = []
        self.server.tags = []
        self.server.timestamp = "2026-01-03T00:00:00Z"
        self.age_sync_state()
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], self.server.timestamp)
        self.assertEqual(self.search(), ["No bookmarks found!"])

    def test_ps_preserves_a_cached_bookmark_when_a_malformed_row_hides_another_omission(self):
        omitted = bookmark("https://example.test/omitted", "Cached bookmark", "protect", OLDER)
        cached = bookmark("https://example.test/malformed", "Cached dashboard", "protect", OLDER)
        remote = bookmark("https://example.test/new", "Remote bookmark", "Protect", NEWER)
        self.server.pins = [omitted, cached]
        self.configure()
        self.assertEqual(set(self.search()), {"Cached bookmark", "Cached dashboard"})

        self.server.timestamp = NEWER
        for tags, count in (("protect protect", 3), (None, 3), ("other", 2)):
            with self.subTest(malformed_tags=tags, protect_count=count):
                self.server.tags = {"PROTECT": count}
                malformed = {**cached, "description": False, "tags": tags}
                # Repeated malformed rows for the same URL are still one bookmark.
                self.server.pins = [remote, malformed, malformed]
                self.age_sync_state()
                started = self.run_keyword("ps", "protect")
                self.assertEqual(started["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], OLDER)
                self.assertEqual(state["deferred_update"], NEWER)
                waiting = self.run_keyword("ps", "protect", variables=started["variables"])
                self.assertEqual({row["title"] for row in waiting["items"]},
                                 {"Cached bookmark", "Cached dashboard"})
                self.assertEqual(waiting["rerun"], 5)

        # Honest deletion is still allowed when the two rows can explain both counts.
        self.server.tags = {"protect": 2}
        self.server.pins = [remote, {**cached, "description": False}]
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark", "Cached dashboard"])

        self.server.pins = [remote, {**cached, "description": "Recovered dashboard"}]
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertIsNone(state["deferred_update"])
        self.assertEqual(self.search(), ["Remote bookmark", "Recovered dashboard"])

    def test_ps_accounts_for_skipped_invalid_urls_without_hiding_missing_bookmarks(self):
        cached = bookmark("https://example.test/cached", "Cached bookmark", "protect former", OLDER)
        remote = bookmark("https://example.test/remote", "New remote bookmark", "protect", NEWER)
        skipped = bookmark("/", "Invalid URL", "Protect protect Former", NEWER)
        self.server.pins = [cached]
        self.configure()
        self.assertEqual(self.search(), ["Cached bookmark"])

        # The skipped URL explains one count even if the server repeats its
        # row and the row repeats its tag. The third count is still missing.
        self.server.pins = [cached, skipped, skipped]
        self.server.tags = {"protect": 3, "former": 1}
        self.server.timestamp = NEWER
        self.age_sync_state()
        first = self.run_keyword("ps", "protect")
        self.assertEqual(first["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("ps", "protect", variables=first["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([row["title"] for row in waiting["items"]], ["Cached bookmark"])

        # The remaining count only belongs to the URL we deliberately skip.
        self.server.pins = [cached, remote, skipped, skipped]
        self.age_sync_state(minutes=0)
        retry = self.run_keyword("ps", "protect", variables=waiting["variables"])
        self.assertEqual(retry["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertIsNone(state["deferred_update"])
        result = self.run_keyword("ps", "protect", variables=retry["variables"])
        self.assertNotIn("rerun", result)
        self.assertEqual([row["title"] for row in result["items"]],
                         ["New remote bookmark", "Cached bookmark"])

        # A newly introduced invalid URL cannot confirm a cached URL's deletion.
        newest = "2026-01-03T00:00:00Z"
        self.server.pins = [{**remote, "description": "Updated remote bookmark"},
                            {**skipped, "href": "/newly-introduced"}]
        self.server.tags = {"protect": 2}
        self.server.timestamp = newest
        self.age_sync_state()
        first = self.run_keyword("ps", "protect")
        self.assertEqual(first["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertEqual(state["deferred_update"], newest)
        self.assertEqual(self.search(), ["New remote bookmark", "Cached bookmark"])

        # The endpoints now agree that the cached bookmark and its old tag
        # disappeared. The skipped URL's tag names don't prevent that deletion.
        self.server.tags = {"protect": 1}
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], newest)
        self.assertIsNone(state["deferred_update"])
        self.assertEqual(self.search(), ["Updated remote bookmark"])

    def test_ps_retries_when_a_truncated_export_contains_every_cached_url(self):
        cached = bookmark("https://example.test/cached", "Cached bookmark", "protect", OLDER)
        readable = bookmark("https://example.test/readable", "Readable change", "protect", NEWER)
        omitted = bookmark("https://example.test/omitted", "New remote bookmark", "protect", NEWER)
        self.server.pins = [cached]
        self.configure()
        self.assertEqual(self.search(), ["Cached bookmark"])

        # All the old URLs are present. The tag index is the only evidence
        # that a new bookmark has not arrived in the full export yet.
        self.server.pins = [cached, readable]
        self.server.tags = {"Protect": 3}
        self.server.timestamp = NEWER
        self.age_sync_state()
        first = self.run_keyword("ps", "protect")
        self.assertEqual(first["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("ps", "protect", variables=first["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual([row["title"] for row in waiting["items"]],
                         ["Readable change", "Cached bookmark"])

        # The corrected export keeps the same server timestamp.
        self.server.pins = [cached, readable, omitted]
        checks = self.server.paths["/v1/posts/update"]
        self.age_sync_state(minutes=0)
        retried = self.run_keyword("ps", "protect", variables=waiting["variables"])
        self.assertEqual(retried["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertIsNone(state["deferred_update"])
        self.assertEqual(self.server.paths["/v1/posts/update"], checks)
        result = self.run_keyword("ps", "protect", variables=retried["variables"])
        self.assertNotIn("rerun", result)
        self.assertEqual({row["title"] for row in result["items"]},
                         {"Cached bookmark", "Readable change", "New remote bookmark"})

    def test_pt_preserves_a_known_tag_carried_only_by_a_malformed_row(self):
        old = bookmark("https://example.test/old", "Retained bookmark", "protect former", OLDER)
        other = bookmark("https://example.test/other", "Backend bookmark", "backend", NEWER)
        self.server.pins = [old, other]
        self.configure()
        self.assertEqual(self.search(), ["Retained bookmark"])

        def tag_names(result):
            return {row["arg"] for row in result["items"] if row.get("arg")}

        self.assertEqual(tag_names(self.run_keyword("pt")), {"protect", "former", "backend"})
        self.server.pins = [other, {**old, "description": False, "tags": "Protect"}]
        self.server.tags = {"backend": 1}
        self.server.timestamp = NEWER
        self.age_sync_state()
        first = self.run_keyword("pt")
        self.assertEqual(first["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], OLDER)
        self.assertEqual(state["deferred_update"], NEWER)
        waiting = self.run_keyword("pt", variables=first["variables"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual(tag_names(waiting), {"protect", "former", "backend"})
        self.assertEqual(self.search(), ["Retained bookmark"])

        # Raw tags still establish that protect exists. The retained bookmark's
        # old cached value does not establish that former still exists.
        self.server.tags = {"protect": 1, "backend": 1}
        self.age_sync_state(minutes=0)
        retry = self.run_keyword("pt", variables=waiting["variables"])
        self.assertEqual(retry["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)
        visible = self.run_keyword("pt", variables=retry["variables"])
        self.assertEqual(tag_names(visible), {"protect", "backend"})
        selected = next(row for row in visible["items"] if row.get("arg") == "protect")
        action = self.select_result("pt", visible, selected)
        self.assertEqual(self.object_types[action[0]], "alfred.workflow.input.scriptfilter")
        result = self.run_filter(*action[:2], variables=action[2])
        self.assertEqual([row["title"] for row in result["items"]], ["Retained bookmark"])

        self.server.pins = [other, {**old, "description": "Recovered bookmark", "tags": "protect"}]
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("pt", variables=visible["variables"])["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Recovered bookmark"])

    def test_pt_keeps_known_tags_when_retained_bookmark_tags_are_unreadable(self):
        old = bookmark("https://example.test/old", "Retained protect bookmark", "protect", OLDER)
        other = bookmark("https://example.test/other", "Backend bookmark", "backend", NEWER)
        self.server.pins = [old, other]
        self.configure()
        self.assertEqual(self.search(), ["Retained protect bookmark"])
        variables = {}
        for malformed in ({**old, "description": False, "tags": None},
                          {key: value for key, value in old.items() if key != "tags"}):
            with self.subTest(malformed=malformed):
                self.server.pins = [other, malformed]
                self.server.tags = {"backend": 1}
                self.server.timestamp = NEWER
                self.age_sync_state()
                started = self.run_keyword("pt", variables=variables)
                self.assertEqual(started["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], OLDER)
                self.assertEqual(state["deferred_update"], NEWER)
                waiting = self.run_keyword("pt", variables=started["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual({row.get("arg") for row in waiting["items"]}, {"protect", "backend"})
                selected = self.run_keyword("pt", "protect", variables=waiting["variables"])
                row = next(row for row in selected["items"] if row.get("arg") == "protect")
                self.assertNotEqual(row["subtitle"], "NEW TAG")
                child, query, context = self.select_result("pt", selected, row)
                found = self.run_filter(child, query, variables=context)
                self.assertEqual([item["title"] for item in found["items"]], ["Retained protect bookmark"])
                self.open_bookmark_result(child, found, found["items"][0])
                variables = waiting["variables"]

        # Readable current tags can confirm a real removal on the pending retry.
        self.server.pins = [other, {**old, "description": "Recovered bookmark", "tags": "backend"}]
        self.server.tags = {"backend": 2}
        self.age_sync_state(minutes=0)
        resumed = self.run_keyword("pt", variables=variables)
        self.assertEqual(resumed["rerun"], 1)
        state = self.wait_for_sync()
        self.assertEqual(state["remote_update"], NEWER)
        self.assertIsNone(state["deferred_update"])
        tags = self.run_keyword("pt", variables=resumed["variables"])
        self.assertNotIn("rerun", tags)
        self.assertEqual({row.get("arg") for row in tags["items"]}, {"backend"})
        self.assertEqual(self.run_keyword("pt", "protect")["items"][0]["subtitle"], "NEW TAG")
        found = self.run_keyword("ps", "backend")
        row = next(row for row in found["items"] if row["title"] == "Recovered bookmark")
        self.open_bookmark_result("ps", found, row)

    def test_ps_retries_a_truncated_nonempty_export_at_the_same_watermark(self):
        dashboard = bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        remote = bookmark("https://example.test/remote", "Remote bookmark", "Protect", NEWER)
        self.server.pins = [dashboard]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])

        self.server.tags = {"PROTECT": 2}
        self.server.timestamp = NEWER
        variables = {}
        for export in ([remote], [remote, remote]):
            with self.subTest(export_count=len(export)):
                self.server.pins = export
                self.age_sync_state()
                start = self.run_keyword("ps", "protect", variables=variables)
                self.assertEqual(start["rerun"], 1)
                failed = self.wait_for_sync()
                self.assertEqual(failed["remote_update"], OLDER)
                self.assertEqual(failed["deferred_update"], NEWER)
                waiting = self.run_keyword("ps", "protect", variables=start["variables"])
                self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
                self.assertEqual(waiting["rerun"], 5)
                variables = waiting["variables"]

        self.server.pins = [dashboard, remote]
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect", variables=variables)["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark", "Dashboard"])

        # A real removal with counts that reflect the deletion succeeds.
        self.server.pins = [remote]
        self.server.tags = {"PROTECT": 1, "unused": 0}
        self.server.timestamp = "2026-01-03T00:00:00Z"
        self.age_sync_state()
        self.run_keyword("ps", "protect")
        self.assertEqual(self.wait_for_sync()["remote_update"], self.server.timestamp)
        self.assertEqual(self.search(), ["Remote bookmark"])

    def test_pt_keeps_usable_tags_when_tag_export_disagrees_with_bookmarks(self):
        page = bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        self.server.pins = [page]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])

        def tag_rows(result):
            return {item["title"] for item in result["items"] if item.get("arg")}

        self.server.pins = [{**page, "tags": "protect backend"}]
        self.server.timestamp = NEWER
        variables = {}
        for tags in ([], {"backend": 1}, {"backend": 1, "protect": 0}):
            with self.subTest(tags=tags):
                self.server.tags = tags
                self.age_sync_state()
                started = self.run_keyword("pt", variables=variables)
                self.assertEqual(started["rerun"], 1)
                self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)
                waiting = self.run_keyword("pt", variables=started["variables"])
                self.assertEqual(tag_rows(waiting), {"protect"})
                self.assertEqual(waiting["rerun"], 5)
                variables = waiting["variables"]

        self.server.tags = {"protect": 1, "backend": 1}
        self.age_sync_state(minutes=0)
        started = self.run_keyword("pt", variables=variables)
        self.assertEqual(started["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(tag_rows(self.run_keyword("pt", variables=started["variables"])),
                         {"protect", "backend"})

        # Genuine untagged bookmarks are compatible with an empty or zero-use tag map.
        for index, tags in enumerate(([], {"unused": 0}), 3):
            with self.subTest(tags=tags):
                self.server.pins = [{**page, "tags": "", "description": "Untagged bookmark"}]
                self.server.tags = tags
                self.server.timestamp = f"2026-01-0{index}T00:00:00Z"
                self.age_sync_state()
                self.run_keyword("pt")
                self.assertEqual(self.wait_for_sync()["remote_update"], self.server.timestamp)
                self.assertEqual([item["title"] for item in self.run_keyword("ps", "Untagged")["items"]],
                                 ["Untagged bookmark"])

    def test_partial_pinboard_export_does_not_publish_a_conflicting_tag_index(self):
        old = bookmark("https://example.test/old", "Old dashboard", "protect former", OLDER)
        new = bookmark("https://example.test/new", "New dashboard", "protect backend", NEWER)
        self.server.pins = [old]
        self.configure()
        self.assertEqual(self.search(), ["Old dashboard"])

        # The old row is unreadable. Only the newly decoded row is evidence
        # about today's tags; 'former' may genuinely have been removed.
        self.server.pins = [new, {**old, "description": False, "tags": "protect"}]
        self.server.timestamp = NEWER
        for tags in ([], {"backend": 1}):
            with self.subTest(tags=tags):
                self.server.tags = tags
                self.age_sync_state()
                started = self.run_keyword("pt")
                self.assertEqual(started["rerun"], 1)
                self.assertEqual(self.wait_for_sync()["remote_update"], OLDER)
                visible = self.run_keyword("pt", variables=started["variables"])
                self.assertEqual(visible["rerun"], 5)
                self.assertEqual({row["arg"] for row in visible["items"] if row.get("arg")},
                                 {"protect", "former"})
                self.assertEqual(self.search(), ["Old dashboard"])

        self.server.tags = {"protect": 2, "backend": 1}
        self.age_sync_state(minutes=0)
        self.run_keyword("pt")
        partial = self.wait_for_sync()
        self.assertEqual(partial["remote_update"], OLDER)
        self.assertEqual(partial["deferred_update"], NEWER)
        self.assertEqual(self.search(), ["New dashboard", "Old dashboard"])
        visible = self.run_keyword("pt")
        self.assertEqual({row["arg"] for row in visible["items"] if row.get("arg")},
                         {"protect", "backend"})

        self.server.pins = [new, {**old, "tags": "protect"}]
        self.age_sync_state(minutes=0)
        self.run_keyword("pt")
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["New dashboard", "Old dashboard"])

    def test_ps_does_not_promote_or_commit_bookmarks_with_incomplete_metadata(self):
        dashboard = bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)
        remote = bookmark("https://example.test/remote", "Remote bookmark", "protect", NEWER)
        self.server.pins = [dashboard]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])

        for field, invalid in (("time", None), ("shared", "unknown"), ("toread", "unknown")):
            with self.subTest(field=field):
                malformed = {**dashboard, "description": "Malformed dashboard"}
                if invalid is None:
                    del malformed[field]
                else:
                    malformed[field] = invalid
                self.server.pins = [remote, malformed]
                self.server.timestamp = NEWER
                self.age_sync_state()
                started = self.run_keyword("ps", "protect")
                self.assertEqual(started["rerun"], 1)
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], OLDER)
                self.assertEqual(state["deferred_update"], NEWER)
                self.assertEqual(self.search(), ["Remote bookmark", "Dashboard"])

        self.server.pins = [remote, {**dashboard, "description": "Recovered dashboard"}]
        self.age_sync_state(minutes=0)
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote bookmark", "Recovered dashboard"])

    def test_post_reaches_pinboard_while_background_suggestions_wait_for_headers(self):
        url = "https://example.test/new"
        self.configure()
        self.show_browser_page(url)
        self.env.update(page_is_bookmarked="0", suggest_tags="1", auto_update_cache="0")
        self.search()
        self.wait_for_request_slot()
        self.server.block_suggestions_for = url
        try:
            choices = self.run_keyword("p", "protect")
            self.assertTrue(self.server.suggestion_started.is_set())
            self.assertEqual(choices["rerun"], 1)
            row = next(row for row in choices["items"] if row.get("arg") == "protect")
            self.assert_post_reaches_pinboard_before_background_timeout(choices, row)
            self.assertEqual(self.server.saves[0]["url"], url)
            self.assertFalse(self.server.resume_suggestion.is_set())
            self.assertEqual(self.server.paths["/v1/posts/suggest"], 1)
        finally:
            self.server.resume_suggestion.set()

    def test_post_reaches_pinboard_while_background_timestamp_check_waits_for_headers(self):
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.show_browser_page(url)
        self.env.update(page_is_bookmarked="0", suggest_tags="0")
        self.age_sync_state(age_download=False)
        self.server.block_update = True
        waiting = self.run_keyword("ps", "protect")
        self.assertEqual(waiting["rerun"], 1)
        try:
            self.assertTrue(self.server.update_started.wait(2))
            choices = self.run_keyword("p", "; save during timestamp check")
            row = next(row for row in choices["items"]
                       if row.get("arg") == "; save during timestamp check")
            self.assert_post_reaches_pinboard_before_background_timeout(choices, row)
            self.assertEqual(self.server.saves[0]["extended"], "save during timestamp check")
        finally:
            self.server.resume_update.set()
        self.wait_for_sync()

    def assert_post_reaches_pinboard_before_background_timeout(self, choices, row):
        action, value, variables = self.select_result("p", choices, row)
        result = self.run_command(["bash", "-c", self.objects[action]["script"], action, value],
                                  env={**self.env, **variables}, timeout=2)
        self.assertIn("Successfully posted:", result.stdout)
        self.assertTrue(self.server.bookmark_added.is_set())

    def test_open_ps_backs_off_after_an_expired_pinboard_retry_after(self):
        endpoint = "/v1/posts/update"
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.pins.append(bookmark("https://example.test/new", "Remote", "protect", NEWER))
        self.server.timestamp = NEWER

        for retry_after in ("0", "Wed, 01 Jan 2020 00:00:00 GMT"):
            with self.subTest(retry_after=retry_after):
                self.age_sync_state(minutes=61, age_download=False)
                self.server.status[endpoint] = 429
                self.server.retry_after[endpoint] = retry_after
                attempts = self.server.paths[endpoint]
                started = self.run_keyword("ps", "protect")
                self.assertEqual(started["rerun"], 1)
                state = self.wait_for_sync()
                self.assertTrue(state["retry_update_check"])
                self.assertGreater(state["throttle_minutes"], 0)
                self.assertEqual(self.server.paths[endpoint], attempts + 1)
                waiting = self.run_keyword("ps", "protect", variables=started["variables"])
                self.assertEqual(waiting["rerun"], 5)
                self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                self.assertEqual(self.server.paths[endpoint], attempts + 1)

        self.server.status.clear()
        self.age_sync_state()
        self.assertEqual(self.run_keyword("ps", "protect")["rerun"], 1)
        self.assertEqual(self.wait_for_sync()["remote_update"], NEWER)
        self.assertEqual(self.search(), ["Remote", "Dashboard"])

    def test_pinboard_errors_preserve_valid_cached_results(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        for failure in ("status", "tags", "bookmarks"):
            with self.subTest(failure=failure):
                self.server.pins = [{"tags": "protect"}] if failure == "bookmarks" else []
                self.server.timestamp = NEWER
                self.server.status = {"/v1/posts/update": 520} if failure == "status" else {}
                self.server.tags = [{"unexpected": "tag list"}] if failure == "tags" else None
                self.age_sync_state()
                self.assertEqual(self.search(), ["Dashboard"])
                state = self.wait_for_sync()
                self.assertEqual(state["remote_update"], OLDER)
                self.assertEqual(self.search(), ["Dashboard"])

        self.server.status = {"/v1/posts/update": 520}
        self.server.pins = []
        self.server.tags = {}
        self.assertIn("Updated cache files", self.run_command(["./pinion", "update"]).stdout)
        self.assertNotIn("Dashboard", self.search())

    def test_alfred_title_edit_prefills_and_saves_without_python(self):
        url = "https://example.test/title-editor"
        current = '- Original "title" \\ 🦜'
        new_title = '- Updated "title" \\ 📌'
        self.server.pins = [bookmark(url, current, "protect", OLDER, notes="Keep this note")]
        self.configure()
        self.env["auto_update_cache"] = "0"
        # The real workflow scripts need only their shell and the bundled binary.
        # cat is for the fixture Keychain helper.
        for command in ("bash", "cat"):
            (self.workflow / command).symlink_to(f"/bin/{command}")
        self.env["PATH"] = str(self.workflow)
        self.assertIsNone(shutil.which("python3", path=self.env["PATH"]))

        trigger = next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}0001"))
        start = self.follow_connection(trigger, url)
        opened = self.run_object(*start)
        editor, title, context = self.follow_workflow_output(start[0], opened.stdout, start[2])
        self.assertEqual(title, current)
        unchanged = self.run_filter(editor, title, variables=context)
        self.assertFalse(unchanged["items"][0]["valid"])
        edited = self.run_filter(editor, new_title, variables=context)
        save = self.select_result(editor, edited, edited["items"][0])
        self.assertEqual(self.server.paths["/v1/posts/get"], 1)

        result = self.run_object(*save)
        notice, text, _ = self.follow_connection(save[0], result.stdout, save[2], include_outputs=True)
        self.assertEqual(self.object_types[notice], "alfred.workflow.output.notification")
        self.assertEqual(text.strip(), f"Renamed to: {new_title}")
        (saved,) = self.server.saves
        self.assertEqual((saved["url"], saved["description"], saved["tags"], saved["extended"]),
                         (url, new_title, "protect", "Keep this note"))
        self.assertEqual(self.server.paths["/v1/posts/get"], 2)
        self.assertEqual(self.server.paths["/v1/posts/all"], 0)

    def test_alfred_title_edit_rejects_null_notes_and_recovers_without_data_loss(self):
        url = "https://example.test/title-notes"
        original = bookmark(url, "Original title", "protect", OLDER,
                            notes="Existing notes must survive", shared="yes", toread="yes")
        self.server.pins = [original]
        self.configure()
        self.env["auto_update_cache"] = "0"
        trigger = next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}0001"))
        start = self.follow_connection(trigger, url)
        opened = self.run_object(*start)
        editor, current, context = self.follow_workflow_output(start[0], opened.stdout, start[2])
        self.assertEqual(current, original["description"])
        changed = self.run_filter(editor, "Changed title", variables=context)
        save = self.select_result(editor, changed, changed["items"][0])

        self.server.lookup_posts = [{**original, "extended": None}]
        rejected = self.run_object(*save)
        notice, message, _ = self.follow_connection(save[0], rejected.stdout, save[2],
                                                    include_outputs=True)
        self.assertEqual(self.object_types[notice], "alfred.workflow.output.notification")
        self.assertIn("Error: Couldn't look up the bookmark", message)
        self.assertEqual(self.server.saves, [])
        self.assertEqual(self.server.pins, [original])

        # The same invalid metadata must also block a newly opened editor.
        opened = self.run_object(*start)
        failed_editor, title, failed_context = self.follow_workflow_output(
            start[0], opened.stdout, start[2])
        self.assertEqual(title, "")
        blocked = self.run_filter(failed_editor, "Attempted edit", variables=failed_context)
        self.assertFalse(blocked["items"][0]["valid"])
        self.assertIn("incomplete bookmark metadata", blocked["items"][0]["subtitle"])

        self.server.lookup_posts = None
        saved = self.run_object(*save)
        self.assertEqual(saved.stdout.strip(), "Renamed to: Changed title")
        (replacement,) = self.server.saves
        for field in ("tags", "extended", "shared", "toread"):
            self.assertEqual(replacement[field], original[field])
        self.assertEqual(replacement["dt"], OLDER)
        self.assertEqual(replacement["description"], "Changed title")

    def test_alfred_title_editor_shows_the_actual_lookup_error(self):
        url = "https://example.test/title-editor"
        trigger = next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}0001"))
        start = self.follow_connection(trigger, url)

        def assert_lookup_error(expected, context_overrides=None):
            opened = self.run_object(start[0], start[1], {**start[2], **(context_overrides or {})})
            editor, title, context = self.follow_workflow_output(start[0], opened.stdout, start[2])
            self.assertEqual(title, "")
            # Alfred has to run this filter before the user types anything.
            self.assertEqual(self.objects[editor]["argumenttype"], 1)
            for typed in (title, "An attempted edit"):
                [row] = self.run_filter(editor, typed, variables=context)["items"]
                self.assertFalse(row["valid"])
                self.assertNotIn("arg", row)
                self.assertIn(expected, row["subtitle"])
                self.assertNotIn(TOKEN, row["subtitle"])

        assert_lookup_error("Run `pa` to configure Pinboard")
        invalid_data = self.workflow / "invalid-data-parent"
        invalid_data.write_text("Not a directory")
        assert_lookup_error("Couldn't read Pinion settings", {
            "alfred_workflow_data": str(invalid_data / "data"),
        })
        self.assertEqual(self.server.paths["/v1/posts/get"], 0)
        self.configure()
        self.server.status["/v1/posts/get"] = 401
        assert_lookup_error("Check the API token with `pa`")
        self.server.status.pop("/v1/posts/get")
        assert_lookup_error("That bookmark no longer exists on Pinboard")
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

    def test_stalled_title_prefill_does_not_block_another_bookmark_save(self):
        preview_url = "https://example.test/prefill"
        save_url = "https://example.test/another-bookmark"
        self.server.pins = [bookmark(preview_url, "Original title", "protect", OLDER)]
        self.env.update(auto_update_cache="0", suggest_tags="0")
        self.configure()
        self.assertEqual(self.search(), ["Original title"])
        self.show_browser_page(save_url, "Another bookmark")
        post_view = self.run_keyword("p", "protect")
        tag = next(row for row in post_view["items"] if row.get("arg") == "protect")
        save = self.select_result("p", post_view, tag)
        trigger = next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}0001"))
        start = self.follow_connection(trigger, preview_url)

        self.server.block_lookup_headers = True
        with subprocess.Popen(
            ["bash", "-c", self.objects[start[0]]["script"], *start[:2]],
            cwd=self.workflow, env={**self.env, **start[2]}, stdout=subprocess.PIPE, text=True,
        ) as opening:
            try:
                self.assertTrue(self.server.lookup_started.wait(3))
                saved = self.run_object(*save)
                self.assertIn("Successfully posted: Another bookmark", saved.stdout)
                self.assertEqual(self.server.saves[-1]["url"], save_url)
                self.assertIsNone(opening.poll(), "The prefill should still be waiting for headers")
            finally:
                self.server.resume_lookup.set()
            editor, prefilled, context = self.follow_workflow_output(
                start[0], opening.communicate(timeout=3)[0], start[2],
            )
        self.assertEqual(prefilled, "Original title")
        self.assertFalse(self.run_filter(editor, prefilled, variables=context)["items"][0]["valid"])

    def test_open_post_filter_retries_malformed_payloads_and_respects_empty_suggestions(self):
        url = "https://example.test/browser-page"
        endpoint = "/v1/posts/suggest"
        self.server.pins = [bookmark(url, "Browser page", "local", OLDER)]
        self.server.popular_tags[url] = ["recovered-from-pinboard"]
        self.env.update(auto_update_cache="0", page_is_bookmarked="0", suggest_tags="1")
        self.configure()
        self.show_browser_page(url)
        self.run_keyword("ps", "local")
        self.wait_for_sync()

        malformed = (
            b"<html>Temporary gateway error</html>",
            b'{"result_code":"something went wrong"}',
            b'[{"result_code":"something went wrong"}]',
            b'[{"popular":null}]',
            b'[{"popular":"bad"}]',
            b'[{"popular":[42]}]',
            b'[{"popular":["", " "]}]',
        )
        for payload in malformed:
            with self.subTest(payload=payload):
                self.wait_for_request_slot()
                self.server.raw_suggestions = payload
                count = self.server.paths[endpoint]
                failed = self.run_keyword("p", "local")
                self.assertEqual(failed["rerun"], 5)
                self.assertEqual(self.server.paths[endpoint], count + 1)
                self.assertIn("local", [row["title"] for row in failed["items"]])
                variables = failed["variables"]
                session = json.loads(variables["pinion_post_page"])
                self.assertIsNone(session["suggested_tags"])
                self.assertIsNotNone(session["suggestion_retry_at"])

                self.server.raw_suggestions = None
                waiting = self.run_keyword("p", "local", variables=variables)
                self.assertIn("rerun", waiting)
                self.assertEqual(self.server.paths[endpoint], count + 1)
                variables = waiting["variables"]
                session["suggestion_retry_at"] = OLDER
                variables["pinion_post_page"] = json.dumps(session)
                self.wait_for_request_slot()
                ready = self.run_keyword("p", "local", variables=variables)
                self.assertNotIn("rerun", ready)
                self.assertIn("recovered-from-pinboard", [row["title"] for row in ready["items"]])
                self.assertEqual(self.server.paths[endpoint], count + 2)

        for payload in (b"[]", b'[{"popular":[]},{"recommended":[]}]'):
            with self.subTest(empty=payload):
                self.wait_for_request_slot()
                self.server.raw_suggestions = payload
                count = self.server.paths[endpoint]
                empty = self.run_keyword("p", "local")
                self.assertNotIn("rerun", empty)
                self.assertEqual(json.loads(empty["variables"]["pinion_post_page"])["suggested_tags"], [])
                self.assertEqual(self.server.paths[endpoint], count + 1)
                self.server.raw_suggestions = None
                self.wait_for_request_slot()
                same_session = self.run_keyword("p", "local", variables=empty["variables"])
                self.assertNotIn("rerun", same_session)
                self.assertEqual(self.server.paths[endpoint], count + 1)

    def test_retitle_serializes_local_posts_across_lookup_and_replace(self):
        url = "https://example.test/retitle"
        self.server.pins = [
            bookmark(url, "Original title", "protect", OLDER, notes="Original note")
        ]
        self.configure()
        self.assertEqual(self.search(), ["Original title"])
        self.env["auto_update_cache"] = "0"
        browser = self.workflow / "osascript"
        browser_started = self.workflow / "browser-read"
        browser.write_text(
            "#!/bin/sh\n"
            'touch "$PINION_BROWSER_READ"\n'
            f"printf '%s\\n' '{{\"{url} fd850fc2e63511e79f720023dfdf24ec Browser title\"}}'\n"
        )
        browser.chmod(0o700)
        self.env["PINION_BROWSER_READ"] = str(browser_started)

        self.server.block_lookup = True
        with subprocess.Popen(
            ["./pinion", "retitle", "--url", url, "--expected-title=Original title",
             "--", "Renamed title"],
            cwd=self.workflow, env=self.env, stdout=subprocess.PIPE, text=True,
        ) as retitle:
            try:
                self.assertTrue(self.server.lookup_started.wait(3))
                with subprocess.Popen(
                    ["./pinion", "post", "--tags", "new-tag", "--description", "New note"],
                    cwd=self.workflow, env=self.env, stdout=subprocess.PIPE, text=True,
                ) as post:
                    try:
                        deadline = time.monotonic() + 3
                        while not browser_started.exists() and time.monotonic() < deadline:
                            time.sleep(0.01)
                        self.assertTrue(browser_started.exists())
                        self.assertFalse(self.server.bookmark_added.wait(0.5))
                    finally:
                        self.server.resume_lookup.set()
                    self.assertIn("Successfully posted:", post.communicate(timeout=6)[0])
                self.assertIn("Renamed to:", retitle.communicate(timeout=3)[0])
            finally:
                self.server.resume_lookup.set()
        rename_save, post_save = self.server.saves
        self.assertEqual((rename_save["description"], rename_save["tags"], rename_save["extended"]),
                         ("Renamed title", "protect", "Original note"))
        self.assertEqual((post_save["description"], post_save["tags"], post_save["extended"]),
                         ("Browser title", "new-tag", "New note"))

    def test_retitle_keeps_selected_url_and_checks_remote_bookmark(self):
        self.configure()
        upper = bookmark("https://example.test/ABC", "Wrong case", "other", NEWER, shared="yes")
        lower = bookmark("https://example.test/abc", "Selected bookmark", "initial", OLDER)
        self.server.pins = [upper, lower]
        start, filter_uid, save = [next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}{n}"))
                                  for n in ("0002", "0003", "0004")]
        self.assertEqual(self.connections[start][0]["destinationuid"], filter_uid)
        self.assertEqual(self.connections[filter_uid][0]["destinationuid"], save)
        self.assertFalse(self.objects[filter_uid].get("keyword"))
        opened = json.loads(self.run_object(start, lower["href"]).stdout)["alfredworkflow"]
        self.assertEqual(opened["arg"], "Selected bookmark")
        title = '- New "quoted" title \\ and 🦜'
        self.assertFalse(json.loads(self.run_object(filter_uid, title).stdout)["items"][0]["valid"])
        row = json.loads(self.run_object(filter_uid, title, opened["variables"]).stdout)["items"][0]
        self.assertTrue(row["valid"])
        self.run_object(start, upper["href"])

        lower.update(tags="latest private", extended="Changes from another Mac", toread="yes")
        self.assertIn(title, self.run_object(save, row["arg"]).stdout)
        (saved,) = self.server.saves
        self.assertEqual({key: saved[key] for key in
                          ("url", "description", "tags", "extended", "shared", "toread", "replace", "dt")},
                         dict(url=lower["href"], description=title, tags=lower["tags"],
                              extended=lower["extended"], shared="no", toread="yes", replace="yes",
                              dt=OLDER))

        expired = json.loads(row["arg"])
        expired["started_at"] = int(time.time()) - 601
        self.run_object(save, json.dumps(expired))
        self.assertEqual(len(self.server.saves), 1)
        self.server.pins = [upper]
        self.run_object(save, row["arg"])
        self.assertEqual(len(self.server.saves), 1)

    def test_retitle_rejects_a_title_changed_after_opening_the_editor(self):
        self.configure()
        original = '- Original "quoted" title 🦜'
        pin = bookmark("https://example.test/retitle", original, "protect", OLDER)
        self.server.pins = [pin]
        start, filter_uid, save = [next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}{n}"))
                                  for n in ("0002", "0003", "0004")]
        opened = json.loads(self.run_object(start, pin["href"]).stdout)["alfredworkflow"]
        self.assertEqual(opened["arg"], original)
        row = json.loads(self.run_object(filter_uid, "Local edit", opened["variables"]).stdout)["items"][0]
        self.assertTrue(row["valid"])

        pin["description"] = "Title from another Mac"
        result = self.run_object(save, row["arg"])
        self.assertIn("The title changed on Pinboard.", result.stdout)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        reopened = json.loads(self.run_object(start, pin["href"]).stdout)["alfredworkflow"]
        self.assertEqual(reopened["arg"], pin["description"])
        row = json.loads(self.run_object(filter_uid, "- Local edit", reopened["variables"]).stdout)["items"][0]
        self.assertIn("Renamed to: - Local edit", self.run_object(save, row["arg"]).stdout)
        self.assertEqual([saved["description"] for saved in self.server.saves], ["- Local edit"])

    def test_retitle_save_reports_startup_failure_through_its_notification(self):
        url = "https://example.test/retitle"
        self.server.pins = [bookmark(url, "Original title", "protect", OLDER)]
        self.configure()
        trigger = next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}0001"))
        start = self.follow_connection(trigger, url)
        prefilled = self.run_object(*start)
        editor, title, context = self.follow_workflow_output(start[0], prefilled.stdout, start[2])
        self.assertEqual(title, "Original title")
        results = self.run_filter(editor, "- Changed title", variables=context)
        [row] = results["items"]
        save = self.select_result(editor, results, row)
        (self.data / "settings.json").unlink()

        blocked_dir = self.workflow / "blocked-data-parent"
        blocked_dir.write_text("Not a directory")
        for settings, expected in (
            ({}, "did you set API token?"),
            ({"alfred_workflow_data": str(blocked_dir / "data")}, "Couldn't read Pinion settings"),
        ):
            with self.subTest(settings=settings):
                uid, query, variables = save
                result = self.run_object(uid, query, {**variables, **settings})
                self.assertTrue(result.stdout.startswith("Error: "), result.stdout)
                self.assertNotIn("panicked", result.stderr)
                notice, text, _ = self.follow_connection(uid, result.stdout, variables,
                                                         include_outputs=True)
                self.assertEqual(self.object_types[notice], "alfred.workflow.output.notification")
                self.assertIn(expected, text)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        direct = self.run_command(["./pinion", "retitle", "--url", url, "--", "Changed"],
                                  check=False)
        self.assertEqual(direct.returncode, 1)
        self.assertEqual(json.loads(direct.stdout)["items"][0]["title"], "Error")

    def test_retitle_refuses_to_save_after_the_pinboard_account_changes(self):
        url = "https://example.test/retitle"
        original = "Shared title between accounts"
        self.server.pins = [bookmark(url, original, "first", OLDER)]
        self.server.second_pins = [bookmark(url, original, "second", NEWER)]
        self.configure()
        start, filter_uid, save = [next(uid for uid in self.objects if uid.startswith(f"{RETITLE_UID}{n}"))
                                  for n in ("0002", "0003", "0004")]
        opened = json.loads(self.run_object(start, url).stdout)["alfredworkflow"]
        self.assertEqual(opened["arg"], original)
        row = json.loads(self.run_object(filter_uid, "Local edit", opened["variables"]).stdout)["items"][0]
        self.assertTrue(row["valid"])

        self.configure(SECOND_TOKEN)
        self.assertIn("The Pinboard account changed.", self.run_object(save, row["arg"]).stdout)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

        reopened = json.loads(self.run_object(start, url).stdout)["alfredworkflow"]
        row = json.loads(self.run_object(filter_uid, "Local edit", reopened["variables"]).stdout)["items"][0]
        self.assertIn("Renamed to: Local edit", self.run_object(save, row["arg"]).stdout)
        (saved,) = self.server.saves
        self.assertEqual(saved["auth_token"], SECOND_TOKEN)
        self.assertEqual(saved["tags"], "second")

    def test_pcheck_can_only_download_newer_releases(self):
        # pcheck runs without a Pinboard token or any previous updater state.
        shutil.rmtree(self.cache)
        # An update check has no reason to create or read the Pinboard data directory.
        self.data.write_text("Not a directory")
        self.env["alfred_workflow_data"] = str(self.data / "unavailable")
        latest = self.run_keyword("pcheck")["items"][0]
        self.assertFalse(latest["valid"])
        self.assertEqual(self.server.paths[RELEASE_PATH], 1)
        self.server.release_tag = "not-a-version"
        failed = self.run_keyword("pcheck")["items"][0]
        self.assertFalse(failed["valid"])
        self.assertNotEqual(failed["title"], latest["title"])
        for tag in ("v1.0.0", "v0.9.0"):
            with self.subTest(tag=tag):
                self.server.release_tag = tag
                self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                self.run_command(["./pinion", "self", "-d"], check=False)
                self.assertEqual(self.server.paths[ASSET_PATH], 0)
        self.server.release_tag = "v1.1.0"
        self.assertTrue(self.run_keyword("pcheck")["items"][0].get("valid", True))
        self.run_command(["./pinion", "self", "-d"])
        self.assertEqual(self.server.paths[ASSET_PATH], 1)

    def test_withdrawn_workflow_offer_is_removed_after_a_successful_check(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        offered = self.run_keyword("pcheck")["items"][0]
        self.assertTrue(offered.get("valid", True))
        self.assertEqual(self.search(), ["Dashboard", offered["title"]])

        self.server.status[RELEASE_PATH] = 503
        self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
        self.assertEqual(self.search(), ["Dashboard", offered["title"]])

        self.server.status[RELEASE_PATH] = 404
        self.assertEqual(self.run_keyword("pcheck")["items"][0]["title"],
                         "No newer version of Pinion is available.")
        self.assertEqual(self.search(), ["Dashboard"])
        result = self.run_object("A2DA0FE3-D7E7-47E0-9383-B42E0EFDF202", offered["arg"])
        self.assertIn("Run pcheck again", result.stdout)
        self.assertEqual(self.server.paths[ASSET_PATH], 0)

        self.server.status.clear()
        self.assertTrue(self.run_keyword("pcheck")["items"][0].get("valid", True))
        self.server.status[ASSET_PATH] = 404
        result = self.run_object("A2DA0FE3-D7E7-47E0-9383-B42E0EFDF202", offered["arg"])
        self.assertIn("Run pcheck again", result.stdout)
        self.assertEqual(self.search(), ["Dashboard"])

    def test_automatic_update_check_recovers_after_a_missing_workflow_asset(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        download = "A2DA0FE3-D7E7-47E0-9383-B42E0EFDF202"
        release_cache = self.cache / "last_check_status_async.json"

        for source in (404, 410, "metadata"):
            with self.subTest(source=source):
                self.server.release_tag = "v1.1.0"
                offered = self.run_keyword("pcheck")["items"][0]
                self.assertEqual(offered["arg"], "1.1.0")
                if source == "metadata":
                    self.server.release_has_workflow = False
                    try:
                        row = self.run_keyword("pcheck")["items"][0]
                        self.assertEqual(row["title"], "No newer version of Pinion is available.")
                    finally:
                        self.server.release_has_workflow = True
                else:
                    self.server.status[ASSET_PATH] = source
                    try:
                        result = self.run_object(download, offered["arg"])
                        self.assertIn("Run pcheck again", result.stdout)
                    finally:
                        self.server.status.pop(ASSET_PATH, None)

                checks = self.server.paths[RELEASE_PATH]
                waiting = self.run_keyword("ps", "protect")
                self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                self.assertGreater(waiting.get("rerun", 0), 0)
                self.assertEqual(self.server.paths[RELEASE_PATH], checks)

                # Simulate the end of the short retry interval, keeping the last
                # regular GitHub check recent enough to prevent a daily check.
                pending = json.loads(release_cache.read_text())
                pending["retry_after"] = OLDER
                release_cache.write_text(json.dumps(pending))
                attempt = self.cache / "workflow-update-attempt.json"
                attempt.write_text(json.dumps(OLDER))

                if source == 404:
                    # A failed short retry should preserve the pending asset and
                    # keep the same Alfred search eligible for the next check.
                    self.server.status[RELEASE_PATH] = 503
                    self.server.release_started.clear()
                    self.server.resume_release.clear()
                    self.server.block_release = True
                    try:
                        self.run_keyword("ps", "protect")
                        self.assertTrue(self.server.release_started.wait(3))
                    finally:
                        self.server.resume_release.set()
                        self.server.block_release = False
                        self.wait_for_update_check()
                    checks += 1
                    self.assertEqual(json.loads(release_cache.read_text()), pending)
                    after_grace = datetime.now(timezone.utc) - timedelta(seconds=3)
                    attempt.write_text(json.dumps(after_grace.isoformat()))
                    waiting = self.run_keyword("ps", "protect")
                    self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                    self.assertGreater(waiting.get("rerun", 0), 0)
                    self.assertEqual(self.server.paths[RELEASE_PATH], checks)
                    self.server.status.pop(RELEASE_PATH)
                    attempt.write_text(json.dumps(OLDER))

                self.server.release_started.clear()
                self.server.resume_release.clear()
                self.server.block_release = True
                try:
                    waiting = self.run_keyword("ps", "protect")
                    self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                    self.assertGreater(waiting.get("rerun", 0), 0)
                    self.assertTrue(self.server.release_started.wait(3))
                finally:
                    self.server.resume_release.set()
                    self.server.block_release = False
                    self.wait_for_update_check()

                self.assertEqual(self.server.paths[RELEASE_PATH], checks + 1)
                self.assertEqual(self.search(), ["Dashboard", offered["title"]])
                self.assertEqual(self.server.paths[RELEASE_PATH], checks + 1)

        self.server.status[ASSET_PATH] = 404
        self.assertIn("Run pcheck again", self.run_object(download, offered["arg"]).stdout)
        self.server.status.pop(ASSET_PATH)
        checks = self.server.paths[RELEASE_PATH]
        self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], offered["arg"])
        self.assertEqual(self.server.paths[RELEASE_PATH], checks + 1)

    def test_install_uses_the_selected_workflow_version_and_reported_path(self):
        download_uid = "A2DA0FE3-D7E7-47E0-9383-B42E0EFDF202"
        install_uid = "221EA45C-D3F1-4FD8-9143-CD10110D5BFC"
        self.server.release_tag = "v1.1.0"
        offered = self.run_keyword("pcheck")["items"][0]
        self.assertIn("1.1.0", offered["subtitle"])
        self.server.release_tag = "v1.2.0"
        results = self.run_keyword("pcheck")
        new_offer = results["items"][0]

        stale = self.run_object(download_uid, offered["arg"])
        self.assertTrue(stale.stdout.startswith("Error: "))
        self.assertEqual(self.server.paths[ASSET_PATH], 0)
        for invalid in ("", "--not-a-version"):
            with self.subTest(invalid=invalid):
                result = self.run_object(download_uid, invalid)
                self.assertTrue(result.stdout.startswith("Error: "))
        self.assertEqual(self.server.paths[ASSET_PATH], 0)

        download = self.select_result("pcheck", results, new_offer)
        downloaded = self.run_object(*download).stdout.strip()
        self.assertTrue(downloaded.startswith("Download Successful: "))
        path = Path(downloaded.removeprefix("Download Successful: "))
        self.assertRegex(path.name, r"^latest_release_Pinion_v1\.2\.0_[A-Za-z0-9]+\.alfredworkflow$")
        self.assertTrue(path.is_file())

        open_script = self.workflow / "open"
        open_script.write_text('#!/bin/sh\nprintf "%s\\n" "$1" "$2" "$3" > "$PINION_OPEN_LOG"\n')
        open_script.chmod(0o700)
        opened = self.workflow / "open.log"
        self.env["PINION_OPEN_LOG"] = str(opened)
        self.run_object(install_uid, stale.stdout.strip())
        self.assertFalse(opened.exists())
        self.run_object(*self.follow_connection(download[0], downloaded, download[2]))
        self.assertEqual(opened.read_text().splitlines(),
                         ["-b", "com.runningwithcrayons.Alfred", str(path)])

    def test_installer_rejects_a_withdrawal_observed_after_the_download(self):
        self.server.release_tag = "v1.1.0"
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        output = self.run_object(*download).stdout
        self.assertTrue(output.startswith("Download Successful: "), output)
        self.assertTrue(Path(output.removeprefix("Download Successful: ")).is_file())
        install = self.follow_connection(download[0], output, download[2], include_outputs=True)

        self.server.status[RELEASE_PATH] = 404
        self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\n: > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        self.assertIn("The offered version changed or is unavailable.", self.run_object(*install).stdout)
        self.assertFalse(opened.exists())

    def test_update_download_rechecks_an_open_offer_after_github_changes(self):
        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        offer = result["items"][0]
        self.assertEqual(offer["arg"], "1.1.0")
        download = self.select_result("pcheck", result, offer)

        self.server.release_tag = "v1.0.0"
        output = self.run_object(*download).stdout
        self.assertIn("The offered version changed or is unavailable.", output)
        self.assertEqual(self.server.paths[RELEASE_PATH], 2)
        self.assertEqual(self.server.paths[ASSET_PATH], 0)
        self.assertFalse(list(self.cache.glob("*.alfredworkflow")))

    def test_update_download_checks_github_when_the_open_offers_cache_was_evicted(self):
        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        offer = result["items"][0]
        self.assertEqual(offer["arg"], "1.1.0")
        shutil.rmtree(self.cache)

        download = self.select_result("pcheck", result, offer)
        output = self.run_object(*download).stdout
        self.assertTrue(output.startswith("Download Successful: "), output)
        self.assertEqual(self.server.paths[RELEASE_PATH], 2)
        self.assertEqual(self.server.paths[ASSET_PATH], 1)
        install = self.follow_connection(download[0], output, download[2])
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\nprintf "%s\\n" "$3" > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        self.assertIn("Opening Alfred to install", self.run_object(*install).stdout)
        self.assertEqual(opened.read_text().strip(), output.removeprefix("Download Successful: "))

    def test_rejected_same_version_download_cannot_replace_verified_installer_bundle(self):
        self.server.release_tag = "v1.1.0"
        self.server.asset_body = "withdrawn bundle"
        first = self.run_keyword("pcheck")
        uid, selected, context = self.select_result("pcheck", first, first["items"][0])
        self.server.block_asset = True
        with subprocess.Popen(
            ["bash", "-c", self.objects[uid]["script"], uid, selected],
            cwd=self.workflow, env={**self.env, **context},
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ) as slow:
            try:
                self.assertTrue(self.server.asset_started.wait(3))
                # GitHub's browser URL is unchanged when an asset is reuploaded
                # under the same tag and filename, but its ID changes.
                self.server.asset_id += 1
                self.server.asset_body = "replacement bundle"
                self.server.block_asset = False
                refreshed = self.run_keyword("pcheck")
                verified = self.select_result("pcheck", refreshed, refreshed["items"][0])
                approved = self.run_object(*verified).stdout
                self.assertTrue(approved.startswith("Download Successful: "), approved)
                bundle = Path(approved.removeprefix("Download Successful: "))
                self.assertEqual(bundle_marker(bundle), "replacement bundle")
            finally:
                self.server.resume_asset.set()
            rejected, _ = slow.communicate(timeout=5)

        self.assertIn("The offered version changed or is unavailable.", rejected)
        self.assertEqual(list(self.cache.glob("*.alfredworkflow")), [bundle])
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\ncat "$3" > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        install = self.follow_connection(verified[0], approved, verified[2])
        self.assertIn("Opening Alfred to install", self.run_object(*install).stdout)
        self.assertEqual(bundle_marker(opened), "replacement bundle")

    def test_selected_old_offer_rejects_replacement_with_the_same_version(self):
        self.server.release_tag = "v1.1.0"
        first = self.run_keyword("pcheck")
        previous = first["items"][0]
        selected = self.select_result("pcheck", first, previous)

        self.server.asset_id += 1
        self.server.asset_body = "replacement bundle"
        self.assertIn("The offered version changed or is unavailable.",
                      self.run_object(*selected).stdout)
        self.assertEqual(self.server.paths[ASSET_PATH], 0)

        refreshed = self.run_keyword("pcheck")
        current = refreshed["items"][0]
        self.assertEqual(current["arg"], previous["arg"])
        self.assertNotEqual(current["variables"]["workflow_update_asset"],
                            previous["variables"]["workflow_update_asset"])
        result = self.run_object(*self.select_result("pcheck", refreshed, current)).stdout
        self.assertTrue(result.startswith("Download Successful: "), result)
        bundle = Path(result.removeprefix("Download Successful: "))
        self.assertEqual(bundle_marker(bundle), "replacement bundle")

    def test_automatic_update_offer_refreshes_legacy_cache_before_becoming_selectable(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        original = self.run_keyword("pcheck")["items"][0]
        cache = self.cache / "last_check_status_async.json"
        release = json.loads(cache.read_text())
        release.pop("asset_api_url")
        cache.write_text(json.dumps(release))

        self.server.asset_id += 1
        self.server.block_release = True
        try:
            waiting = self.run_keyword("ps", "protect", timeout=2)
            self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
            self.assertEqual(waiting["rerun"], 1)
            self.assertTrue(self.server.release_started.wait(3))
        finally:
            self.server.resume_release.set()
            self.server.block_release = False
        self.wait_for_update_check()
        ready = self.run_keyword("ps", "protect", variables=waiting["variables"])
        offer = next(item for item in ready["items"] if item.get("arg") == "1.1.0")
        self.assertNotEqual(offer["variables"]["workflow_update_asset"],
                            original["variables"]["workflow_update_asset"])
        selected = self.select_result("ps", ready, offer)
        output = self.run_object(*selected).stdout
        self.assertTrue(output.startswith("Download Successful: "), output)

    def test_update_download_does_not_forward_an_offer_withdrawn_during_asset_transfer(self):
        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        offer = result["items"][0]
        uid, selected, context = self.select_result("pcheck", result, offer)
        self.server.block_asset = True
        with subprocess.Popen(
            ["bash", "-c", self.objects[uid]["script"], uid, selected],
            cwd=self.workflow, env={**self.env, **context},
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ) as download:
            try:
                self.assertTrue(self.server.asset_started.wait(3))
                self.server.status[RELEASE_PATH] = 404
                withdrawn = self.run_keyword("pcheck")["items"][0]
                self.assertFalse(withdrawn["valid"])
            finally:
                self.server.resume_asset.set()
            output, _ = download.communicate(timeout=5)
        self.assertIn("The offered version changed or is unavailable.", output)
        self.assertFalse(output.startswith("Download Successful: "), output)

    def test_missing_download_only_clears_the_same_github_asset(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        for replaced in (False, True):
            with self.subTest(replaced=replaced):
                self.server.status.pop(ASSET_PATH, None)
                result = self.run_keyword("pcheck")
                offer = result["items"][0]
                uid, selected, context = self.select_result("pcheck", result, offer)
                self.server.block_asset = True
                self.server.asset_started.clear()
                self.server.resume_asset.clear()
                with subprocess.Popen(
                    ["bash", "-c", self.objects[uid]["script"], uid, selected],
                    cwd=self.workflow, env={**self.env, **context},
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                ) as download:
                    try:
                        self.assertTrue(self.server.asset_started.wait(3))
                        if replaced:
                            self.server.asset_id += 1
                        refreshed = self.run_keyword("pcheck")["items"][0]
                        self.assertEqual(refreshed["arg"], selected)
                        self.server.status[ASSET_PATH] = 404
                    finally:
                        self.server.resume_asset.set()
                        self.server.block_asset = False
                    output, _ = download.communicate(timeout=5)
                self.assertIn("The offered version changed or is unavailable.", output)
                visible = self.run_keyword("ps", "protect")
                if replaced:
                    self.assertEqual(visible["items"][-1]["arg"], selected)
                    self.assertNotIn("rerun", visible)
                else:
                    self.assertEqual([row["title"] for row in visible["items"]], ["Dashboard"])
                    self.assertGreater(visible.get("rerun", 0), 0)

    def test_selected_download_uses_the_asset_api_and_requires_downloadable_bytes(self):
        self.server.release_tag = "v1.1.0"
        result = self.run_keyword("pcheck")
        download = self.select_result("pcheck", result, result["items"][0])

        self.server.status[ASSET_PATH] = 204
        self.assertIn("Couldn't download the latest workflow.", self.run_object(*download).stdout)
        self.server.status.pop(ASSET_PATH)
        self.server.asset_content_type = "Application/JSON; charset=utf-8"
        self.assertIn("Run pcheck again", self.run_object(*download).stdout)
        self.assertFalse(list(self.cache.glob("*.alfredworkflow")))
        self.assertEqual(self.server.paths[ASSET_BROWSER_PATH], 0)
        self.assertEqual(self.server.asset_headers, [("application/octet-stream", "2022-11-28")] * 2)

    def test_missing_asset_retries_survive_an_unavailable_release_response(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        self.server.release_has_workflow = False
        self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
        release_cache = self.cache / "last_check_status_async.json"
        attempt = self.cache / "workflow-update-attempt.json"

        def expire_retry():
            state = json.loads(release_cache.read_text())
            state["retry_after"] = OLDER
            release_cache.write_text(json.dumps(state))
            attempt.write_text(json.dumps(OLDER))

        waiting = self.run_keyword("ps", "protect")
        self.assertGreater(waiting.get("rerun", 0), 0)
        for status in (404, 410, None):
            with self.subTest(status=status):
                expire_retry()
                if status is None:
                    self.server.status.pop(RELEASE_PATH)
                    self.server.release_has_workflow = True
                else:
                    self.server.status[RELEASE_PATH] = status
                self.server.block_release = True
                self.server.release_started.clear()
                self.server.resume_release.clear()
                checks = self.server.paths[RELEASE_PATH]
                try:
                    waiting = self.run_keyword("ps", "protect", variables=waiting["variables"])
                    self.assertGreater(waiting.get("rerun", 0), 0)
                    self.assertTrue(self.server.release_started.wait(3))
                finally:
                    self.server.resume_release.set()
                    self.server.block_release = False
                    self.wait_for_update_check()
                self.assertEqual(self.server.paths[RELEASE_PATH], checks + 1)
                waiting = self.run_keyword("ps", "protect", variables=waiting["variables"])
                if status is not None:
                    self.assertGreater(waiting.get("rerun", 0), 0)
                    self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                else:
                    self.assertNotIn("rerun", waiting)
                    self.assertEqual(waiting["items"][-1]["arg"], "1.1.0")

    def test_recheck_keeps_advertised_but_missing_asset_hidden_until_it_recovers(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        offered = self.run_keyword("pcheck")
        selection = self.select_result("pcheck", offered, offered["items"][0])
        cache = self.cache / "last_check_status_async.json"
        attempt = self.cache / "workflow-update-attempt.json"
        for recovery in ("manual", "automatic", "replacement"):
            with self.subTest(recovery=recovery):
                requests = self.server.paths[ASSET_PATH]
                self.server.status[ASSET_PATH] = 404
                self.assertIn("Run pcheck again", self.run_object(*selection).stdout)
                self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                self.assertIn("rerun", self.run_keyword("ps", "protect"))
                self.assertIn("Run pcheck again", self.run_object(*selection).stdout)
                self.assertEqual(self.server.paths[ASSET_PATH], requests + 3)
                self.assertTrue(json.loads(cache.read_text())["failed_asset"].endswith(ASSET_PATH))

                if recovery == "manual":
                    self.server.status.pop(ASSET_PATH)
                    self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
                    self.assertEqual(self.server.paths[ASSET_PATH], requests + 4)
                elif recovery == "automatic":
                    state = json.loads(cache.read_text())
                    state["retry_after"] = OLDER
                    cache.write_text(json.dumps(state))
                    attempt.write_text(json.dumps(OLDER))
                    self.server.status.pop(ASSET_PATH)
                    self.server.block_release = True
                    self.server.release_started.clear()
                    self.server.resume_release.clear()
                    try:
                        self.assertIn("rerun", self.run_keyword("ps", "protect"))
                        self.assertTrue(self.server.release_started.wait(3))
                    finally:
                        self.server.resume_release.set()
                        self.server.block_release = False
                        self.wait_for_update_check()
                    self.assertEqual(self.run_keyword("ps", "protect")["items"][-1]["arg"], "1.1.0")
                    self.assertEqual(self.server.paths[ASSET_PATH], requests + 4)
                else:
                    self.server.asset_id += 1
                    replacement = self.run_keyword("pcheck")["items"][0]
                    self.assertEqual(replacement["arg"], "1.1.0")
                    self.assertNotEqual(replacement["variables"], offered["items"][0]["variables"])
                    self.assertEqual(self.server.paths[ASSET_PATH], requests + 3)
                    self.assertEqual(self.server.paths[ASSET_ENDPOINT + str(self.server.asset_id)], 0)

    def test_automatic_github_check_rebuilds_missing_recent_release_result(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
        result_path = self.cache / "last_check_status_async.json"
        attempt_path = self.cache / "workflow-update-attempt.json"

        def rebuild(variables=None):
            result_path.unlink()
            checks = self.server.paths[RELEASE_PATH]
            self.server.release_started.clear()
            self.server.resume_release.clear()
            self.server.block_release = True
            try:
                first = self.run_keyword("ps", "protect", variables=variables)
                self.assertEqual(first["rerun"], 1)
                self.assertTrue(self.server.release_started.wait(3))
            finally:
                self.server.resume_release.set()
                self.server.block_release = False
                self.wait_for_update_check()
            self.assertEqual(self.server.paths[RELEASE_PATH], checks + 1)
            return first["variables"]

        self.assertFalse(attempt_path.exists())
        variables = rebuild()
        self.assertTrue(attempt_path.exists())
        variables = rebuild(variables)
        recovered = self.run_keyword("ps", "protect", variables=variables)
        self.assertEqual(recovered["items"][-1]["arg"], "1.1.0")
        self.assertNotIn("rerun", recovered)

        # A newer failed attempt still backs off while the cache is missing.
        self.server.status[RELEASE_PATH] = 503
        variables = rebuild(variables)
        checks = self.server.paths[RELEASE_PATH]
        failed_attempt = attempt_path.read_text()
        self.assertFalse(result_path.exists())
        waiting = self.run_keyword("ps", "protect", variables=variables)
        self.assertIn("rerun", waiting)
        self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
        self.assertEqual(attempt_path.read_text(), failed_attempt)
        self.assertEqual(self.server.paths[RELEASE_PATH], checks)

    def test_malformed_or_wrong_workflow_bundles_cannot_reach_the_installer(self):
        self.server.release_tag = "v1.1.0"
        offered = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offered, offered["items"][0])
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\n: > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        for body in (b"<html>unexpected gateway response</html>", workflow_bundle("1.1.0")[:-20],
                     workflow_bundle("1.1.0", info=b""),
                     workflow_bundle("1.1.0", info=b"not a property list"),
                     workflow_bundle("1.1.0", bundle_id="test.unrelated.workflow"),
                     workflow_bundle("1.0.0")):
            with self.subTest(body=body[:35]):
                self.server.asset_body = body
                rejected = self.run_object(*download).stdout
                self.assertTrue(rejected.startswith("Error: "), rejected)
                target, _, _ = self.follow_connection(download[0], rejected, download[2],
                                                       include_outputs=True)
                self.assertEqual(self.object_types[target], "alfred.workflow.output.notification")
                self.assertFalse(list(self.cache.glob("*.alfredworkflow")))
                self.assertFalse(opened.exists())

        self.server.asset_body = workflow_bundle("1.1.0")
        accepted = self.run_object(*download).stdout
        self.assertTrue(accepted.startswith("Download Successful: "), accepted)
        bundle = Path(accepted.removeprefix("Download Successful: "))
        installer = self.follow_connection(download[0], accepted, download[2])
        bundle.write_bytes(workflow_bundle("1.0.0"))
        self.assertIn("different workflow version", self.run_object(*installer).stdout)
        self.assertFalse(opened.exists())
        bundle.write_bytes(workflow_bundle("v1.1.0"))
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertTrue(opened.exists())

    def test_aliased_workflow_manifests_never_reach_alfred(self):
        import warnings

        def ambiguous_bundle(alias):
            archive = BytesIO()
            with warnings.catch_warnings():
                # ZipFile warns about the identical-name test case, but still writes it.
                warnings.simplefilter("ignore", UserWarning)
                with ZipFile(archive, "w", ZIP_DEFLATED) as bundle:
                    add_fixture_executable(bundle)
                    for name, version in (("info.plist", "1.1.0"), (alias, "1.0.0")):
                        bundle.writestr(name, plistlib.dumps({
                            "bundleid": "com.victorrodrigues.pinion", "name": "Pinion",
                            "version": version, "objects": [], "connections": {},
                        }))
            return archive.getvalue()

        self.server.release_tag = "v1.1.0"
        self.server.asset_body = workflow_bundle("1.1.0")
        result = self.run_keyword("pcheck")
        download = self.select_result("pcheck", result, result["items"][0])
        approved = self.run_object(*download).stdout
        self.assertTrue(approved.startswith("Download Successful: "), approved)
        bundle = Path(approved.removeprefix("Download Successful: "))
        installer = self.follow_connection(download[0], approved, download[2])
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\nprintf "%s\\n" "$3" > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        for alias in ("INFO.PLIST", "././info.plist", "subdir/../info.plist", "info.plist",
                      "info.pliſt"):
            with self.subTest(alias=alias):
                bundle.write_bytes(ambiguous_bundle(alias))
                self.assertTrue(self.run_object(*installer).stdout.startswith("Error: "))
                self.assertFalse(opened.exists())

                self.server.asset_body = bundle.read_bytes()
                rejected = self.run_object(*download).stdout
                self.assertTrue(rejected.startswith("Error: "), rejected)
                error, _, _ = self.follow_connection(download[0], rejected, download[2],
                                                     include_outputs=True)
                self.assertEqual(self.object_types[error], "alfred.workflow.output.notification")
                self.assertEqual(list(self.cache.glob("*.alfredworkflow")), [bundle])
                # Confirmed invalid assets have to pass verification before they can be offered.
                self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                self.server.asset_body = workflow_bundle("1.1.0")
                self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")

        bundle.write_bytes(workflow_bundle("1.1.0"))
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text().strip(), str(bundle))

    def test_update_rejects_files_used_as_extraction_directories(self):
        self.server.release_tag = "v1.1.0"
        healthy = workflow_bundle("1.1.0")
        self.server.asset_body = healthy
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        approved = self.run_object(*download).stdout
        self.assertTrue(approved.startswith("Download Successful: "), approved)
        bundle = Path(approved.removeprefix("Download Successful: "))
        installer = self.follow_connection(download[0], approved, download[2])
        opened = self.workflow / "opened"
        open_program = self.workflow / "open"
        open_program.write_text('#!/bin/sh\nprintf "%s" "$3" > opened\n')
        open_program.chmod(0o700)

        for child in ("INFO.PLIST/child", "PiNiOn/resource"):
            for manifest_first in (False, True):
                with self.subTest(child=child, manifest_first=manifest_first):
                    conflicting = BytesIO()
                    with ZipFile(conflicting, "w", ZIP_DEFLATED) as archive:
                        if not manifest_first:
                            archive.writestr(child, "a file below a file")
                        with ZipFile(BytesIO(healthy)) as base:
                            for entry in base.infolist():
                                archive.writestr(entry, base.read(entry))
                        if manifest_first:
                            archive.writestr(child, "a file below a file")

                    bundle.write_bytes(conflicting.getvalue())
                    rejection = self.run_object(*installer).stdout
                    self.assertIn("used as a directory", rejection)
                    self.assertFalse(opened.exists())

                    self.server.asset_body = conflicting.getvalue()
                    rejection = self.run_object(*download).stdout
                    self.assertTrue(rejection.startswith("Error: "), rejection)
                    notification, _, _ = self.follow_connection(
                        download[0], rejection, download[2], include_outputs=True)
                    self.assertEqual(self.object_types[notification], "alfred.workflow.output.notification")
                    self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                    self.server.asset_body = healthy
                    self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")

        bundle.write_bytes(healthy)
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text(), str(bundle))

    def test_update_requires_a_regular_executable_for_pinion(self):
        manifest = plistlib.loads((REPO / "res/workflow/info.plist").read_bytes())
        manifest["version"] = "1.1.0"
        info = plistlib.dumps(manifest)
        healthy = workflow_bundle("1.1.0", info=info)
        missing = workflow_bundle("1.1.0", info=info, executable=False)
        self.server.release_tag = "v1.1.0"
        self.server.asset_body = healthy
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        approved = self.run_object(*download).stdout
        self.assertTrue(approved.startswith("Download Successful: "), approved)
        bundle = Path(approved.removeprefix("Download Successful: "))
        installer = self.follow_connection(download[0], approved, download[2])
        opened = self.workflow / "opened"
        open_program = self.workflow / "open"
        open_program.write_text('#!/bin/sh\nprintf "%s" "$3" > opened\n')
        open_program.chmod(0o700)

        for kind in ("missing", "not executable", "empty", "directory"):
            with self.subTest(kind=kind):
                invalid = BytesIO(missing)
                if kind != "missing":
                    with ZipFile(invalid, "a", ZIP_DEFLATED) as archive:
                        entry = ZipInfo("pinion/" if kind == "directory" else "pinion")
                        entry.create_system = 3
                        mode = {"not executable": 0o100644, "empty": 0o100755,
                                "directory": 0o40755}[kind]
                        entry.external_attr = mode << 16
                        body = b"#!/bin/sh\nexit 0\n" if kind == "not executable" else b""
                        archive.writestr(entry, body)
                bundle.write_bytes(invalid.getvalue())
                rejection = self.run_object(*installer).stdout
                self.assertIn("does not contain a valid pinion executable", rejection)
                self.assertFalse(opened.exists())

                self.server.asset_body = invalid.getvalue()
                rejection = self.run_object(*download).stdout
                self.assertTrue(rejection.startswith("Error: "), rejection)
                notification, _, _ = self.follow_connection(
                    download[0], rejection, download[2], include_outputs=True)
                self.assertEqual(self.object_types[notification], "alfred.workflow.output.notification")
                self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                self.server.asset_body = healthy
                self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")

        bundle.write_bytes(healthy)
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text(), str(bundle))

    def test_first_update_check_uses_the_version_accepted_by_release_validation(self):
        bundle = self.workflow / "Pinion.alfredworkflow"
        bundle.write_bytes(workflow_bundle("v1.1.0"))
        self.run_command(["./pinion", "verify-workflow-release", "--tag=v1.1.0",
                          f"--bundle={bundle}"])
        self.env["alfred_workflow_version"] = "v1.1.0"
        self.server.release_tag = "v1.1.0"
        self.updater_file.unlink()
        (self.cache / "last_check_status_async.json").unlink()
        first = self.run_keyword("pcheck")
        self.assertFalse(first["items"][0]["valid"])
        self.assertEqual(first["items"][0]["title"], "No newer version of Pinion is available.")
        self.assertEqual(self.server.paths[RELEASE_PATH], 1)

        # A saved older version cannot override the installed workflow's new version.
        prior = json.loads(self.updater_file.read_text())
        prior["current_version"] = "1.0.0"
        self.updater_file.write_text(json.dumps(prior))
        warm = self.run_keyword("pcheck")
        self.assertEqual(warm["items"][0]["title"], "No newer version of Pinion is available.")
        self.server.release_tag = "v1.2.0"
        self.server.asset_body = workflow_bundle("v1.2.0")
        updated = self.run_keyword("pcheck")
        self.assertEqual(updated["items"][0]["arg"], "1.2.0")
        self.assertIn("1.1.0", updated["items"][0]["subtitle"])
        download = self.select_result("pcheck", updated, updated["items"][0])
        downloaded = self.run_object(*download).stdout
        self.assertTrue(downloaded.startswith("Download Successful: "), downloaded)
        opened = self.workflow / "opened"
        open_program = self.workflow / "open"
        open_program.write_text('#!/bin/sh\nprintf "%s" "$3" > opened\n')
        open_program.chmod(0o700)
        installer = self.follow_connection(download[0], downloaded, download[2])
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text(), downloaded.removeprefix("Download Successful: "))

    def test_late_download_failure_keeps_a_newer_verified_asset(self):
        from unittest.mock import patch

        self.configure()
        self.server.release_tag = "v1.1.0"
        offer = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offer, offer["items"][0])
        started = Event()
        resume = Event()
        original_send_error = MockHandler.send_error
        self.server.status[ASSET_PATH] = 404

        def hold_first_asset_404(handler, code, *args, **kwargs):
            if code == 404 and urlparse(handler.path).path == ASSET_PATH and not started.is_set():
                started.set()
                resume.wait(10)
            return original_send_error(handler, code, *args, **kwargs)

        cache = self.cache / "last_check_status_async.json"
        with patch.object(MockHandler, "send_error", hold_first_asset_404):
            with subprocess.Popen(
                ["bash", "-c", self.objects[download[0]]["script"], download[0], download[1]],
                cwd=self.workflow, env={**self.env, **download[2]}, stdout=subprocess.PIPE,
                text=True,
            ) as late:
                try:
                    self.assertTrue(started.wait(3))
                    self.assertIn("Run pcheck again", self.run_object(*download).stdout)
                    self.assertIn("failed_asset", json.loads(cache.read_text()))
                    del self.server.status[ASSET_PATH]
                    self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
                    stamp = json.loads(cache.read_text())["asset_verified_at"]
                    # A subsequent metadata-only check must retain the proof.
                    self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
                    self.assertEqual(json.loads(cache.read_text())["asset_verified_at"], stamp)
                    self.assertEqual(self.server.paths[ASSET_PATH], 3)
                finally:
                    resume.set()
                self.assertIn("Run pcheck again", late.communicate(timeout=5)[0])
        self.assertEqual(json.loads(cache.read_text())["asset_verified_at"], stamp)

        # A later failure still clears the offer even after an intervening release check.
        self.server.status[ASSET_PATH] = 404
        self.assertIn("Run pcheck again", self.run_object(*download).stdout)
        self.assertIn("failed_asset", json.loads(cache.read_text()))

        # A newer successful selected download is also evidence that the bytes work.
        started.clear()
        resume.clear()
        del self.server.status[ASSET_PATH]
        self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
        self.server.status[ASSET_PATH] = 404
        with patch.object(MockHandler, "send_error", hold_first_asset_404):
            with subprocess.Popen(
                ["bash", "-c", self.objects[download[0]]["script"], download[0], download[1]],
                cwd=self.workflow, env={**self.env, **download[2]}, stdout=subprocess.PIPE,
                text=True,
            ) as late:
                try:
                    self.assertTrue(started.wait(3))
                    del self.server.status[ASSET_PATH]
                    newer = self.run_object(*download).stdout
                    self.assertTrue(newer.startswith("Download Successful: "), newer)
                    newer_stamp = json.loads(cache.read_text())["asset_verified_at"]
                    self.assertGreater(newer_stamp, stamp)
                finally:
                    resume.set()
                self.assertIn("Run pcheck again", late.communicate(timeout=5)[0])
        self.assertEqual(json.loads(cache.read_text())["asset_verified_at"], newer_stamp)

    def test_same_asset_stays_hidden_until_its_full_bundle_is_installable(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        offered = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offered, offered["items"][0])
        self.server.status[ASSET_PATH] = 404
        self.assertIn("Run pcheck again", self.run_object(*download).stdout)
        del self.server.status[ASSET_PATH]
        result_path = self.cache / "last_check_status_async.json"
        waiting = self.run_keyword("ps", "protect")
        for body in (workflow_bundle("1.0.0"), workflow_bundle("1.1.0")[:-20],
                     workflow_bundle("1.1.0", bundle_id="test.unrelated.workflow")):
            with self.subTest(body=body[:35]):
                self.server.asset_body = body
                self.assertFalse(self.run_keyword("pcheck")["items"][0]["valid"])
                waiting = self.run_keyword("ps", "protect", variables=waiting["variables"])
                self.assertEqual([row["title"] for row in waiting["items"]], ["Dashboard"])
                self.assertIn("rerun", waiting)
                self.assertTrue(json.loads(result_path.read_text())["failed_asset"].endswith(ASSET_PATH))

        self.server.asset_body = workflow_bundle("1.1.0")
        recovered = self.run_keyword("pcheck")
        download = self.select_result("pcheck", recovered, recovered["items"][0])
        approved = self.run_object(*download).stdout
        self.assertTrue(approved.startswith("Download Successful: "), approved)
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\nprintf "%s\\n" "$3" > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        installer = self.follow_connection(download[0], approved, download[2])
        self.assertIn("Opening Alfred to install", self.run_object(*installer).stdout)
        self.assertEqual(opened.read_text().strip(), approved.removeprefix("Download Successful: "))

    def test_pcheck_reuses_background_check_while_recovering_a_failed_asset(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"
        offered = self.run_keyword("pcheck")
        download = self.select_result("pcheck", offered, offered["items"][0])
        self.server.status[ASSET_PATH] = 404
        self.assertIn("Run pcheck again", self.run_object(*download).stdout)
        del self.server.status[ASSET_PATH]
        metadata_requests = self.server.paths[RELEASE_PATH]
        asset_requests = self.server.paths[ASSET_PATH]
        release_path = self.cache / "last_check_status_async.json"
        release = json.loads(release_path.read_text())
        release["retry_after"] = OLDER
        release_path.write_text(json.dumps(release))
        (self.cache / "workflow-update-attempt.json").write_text(json.dumps(OLDER))
        self.server.block_asset = True
        self.server.asset_started.clear()
        self.server.resume_asset.clear()
        try:
            self.assertIn("rerun", self.run_keyword("ps", "protect"))
            self.assertTrue(self.server.asset_started.wait(3))
            with subprocess.Popen(
                ["bash", "-c", self.keyword_script("pcheck", "")],
                cwd=self.workflow, env={**self.env, "RUST_LOG": "pinion=debug"},
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            ) as check:
                try:
                    deadline = time.monotonic() + 3
                    error_log = bytearray()
                    while time.monotonic() < deadline:
                        ready, _, _ = select.select([check.stderr], [], [], .1)
                        if ready:
                            chunk = os.read(check.stderr.fileno(), 4096)
                            if not chunk:
                                self.fail("pcheck exited while asset verification held the lock")
                            error_log.extend(chunk)
                            if b"Waiting for the active workflow update check" in error_log:
                                break
                    else:
                        self.fail("pcheck did not reach the held workflow update lock")
                finally:
                    self.server.resume_asset.set()
                output, _ = check.communicate(timeout=5)
                self.assertEqual(check.returncode, 0)
        finally:
            self.server.resume_asset.set()
            self.server.block_asset = False
            self.wait_for_update_check()
        self.assertEqual(json.loads(output)["items"][0]["arg"], "1.1.0")
        self.assertEqual(self.server.paths[RELEASE_PATH], metadata_requests + 1)
        self.assertEqual(self.server.paths[ASSET_PATH], asset_requests + 1)

    def test_direct_update_download_checks_bundle_identity_and_keeps_original_path(self):
        self.server.release_tag = "v1.1.0"
        self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
        bundle = self.cache / "latest_release_Pinion.alfredworkflow"
        for body in (workflow_bundle("1.0.0"),
                     workflow_bundle("1.1.0", bundle_id="test.unrelated.workflow")):
            self.server.asset_body = body
            rejected = self.run_command(["./pinion", "self", "-d"], check=False)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertTrue(rejected.stdout.startswith("Error: "), rejected.stdout)
            self.assertFalse(bundle.exists())

        self.server.asset_body = workflow_bundle("1.1.0")
        accepted = self.run_command(["./pinion", "self", "-d"])
        self.assertEqual(accepted.stdout, f"Download Successful: {bundle}")
        self.assertEqual(bundle.read_bytes(), self.server.asset_body)

    def test_update_offers_disable_existing_bookmark_modifier_actions(self):
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.show_browser_page(url)
        tags = self.run_keyword("pt", "protect")
        tag = next(row for row in tags["items"] if row.get("arg") == "protect")
        matches, query, context = self.select_result("pt", tags, tag)

        self.server.release_tag = "v1.1.0"
        self.run_keyword("pcheck")
        key_names = {262144: "ctrl", 524288: "alt", 786432: "alt+ctrl",
                     1048576: "cmd", 1572864: "cmd+alt"}
        for source, result in (("p", self.run_keyword("p")),
                               ("ps", self.run_keyword("ps", "protect")),
                               (matches, self.run_filter(matches, query, variables=context))):
            with self.subTest(source=source):
                offer = next(row for row in result["items"] if row.get("arg") == "1.1.0")
                modifiers = {edge["modifiers"] for edge in self.connections[self.uid(source)]} - {0}
                self.assertTrue(modifiers)
                for keys in modifiers:
                    self.assertFalse(offer["mods"][key_names[keys]]["valid"])
                    with self.assertRaises(AssertionError):
                        self.select_result(source, result, offer, modifiers=keys)
                download = self.select_result(source, result, offer)
                self.assertTrue(self.run_object(*download).stdout.startswith("Download Successful: "))
        self.assertEqual(self.server.paths[ASSET_PATH], 3)
        self.assertEqual(self.server.paths["/v1/posts/add"], 0)

    def test_update_offer_in_tag_results_routes_to_the_installer(self):
        url = "https://example.test/dashboard"
        self.server.pins = [bookmark(url, "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        tags = self.run_keyword("pt", "protect")
        selected = next(item for item in tags["items"] if item.get("arg") == "protect")
        matches, query, context = self.select_result("pt", tags, selected)

        self.server.release_tag = "v1.1.0"
        self.assertEqual(self.run_keyword("pcheck")["items"][0]["arg"], "1.1.0")
        result = self.run_filter(matches, query, variables=context)
        bookmark_result = next(item for item in result["items"] if item.get("arg") == url)
        self.open_bookmark_result(matches, result, bookmark_result)

        offer = next(item for item in result["items"] if item.get("arg") == "1.1.0")
        download = self.select_result(matches, result, offer)
        output = self.run_object(*download).stdout
        self.assertTrue(output.startswith("Download Successful: "), output)
        self.assertEqual(self.server.paths[ASSET_PATH], 1)
        install = self.follow_connection(download[0], output, download[2])
        opened = self.workflow / "opened"
        open_command = self.workflow / "open"
        open_command.write_text('#!/bin/sh\nprintf "%s\\n" "$3" > "$PINION_OPEN_LOG"\n')
        open_command.chmod(0o700)
        self.env["PINION_OPEN_LOG"] = str(opened)
        self.assertIn("Opening Alfred to install", self.run_object(*install).stdout)
        self.assertEqual(opened.read_text().strip(), output.removeprefix("Download Successful: "))

    def test_cached_search_returns_while_github_check_is_inflight(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        state = json.loads(self.updater_file.read_text())
        state["last_check"] = OLDER
        self.updater_file.write_text(json.dumps(state))
        attempt = self.cache / "workflow-update-attempt.json"
        self.server.status[RELEASE_PATH] = 503
        self.server.block_release = True

        try:
            started = self.run_keyword("ps", "protect", timeout=2)
            self.assertEqual([item["title"] for item in started["items"]], ["Dashboard"])
            self.assertEqual(started["rerun"], 1)
            self.assertTrue(self.server.release_started.wait(3))
            self.assertEqual(self.search(timeout=2), ["Dashboard"])
            self.assertEqual(self.server.paths[RELEASE_PATH], 1)
        finally:
            self.server.resume_release.set()
        self.wait_for_update_check()
        self.assertEqual(self.search(timeout=2), ["Dashboard"])
        self.assertEqual(self.server.paths[RELEASE_PATH], 1)
        self.assertTrue(attempt.exists())

        self.server.status.clear()
        self.server.release_tag = "v1.1.0"
        # Alfred must still ask for reruns after the launch grace, including
        # when GitHub recovers before the five-minute retry interval expires.
        within_cooldown = datetime.now(timezone.utc) - timedelta(seconds=3)
        attempt.write_text(json.dumps(within_cooldown.isoformat()))
        waiting = self.run_keyword("ps", "protect", timeout=2, variables=started["variables"])
        self.assertEqual([item["title"] for item in waiting["items"]], ["Dashboard"])
        self.assertEqual(waiting["rerun"], 5)
        self.assertEqual(self.server.paths[RELEASE_PATH], 1)

        self.server.release_started.clear()
        self.server.resume_release.clear()
        attempt.write_text(json.dumps(OLDER))
        try:
            retry = self.run_keyword("ps", "protect", timeout=2, variables=waiting["variables"])
            self.assertEqual([item["title"] for item in retry["items"]], ["Dashboard"])
            self.assertEqual(retry["rerun"], 1)
            self.assertTrue(self.server.release_started.wait(3))
        finally:
            self.server.resume_release.set()
        published = self.cache / "last_check_status_async.json"
        deadline = time.monotonic() + 5
        while not published.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertTrue(published.exists(), "The detached GitHub worker did not publish a release")
        self.wait_for_update_check()
        self.assertEqual(json.loads(published.read_text())["version"], "1.1.0")
        self.assertEqual(self.server.paths[RELEASE_PATH], 2)
        recovered = self.run_keyword("ps", "protect", variables=retry["variables"])
        items = recovered["items"]
        self.assertEqual(items[0]["title"], "Dashboard")
        self.assertEqual(items[1]["variables"]["workflow_update_ready"], "1")
        self.assertNotIn("rerun", recovered)

        state = json.loads(self.updater_file.read_text())
        state["last_check"] = OLDER
        self.updater_file.write_text(json.dumps(state))
        self.server.status[RELEASE_PATH] = 503
        self.run_command(["./pinion", "self", "-d"])
        self.assertEqual(self.server.paths[ASSET_PATH], 1)
        self.assertEqual(self.server.paths[RELEASE_PATH], 2)

    def test_pcheck_reuses_a_check_that_finishes_while_it_waits(self):
        self.server.pins = [bookmark("https://example.test/dashboard", "Dashboard", "protect", OLDER)]
        self.configure()
        self.assertEqual(self.search(), ["Dashboard"])
        self.server.release_tag = "v1.1.0"

        for status in (200, 503, 404, "metadata"):
            with self.subTest(status=status):
                state = json.loads(self.updater_file.read_text())
                state["last_check"] = OLDER
                self.updater_file.write_text(json.dumps(state))
                (self.cache / "workflow-update-attempt.json").write_text(json.dumps(OLDER))
                self.server.paths.clear()
                self.server.release_has_workflow = status != "metadata"
                if status in (200, "metadata"):
                    self.server.status.pop(RELEASE_PATH, None)
                else:
                    self.server.status[RELEASE_PATH] = status
                self.server.block_release = True
                self.server.release_started.clear()
                self.server.resume_release.clear()
                try:
                    self.run_keyword("ps", "protect")
                    self.assertTrue(self.server.release_started.wait(3))
                    with subprocess.Popen(
                        ["bash", "-c", self.keyword_script("pcheck", "")],
                        cwd=self.workflow, env={**self.env, "RUST_LOG": "pinion=debug"},
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    ) as check:
                        try:
                            deadline = time.monotonic() + 3
                            error_log = bytearray()
                            while time.monotonic() < deadline:
                                ready, _, _ = select.select([check.stderr], [], [], .1)
                                if ready:
                                    chunk = os.read(check.stderr.fileno(), 4096)
                                    if not chunk:
                                        self.fail("pcheck exited while the workflow update lock was held")
                                    error_log.extend(chunk)
                                    if b"Waiting for the active workflow update check" in error_log:
                                        break
                            else:
                                self.fail("pcheck did not reach the held workflow update lock")
                        finally:
                            self.server.resume_release.set()
                        output, _ = check.communicate(timeout=5)
                        self.assertEqual(check.returncode, 0)
                finally:
                    self.server.resume_release.set()
                    self.server.block_release = False
                    self.wait_for_update_check()

                row = json.loads(output)["items"][0]
                if status == 200:
                    self.assertEqual(row["arg"], "1.1.0")
                elif status in (404, "metadata"):
                    self.assertEqual(row["title"], "No newer version of Pinion is available.")
                    self.assertFalse(row["valid"])
                else:
                    self.assertEqual(row["title"], "Couldn't check for Pinion updates.")
                    self.assertEqual(
                        json.loads((self.cache / "last_check_status_async.json").read_text())["version"],
                        "1.1.0",
                    )
                self.assertEqual(self.server.paths[RELEASE_PATH], 2 if status == 503 else 1)

    def test_download_prunes_old_superseded_workflow_bundles(self):
        download_uid = "A2DA0FE3-D7E7-47E0-9383-B42E0EFDF202"
        install_uid = "221EA45C-D3F1-4FD8-9143-CD10110D5BFC"
        self.server.release_tag = "v1.1.0"
        first_offer = self.run_keyword("pcheck")["items"][0]
        first_download = self.run_object(download_uid, first_offer["arg"]).stdout.strip()
        first_path = Path(first_download.removeprefix("Download Successful: "))
        self.assertTrue(first_path.is_file())

        self.server.release_tag = "v1.2.0"
        next_offer = self.run_keyword("pcheck")["items"][0]
        next_download = self.run_object(download_uid, next_offer["arg"]).stdout.strip()
        next_path = Path(next_download.removeprefix("Download Successful: "))
        self.assertTrue(first_path.is_file(), "A recent earlier selection is still available to install")
        self.assertTrue(next_path.is_file())

        old_time = time.time() - 2 * 24 * 60 * 60
        os.utime(first_path, (old_time, old_time))
        unversioned = self.cache / "latest_release_Pinion.alfredworkflow"
        newer = self.cache / "latest_release_Pinion_v9.0.0.alfredworkflow"
        unrelated = self.cache / "latest_release_Other_v1.0.0.alfredworkflow"
        for path in (unversioned, newer, unrelated):
            path.write_text("fixture")
            os.utime(path, (old_time, old_time))

        self.server.status[ASSET_PATH] = 503
        self.assertIn("Error:", self.run_object(download_uid, next_offer["arg"]).stdout)
        self.assertTrue(first_path.is_file(), "A failed download leaves existing bundles intact")
        self.server.status.clear()
        repeated = self.run_object(download_uid, next_offer["arg"]).stdout.strip()
        self.assertTrue(repeated.startswith("Download Successful: "), repeated)
        repeated_path = Path(repeated.removeprefix("Download Successful: "))
        self.assertNotEqual(repeated_path, next_path)
        self.assertFalse(first_path.exists())
        self.assertTrue(all(path.is_file() for path in
                            (repeated_path, next_path, unversioned, newer, unrelated)))

        open_script = self.workflow / "open"
        open_script.write_text('#!/bin/sh\ntest -f "$3"\n')
        open_script.chmod(0o700)
        self.assertIn("Opening Alfred to install", self.run_object(
            install_uid, next_download, next_offer["variables"]
        ).stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
