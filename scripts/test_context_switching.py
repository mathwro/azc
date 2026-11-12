#!/usr/bin/env python3
"""POSIX process/PTY regressions. Build azc, then run with Python 3."""
import errno
import fcntl
import json
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
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BINARY = REPO_ROOT / "target/debug/azc"
BINARY = Path(os.environ.get("AZC_BIN") or DEFAULT_BINARY)
ROWS = [
    {"id": sid, "name": name, "tenantId": tenant, "isDefault": current}
    for sid, name, tenant, current in [
        ("finance-id", "Finance", "tenant-a", True),
        ("dev-id", "Payments Development", "tenant-a", False),
        ("prod-id", "Payments Production", "tenant-b", False),
        ("archive-id", "Archive Payments", "tenant-c", False),
    ]
]
METADATA = [
    {"tenantId": tenant, "displayName": name}
    for tenant, name in [("tenant-a", "Company Alpha"),
                         ("tenant-b", "Company Beta"),
                         ("tenant-c", "Archive Org")]
]
FAKE = '''import json, os, pathlib, sys, time
args = sys.argv[1:]
with open(os.environ['CALL_LOG'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
if args == ['account', 'list', '--output', 'json', '--only-show-errors']:
    print(os.environ['ACCOUNTS'])
elif args == ['account', 'list', '--output', 'json', '--only-show-errors', '--refresh']:
    if os.environ.get('REFRESH_GATE'):
        gate = pathlib.Path(os.environ['REFRESH_GATE'])
        gate.with_suffix('.started').touch()
        while not gate.exists():
            time.sleep(.05)
    print(os.environ['ACCOUNTS'])
    with open(os.environ['CALL_LOG'], 'a') as log:
        log.write(json.dumps(['refresh-returned']) + '\\n')
elif args[:1] == ['rest'] or args[:3] == ['account', 'list', '--all']:
    if os.environ.get('BLOCK_METADATA'):
        pathlib.Path(os.environ['BLOCK_METADATA']).touch()
        while True:
            time.sleep(1)
    print(os.environ['TENANTS'])
elif args[:3] != ['account', 'set', '--subscription']:
    sys.exit(91)
'''


def verify_binary(binary, repo_root, check_freshness):
    if not binary.is_file():
        label = "target/debug/azc" if binary == repo_root / "target/debug/azc" else str(binary)
        raise RuntimeError(f"{label} not found; run 'cargo build --locked' first")
    if not check_freshness:
        return
    binary_mtime = binary.stat().st_mtime
    inputs = [repo_root / name for name in ("Cargo.toml", "Cargo.lock", "build.rs")]
    inputs.extend(sorted((repo_root / "src").glob("*.rs")))
    newer = [
        str(path.relative_to(repo_root))
        for path in inputs
        if path.is_file() and path.stat().st_mtime > binary_mtime
    ]
    if newer:
        raise RuntimeError(
            f"target/debug/azc is stale (older than {', '.join(newer)}); "
            "run 'cargo build --locked' first"
        )


def setUpModule():
    verify_binary(
        BINARY,
        REPO_ROOT,
        check_freshness=not os.environ.get("AZC_BIN")
        and BINARY.resolve() == DEFAULT_BINARY.resolve(),
    )


class ContextSwitching(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="azc-context-tests-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "calls.jsonl"
        launcher = self.root / "az"
        launcher.write_text("#!" + sys.executable + "\n" + FAKE)
        launcher.chmod(0o755)
        self.env = dict(os.environ, AZC_AZ_BIN=str(launcher),
                        AZURE_CONFIG_DIR=str(self.root / "azure"),
                        CALL_LOG=str(self.log), ACCOUNTS=json.dumps(ROWS),
                        TENANTS=json.dumps(METADATA), TERM="xterm-256color")

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def assert_calls(self, selected=None, refresh=False):
        calls = self.calls()
        sets = [call for call in calls if call[:2] == ['account', 'set']]
        self.assertEqual(sets, [] if selected is None else [
                             ['account', 'set', '--subscription', selected, '--only-show-errors']])
        self.assertEqual(sum(call[:2] == ['account', 'list'] and '--all' not in call
                             for call in calls), 1 + refresh, calls)
        if refresh and sets:
            self.assertLess(calls.index(['refresh-returned']), calls.index(sets[0]), calls)

    def session(self, args, keys, selected=None, marker=b'Shortlist', block=False,
                refresh=False):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 140, 0, 0))
        original = termios.tcgetattr(slave)
        if block:
            self.env['BLOCK_METADATA'] = str(self.root / 'metadata-started')
        gate = self.root / 'refresh.gate'
        if refresh:
            self.env['REFRESH_GATE'] = str(gate)
        proc = subprocess.Popen([str(BINARY), *args], env=self.env,
                                stdin=slave, stdout=slave, stderr=slave,
                                start_new_session=True)
        output = bytearray()
        deadline = time.monotonic() + 10
        def read_until(predicate):
            while not predicate():
                self.assertLess(time.monotonic(), deadline, bytes(output))
                if select.select([master], [], [], .05)[0]:
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        data = b''
                    output.extend(data)
                    if b'\x1b[6n' in output:
                        os.write(master, b'\x1b[1;1R')
                        output[:] = output.replace(b'\x1b[6n', b'')
                if proc.poll() is not None and not predicate():
                    self.fail(f'Exited before expected state: {bytes(output)!r}')
        try:
            read_until(lambda: marker in output)
            if block:
                read_until(lambda: (self.root / 'metadata-started').exists())
            if refresh:
                os.write(master, b'\x12')
                read_until(lambda: gate.with_suffix('.started').exists())
            os.write(master, keys)
            if refresh:
                # Enter is deferred while `az account list --refresh` is still running.
                read_until(lambda: b'Waiting for Azure refresh' in output)
                self.assertFalse([call for call in self.calls() if call[:2] == ['account', 'set']])
                gate.touch()
            read_until(lambda: proc.poll() is not None)
            self.assertEqual(proc.returncode, 0, bytes(output))
            self.assertEqual(termios.tcgetattr(slave), original)
            self.assert_calls(selected, refresh)
        finally:
            # Terminate blocked fake-Azure descendants as well as azc.
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait(timeout=5)
            os.close(master)
            os.close(slave)

    def test_subscription_shortlist_switches_exact_id(self):
        self.session(['payments'], b'\r', 'dev-id')

    def test_ordinary_picker_switches_exact_id(self):
        self.session([], b'payments development\r', 'dev-id', b'dev-id')

    def test_ordinary_picker_cannot_select_another_tenants_subscription(self):
        self.session([], b'payments production\r\x03', marker=b'dev-id')

    def test_subscription_shortlist_keeps_cross_tenant_choices(self):
        self.session(['payments'], b'\x1b[B\r', 'prod-id')

    def test_shortlist_ignores_refresh_and_tenant_toggle(self):
        self.session(['payments'], b'\x14\x12\r', 'dev-id')

    def test_tenant_shortlist_switches_first_subscription(self):
        self.session(['tenant', 'company'], b'\x1b[B\r', 'prod-id')

    def test_shortlist_cannot_select_lower_ranked_result(self):
        self.session(['payments'], b'\x15archive\r\x03')

    def test_full_picker_escape_clears_query_and_exposes_outsiders(self):
        self.session(['payments'], b'\x02archive\r', 'archive-id')

    def test_full_picker_escape_preserves_highlight(self):
        self.session(['payments'], b'\x02\r', 'dev-id')

    def test_full_tenant_picker_restores_drilldown(self):
        self.session(['tenant', 'company'], b'\x02\r\x03')

    def test_missing_subscription_query_can_be_corrected(self):
        self.session(['paymnts'], b'\x15finance\r', 'finance-id', b'Type to search')

    def test_missing_tenant_query_can_be_corrected(self):
        self.session(['tenant', 'typo'], b'\x15company beta\r\r', 'prod-id', b'Type to search')

    def test_escape_cancels_and_restores_terminal(self):
        self.session(['payments'], b'\x1b')

    def test_control_c_cancels_and_restores_terminal(self):
        self.session(['payments'], b'\x03')

    def test_subscription_selection_does_not_wait_for_metadata(self):
        self.session(['payments'], b'\r', 'dev-id', block=True)

    def test_enter_during_refresh_switches_after_refreshed_listing(self):
        self.session([], b'payments development\r', 'dev-id', b'finance-id', refresh=True)

    def test_recovered_picker_enter_during_refresh_waits_for_listing(self):
        self.session(['paymnts'], b'\x15payments development\r', 'dev-id',
                     b'Type to search', refresh=True)

    def test_early_cache_save_keeps_previous_tenant_labels(self):
        self.session([], b'\x1b', marker=b'Company Alpha')
        self.log.unlink()
        self.session([], b'\x1b', marker=b'Finance', block=True)
        cache = (self.root / 'azure' / 'azc-picker-cache.json').read_text()
        self.assertIn('Company Alpha', cache)

    def test_duplicate_names_preserve_source_identity(self):
        self.env['ACCOUNTS'] = json.dumps([
            dict(id=sid, name='Same Name', tenantId='tenant-a')
            for sid in ['first-id', 'second-id']])
        self.session(['same'], b'\x1b[B\r', 'second-id')

    def test_unique_query_switches_without_terminal(self):
        result = subprocess.run([str(BINARY), 'payments', 'production'], env=self.env,
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_calls('prod-id')

    def test_redirecting_either_stream_disables_picker_recovery(self):
        for args in [['payments'], ['missing'], ['tenant', 'company'], ['tenant', 'missing']]:
            for terminal_stdin in [False, True]:
                with self.subTest(args=args, terminal_stdin=terminal_stdin):
                    self.log.unlink(missing_ok=True)
                    master, slave = pty.openpty()
                    original = termios.tcgetattr(slave)
                    try:
                        result = subprocess.run([str(BINARY), *args], env=self.env,
                            stdin=slave if terminal_stdin else subprocess.DEVNULL,
                            stdout=subprocess.PIPE if terminal_stdin else slave,
                            stderr=subprocess.PIPE, timeout=10)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn(b'azc:', result.stderr)
                        self.assertEqual(termios.tcgetattr(slave), original)
                        self.assert_calls()
                    finally:
                        os.close(master)
                        os.close(slave)

    def test_verify_binary_rejects_missing_and_stale_default_binary(self):
        with tempfile.TemporaryDirectory(prefix="azc-bin-guard-") as temp:
            repo = Path(temp)
            (repo / "src").mkdir()
            for rel in ["Cargo.toml", "Cargo.lock", "build.rs", "src/main.rs"]:
                (repo / rel).write_text("// stub\n")
            default_bin = repo / "target/debug/azc"

            with self.assertRaisesRegex(RuntimeError, "cargo build --locked"):
                verify_binary(default_bin, repo, check_freshness=True)

            default_bin.parent.mkdir(parents=True)
            default_bin.write_text("binary")
            now = time.time()
            os.utime(default_bin, (now - 10, now - 10))
            os.utime(repo / "src/main.rs", (now, now))

            with self.assertRaisesRegex(RuntimeError, "stale.*cargo build --locked"):
                verify_binary(default_bin, repo, check_freshness=True)

            verify_binary(default_bin, repo, check_freshness=False)
            os.utime(default_bin, (now + 10, now + 10))
            verify_binary(default_bin, repo, check_freshness=True)


if __name__ == '__main__':
    unittest.main(verbosity=2)
