"""Isolated lane allocation regressions: python -I Scripts/test_lane.py.

Runs the real lane.sh in Git Bash (or Unix Bash), with fake Git/netstat and all paths
inside a disposable fixture. No real registry, worktrees, databases, builds or servers.
Barriers in fake Git hold a creator after prefix selection, rather than relying on sleeps
to make the allocation race happen. Requires only Bash and Python's standard library.
"""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


SCRIPT = Path(__file__).with_name("lane.sh")
HEAD = "a" * 40


def wait_for(path, timeout=30):
    return wait_for_any((path,), timeout)


def wait_for_any(paths, timeout=30):
    deadline = time.monotonic() + timeout
    while not any(path.exists() for path in paths):
        if time.monotonic() >= deadline:
            raise TimeoutError(f"barrier not reached: {paths}")
        time.sleep(0.02)


def fake_git(args):
    """Only implement commands used by new/rollback; unexpected commands fail closed."""
    root = Path(os.environ["LANE_TEST_ROOT"]).resolve()
    refs = root / "refs"
    if args[:1] != ["-C"]:
        raise AssertionError(args)
    args = args[2:]

    def ref_path(ref):
        return refs / ref.removeprefix("refs/heads/")

    def owned_path(value):
        path = Path(value).resolve()
        if not path.is_relative_to(root):
            raise AssertionError(f"path outside fixture: {path}")
        return path

    if args[:1] == ["check-ref-format"]:
        return 0
    if args[:3] == ["show-ref", "--verify", "--quiet"]:
        return 0 if ref_path(args[3]).is_file() else 1
    if args[:2] == ["rev-parse", "--verify"]:
        if args[2] == "missing^{commit}":
            return 70
        print(HEAD)
        return 0
    if args[:2] == ["worktree", "add"]:
        _, _, quiet, wt, flag, branch, base = args
        assert (quiet, flag) == ("-q", "-b")
        wt = owned_path(wt)
        name = wt.name
        (root / f"add.{name}").touch()
        if os.environ.get("LANE_TEST_HOLD") == name:
            wait_for(root / f"release.{name}")
        failure = os.environ.get("LANE_TEST_FAIL")
        if failure == "add":
            return 70
        ref = ref_path(branch)
        ref.parent.mkdir(parents=True, exist_ok=True)
        with ref.open("x") as file:
            file.write(HEAD)
        wt.mkdir()
        (wt / ".git").write_text("isolated fake worktree\n")
        if failure == "partial-add":
            return 70
        return 0
    if args[:2] == ["worktree", "remove"]:
        if os.environ.get("LANE_TEST_FAIL") == "config-remove":
            return 73
        wt = owned_path(args[2])
        shutil.rmtree(wt)
        return 0
    if args[:2] == ["update-ref", "-d"]:
        ref = ref_path(args[2])
        if not ref.is_file() or ref.read_text() != args[3]:
            return 74
        ref.unlink()
        return 0
    raise AssertionError(f"unexpected Git command: {args}")


class AllocationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bash = os.environ.get("LANE_TEST_BASH")
        if not cls.bash and os.name == "nt":
            git = shutil.which("git")
            candidate = Path(git).parent.parent / "bin/bash.exe" if git else None
            cls.bash = str(candidate) if candidate and candidate.exists() else None
        cls.bash = cls.bash or shutil.which("bash")
        if not cls.bash:
            raise unittest.SkipTest("Bash (Git Bash on Windows) is required")

    def setUp(self):
        # Keep scratch space in this checkout and delete it only after our subprocesses exit.
        self.temp = tempfile.TemporaryDirectory(prefix=".lane-lock-test-", dir=SCRIPT.parent.parent)
        self.root = Path(self.temp.name).resolve()
        self.addCleanup(self.temp.cleanup)
        self.processes = []
        self.addCleanup(self.reap)
        self.repo = self.root / "repo"
        (self.repo / "Scripts").mkdir(parents=True)
        shutil.copyfile(SCRIPT, self.repo / "Scripts/lane.sh")
        self.registry = self.root / "registry"
        self.registry.mkdir()
        self.lock = self.registry / ".allocation.lock"
        self.shims = self.root / "shims"
        self.shims.mkdir()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("AAEMU_")}
        self.env.update({
            "AAEMU_LANES": self.registry.as_posix(),
            "AAEMU_WT_ROOT": (self.root / "wt").as_posix(),
            "AAEMU_TARGET_ROOT": (self.root / "target").as_posix(),
            "AAEMU_LIVE_PREFIX": (self.root / "live-").as_posix(),
            "AAEMU_LIVE_TEMPLATE": (self.root / "template").as_posix(),
            "AAEMU_MYSQL_BIN": self.shims.as_posix(),
            "AAEMU_LANE_BASE": "fixture-base",
            "AAEMU_LANE_JOBS": "2",
            "AAEMU_LANE_LOCK_TIMEOUT": "30",
            "LANE_TEST_ROOT": self.root.as_posix(),
            "LANE_TEST_HELPER": Path(__file__).resolve().as_posix(),
            "LANE_TEST_PYTHON": Path(sys.executable).as_posix(),
            "LANE_TEST_SHIMS": self.shims.as_posix(),
            # Even a broken shim setup must not discover the enclosing real repository.
            "GIT_DIR": (self.repo / ".git").as_posix(),
            "BASH_ENV": "",
            "PATH": str(self.shims) + os.pathsep + self.env["PATH"],
        })
        self.shim("git", 'exec "$LANE_TEST_PYTHON" "$LANE_TEST_HELPER" --fake-git "$@"')
        self.shim("netstat", 'printf "  TCP  127.0.0.1:25237  0.0.0.0:0  LISTENING  1\\n"')
        self.shim("mkdir", '''
case "${*: -1}" in
  */.allocation.lock)
    if /usr/bin/mkdir "$@"; then exit 0; fi
    : > "$LANE_TEST_ROOT/blocked.${LANE_TEST_ID:-test}"
    if [ "${LANE_TEST_HOLD_DENIED:-}" = "${LANE_TEST_ID:-test}" ]; then
      start=$SECONDS
      until [ -f "$LANE_TEST_ROOT/retry.$LANE_TEST_ID" ]; do
        [ $((SECONDS - start)) -lt 30 ] || exit 98
        sleep 0.02
      done
    fi
    exit 1 ;;
esac
if [ "${*: -1}" = "$AAEMU_TARGET_ROOT/broken" ] && [ "${LANE_TEST_FAIL:-}" = term-target ]; then
  /usr/bin/mkdir "$@"
  kill -TERM "$PPID"; exit 0
fi
exec /usr/bin/mkdir "$@"
''')
        self.shim("cat", '''
[ -f "$LANE_TEST_ROOT/refs/fix/broken" ] || exec /usr/bin/cat "$@"
case "${LANE_TEST_FAIL:-}" in
  config|config-remove) exit 71 ;;
  term-config) kill -TERM "$PPID"; exit 0 ;;
  kill-config) kill -KILL "$PPID"; exit 0 ;;
esac
exec /usr/bin/cat "$@"
''')
        self.shim("ln", '''
case "${LANE_TEST_FAIL:-}" in
  publish) exit 72 ;;
  publish-moved)
    printf '%040d' 0 > "$LANE_TEST_ROOT/refs/fix/broken"
    exit 72 ;;
  publish-term)
    /usr/bin/ln "$@"
    kill -TERM "$PPID"; exit 0 ;;
esac
exec /usr/bin/ln "$@"
''')
        for command in ("cargo", "powershell", "taskkill", "mysql.exe"):
            self.shim(command, f'echo "unexpected {command}" >> "$LANE_TEST_ROOT/forbidden"; exit 99')

    def shim(self, name, body):
        path = self.shims / name
        path.write_text("#!/usr/bin/env bash\nset -eu\n" + body + "\n", encoding="utf-8", newline="\n")
        path.chmod(0o755)

    def reap(self):
        # Unblock only our fixture children, even when an assertion fails.
        for path in self.root.glob("add.*"):
            (self.root / path.name.replace("add.", "release.", 1)).touch()
        for path in self.root.glob("blocked.*"):
            (self.root / path.name.replace("blocked.", "retry.", 1)).touch()
        for process in self.processes:
            try:
                process.communicate(timeout=40)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=40)
        self.assertFalse((self.root / "forbidden").exists(), "a forbidden tool was invoked")

    def start(self, *args, **env):
        process = subprocess.Popen(
            # Git's bin/bash.exe launcher prepends /mingw64/bin and /usr/bin to PATH.
            # Install shims inside that shell, then exec Bash directly so they stay first.
            [self.bash, "--noprofile", "--norc", "-c",
             'export PATH="$(cd "$LANE_TEST_SHIMS" && pwd):$PATH"; '
             'exec /usr/bin/bash --noprofile --norc "$@"', "lane-test",
             (self.repo / "Scripts/lane.sh").as_posix(), *args],
            cwd=self.repo, env=self.env | env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True,
        )
        self.processes.append(process)
        return process

    def finish(self, process, success=True):
        output, _ = process.communicate(timeout=40)
        if success:
            self.assertEqual(process.returncode, 0, output)
        else:
            self.assertNotEqual(process.returncode, 0, output)
        return output

    def create(self, name, **env):
        return self.start("new", name, **env)

    def entry(self, name):
        return dict(line.split("=", 1) for line in (self.registry / f"{name}.env").read_text().splitlines())

    def assert_rolled_back(self, name):
        for path in (self.root / "wt" / name, self.root / "target" / name,
                     self.root / "refs/fix" / name, self.registry / f"{name}.env", self.lock):
            self.assertFalse(path.exists(), f"rollback left {path}")

    def test_concurrent_creators_get_distinct_prefixes_and_overrides(self):
        # Preserve an existing lane and skip an occupied OS port (25237).
        existing = self.registry / "running.env"
        existing.write_text("PORT_PREFIX=24\nOTHER=unchanged\n", newline="\n")
        first = self.create("first", LANE_TEST_HOLD="first", LANE_TEST_ID="first")
        wait_for(self.root / "add.first")
        second = self.start("new", "second", "--type", "feat", "--base", "explicit-base",
                            LANE_TEST_ID="second", LANE_TEST_HOLD_DENIED="second")
        wait_for_any((self.root / "blocked.second", self.root / "add.second"))
        self.assertFalse((self.root / "add.second").exists(), "second creator entered locked section")
        (self.root / "release.first").touch()
        self.finish(first)
        # Return the failed mkdir only AFTER the owner removed the lock: acquisition must retry
        # rather than treating this normal release race as an inability to create the lock.
        (self.root / "retry.second").touch()
        self.finish(second)
        a, b = self.entry("first"), self.entry("second")
        self.assertEqual(set(b), {"LANE", "BRANCH", "BASE", "WT", "TARGET", "PORT_PREFIX", "LIVE",
                                  "DB_GAME", "DB_LOGIN", "CREATED", "CHECKED"})
        self.assertEqual((a["PORT_PREFIX"], b["PORT_PREFIX"]), ("26", "27"))
        self.assertEqual((a["BASE"], b["BASE"], b["BRANCH"]), ("fixture-base", "explicit-base", "feat/second"))
        self.assertEqual(b["WT"], (self.root / "wt/second").as_posix())
        self.assertEqual(b["TARGET"], (self.root / "target/second").as_posix())
        self.assertEqual(b["LIVE"], (self.root / "live-second").as_posix())
        self.assertEqual(b["DB_GAME"], "lane_second_game")
        self.assertEqual(b["DB_LOGIN"], "lane_second_login")
        self.assertEqual(b["CHECKED"], "")
        config = (self.root / "wt/second/rust/.cargo/config.toml").read_text()
        self.assertIn("jobs = 2", config)
        self.assertIn(f'target-dir = "{b["TARGET"]}"', config)
        self.assertTrue(Path(b["TARGET"]).is_dir())
        self.assertEqual(existing.read_text(), "PORT_PREFIX=24\nOTHER=unchanged\n")
        self.assertFalse(self.lock.exists())

    def test_concurrent_same_name_has_one_owner(self):
        first = self.create("same", LANE_TEST_HOLD="same")
        wait_for(self.root / "add.same")
        second = self.create("same", LANE_TEST_ID="second")
        wait_for(self.root / "blocked.second")
        (self.root / "release.same").touch()
        self.finish(first)
        self.assertIn("lane 'same' exists", self.finish(second, success=False))
        self.assertTrue((self.root / "wt/same/.git").is_file())
        self.assertEqual(self.entry("same")["PORT_PREFIX"], "24")
        self.assertFalse(self.lock.exists())

    def test_existing_registry_path_target_and_branch_are_never_removed(self):
        paths = {
            "entry": self.registry / "entry.env",
            "worktree": self.root / "wt/worktree/keep",
            "target": self.root / "target/target/in-use",
            "branch": self.root / "refs/fix/branch",
        }
        for name, path in paths.items():
            with self.subTest(name=name):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("preserve")
                self.finish(self.create(name), success=False)
                self.assertEqual(path.read_text(), "preserve")
                self.assertFalse((self.root / f"add.{name}").exists())
                self.assertFalse(self.lock.exists())

    def test_failures_roll_back_only_the_failed_creator_and_release_lock(self):
        self.finish(self.create("keeper"))
        before = (self.registry / "keeper.env").read_bytes()
        for failure in ("add", "config", "publish", "term-config"):
            with self.subTest(failure=failure):
                self.finish(self.create("broken", LANE_TEST_FAIL=failure), success=False)
                self.assert_rolled_back("broken")
                self.assertEqual((self.registry / "keeper.env").read_bytes(), before)
                self.assertTrue((self.root / "wt/keeper/.git").exists())
        self.finish(self.create("broken"))
        self.assertEqual(self.entry("broken")["PORT_PREFIX"], "26")

    def test_missing_base_or_exhausted_prefixes_release_lock_without_creating_resources(self):
        self.finish(self.start("new", "broken", "--base", "missing"), success=False)
        self.assert_rolled_back("broken")
        for prefix in range(24, 40):
            (self.registry / f"occupied{prefix}.env").write_text(f"PORT_PREFIX={prefix}\n", newline="\n")
        output = self.finish(self.create("broken"), success=False)
        self.assertIn("no free port block", output)
        self.assert_rolled_back("broken")
        self.assertFalse((self.root / "add.broken").exists())

    def test_partial_git_failure_retains_reservation(self):
        output = self.finish(self.create("broken", LANE_TEST_FAIL="partial-add"), success=False)
        self.assertIn("partial creation retained", output)
        self.assertTrue((self.root / "refs/fix/broken").exists())
        self.assertTrue((self.root / "wt/broken/.git").exists())
        self.assertTrue((self.root / "target/broken").exists())
        self.assertTrue(self.lock.exists())
        self.finish(self.create("other", AAEMU_LANE_LOCK_TIMEOUT="0"), success=False)
        self.assertFalse((self.root / "add.other").exists())

    def test_registry_publication_never_clobbers_an_existing_file_or_directory(self):
        for name, directory in (("fileowner", False), ("dirowner", True)):
            with self.subTest(directory=directory):
                process = self.create(name, LANE_TEST_HOLD=name)
                wait_for(self.root / f"add.{name}")
                entry = self.registry / f"{name}.env"
                if directory:
                    entry.mkdir()
                    marker = entry / "keep"
                else:
                    marker = entry
                marker.write_text("another owner")
                (self.root / f"release.{name}").touch()
                self.finish(process, success=False)
                self.assertEqual(marker.read_text(), "another owner")
                if directory:
                    self.assertEqual(list(entry.iterdir()), [marker])
                for path in (self.root / "wt" / name, self.root / "target" / name,
                             self.root / "refs/fix" / name, self.lock):
                    self.assertFalse(path.exists(), f"rollback left {path}")

    def test_signal_immediately_after_publication_preserves_committed_lane(self):
        self.finish(self.create("broken", LANE_TEST_FAIL="publish-term"), success=False)
        self.assertEqual(self.entry("broken")["PORT_PREFIX"], "24")
        self.assertTrue((self.root / "wt/broken/.git").exists())
        self.assertTrue((self.root / "target/broken").exists())
        self.assertTrue((self.root / "refs/fix/broken").exists())
        self.assertFalse(self.lock.exists())
        self.finish(self.create("other"))
        self.assertEqual(self.entry("other")["PORT_PREFIX"], "26")

    def test_cleanup_failure_does_not_delete_moved_branch(self):
        self.finish(self.create("broken", LANE_TEST_FAIL="publish-moved"), success=False)
        self.assertEqual((self.root / "refs/fix/broken").read_text(), "0" * 40)
        self.assertTrue(self.lock.exists())
        self.assertTrue((self.root / "target/broken").exists())

    def test_failed_worktree_removal_retains_branch_and_lock(self):
        self.finish(self.create("broken", LANE_TEST_FAIL="config-remove"), success=False)
        self.assertTrue((self.root / "wt/broken/.git").exists())
        self.assertEqual((self.root / "refs/fix/broken").read_text(), HEAD)
        self.assertTrue(self.lock.exists())

    def test_hard_interruption_keeps_lock_until_manual_inspection(self):
        self.finish(self.create("broken", LANE_TEST_FAIL="kill-config"), success=False)
        self.assertTrue(self.lock.exists())
        self.finish(self.create("other", AAEMU_LANE_LOCK_TIMEOUT="0"), success=False)
        self.assertTrue((self.root / "wt/broken/.git").exists())
        self.assertTrue((self.root / "refs/fix/broken").exists())
        self.assertFalse((self.root / "add.other").exists())
        journal, = self.lock.glob("owner.*/lane.env")
        self.assertIn("LANE=broken\n", journal.read_text())
        self.assertIn("PORT_PREFIX=24\n", journal.read_text())

    def test_signal_after_target_mkdir_retains_ambiguous_reservation(self):
        output = self.finish(self.create("broken", LANE_TEST_FAIL="term-target"), success=False)
        self.assertIn("partial creation retained", output)
        self.assertTrue((self.root / "target/broken").is_dir())
        self.assertTrue(self.lock.is_dir())
        self.assertFalse((self.root / "add.broken").exists())
        self.finish(self.create("other", AAEMU_LANE_LOCK_TIMEOUT="0"), success=False)
        self.assertFalse((self.root / "add.other").exists())

    def test_stale_empty_or_pid_reused_lock_is_never_reclaimed(self):
        self.lock.mkdir()
        self.finish(self.create("other", AAEMU_LANE_LOCK_TIMEOUT="0"), success=False)
        self.assertEqual(list(self.lock.iterdir()), [])
        for content in ("", f"PID={os.getpid()}\nSTARTED=1900-01-01\n", "PID=999999999\n"):
            with self.subTest(content=content):
                owner = self.lock / "owner.stale"
                owner.write_text(content)
                os.utime(self.lock, (1, 1))
                output = self.finish(self.create("other", AAEMU_LANE_LOCK_TIMEOUT="0"), success=False)
                self.assertIn("never remove a lock by PID or age", output)
                self.assertEqual(owner.read_text(), content)
                self.assertFalse((self.root / "add.other").exists())

    def test_cleanup_never_releases_a_replacement_owners_lock(self):
        process = self.create("broken", LANE_TEST_HOLD="broken", LANE_TEST_FAIL="config")
        wait_for(self.root / "add.broken")
        self.lock.rename(self.registry / "old-lock")
        self.lock.mkdir()
        replacement = self.lock / "owner.replacement"
        replacement.write_text("another creator")
        entry = self.registry / "other.env"
        entry.write_text("PORT_PREFIX=24\n")
        (self.root / "release.broken").touch()
        self.assertIn("ownership lost", self.finish(process, success=False))
        self.assertEqual(replacement.read_text(), "another creator")
        self.assertEqual(entry.read_text(), "PORT_PREFIX=24\n")
        self.assertTrue((self.root / "wt/broken/.git").exists())
        self.assertTrue((self.root / "target/broken").exists())

    def test_waiters_and_destructive_cleanup_cannot_release_an_active_creators_lock(self):
        process = self.create("first", LANE_TEST_HOLD="first")
        wait_for(self.root / "add.first")
        for args in (("new", "second"), ("done", "first", "--force"), ("gc", "--apply")):
            with self.subTest(args=args):
                self.finish(self.start(*args, AAEMU_LANE_LOCK_TIMEOUT="0"), success=False)
                self.assertTrue(self.lock.exists())
                self.assertTrue((self.root / "target/first").is_dir())
        (self.root / "release.first").touch()
        self.finish(process)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--fake-git"]:
        sys.exit(fake_git(sys.argv[2:]))
    unittest.main(verbosity=2)
