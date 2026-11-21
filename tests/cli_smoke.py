"""Run the real CLI on a pseudo-terminal with an isolated home and no API calls."""

import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import stat
import subprocess
import tempfile
import termios
import time
import unittest


BINARY = Path(os.environ.get("ADAPT_TEST_BINARY", "target/debug/adapt")).resolve()
TIMEOUT = 5


class Terminal:
    def __init__(self, env, cwd, *args):
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            try:
                os.chdir(cwd)
                os.execve(BINARY, [str(BINARY), *args], env)
            except OSError as error:
                os.write(2, str(error).encode())
                os._exit(127)
        self.transcript = bytearray()
        self.cursor = 0

    def close(self):
        try:
            if self.pid is not None:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
        finally:
            os.close(self.master)

    def send(self, text):
        os.write(self.master, text.encode())

    def echo_enabled(self):
        return bool(termios.tcgetattr(self.master)[3] & termios.ECHO)

    def _read(self, timeout):
        if not select.select([self.master], [], [], timeout)[0]:
            return None
        try:
            return os.read(self.master, 8192)
        except OSError as error:
            if error.errno == errno.EIO:
                return b""  # The slave closed.
            raise

    def expect(self, text):
        token = text.encode()
        deadline = time.monotonic() + TIMEOUT
        while (position := self.transcript.find(token, self.cursor)) == -1:
            remaining = deadline - time.monotonic()
            chunk = self._read(remaining) if remaining > 0 else None
            if not chunk:
                raise AssertionError(f"Expected {text!r}. Terminal output: {self.transcript!r}")
            self.transcript.extend(chunk)
        self.cursor = position + len(token)

    def exit_code(self):
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.pid = None
                while chunk := self._read(0):
                    self.transcript.extend(chunk)
                return os.waitstatus_to_exitcode(status)
            if chunk := self._read(0.05):
                self.transcript.extend(chunk)
        raise AssertionError(f"CLI did not exit. Terminal output: {self.transcript!r}")


class CliSmokeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.home = root / "home"
        self.project = root / "project"
        self.home.mkdir()
        self.project.mkdir()
        self.settings = self.home / ".adaption-code" / "settings.json"
        self.env = {
            "HOME": str(self.home),
            "PATH": os.defpath,
            "TERM": "xterm-256color",
            "NO_COLOR": "1",
        }

    def terminal(self, *args, cwd=None):
        terminal = Terminal(self.env, cwd or self.project, *args)
        self.addCleanup(terminal.close)
        return terminal

    def run_piped(self, *args, text=""):
        return subprocess.run(
            [str(BINARY), *args],
            cwd=self.project,
            env=self.env,
            input=text,
            text=True,
            capture_output=True,
            timeout=TIMEOUT,
        )

    def test_first_run_key_is_hidden_saved_privately_and_reloaded(self):
        terminal = self.terminal()
        terminal.expect("Choose [1]: ")
        terminal.send("\n")
        terminal.expect("Choose: ")
        terminal.send("1\n")
        terminal.expect("API key (input hidden, Enter to keep): ")
        self.assertFalse(terminal.echo_enabled())
        terminal.send("fixture-secret-key\n")
        terminal.expect("set (hidden)")
        terminal.expect("Choose: ")
        self.assertTrue(terminal.echo_enabled())
        terminal.send("5\n")
        terminal.expect("Settings saved.")
        terminal.expect("adapt> ")
        terminal.send("/quit\n")
        self.assertEqual(terminal.exit_code(), 0)
        self.assertNotIn(b"fixture-secret-key", terminal.transcript)
        self.assertNotIn(b"\x1b[", terminal.transcript)
        self.assertEqual(stat.S_IMODE(self.settings.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.settings.parent.stat().st_mode), 0o700)
        self.assertEqual(json.loads(self.settings.read_text())["api_key"], "fixture-secret-key")

        reopened = self.terminal()
        reopened.expect("adapt> ")
        reopened.send("/settings\n")
        reopened.expect("set (hidden)")
        reopened.expect("Choose: ")
        reopened.send("0\n")
        reopened.expect("adapt> ")
        reopened.send("exit\n")
        self.assertEqual(reopened.exit_code(), 0)
        self.assertNotIn(b"Adapt setup", reopened.transcript)
        self.assertNotIn(b"fixture-secret-key", reopened.transcript)

    def test_interrupt_at_hidden_prompt_restores_echo_and_saves_nothing(self):
        terminal = self.terminal()
        terminal.expect("Choose [1]: ")
        terminal.send("1\n")
        terminal.expect("Choose: ")
        terminal.send("1\n")
        terminal.expect("API key (input hidden, Enter to keep): ")
        self.assertFalse(terminal.echo_enabled())
        terminal.send("\x03")
        self.assertEqual(terminal.exit_code(), 0)
        self.assertTrue(terminal.echo_enabled())
        self.assertFalse(self.settings.exists())

    def test_cancel_settings_and_interrupt_at_main_prompt(self):
        terminal = self.terminal()
        terminal.expect("Choose [1]: ")
        terminal.send("2\n")
        terminal.expect("adapt> ")
        terminal.send("/settings\n")
        terminal.expect("Choose: ")
        terminal.send("0\n")
        terminal.expect("adapt> ")
        terminal.send("/permissions\n")
        terminal.expect("No remembered approvals.")
        terminal.expect("adapt> ")
        terminal.send("/revoke\n")
        terminal.expect("Remembered approvals cleared.")
        terminal.expect("adapt> ")
        terminal.send("\x03")
        self.assertEqual(terminal.exit_code(), 0)
        self.assertFalse(self.settings.exists())

    def test_git_root_discovery_and_explicit_directory(self):
        (self.project / ".git").mkdir()
        nested = self.project / "src"
        nested.mkdir()
        for args, expected in [((), self.project), (("-C", str(nested)), nested)]:
            with self.subTest(args=args):
                terminal = self.terminal(*args, cwd=nested)
                terminal.expect(f"Workspace: {expected.resolve()}")
                terminal.expect("Choose [1]: ")
                terminal.send("quit\n")
                self.assertEqual(terminal.exit_code(), 0)

    def test_noninteractive_invocations_fail_locally_without_configuration(self):
        cases = [
            ((), "", "use adapt ask"),
            (("models",), "", "Adaption is unconfigured"),
            (("ask", "-"), "List files\n", "Adaption is unconfigured"),
            (("ask", "-"), "x" * (32 * 1024 + 1), "input line is too long"),
            (("ask", "-"), ("x" * 1024 + "\n") * 32, "prompt exceeds 32 KiB"),
            (("--memory", "--no-memory", "ask", "List files"), "", "cannot be used with"),
        ]
        for args, prompt, expected in cases:
            with self.subTest(args=args, prompt_size=len(prompt)):
                result = self.run_piped(*args, text=prompt)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)
                self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
