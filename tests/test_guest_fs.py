"""Unit tests for guest_fs.py against an in-process fake of the guest agent's
``fs`` verb (no guest, no vsock, no asyncssh). The real agent's side of the
contract is pinned by tests/fs_tests.rs; the two meet in tests/test_scp_e2e.py.
"""

import asyncio
import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import guest_cmd  # noqa: E402
import guest_fs  # noqa: E402


# How long to wait for something that should happen promptly. Waits poll for
# the event itself, so a slow or busy machine only makes a test slower.
SOON = 30


async def eventually(what, cond):
    deadline = asyncio.get_running_loop().time() + SOON
    while not cond():
        if asyncio.get_running_loop().time() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        await asyncio.sleep(0.01)


class PayloadCodecTests(unittest.TestCase):
    def test_roundtrip_keeps_binary_data_separate_from_header(self):
        data = bytes(range(256)) * 3
        payload = guest_fs.encode_fs_payload({"op": "write", "h": 3, "off": 9}, data)
        hdr_len = int.from_bytes(payload[:4], "big")
        self.assertEqual(json.loads(payload[4:4 + hdr_len]), {"op": "write", "h": 3, "off": 9})
        self.assertEqual(guest_fs.decode_fs_payload(payload),
                         ({"op": "write", "h": 3, "off": 9}, data))

    def test_malformed_payloads_are_rejected(self):
        for bad in (b"", b"\x00\x00", b"\x00\x00\x00\x09{}", b"\x00\x00\x00\x02[]"):
            with self.assertRaises(guest_cmd.GuestCommandError):
                guest_fs.decode_fs_payload(bad)

    def test_frame_types_do_not_collide_with_the_shared_ones(self):
        shared = {getattr(guest_cmd, n) for n in dir(guest_cmd) if n.startswith("FRAME_")
                  and isinstance(getattr(guest_cmd, n), int) and n != "FRAME_READ_TIMEOUT_S"}
        self.assertNotIn(guest_fs.FRAME_FS_REQ, shared)
        self.assertNotIn(guest_fs.FRAME_FS_RESP, shared)


class FakeGuest:
    """Minimal stand-in for vhost-device-vsock + the agent's fs verb: one
    in-memory file per handle, answering strictly in request order."""

    def __init__(self):
        self.files = {}        # path-hex -> bytearray
        self.handles = {}      # handle -> path-hex
        self.requests = []     # every decoded request header, in arrival order
        self.verbs = []
        self.pings = 0
        self.max_write = 0
        self.hold = None       # if set, an Event responses wait on (to force pipelining)
        self.connections = 0
        self.closed = asyncio.Event()
        self.closed_by_test = asyncio.Event()
        self.reject_verb = False
        self.oversize_reads = False  # misbehaving guest: more bytes than asked
        self.mute_handshake = False  # accept the connection but never say OK
        self.stop_reading = False    # complete the handshake, then read nothing

    async def serve(self, reader, writer):
        self.connections += 1
        try:
            line = await reader.readline()
            if self.mute_handshake:
                await reader.read()  # hold the connection open until the client gives up
                return
            port = line.decode().strip().split(" ", 1)[1]
            writer.write(f"OK {port}\n".encode())
            if self.stop_reading:
                await self.closed_by_test.wait()
                return
            while True:
                hdr = await reader.readexactly(5)
                payload = await reader.readexactly(int.from_bytes(hdr[1:5], "big"))
                if hdr[0] == guest_cmd.FRAME_REQUEST:
                    self.verbs.append(json.loads(payload)["verb"])
                    if self.reject_verb:
                        msg = json.dumps({"message": "unsupported verb: \"fs\""}).encode()
                        writer.write(guest_fs.encode_frame(guest_cmd.FRAME_ERROR, msg))
                        return
                elif hdr[0] == guest_cmd.FRAME_PING:
                    self.pings += 1
                elif hdr[0] == guest_fs.FRAME_FS_REQ:
                    req, data = guest_fs.decode_fs_payload(payload)
                    self.requests.append(req)
                    if self.hold is not None:
                        await self.hold.wait()
                    resp, body = self.handle(req, data)
                    writer.write(guest_fs.encode_frame(
                        guest_fs.FRAME_FS_RESP, guest_fs.encode_fs_payload(resp, body)))
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            writer.close()
            self.closed.set()

    def handle(self, req, data):
        op = req["op"]
        if op in ("read", "write") and req.get("h") not in self.handles:
            # As the real agent does. (Raising here instead would kill this
            # connection's task and have asyncio log a traceback mid-suite.)
            return {"ok": False, "kind": "invalid", "errno": 0,
                    "message": "not an open file handle"}, b""
        if op == "open":
            if not req.get("create") and req["path"] not in self.files:
                return {"ok": False, "kind": "noent", "errno": 2,
                        "message": "No such file or directory (os error 2)"}, b""
            self.files.setdefault(req["path"], bytearray())
            h = len(self.handles) + 1
            self.handles[h] = req["path"]
            return {"ok": True, "h": h}, b""
        if op == "write":
            self.max_write = max(self.max_write, len(data))
            buf = self.files[self.handles[req["h"]]]
            end = req["off"] + len(data)
            if len(buf) < end:
                buf.extend(b"\0" * (end - len(buf)))
            buf[req["off"]:end] = data
            return {"ok": True}, b""
        if op == "read":
            buf = self.files[self.handles[req["h"]]]
            n = min(req["len"], guest_fs.FS_CHUNK)
            body = bytes(buf[req["off"]:req["off"] + n])
            if self.oversize_reads:
                body += b"X" * 1000
            return {"ok": True, "eof": len(body) < n}, body
        if op == "close":
            self.handles.pop(req["h"], None)  # dir handles aren't tracked
            return {"ok": True}, b""
        if op == "opendir":
            return {"ok": True, "h": 99}, b""
        if op == "readdir":
            page = self.requests.count(req)  # 1st call -> 1, 2nd -> 2
            entries = [{"name": f"e{page}".encode().hex(), "attrs": {"mode": 0o100644, "size": page}}]
            return {"ok": True, "entries": entries, "eof": page >= 2}, b""
        if op == "setstat":
            return {"ok": True}, b""
        return {"ok": False, "kind": "unsupported", "errno": 0,
                "message": f"unsupported fs op: {op!r}"}, b""


class GuestFsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="gfs")
        self.sock = os.path.join(self.tmp, "vsocket")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_with_guest(self, body, guest=None):
        """Run ``await body(fs, guest)`` against a fresh FakeGuest."""
        guest = guest or FakeGuest()

        async def drive():
            server = await asyncio.start_unix_server(guest.serve, self.sock)
            try:
                fs = await guest_fs.GuestFs.connect(self.sock, 4242)
                try:
                    return await body(fs, guest)
                finally:
                    await fs.close()
            finally:
                server.close()
                await server.wait_closed()

        return asyncio.run(asyncio.wait_for(drive(), 4 * SOON))

    def test_connect_requests_the_fs_verb(self):
        async def body(fs, guest):
            await fs.call("setstat", path="2f")
            return guest.verbs

        self.assertEqual(self.run_with_guest(body), ["fs"])

    def test_large_write_and_read_are_chunked_and_reassembled(self):
        data = os.urandom(guest_fs.FS_CHUNK * 40 + 123)  # > one pipeline window

        async def body(fs, guest):
            h = await fs.open(b"/f", write=True, create=True)
            await fs.write(h, 0, data)
            back = await fs.read(h, 0, len(data) + 5000)  # asks past EOF
            middle = await fs.read(h, 1000, 70000)
            return guest, back, middle

        guest, back, middle = self.run_with_guest(body)
        self.assertEqual(bytes(guest.files[b"/f".hex()]), data)
        self.assertEqual(back, data)
        self.assertEqual(middle, data[1000:71000])
        # No frame may exceed the chunk size: the agent's inbound cap can be as
        # low as 64 KiB.
        self.assertLessEqual(guest.max_write, guest_fs.FS_CHUNK)

    def test_calls_are_pipelined_and_matched_in_order(self):
        async def body(fs, guest):
            h = await fs.open(b"/f", write=True, create=True)
            await fs.write(h, 0, b"abcdefghij")
            before = len(guest.requests)
            guest.hold = asyncio.Event()
            calls = [asyncio.ensure_future(fs.call("read", h=h, off=i, len=1)) for i in range(10)]
            # The guest reads requests one at a time and is holding the first,
            # so the rest can only have got as far as its socket: all ten are
            # in flight together, none answered.
            await eventually("the first request to reach the guest",
                             lambda: len(guest.requests) > before)
            self.assertFalse(any(c.done() for c in calls))
            guest.hold.set()
            results = await asyncio.gather(*calls)
            self.assertEqual(len(guest.requests) - before, 10)
            return b"".join(body for _hdr, body in results)

        self.assertEqual(self.run_with_guest(body), b"abcdefghij")

    def test_guest_refusal_raises_typed_error_and_session_survives(self):
        async def body(fs, guest):
            with self.assertRaises(guest_fs.GuestFsError) as cm:
                await fs.open(b"/missing")
            self.assertEqual(cm.exception.kind, "noent")
            self.assertEqual(cm.exception.errno, 2)
            self.assertIn("No such file", str(cm.exception))
            with self.assertRaises(guest_fs.GuestFsError) as cm:
                await fs.call("frobnicate")
            self.assertEqual(cm.exception.kind, "unsupported")
            return await fs.open(b"/ok", create=True, write=True)

        self.assertEqual(self.run_with_guest(body), 1)

    def test_scandir_pages_until_eof_and_closes_the_handle(self):
        async def body(fs, guest):
            entries = [e async for e in fs.scandir(b"/d")]
            return entries, [r["op"] for r in guest.requests]

        entries, ops = self.run_with_guest(body)
        self.assertEqual([name for name, _ in entries], [b"e1", b"e2"])
        self.assertEqual(entries[1][1]["size"], 2)
        self.assertEqual(ops, ["opendir", "readdir", "readdir", "close"])

    def test_setstat_sends_only_the_given_attributes(self):
        async def body(fs, guest):
            await fs.setstat(b"/f", mode=0o600, mtime=None, size=None)
            await fs.setstat(handle=7, atime=1, mtime=2)
            return guest.requests

        by_path, by_handle = self.run_with_guest(body)
        self.assertEqual(by_path, {"op": "setstat", "path": b"/f".hex(), "follow": True, "mode": 0o600})
        self.assertEqual(by_handle, {"op": "setstat", "h": 7, "atime": 1, "mtime": 2})

    def test_agent_without_fs_verb_fails_calls_with_its_message(self):
        guest = FakeGuest()
        guest.reject_verb = True

        async def body(fs, guest):
            with self.assertRaises(guest_cmd.GuestCommandError) as cm:
                await fs.stat(b"/")
            self.assertNotIsInstance(cm.exception, guest_fs.GuestFsError)
            self.assertIn("unsupported verb", str(cm.exception))
            # Dead session: later calls fail fast rather than hanging.
            with self.assertRaises(guest_cmd.GuestCommandError):
                await fs.stat(b"/")

        self.run_with_guest(body, guest)

    def test_close_drops_the_connection_and_fails_inflight_calls(self):
        async def body(fs, guest):
            guest.hold = asyncio.Event()  # never answered
            pending = asyncio.ensure_future(fs.call("read", h=1, off=0, len=1))
            await eventually("the request to reach the guest", lambda: guest.requests)
            await fs.close()
            with self.assertRaises(guest_cmd.GuestCommandError):
                await pending
            guest.hold.set()
            await asyncio.wait_for(guest.closed.wait(), SOON)

        self.run_with_guest(body)

    def test_idle_session_sends_keepalive_pings(self):
        old = guest_fs.PING_INTERVAL_S
        guest_fs.PING_INTERVAL_S = 0.05
        try:
            async def body(fs, guest):
                await eventually("keepalive PINGs", lambda: guest.pings >= 2)

            self.run_with_guest(body)
        finally:
            guest_fs.PING_INTERVAL_S = old

    def test_read_is_capped_so_a_huge_request_cannot_buffer_unbounded_data(self):
        # The size comes (via SFTP) from an unauthenticated client and the
        # result is held in memory, so one call must never return -- or fetch --
        # more than FS_READ_LIMIT, however much is asked for.
        old = guest_fs.FS_READ_LIMIT
        guest_fs.FS_READ_LIMIT = 100_000
        try:
            async def body(fs, guest):
                h = await fs.open(b"/f", write=True, create=True)
                await fs.write(h, 0, b"z" * 1_000_000)
                before = len(guest.requests)
                data = await fs.read(h, 0, 4 * 1024 ** 3)
                return len(data), len(guest.requests) - before

            got, requests = self.run_with_guest(body)
        finally:
            guest_fs.FS_READ_LIMIT = old
        self.assertEqual(got, 100_000)
        self.assertLessEqual(requests, 100_000 // guest_fs.FS_CHUNK + 1)

    def test_guest_returning_more_than_requested_is_rejected(self):
        guest = FakeGuest()
        guest.oversize_reads = True

        async def body(fs, guest):
            h = await fs.open(b"/f", write=True, create=True)
            await fs.write(h, 0, b"abc" * 100)
            with self.assertRaises(guest_cmd.GuestCommandError) as cm:
                await fs.read(h, 0, 10)
            self.assertIn("10-byte read", str(cm.exception))

        self.run_with_guest(body, guest)

    def test_unanswered_call_times_out_and_kills_the_session(self):
        old = guest_fs.CALL_TIMEOUT_S
        guest_fs.CALL_TIMEOUT_S = 0.2
        try:
            async def body(fs, guest):
                guest.hold = asyncio.Event()  # the guest never answers
                with self.assertRaises(guest_cmd.GuestCommandError) as cm:
                    await fs.stat(b"/")
                self.assertIn("did not answer", str(cm.exception))
                # Order-matched responses can't recover from a lost one: the
                # session is dead, and says so immediately.
                with self.assertRaises(guest_cmd.GuestCommandError):
                    await asyncio.wait_for(fs.stat(b"/"), SOON)
                guest.hold.set()

            self.run_with_guest(body)
        finally:
            guest_fs.CALL_TIMEOUT_S = old

    def test_request_that_cannot_be_sent_times_out_and_kills_the_session(self):
        # A guest that stops reading leaves the socket full, and the call stuck
        # *sending* -- before any response timeout is armed. One frame far
        # larger than a socket buffer guarantees that is where it is stuck.
        guest = FakeGuest()
        guest.stop_reading = True
        old = guest_fs.CALL_TIMEOUT_S
        guest_fs.CALL_TIMEOUT_S = 0.3
        try:
            async def body(fs, guest):
                with self.assertRaises(guest_cmd.GuestCommandError) as cm:
                    await asyncio.wait_for(fs.call("write", b"x" * (8 * 1024 * 1024), h=1, off=0), SOON)
                self.assertIn("stopped reading", str(cm.exception))
                with self.assertRaises(guest_cmd.GuestCommandError):
                    await asyncio.wait_for(fs.stat(b"/"), SOON)
                guest.closed_by_test.set()

            self.run_with_guest(body, guest)
        finally:
            guest_fs.CALL_TIMEOUT_S = old

    def test_unexpected_reader_failure_fails_pending_calls_instead_of_hanging(self):
        async def body(fs, guest):
            guest.hold = asyncio.Event()
            pending = asyncio.ensure_future(fs.stat(b"/"))
            await eventually("the request to reach the guest", lambda: guest.requests)

            async def boom(_n):
                raise RuntimeError("boom")

            fs._reader.readexactly = boom  # next read in the loop blows up
            guest.hold.set()               # ...which the response triggers
            with self.assertRaises(guest_cmd.GuestCommandError) as cm:
                await asyncio.wait_for(pending, SOON)
            self.assertIn("boom", str(cm.exception))
            with self.assertRaises(guest_cmd.GuestCommandError):
                await asyncio.wait_for(fs.stat(b"/"), SOON)

        self.run_with_guest(body)

    def test_handshake_that_never_completes_times_out(self):
        guest = FakeGuest()
        guest.mute_handshake = True
        old = guest_fs.CONNECT_TIMEOUT_S
        guest_fs.CONNECT_TIMEOUT_S = 0.2

        async def drive():
            server = await asyncio.start_unix_server(guest.serve, self.sock)
            try:
                with self.assertRaises(guest_cmd.GuestCommandError) as cm:
                    await guest_fs.GuestFs.connect(self.sock, 1)
                self.assertIn("timed out", str(cm.exception))
            finally:
                server.close()
                await server.wait_closed()

        try:
            asyncio.run(asyncio.wait_for(drive(), 4 * SOON))
        finally:
            guest_fs.CONNECT_TIMEOUT_S = old

    def test_handshake_failure_is_reported(self):
        async def bad_guest(reader, writer):
            await reader.readline()
            writer.write(b"NOPE\n")
            writer.close()

        async def drive():
            server = await asyncio.start_unix_server(bad_guest, self.sock)
            try:
                with self.assertRaises(guest_cmd.GuestCommandError):
                    await guest_fs.GuestFs.connect(self.sock, 1)
                with self.assertRaises(guest_cmd.GuestCommandError):
                    await guest_fs.GuestFs.connect(self.sock + ".absent", 1)
            finally:
                server.close()
                await server.wait_closed()

        asyncio.run(asyncio.wait_for(drive(), 4 * SOON))


if __name__ == "__main__":
    unittest.main()
