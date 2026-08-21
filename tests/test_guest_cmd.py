import json
import os
import struct
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import guest_cmd  # noqa: E402  (path injected above so the test can import the client)


def _frame(ftype, payload=b""):
    if isinstance(payload, str):
        payload = payload.encode("utf-8")
    return bytes([ftype]) + struct.pack(">I", len(payload)) + payload


class FakeSocket:
    """Byte-accurate fake: recv(n) never returns more than n bytes.

    The frame reader relies on exact-length reads, so unlike the old
    whole-chunk fake this serves from a single byte buffer.
    """

    def __init__(self, incoming=b""):
        self.incoming = bytearray(incoming)
        self.sent = bytearray()

    def sendall(self, data):
        self.sent.extend(data)

    def recv(self, size):
        if not self.incoming:
            return b""
        n = min(size, len(self.incoming))
        chunk = bytes(self.incoming[:n])
        del self.incoming[:n]
        return chunk

    def sent_frames(self):
        """Parse everything sent after the CONNECT line into frames."""
        data = bytes(self.sent)
        nl = data.index(b"\n") + 1
        line, rest = data[:nl], data[nl:]
        frames = []
        i = 0
        while i < len(rest):
            ftype = rest[i]
            (length,) = struct.unpack(">I", rest[i + 1:i + 5])
            payload = rest[i + 5:i + 5 + length]
            frames.append((ftype, payload))
            i += 5 + length
        return line, frames


class GuestCmdTests(unittest.TestCase):
    def test_find_vsocket_returns_first_sorted_match(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            os.makedirs(os.path.join(tmpdir, "b"))
            os.makedirs(os.path.join(tmpdir, "a"))
            open(os.path.join(tmpdir, "b", "vsocket-2"), "w").close()
            open(os.path.join(tmpdir, "a", "vsocket-1"), "w").close()

            expected = os.path.join(tmpdir, "a", "vsocket-1")
            self.assertEqual(guest_cmd.find_vsocket(tmpdir), expected)

    def test_find_vsocket_errors_when_missing(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with self.assertRaisesRegex(guest_cmd.GuestCommandError, "No vsocket"):
                guest_cmd.find_vsocket(tmpdir)

    def test_prepare_command_exports_path(self):
        self.assertEqual(
            guest_cmd.prepare_command("echo hi"),
            "export PATH=/igloo/utils:$PATH; echo hi",
        )

    def test_frame_roundtrip_is_binary_safe(self):
        # A payload with NUL and high bytes must survive the codec intact.
        payload = bytes([0, 255, 1, 0, 254]) + b"tail"
        sock = FakeSocket(_frame(guest_cmd.FRAME_STDOUT, payload))
        ftype, got = guest_cmd.read_frame(sock)
        self.assertEqual(ftype, guest_cmd.FRAME_STDOUT)
        self.assertEqual(got, payload)

    def test_read_frame_clean_eof_returns_none(self):
        self.assertIsNone(guest_cmd.read_frame(FakeSocket(b"")))

    def test_read_frame_rejects_oversize(self):
        hdr = bytes([guest_cmd.FRAME_STDOUT]) + struct.pack(">I", guest_cmd.MAX_FRAME_LEN + 1)
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "exceeds maximum"):
            guest_cmd.read_frame(FakeSocket(hdr))

    def test_run_guest_with_socket_validates_connect_response(self):
        sock = FakeSocket(b"ERR 123\n")
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "Unexpected response"):
            guest_cmd.run_guest_with_socket(sock, 123, "true")

    def test_run_guest_with_socket_sends_request_and_collects_streams(self):
        incoming = (
            b"OK 123\n"
            + _frame(guest_cmd.FRAME_STDOUT, b"out")
            + _frame(guest_cmd.FRAME_STDERR, b"err")
            + _frame(guest_cmd.FRAME_EXIT, json.dumps({"code": 0}).encode())
        )
        sock = FakeSocket(incoming)

        result = guest_cmd.run_guest_with_socket(sock, 123, "echo out")

        self.assertEqual(result, {"stdout": "out", "stderr": "err", "exit_code": 0})
        line, frames = sock.sent_frames()
        self.assertEqual(line, b"CONNECT 123\n")
        self.assertEqual(len(frames), 1)
        ftype, payload = frames[0]
        self.assertEqual(ftype, guest_cmd.FRAME_REQUEST)
        req = json.loads(payload)
        self.assertEqual(req["verb"], "exec")
        self.assertEqual(req["cmd"], "export PATH=/igloo/utils:$PATH; echo out")

    def test_streamed_stdout_reassembles_across_frames(self):
        incoming = (
            b"OK 1\n"
            + _frame(guest_cmd.FRAME_STDOUT, b"foo")
            + _frame(guest_cmd.FRAME_STDOUT, b"bar")
            + _frame(guest_cmd.FRAME_EXIT, json.dumps({"code": 7}).encode())
        )
        result = guest_cmd.run_guest_with_socket(FakeSocket(incoming), 1, "x")
        self.assertEqual(result["stdout"], "foobar")
        self.assertEqual(result["exit_code"], 7)

    def test_error_frame_raises(self):
        incoming = b"OK 1\n" + _frame(
            guest_cmd.FRAME_ERROR, json.dumps({"message": "boom"}).encode()
        )
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "boom"):
            guest_cmd.run_guest_with_socket(FakeSocket(incoming), 1, "x")

    def test_missing_exit_frame_raises(self):
        incoming = b"OK 1\n" + _frame(guest_cmd.FRAME_STDOUT, b"partial")
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "without an EXIT frame"):
            guest_cmd.run_guest_with_socket(FakeSocket(incoming), 1, "x")

    def test_open_pty_request_encodes_verb_and_size(self):
        req = json.loads(guest_cmd.open_pty_request(30, 100))
        self.assertEqual(req, {"verb": "open-pty", "rows": 30, "cols": 100})

    def test_resize_payload_encodes_size(self):
        self.assertEqual(json.loads(guest_cmd.resize_payload(40, 120)), {"rows": 40, "cols": 120})

    def test_shell_handshake_sends_connect_then_open_pty(self):
        sock = FakeSocket(b"OK 7\n")
        guest_cmd.shell_handshake(sock, 7, 24, 80)
        line, frames = sock.sent_frames()
        self.assertEqual(line, b"CONNECT 7\n")
        self.assertEqual(len(frames), 1)
        ftype, payload = frames[0]
        self.assertEqual(ftype, guest_cmd.FRAME_REQUEST)
        self.assertEqual(json.loads(payload), {"verb": "open-pty", "rows": 24, "cols": 80})

    def test_shell_handshake_rejects_bad_ok(self):
        sock = FakeSocket(b"ERR 7\n")
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "Unexpected response"):
            guest_cmd.shell_handshake(sock, 7, 24, 80)

    def test_run_shell_sends_single_stdin_eof_then_stops_reading_stdin(self):
        """After stdin EOF, run_shell must send STDIN_EOF exactly once and then
        only read frames (waiting for EXIT).

        Regression: it used to keep polling the closed stdin and re-send
        STDIN_EOF every loop, spamming a guest that had stopped reading once
        its shell exited. The unconsumed inbound data made the guest RST the
        connection, which on a fast guest beat the EXIT frame -- the client
        then lost the exit code and reported "connection reset by peer".
        Drives the real select/fd path over a unix socketpair + a stdin pipe.
        """
        import socket
        import threading

        tmpdir = tempfile.mkdtemp()
        sockpath = os.path.join(tmpdir, "vsocket")
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(sockpath)
        srv.listen(1)

        stdin_r, stdin_w = os.pipe()

        class _Stdin:
            def fileno(self):
                return stdin_r

        old_stdin = sys.stdin
        sys.stdin = _Stdin()
        result = {}

        def client():
            try:
                result["code"] = guest_cmd.run_shell(sockpath, 7)
            except Exception as e:  # noqa: BLE001 - record for the assertion
                result["err"] = e

        t = threading.Thread(target=client)
        t.start()
        stdin_w_closed = False
        try:
            conn, _ = srv.accept()
            # Transport handshake.
            line = b""
            while not line.endswith(b"\n"):
                line += conn.recv(1)
            self.assertEqual(line, b"CONNECT 7\n")
            conn.sendall(b"OK 7\n")
            # First frame is the open-pty REQUEST.
            ftype, payload = _read_frame_from(conn)
            self.assertEqual(ftype, guest_cmd.FRAME_REQUEST)
            self.assertEqual(json.loads(payload)["verb"], "open-pty")

            # Feed some input, then EOF.
            os.write(stdin_w, b"hi\n")
            os.close(stdin_w)
            stdin_w_closed = True

            # Collect frames until the socket goes quiet (no busy-loop).
            conn.settimeout(2.0)
            eof_count = 0
            saw_stdin = False
            while True:
                try:
                    ftype, payload = _read_frame_from(conn)
                except socket.timeout:
                    break  # client is blocked reading -> not spamming stdin
                if ftype == guest_cmd.FRAME_STDIN:
                    saw_stdin = True
                elif ftype == guest_cmd.FRAME_STDIN_EOF:
                    eof_count += 1
                    if eof_count > 1:
                        break  # the bug: don't hang collecting spam

            self.assertTrue(saw_stdin, "client never forwarded stdin bytes")
            self.assertEqual(eof_count, 1, "client should send STDIN_EOF exactly once")

            # A clean EXIT should be received and its code returned.
            conn.sendall(_frame(guest_cmd.FRAME_EXIT, json.dumps({"code": 0}).encode()))
            conn.close()
            t.join(timeout=5)
            self.assertFalse(t.is_alive(), "run_shell did not return")
            self.assertNotIn("err", result, f"run_shell raised: {result.get('err')}")
            self.assertEqual(result.get("code"), 0)
        finally:
            sys.stdin = old_stdin
            if not stdin_w_closed:
                os.close(stdin_w)
            os.close(stdin_r)
            srv.close()


def _read_frame_from(conn):
    """Read one length-prefixed frame from a blocking socket."""
    hdr = b""
    while len(hdr) < 5:
        chunk = conn.recv(5 - len(hdr))
        if not chunk:
            raise EOFError("closed before frame header")
        hdr += chunk
    (length,) = struct.unpack(">I", hdr[1:5])
    payload = b""
    while len(payload) < length:
        chunk = conn.recv(length - len(payload))
        if not chunk:
            raise EOFError("closed before frame payload")
        payload += chunk
    return hdr[0], payload


if __name__ == "__main__":
    unittest.main()
