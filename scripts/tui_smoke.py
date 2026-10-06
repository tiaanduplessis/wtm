#!/usr/bin/env python3
"""Exercise the real terminal UI using temporary Git repositories.

Run: uv run --with pyte scripts/tui_smoke.py target/release/wtm
"""

import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

import pyte


def git(repo, *args):
    result = subprocess.run(
        ["git", "-C", str(repo), *map(str, args)],
        capture_output=True,
        check=True,
        env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"},
    )
    return result.stdout.decode().strip()


def initialize(repo):
    repo.mkdir(parents=True)
    git(repo, "init", "-b", "main")
    git(repo, "config", "user.name", "Terminal Fixture")
    git(repo, "config", "user.email", "fixture@example.invalid")
    git(repo, "config", "commit.gpgsign", "false")
    (repo / "file").write_text("initial\n")
    git(repo, "add", "file")
    git(repo, "commit", "-m", "initial")


class Session:
    def __init__(self, binary, root, *args):
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.state = root / f"terminal-state-{id(self)}"
        self.screen = pyte.Screen(120, 32)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()
        self.mouse_enabled = "--no-mouse" not in args
        self.resize(120, 32, notify=False)

        def attach_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [sys.executable, "-c",
             "import pathlib, subprocess, sys, termios; "
             "code = subprocess.call(sys.argv[2:]); "
             "pathlib.Path(sys.argv[1]).write_text(repr(termios.tcgetattr(0))); "
             "sys.exit(code)",
             str(self.state), str(binary), *args, str(root)],
            stdin=self.slave,
            stderr=self.slave,
            stdout=subprocess.PIPE,
            env={**os.environ, "TERM": "xterm-256color"},
            preexec_fn=attach_terminal,
        )

    def resize(self, width, height, notify=True):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.screen.resize(height, width)
        if notify:
            os.killpg(self.process.pid, signal.SIGWINCH)

    def text(self):
        return "\n".join(self.screen.display)

    def pump(self):
        if select.select([self.master], [], [], 0.05)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return False
            if not data:
                return False
            self.raw.extend(data)
            self.stream.feed(data)
            if b"\x1b[6n" in data:
                self.send("\x1b[1;1R")
            return True
        return False

    def wait(self, predicate, label, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            if predicate():
                return
            if self.process.poll() is not None:
                if predicate():
                    return
                raise AssertionError(f"UI exited before {label}: {self.text()}")
        raise AssertionError(f"Timed out waiting for {label}:\n{self.text()}")

    def contains(self, text):
        self.wait(lambda: text in self.text(), text)

    def idle(self):
        self.wait(lambda: "SCANNING |" not in self.text()
                  and "Discovering nested repositories:" not in self.text()
                  and "Refreshing known repositories:" not in self.text(), "completed scan")

    def send(self, text):
        os.write(self.master, text.encode())

    def locate(self, text):
        self.contains(text)
        for y, line in enumerate(self.screen.display):
            x = line.find(text)
            if x >= 0:
                return x, y
        raise AssertionError(text)

    def mouse(self, button, x, y):
        self.send(f"\x1b[<{button};{x + 1};{y + 1}M")
        if button < 64:
            self.send(f"\x1b[<{button};{x + 1};{y + 1}m")

    def click(self, text, button=0):
        x, y = self.locate(text)
        self.mouse(button, x, y)

    def paste(self, text):
        self.send("\x1b[200~" + text + "\x1b[201~")

    def search(self, query):
        self.send("c/" + query + "\r")
        self.contains("/" + query)

    def cancel(self, title):
        self.send("\x1b")
        self.wait(lambda: title not in self.text(), "cancel " + title)

    def exit(self, key="q"):
        self.send(key)
        self.wait(lambda: self.process.poll() is not None, "normal exit")
        while self.pump():
            pass
        output = self.process.stdout.read()
        assert self.process.returncode == 0, self.text()
        assert self.state.read_text() == repr(self.original), "terminal mode was not restored"
        self.state.unlink()
        assert b"\x1b[?1049l" in self.raw, "alternate screen was not restored"
        assert (b"\x1b[?1006h" in self.raw) == self.mouse_enabled, "mouse capture preference was ignored"
        assert b"\x1b[?1006l" in self.raw, "mouse capture was not disabled"
        assert b"\x1b[?2004l" in self.raw, "bracketed paste was not disabled"
        os.close(self.master)
        os.close(self.slave)
        return output

    def abort(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=5)
        os.close(self.master)
        os.close(self.slave)


def run(binary):
    with tempfile.TemporaryDirectory(prefix="wtm-terminal-") as temporary:
        root = Path(temporary).resolve()
        owner = root / "umbrella"
        initialize(owner)
        initialize(owner / "services" / "api")
        git(owner, "remote", "add", "origin", "git@github.com:example/project.git")
        feature = root / ".worktrees" / "feature"
        old = root / ".worktrees" / "cleanup-topic"
        git(owner, "worktree", "add", "-b", "feature", feature)
        git(owner, "worktree", "add", "-b", "cleanup-topic", old)
        (feature / "local").write_text("uncommitted\n")

        ui = Session(binary, root)
        try:
            ui.contains("2 repositories | 4/4 worktrees")
            ui.idle()
            for heading in ("Repository", "Branch", "Commit", "Updated", "Changes"):
                assert heading in ui.text(), ui.text()
            ui.search("feature")
            ui.contains("1/4 worktrees")
            ui.contains("untracked")
            ui.send("x")
            ui.contains("Removal blocked")
            assert feature.exists()
            ui.send("i")
            ui.contains("Worktree details")
            ui.contains("github.com/example/project/commit/")
            ui.cancel("Worktree details")
            ui.resize(48, 16)
            ui.contains("feature")
            ui.resize(20, 8)
            ui.contains("Enlarge terminal")
            ui.send("xREMOVE\r")
            assert feature.exists()
            ui.resize(120, 32)
            ui.contains("Repository")
            ui.exit("\x03")
        except BaseException:
            ui.abort()
            raise

        ui = Session(binary, root)
        try:
            ui.contains("4/4 worktrees")
            ui.idle()
            ui.search("cleanup-topic")
            ui.contains("1/4 worktrees")
            ui.send("l")
            ui.contains("Lock reason")
            ui.send("retain\r")
            ui.contains("Worktree locked.")
            ui.send("l")
            ui.contains("Unlock worktree")
            ui.send("UNLOCK\r")
            ui.contains("Worktree unlocked.")
            ui.send("m")
            ui.contains("Target path")
            moved = root / "moved"
            ui.send(str(moved) + "\r")
            ui.contains("Move worktree")
            ui.send("MOVE\r")
            ui.wait(lambda: moved.exists() and not old.exists(), "move")
            ui.contains("Moved worktree to")
            ui.search("moved")
            ui.send("x")
            ui.contains("Remove worktrees")
            ui.cancel("Remove worktrees")
            assert moved.exists(), "cancellation removed a worktree"
            ui.send("xREMOVE\r")
            ui.wait(lambda: not moved.exists(), "confirmed removal")
            ui.contains("Removed 1 worktree(s)")
            ui.send("c")
            ui.contains("3/3 worktrees")
            ui.send("a")
            ui.contains("Create new branch")
            created = root / "created"
            ui.send(str(created) + "\rtui-topic\r\rmain\r")
            ui.wait(created.exists, "creation")
            ui.contains("Added")
            ui.search("created")
            ui.contains("1/4 worktrees")
            ui.send(" ")
            ui.contains("1 marked")
            second = root / "created-second"
            git(owner, "worktree", "add", "-b", "bulk-topic", second)
            ui.send("r")
            ui.contains("2/5 worktrees")
            ui.idle()
            ui.send("j ")
            ui.contains("2 marked")
            ui.send("xREMOVE\r")
            ui.wait(lambda: not created.exists() and not second.exists(), "bulk removal")
            ui.contains("Removed 2 worktree(s)")
            ui.send("c")
            ui.contains("3/3 worktrees")
            ui.send("p")
            ui.contains("Prune stale registrations")
            ui.cancel("Prune stale registrations")
            ui.exit()
        except BaseException:
            ui.abort()
            raise

        ui = Session(binary, root, "cd", "--root")
        try:
            ui.contains("3/3 worktrees")
            ui.idle()
            ui.search("feature")
            ui.contains("1/3 worktrees")
            ui.click("[Choose]")
            assert ui.exit("") == (str(feature) + "\n").encode()
        except BaseException:
            ui.abort()
            raise
        ui = Session(binary, root)
        try:
            ui.contains("3/3 worktrees")
            ui.idle()
            ui.send("P")
            ui.contains("Last completed scan performance")
            ui.contains("first_result_ms")
            ui.cancel("Last completed scan performance")
            added_owner = root / "new" / "nested-owner"
            initialize(added_owner)
            ui.send("r")
            ui.contains("Refreshing known repositories:")
            ui.idle()
            ui.contains("3/3 worktrees")
            ui.send("P")
            ui.contains("Last completed scan performance")
            ui.contains('"commit_cache_hits": 3')
            ui.cancel("Last completed scan performance")
            ui.send("R")
            ui.contains("4/4 worktrees")
            ui.idle()
            ui.search("nested-owner")
            ui.contains("1/4 worktrees")
            ui.exit()
        except BaseException:
            ui.abort()
            raise
        ui = Session(binary, root)
        try:
            ui.contains("4/4 worktrees")
            ui.idle()
            ui.click("[Search]")
            ui.paste("feature")
            ui.contains("1/4 worktrees")
            ui.wait(lambda: not ui.screen.cursor.hidden, "search text cursor")
            x, y = ui.locate("[ ]")
            ui.mouse(0, x + 1, y)
            ui.contains("1 marked")
            ui.click("[Remove]")
            ui.contains("Removal blocked")
            assert feature.exists()
            ui.mouse(2, x + 8, y)
            ui.contains("Full recursive discovery")
            ui.click("Clear marks and filters")
            ui.contains("4/4 worktrees")
            ui.contains("0 marked")
            ui.click("Updated")
            ui.contains("sort: recent activity")
            ui.click("Updated")
            ui.contains("recent activity (reverse)")
            ui.click("[Details]")
            ui.contains("Worktree details")
            x, y = ui.locate("Worktree details")
            ui.mouse(65, x + 1, y + 2)
            ui.click("[Close]")
            ui.wait(lambda: "Worktree details" not in ui.text(), "mouse close details")
            ui.click("[Add]")
            ui.contains("Add worktree")
            destination = root / "mouse-created"
            ui.paste(str(destination))
            ui.click("[Next]")
            ui.paste("mouse-topic")
            ui.click("[Next]")
            ui.click("[Next]")
            ui.contains("[Submit]")
            ui.paste("main")
            ui.click("[Submit]")
            ui.wait(destination.exists, "mouse-created worktree")
            ui.contains("Added")
            ui.idle()
            ui.click("[Search]")
            ui.paste("mouse-created")
            ui.contains("1/5 worktrees")
            ui.click("[Remove]")
            ui.contains("Remove worktrees")
            ui.click("[Confirm]")
            assert destination.exists(), "empty mouse confirmation removed worktree"
            ui.paste("REMOVE\n")
            ui.wait(lambda: not ui.screen.cursor.hidden, "confirmation text cursor")
            assert destination.exists(), "paste submitted cleanup"
            ui.click("[Cancel]")
            ui.wait(lambda: "Remove worktrees" not in ui.text(), "mouse cancelled removal")
            ui.click("[Remove]")
            ui.contains("Remove worktrees")
            ui.paste("REMOVE")
            ui.click("[Confirm]")
            ui.wait(lambda: not destination.exists(), "mouse confirmed removal")
            ui.contains("Removed 1 worktree(s)")
            ui.idle()
            ui.resize(30, 14)
            ui.contains("[Actions]")
            ui.click("[Actions]")
            ui.contains("Choose directory")
            ui.click("[Close]")
            ui.wait(lambda: "Choose directory" not in ui.text(), "compact menu close")
            ui.click("[Quit]")
            ui.exit("")
        except BaseException:
            ui.abort()
            raise

        ui = Session(binary, root, "--no-mouse")
        try:
            ui.contains("4/4 worktrees")
            ui.idle()
            ui.exit()
        except BaseException:
            ui.abort()
            raise
        print("PTY checks passed: discovery, metadata, dirty protection, resizing, Ctrl-C, lock/unlock, move, cancelled and confirmed removal, creation, marks, prune preview, selection, incremental refresh, cached metadata, full rescan, profiling, mouse selection/marking/sorting/menus/forms/cleanup, clipboard paste, compact controls, no-mouse mode, terminal restoration.")


if __name__ == "__main__":
    run(Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/wtm").resolve())
