import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import guest_cmd


class FakeSocket:
    def __init__(self, responses):
        self.responses = list(responses)
        self.sent = []

    def sendall(self, data):
        self.sent.append(data)

    def recv(self, _size):
        if self.responses:
            return self.responses.pop(0)
        return b""


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
        self.assertEqual(
            guest_cmd.prepare_command("PATH=/bin echo hi"),
            "export PATH=/igloo/utils:$PATH; PATH=/bin echo hi",
        )

    def test_run_guest_with_socket_validates_connect_response(self):
        sock = FakeSocket([b"ERR 123\n"])

        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "Unexpected response"):
            guest_cmd.run_guest_with_socket(sock, 123, "true")

    def test_run_guest_with_socket_decodes_result(self):
        payload = json.dumps({"stdout": "out", "stderr": "", "exit_code": 0}).encode()
        sock = FakeSocket([b"OK 123\n", payload])

        result = guest_cmd.run_guest_with_socket(sock, 123, "echo out")

        self.assertEqual(result["stdout"], "out")
        self.assertEqual(sock.sent[0], b"CONNECT 123\n")
        self.assertEqual(sock.sent[1], b"export PATH=/igloo/utils:$PATH; echo out")

    def test_decode_response_rejects_invalid_json(self):
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "valid JSON"):
            guest_cmd.decode_response(b"not json")

    def test_decode_response_requires_expected_keys(self):
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "missing 'exit_code'"):
            guest_cmd.decode_response(b'{"stdout": "", "stderr": ""}')

    def test_decode_response_requires_object(self):
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "JSON object"):
            guest_cmd.decode_response(b"[]")

    def test_decode_response_requires_expected_types(self):
        with self.assertRaisesRegex(guest_cmd.GuestCommandError, "exit_code"):
            guest_cmd.decode_response(b'{"stdout": "", "stderr": "", "exit_code": "0"}')


if __name__ == "__main__":
    unittest.main()
