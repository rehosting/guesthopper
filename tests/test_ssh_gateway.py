import asyncio
import json
import os
import struct
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import guest_cmd  # noqa: E402
import ssh_gateway as sg  # noqa: E402  (imports even without asyncssh: SSH glue is guarded)


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

    async def scandir(self, path):
        for name in self.names:
            yield name, {"mode": 0o100644, "size": 1}


@unittest.skipUnless(sg.asyncssh is not None, "asyncssh not installed")
class GuestSFTPServerTests(unittest.TestCase):
    """The SFTP bridge's own logic, with the guest faked out."""

    def _server(self, fs):
        server = sg.GuestSFTPServer(object(), "/nonexistent", 1)
        server._fs = fs
        return server

    def test_read_size_from_the_client_is_clamped(self):
        # An SFTP client may ask for any u32 length; passing it through would
        # make the gateway buffer that much guest data per request.
        fs = _FakeFs()

        async def drive():
            server = self._server(fs)
            await server.read(sg._GuestFile(1, b"/f"), 0, 0xFFFFFFFF)
            await server.read(sg._GuestFile(1, b"/f"), 0, 100)

        asyncio.run(drive())
        self.assertEqual(fs.read_sizes, [sg.FS_READ_LIMIT, 100])
        # Never above what asyncssh advertises to clients as the server limit.
        self.assertLessEqual(sg.FS_READ_LIMIT, sg.asyncssh.sftp.MAX_SFTP_READ_LEN)

    def test_close_after_the_guest_session_died_does_not_raise(self):
        # asyncssh closes open files during teardown before calling exit(); an
        # exception there would skip exit() and the channel close.
        fs = _FakeFs()
        fs.dead = True
        asyncio.run(self._server(fs).close(sg._GuestFile(1, b"/f")))

    def test_close_refused_by_a_live_guest_is_still_an_sftp_error(self):
        with self.assertRaises(sg.asyncssh.SFTPError):
            asyncio.run(self._server(_FakeFs()).close(sg._GuestFile(1, b"/f")))

    def test_directory_listing_drops_names_that_are_not_single_components(self):
        fs = _FakeFs()
        fs.names = [b"ok", b"../escape", b"a/b", b"", b"..", b".", b"also-ok"]

        async def drive():
            return [n.filename async for n in self._server(fs).scandir(b"/d")]

        self.assertEqual(asyncio.run(drive()), [b".", b"..", b"ok", b"also-ok"])


if __name__ == "__main__":
    unittest.main()
