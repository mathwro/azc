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

BINARY = Path(__file__).resolve().parents[1] / "target/debug/azc"
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
elif args[:1] == ['rest'] or args[:3] == ['account', 'list', '--all']:
    if os.environ.get('BLOCK_METADATA'):
        pathlib.Path(os.environ['BLOCK_METADATA']).touch()
        while True:
            time.sleep(1)
    print(os.environ['TENANTS'])
elif args[:3] != ['account', 'set', '--subscription']:
    sys.exit(91)
'''


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

    def assert_calls(self, selected=None):
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual([call for call in calls if call[:2] == ['account', 'set']],
                         [] if selected is None else [
                             ['account', 'set', '--subscription', selected, '--only-show-errors']])
        self.assertEqual(sum(call[:2] == ['account', 'list'] and '--all' not in call
                             for call in calls), 1, calls)

    def session(self, args, keys, selected=None, marker=b'Shortlist', block=False):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 140, 0, 0))
        original = termios.tcgetattr(slave)
        if block:
            self.env['BLOCK_METADATA'] = str(self.root / 'metadata-started')
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
            os.write(master, keys)
            read_until(lambda: proc.poll() is not None)
            self.assertEqual(proc.returncode, 0, bytes(output))
            self.assertEqual(termios.tcgetattr(slave), original)
            self.assert_calls(selected)
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
        self.session([], b'payments production\r', 'prod-id', b'prod-id')

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


if __name__ == '__main__':
    unittest.main(verbosity=2)
