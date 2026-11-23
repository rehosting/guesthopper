import asyncio
import contextlib
import io
import json
import os
import struct
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import guest_cmd  # noqa: E402
import ssh_gateway as sg  # noqa: E402  (imports even without asyncssh: SSH glue is guarded)
from prereq import requires  # noqa: E402


def _frame(ftype, payload=b""):
    if isinstance(payload, str):
        payload = payload.encode("utf-8")
    return bytes([ftype]) + struct.pack(">I", len(payload)) + payload


class EncodeFrameTests(unittest.TestCase):
    def test_matches_guest_cmd_wire_format(self):
        # The SSH gateway and guest_cmd must speak byte-identical frames.
        for ftype, payload in [
            (guest_cmd.FRAME_STDIN, b"hello"),
            (guest_cmd.FRAME_PING, b""),
            (guest_cmd.FRAME_RESIZE, b'{"rows":40,"cols":120}'),
            (guest_cmd.FRAME_STDIN, bytes([0, 255, 1, 254])),  # binary-safe
        ]:
            self.assertEqual(sg.encode_frame(ftype, payload), _frame(ftype, payload))

    def test_str_payload_is_utf8_encoded(self):
        self.assertEqual(sg.encode_frame(guest_cmd.FRAME_STDIN, "abc"), _frame(guest_cmd.FRAME_STDIN, b"abc"))

    def test_oversize_payload_rejected(self):
        big = b"x" * (guest_cmd.MAX_FRAME_LEN + 1)
        with self.assertRaises(guest_cmd.GuestCommandError):
            sg.encode_frame(guest_cmd.FRAME_STDIN, big)


class RequestBuilderTests(unittest.TestCase):
    def test_open_pty_request(self):
        self.assertEqual(json.loads(sg.open_pty_request(24, 80)),
                         {"verb": "open-pty", "rows": 24, "cols": 80})

    def test_exec_request(self):
        self.assertEqual(json.loads(sg.exec_request("id")), {"verb": "exec", "cmd": "id"})

    def test_resize_payload(self):
        self.assertEqual(json.loads(sg.resize_payload(40, 120)), {"rows": 40, "cols": 120})


class ReadFrameTests(unittest.TestCase):
    @staticmethod
    def _read_all(data, count=1):
        # StreamReader must be built inside a running loop, so feed + read all
        # happen in one coroutine.
        async def drive():
            r = asyncio.StreamReader()
            r.feed_data(data)
            r.feed_eof()
            return [await sg.read_frame(r) for _ in range(count)]

        return asyncio.run(drive())

    def test_roundtrip_binary_safe(self):
        payload = bytes([0, 255, 1, 0, 254]) + b"tail"
        (frame,) = self._read_all(_frame(guest_cmd.FRAME_STDOUT, payload))
        self.assertEqual(frame, (guest_cmd.FRAME_STDOUT, payload))

    def test_zero_length_frame(self):
        (frame,) = self._read_all(_frame(guest_cmd.FRAME_PING, b""))
        self.assertEqual(frame, (guest_cmd.FRAME_PING, b""))

    def test_clean_eof_returns_none(self):
        (frame,) = self._read_all(b"")
        self.assertIsNone(frame)

    def test_oversize_length_rejected(self):
        hdr = bytes([guest_cmd.FRAME_STDOUT]) + struct.pack(">I", guest_cmd.MAX_FRAME_LEN + 1)
        with self.assertRaises(guest_cmd.GuestCommandError):
            self._read_all(hdr)

    def test_truncated_payload_raises(self):
        # Header claims 10 bytes but only 3 follow, then EOF.
        data = bytes([guest_cmd.FRAME_STDOUT]) + struct.pack(">I", 10) + b"abc"
        with self.assertRaises(guest_cmd.GuestCommandError):
            self._read_all(data)

    def test_two_frames_in_sequence(self):
        data = _frame(guest_cmd.FRAME_STDOUT, b"a") + _frame(guest_cmd.FRAME_EXIT, b'{"code":0}')
        f1, f2 = self._read_all(data, count=2)
        self.assertEqual(f1, (guest_cmd.FRAME_STDOUT, b"a"))
        self.assertEqual(f2, (guest_cmd.FRAME_EXIT, b'{"code":0}'))


class _FakeTransport:
    def __init__(self):
        self.buffered = 0

    def get_write_buffer_size(self):
        return self.buffered


class _FakeConn:
    def __init__(self, chan):
        self._chan = chan
        self._transport = _FakeTransport()
        self.aborted = False

    def abort(self):
        self.aborted = True
        self._chan.closing = True


class _FakeChan:
    """The little of an asyncssh channel that GuestSFTPServer looks at."""

    def __init__(self):
        self.closing = False
        self.buffered = 0          # bytes beyond the client's SSH window
        self.conn = _FakeConn(self)

    def is_closing(self):
        return self.closing

    def get_write_buffer_size(self):
        return self.buffered

    def get_connection(self):
        return self.conn


class _FakeFs:
    """Stands in for guest_fs.GuestFs behind a GuestSFTPServer."""

    def __init__(self):
        self.read_sizes = []
        self.dead = False
        self.names = []

    async def read(self, handle, offset, size):
        self.read_sizes.append(size)
        return b"x" * min(size, 10)

    async def close_handle(self, handle):
        if self.dead:
            raise guest_cmd.GuestCommandError("guest closed the fs session")
        raise sg.GuestFsError("invalid", "not an open handle")

    async def stat(self, path, follow=True):
        return {"mode": 0o040755, "size": 0}

    async def fstatvfs(self, handle):
        self.fstatvfs_handles = getattr(self, "fstatvfs_handles", []) + [handle]
        return dict(bsize=4096, frsize=4096, blocks=10, bfree=5, bavail=4,
                    files=100, ffree=50, favail=40, fsid=1, flag=0, namemax=255)

    async def scandir(self, path):
        for name in self.names:
            yield name, {"mode": 0o100644, "size": 1}


@requires(asyncssh=sg.asyncssh is not None)
class GuestSFTPServerTests(unittest.TestCase):
    """The SFTP bridge's own logic, with the guest faked out."""

    def _server(self, fs):
        server = sg.GuestSFTPServer(_FakeChan(), "/nonexistent", 1)
        server._fs = fs
        return server

    def test_read_size_from_the_client_is_clamped(self):
        # An SFTP client may ask for any u32 length; passing it through would
        # make the gateway buffer that much guest data per request.
        fs = _FakeFs()

        async def drive():
            server = self._server(fs)
            await server.read(sg._GuestFile(1), 0, 0xFFFFFFFF)
            await server.read(sg._GuestFile(1), 0, 100)

        asyncio.run(drive())
        self.assertEqual(fs.read_sizes, [sg.FS_READ_LIMIT, 100])
        # Never above what asyncssh advertises to clients as the server limit.
        self.assertLessEqual(sg.FS_READ_LIMIT, sg.asyncssh.sftp.MAX_SFTP_READ_LEN)

    def test_read_waits_while_the_client_is_not_taking_replies(self):
        # asyncssh queues replies without waiting for the client. A read must
        # not fetch more guest data while the outbound buffer is full, or a
        # client that pipelines reads and stops reading makes the gateway hold
        # every reply.
        fs = _FakeFs()

        async def drive():
            write_ok = asyncio.Event()  # clear: the client is behind
            server = sg.GuestSFTPServer(_FakeChan(), "/nonexistent", 1, write_ok)
            server._fs = fs
            read = asyncio.ensure_future(server.read(sg._GuestFile(1), 0, 100))
            for _ in range(20):
                await asyncio.sleep(0)
            self.assertFalse(read.done())
            self.assertEqual(fs.read_sizes, [], "guest was read while the client was behind")
            write_ok.set()  # the buffer drained
            return await asyncio.wait_for(read, 30)

        self.assertEqual(asyncio.run(drive()), b"x" * 10)
        self.assertEqual(fs.read_sizes, [100])

    def test_read_parked_on_backpressure_fails_fast_once_the_client_is_gone(self):
        # connection_lost releases the wait too. The read must then give up
        # rather than fetch data nobody will receive -- once for itself and
        # again for every request the client left queued behind it.
        fs = _FakeFs()

        async def drive():
            write_ok = asyncio.Event()
            chan = _FakeChan()
            server = sg.GuestSFTPServer(chan, "/nonexistent", 1, write_ok)
            server._fs = fs
            read = asyncio.ensure_future(server.read(sg._GuestFile(1), 0, 100))
            await asyncio.sleep(0)
            chan.closing = True
            write_ok.set()
            with self.assertRaises(sg.asyncssh.SFTPError):
                await asyncio.wait_for(read, 30)
            # And one arriving after the disconnect doesn't touch the guest.
            with self.assertRaises(sg.asyncssh.SFTPError):
                await server.read(sg._GuestFile(1), 0, 100)

        asyncio.run(drive())
        self.assertEqual(fs.read_sizes, [])

    def test_read_waits_on_the_socket_backlog_whatever_the_ssh_window(self):
        # The client picks its SSH window. With a huge one asyncssh never
        # pauses (everything is "within the window") and replies pile up in the
        # connection's socket buffer instead, so that is checked directly.
        fs = _FakeFs()

        async def drive():
            write_ok = asyncio.Event()
            write_ok.set()  # asyncssh's flow control sees nothing wrong
            chan = _FakeChan()
            chan.conn._transport.buffered = sg.REPLY_BACKLOG_MAX + 1
            server = sg.GuestSFTPServer(chan, "/nonexistent", 1, write_ok, stall_timeout=60)
            server._fs = fs
            read = asyncio.ensure_future(server.read(sg._GuestFile(1), 0, 100))
            await asyncio.sleep(4 * sg.CLIENT_POLL_S)
            self.assertFalse(read.done())
            self.assertEqual(fs.read_sizes, [], "guest was read with a full socket buffer")
            chan.conn._transport.buffered = sg.REPLY_BACKLOG_MAX  # client caught up
            await asyncio.wait_for(read, 30)
            self.assertFalse(chan.conn.aborted)

        asyncio.run(drive())
        self.assertEqual(fs.read_sizes, [100])

    def test_client_that_takes_nothing_is_disconnected_after_the_stall_timeout(self):
        # Otherwise a connected-but-frozen client pins its guest session for
        # good: the gateway's keepalive keeps that session alive on its behalf.
        fs = _FakeFs()

        async def drive():
            chan = _FakeChan()
            chan.buffered = sg.REPLY_BACKLOG_MAX * 3
            server = sg.GuestSFTPServer(chan, "/nonexistent", 1, None, stall_timeout=0.3)
            server._fs = fs
            with self.assertRaises(sg.asyncssh.SFTPError):
                await asyncio.wait_for(server.read(sg._GuestFile(1), 0, 100), 30)
            return chan

        log = io.StringIO()
        with contextlib.redirect_stderr(log):  # it says why on stderr
            chan = asyncio.run(drive())
        self.assertIn("took no data", log.getvalue())
        self.assertTrue(chan.conn.aborted, "stalled client was not disconnected")
        self.assertEqual(fs.read_sizes, [])

    def test_slow_client_that_keeps_taking_data_is_not_treated_as_stalled(self):
        # The stall clock restarts on any progress, so only a client that takes
        # nothing at all trips it -- however slow a live one is.
        fs = _FakeFs()

        async def drive():
            chan = _FakeChan()
            chan.buffered = sg.REPLY_BACKLOG_MAX + 40
            stall = 0.4
            server = sg.GuestSFTPServer(chan, "/nonexistent", 1, None, stall_timeout=stall)
            server._fs = fs
            read = asyncio.ensure_future(server.read(sg._GuestFile(1), 0, 100))
            # Drain one byte at a time, each well inside the stall timeout but
            # for several times its length in total.
            for _ in range(40):
                await asyncio.sleep(stall / 8)
                chan.buffered -= 1
                self.assertFalse(chan.conn.aborted)
            await asyncio.wait_for(read, 30)
            return chan

        chan = asyncio.run(drive())
        self.assertFalse(chan.conn.aborted)
        self.assertEqual(fs.read_sizes, [100])

    def test_close_after_the_guest_session_died_does_not_raise(self):
        # asyncssh closes open files during teardown before calling exit(); an
        # exception there would skip exit() and the channel close.
        fs = _FakeFs()
        fs.dead = True
        asyncio.run(self._server(fs).close(sg._GuestFile(1)))

    def test_close_refused_by_a_live_guest_is_still_an_sftp_error(self):
        with self.assertRaises(sg.asyncssh.SFTPError):
            asyncio.run(self._server(_FakeFs()).close(sg._GuestFile(1)))

    def test_fstatvfs_asks_the_guest_about_the_handle_not_a_remembered_path(self):
        # The file may have been renamed or unlinked since it was opened; only
        # the handle still identifies it (and its filesystem).
        fs = _FakeFs()
        attrs = asyncio.run(self._server(fs).fstatvfs(sg._GuestFile(7)))
        self.assertEqual(fs.fstatvfs_handles, [7])
        self.assertEqual((attrs.bsize, attrs.bavail, attrs.namemax), (4096, 4, 255))

    def test_directory_listing_drops_names_that_are_not_single_components(self):
        fs = _FakeFs()
        fs.names = [b"ok", b"../escape", b"a/b", b"", b"..", b".", b"also-ok"]

        async def drive():
            return [n.filename async for n in self._server(fs).scandir(b"/d")]

        self.assertEqual(asyncio.run(drive()), [b".", b"..", b"ok", b"also-ok"])


if __name__ == "__main__":
    unittest.main()
