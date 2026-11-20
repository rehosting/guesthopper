"""End-to-end test of scp/sftp through the SSH gateway.

Real OpenSSH clients -> ssh_gateway.py (asyncssh SFTP server) -> guest_fs.py ->
the real agent session code, served over a Unix socket by the ``uds_agent``
example in place of a guest + vsock. Files land on this machine's filesystem.

Skipped unless everything it needs is present:
  * asyncssh importable by this interpreter,
  * ``scp`` / ``sftp`` on PATH,
  * the agent stand-in built:  cargo build --example uds_agent
    (or GUESTHOPPER_UDS_AGENT pointing at it).
"""

import filecmp
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

try:
    import asyncssh  # noqa: F401
    HAVE_ASYNCSSH = True
except ImportError:
    HAVE_ASYNCSSH = False


def _find_agent():
    explicit = os.environ.get("GUESTHOPPER_UDS_AGENT")
    if explicit:
        return explicit if os.path.exists(explicit) else None
    for profile in ("debug", "release"):
        path = os.path.join(ROOT, "target", profile, "examples", "uds_agent")
        if os.path.exists(path):
            return path
    return None


AGENT = _find_agent()
SSH_OPTS = [
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR",
    "-o", "BatchMode=yes",
]


def _free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _short_tmpdir():
    """A temp dir whose path leaves room for a socket name: AF_UNIX paths are
    limited to ~108 bytes, which a long TMPDIR would blow through."""
    base = tempfile.gettempdir()
    return tempfile.mkdtemp(prefix="scpe2e", dir=base if len(base) < 60 else "/tmp")


def _wait_for(predicate, what, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


def _listening(port):
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


@unittest.skipUnless(HAVE_ASYNCSSH, "asyncssh not installed")
@unittest.skipUnless(shutil.which("scp") and shutil.which("sftp"), "OpenSSH scp/sftp not on PATH")
@unittest.skipUnless(AGENT, "uds_agent not built (cargo build --example uds_agent)")
class ScpEndToEndTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = _short_tmpdir()
        cls.guest = os.path.join(cls.tmp, "guest")   # what scp sees as "remote"
        cls.host = os.path.join(cls.tmp, "host")
        os.mkdir(cls.guest)
        os.mkdir(cls.host)
        sock = os.path.join(cls.tmp, "vsocket")
        cls.procs = []
        cls.logs = {}
        try:
            cls._spawn("agent", [AGENT, sock])
            _wait_for(lambda: os.path.exists(sock), "uds_agent socket")
            # Picking a free port and binding it later is a race with every
            # other process on the machine, so retry if the gateway loses it.
            for attempt in range(5):
                cls.port = _free_port()
                gw = cls._spawn("gateway", [
                    sys.executable, os.path.join(ROOT, "ssh_gateway.py"),
                    "--listen-port", str(cls.port), "--socket", sock,
                ])
                _wait_for(lambda: _listening(cls.port) or gw.poll() is not None,
                          "ssh gateway listener")
                if gw.poll() is None:
                    break
                cls.procs.remove(gw)
            else:
                raise AssertionError("ssh gateway never came up")
        except Exception:
            cls.tearDownClass()
            raise

    @classmethod
    def _spawn(cls, name, argv):
        log = cls.logs.get(name) or open(os.path.join(cls.tmp, name + ".log"), "ab")
        cls.logs[name] = log
        # cwd is the "guest" dir, so relative remote paths (the SFTP home
        # directory is the agent's cwd) land there.
        proc = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT, cwd=cls.guest)
        cls.procs.append(proc)
        return proc

    @classmethod
    def tearDownClass(cls):
        for p in cls.procs:
            p.terminate()
        for p in cls.procs:
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()
        for log in cls.logs.values():
            log.close()
        shutil.rmtree(cls.tmp, ignore_errors=True)

    # -- helpers --

    def remote(self, path):
        return f"root@127.0.0.1:{path}"

    def scp(self, *args, check=True):
        cmd = ["scp", *SSH_OPTS, "-P", str(self.port), *args]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if check and r.returncode != 0:
            self.fail(f"{' '.join(cmd)} exited {r.returncode}: {r.stderr}\n{self._logs()}")
        return r

    def _logs(self):
        out = []
        for name in self.logs:
            with open(os.path.join(self.tmp, name + ".log"), errors="replace") as f:
                out.append(f"--- {name}.log ---\n{f.read()[-4000:]}")
        return "\n".join(out)

    def host_file(self, name, data):
        path = os.path.join(self.host, name)
        with open(path, "wb") as f:
            f.write(data)
        return path

    # -- tests --

    def test_upload_and_download_roundtrip(self):
        # Several pipeline windows of SFTP writes/reads, not chunk-aligned.
        src = self.host_file("big.bin", os.urandom(3 * 1024 * 1024 + 4321))
        dst = os.path.join(self.guest, "big.bin")
        self.scp(src, self.remote(dst))
        self.assertTrue(filecmp.cmp(src, dst, shallow=False))

        back = os.path.join(self.host, "big.back")
        self.scp(self.remote(dst), back)
        self.assertTrue(filecmp.cmp(src, back, shallow=False))

    def test_legacy_scp_protocol(self):
        # `scp -O` execs `scp -t/-f` on the server; the gateway must serve that
        # itself rather than run a (usually absent) scp in the guest.
        src = self.host_file("legacy.bin", os.urandom(200_000))
        dst = os.path.join(self.guest, "legacy.bin")
        self.scp("-O", src, self.remote(dst))
        self.assertTrue(filecmp.cmp(src, dst, shallow=False))
        back = os.path.join(self.host, "legacy.back")
        self.scp("-O", self.remote(dst), back)
        self.assertTrue(filecmp.cmp(src, back, shallow=False))

    def test_empty_file(self):
        src = self.host_file("empty", b"")
        dst = os.path.join(self.guest, "empty")
        self.scp(src, self.remote(dst))
        self.assertEqual(os.path.getsize(dst), 0)

    def test_relative_remote_path_lands_in_agent_cwd(self):
        src = self.host_file("rel.txt", b"relative\n")
        self.scp(src, self.remote("rel-dest.txt"))
        with open(os.path.join(self.guest, "rel-dest.txt"), "rb") as f:
            self.assertEqual(f.read(), b"relative\n")

    def test_recursive_copy_preserving_mode_and_mtime(self):
        tree = os.path.join(self.host, "tree")
        os.makedirs(os.path.join(tree, "sub"))
        for rel, data in (("a", b"A"), ("sub/b", b"B" * 5000)):
            with open(os.path.join(tree, rel), "wb") as f:
                f.write(data)
        os.chmod(os.path.join(tree, "a"), 0o640)
        os.utime(os.path.join(tree, "a"), (1_500_000_000, 1_400_000_000))

        self.scp("-rp", tree, self.remote(self.guest + "/"))
        got = os.path.join(self.guest, "tree")
        st = os.stat(os.path.join(got, "a"))
        self.assertEqual(st.st_mode & 0o777, 0o640)
        self.assertEqual(int(st.st_mtime), 1_400_000_000)

        back = os.path.join(self.host, "tree.back")
        self.scp("-r", self.remote(got), back)
        cmp = filecmp.dircmp(tree, back)
        self.assertEqual((cmp.left_only, cmp.right_only, cmp.diff_files), ([], [], []))
        self.assertTrue(filecmp.cmp(os.path.join(tree, "sub/b"), os.path.join(back, "sub/b"), shallow=False))

    def test_errors_reach_the_client_with_a_failing_exit_status(self):
        r = self.scp(self.remote("/no/such/file"), os.path.join(self.host, "x"), check=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("No such file", r.stderr)

        src = self.host_file("s.txt", b"s")
        r = self.scp(src, self.remote(os.path.join(self.guest, "no-such-dir", "s.txt")), check=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("No such file", r.stderr)

    def test_sftp_batch_session(self):
        src = self.host_file("s.txt", b"sftp\n")
        back = os.path.join(self.host, "s.back")
        batch = "\n".join([
            f"cd {self.guest}",
            "mkdir d",
            f"put {src} d/s.txt",
            "rename d/s.txt d/t.txt",
            "chmod 600 d/t.txt",
            "ln -s t.txt d/sym",
            "ls -l d",
            f"get d/sym {back}",
            "df .",
            "rm d/sym",
        ]) + "\n"
        r = subprocess.run(
            ["sftp", *SSH_OPTS, "-P", str(self.port), "-b", "-", "root@127.0.0.1"],
            input=batch, capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 0, r.stderr + r.stdout + self._logs())
        t = os.path.join(self.guest, "d", "t.txt")
        self.assertEqual(os.stat(t).st_mode & 0o777, 0o600)
        self.assertFalse(os.path.lexists(os.path.join(self.guest, "d", "sym")))
        with open(back, "rb") as f:
            self.assertEqual(f.read(), b"sftp\n")
        # Owner/group columns are the guest's numeric ids, never names looked
        # up in the gateway host's passwd database.
        listing = [ln for ln in r.stdout.splitlines()
                   if ln.startswith("-rw") and ln.endswith("t.txt")]
        self.assertTrue(listing, r.stdout)
        self.assertEqual(listing[0].split()[2:4], [str(os.getuid()), str(os.getgid())])

    def _agent_sessions_ended(self):
        with open(os.path.join(self.tmp, "agent.log"), errors="replace") as f:
            return f.read().count("session ended")

    def test_client_killed_mid_transfer_releases_the_guest_session(self):
        # Each scp holds one of the agent's limited session slots. A client
        # that dies part-way must not strand it (the gateway keeps PINGing a
        # session it never closes, and the agent would keep it forever).
        src = self.host_file("slow.bin", os.urandom(4 * 1024 * 1024))
        for extra in ([], ["-O"]):
            before = self._agent_sessions_ended()
            dst = os.path.join(self.guest, "slow.bin")
            if os.path.exists(dst):
                os.unlink(dst)
            # -l: bandwidth limit (kbit/s) so the copy is still running when killed.
            p = subprocess.Popen(
                ["scp", *SSH_OPTS, *extra, "-l", "800", "-P", str(self.port), src, self.remote(dst)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            _wait_for(lambda: os.path.exists(dst) and os.path.getsize(dst) > 0,
                      f"transfer to start ({extra or 'sftp mode'})")
            p.kill()
            p.wait()
            self.assertLess(os.path.getsize(dst), os.path.getsize(src), "transfer finished before the kill")
            _wait_for(lambda: self._agent_sessions_ended() > before,
                      f"guest session to end after the client died ({extra or 'sftp mode'})")

    def test_oversized_read_request_is_answered_short_not_buffered(self):
        # A raw SFTP READ may carry any u32 length. The gateway must answer
        # with a bounded chunk rather than fetch (and hold) the whole range.
        import asyncio
        import ssh_gateway

        big = os.path.join(self.guest, "sparse.bin")
        with open(big, "wb") as f:
            f.truncate(64 * 1024 * 1024)

        async def drive():
            async with asyncssh.connect("127.0.0.1", self.port, username="root",
                                        known_hosts=None) as conn:
                async with conn.start_sftp_client() as sftp:
                    handler = getattr(sftp, "_handler", None)
                    if handler is None or not hasattr(handler, "read"):
                        self.skipTest("asyncssh client internals changed")
                    handle = await handler.open(big.encode(), asyncssh.FXF_READ, asyncssh.SFTPAttrs())
                    try:
                        result = await handler.read(handle, 0, 60 * 1024 * 1024)
                    finally:
                        await handler.close(handle)
                    return result[0] if isinstance(result, tuple) else result

        data = asyncio.run(asyncio.wait_for(drive(), 60))
        self.assertGreater(len(data), 0)
        self.assertLessEqual(len(data), ssh_gateway.FS_READ_LIMIT)

    def test_exec_and_pty_shell_stay_responsive_during_a_transfer(self):
        # File I/O must not monopolise the agent: with a copy in flight, an
        # exec and an interactive (pty) shell on other sessions still answer
        # promptly, and the copy is still going when they do.
        src = self.host_file("busy.bin", os.urandom(4 * 1024 * 1024))
        dst = os.path.join(self.guest, "busy.bin")
        if os.path.exists(dst):
            os.unlink(dst)
        # -l: bandwidth limit (kbit/s), so the transfer outlasts the checks.
        transfer = subprocess.Popen(
            ["scp", *SSH_OPTS, "-l", "800", "-P", str(self.port), src, self.remote(dst)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            _wait_for(lambda: os.path.exists(dst) and os.path.getsize(dst) > 0, "transfer to start")

            start = time.time()
            r = subprocess.run(
                ["ssh", *SSH_OPTS, "-p", str(self.port), "root@127.0.0.1", "echo exec-during-copy"],
                capture_output=True, text=True, timeout=10)
            self.assertEqual((r.stdout, r.returncode), ("exec-during-copy\n", 0), r.stderr)

            # -tt forces a pty even though stdin is a pipe: the open-pty path.
            r = subprocess.run(
                ["ssh", "-tt", *SSH_OPTS, "-p", str(self.port), "root@127.0.0.1"],
                input="echo pty-during-copy; exit 5\n", capture_output=True, text=True, timeout=10)
            self.assertIn("pty-during-copy", r.stdout)
            self.assertEqual(r.returncode, 5, r.stdout + r.stderr)
            self.assertLess(time.time() - start, 8, "sessions were slow while a transfer ran")

            # Both happened *during* the copy, not after it.
            self.assertIsNone(transfer.poll(), "transfer ended before the checks did")
            self.assertLess(os.path.getsize(dst), os.path.getsize(src))
        finally:
            transfer.kill()
            transfer.wait()

    def test_shell_and_exec_still_work_alongside_file_transfer(self):
        r = subprocess.run(
            ["ssh", *SSH_OPTS, "-p", str(self.port), "root@127.0.0.1", "echo exec-ok; exit 3"],
            capture_output=True, text=True, timeout=60)
        self.assertEqual((r.stdout, r.returncode), ("exec-ok\n", 3))


if __name__ == "__main__":
    unittest.main()
