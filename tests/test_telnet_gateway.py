import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import telnet_gateway as tg  # noqa: E402  (path injected above)
from telnet_gateway import (  # noqa: E402
    DO,
    DONT,
    IAC,
    OPT_ECHO,
    OPT_NAWS,
    OPT_SGA,
    SB,
    SE,
    WILL,
    WONT,
)


class TelnetEscapeTests(unittest.TestCase):
    def test_iac_is_doubled(self):
        self.assertEqual(tg.telnet_escape(b"a\xffb"), b"a\xff\xffb")

    def test_plain_data_unchanged(self):
        self.assertEqual(tg.telnet_escape(b"hello\r\n"), b"hello\r\n")


class TelnetInboundTests(unittest.TestCase):
    def setUp(self):
        self.p = tg.TelnetInbound()

    def test_plain_data_passes_through(self):
        data, resizes, replies = self.p.feed(b"ls -la\n")
        self.assertEqual(data, b"ls -la\n")
        self.assertEqual(resizes, [])
        self.assertEqual(replies, b"")

    def test_escaped_iac_becomes_single_ff(self):
        # IAC IAC in the client stream is a literal 0xFF data byte.
        data, _, _ = self.p.feed(bytes([ord("x"), IAC, IAC, ord("y")]))
        self.assertEqual(data, b"x\xffy")

    def test_will_naws_gets_no_reply(self):
        # We proactively sent DO NAWS, so a client WILL NAWS needs no answer.
        data, resizes, replies = self.p.feed(bytes([IAC, WILL, OPT_NAWS]))
        self.assertEqual(data, b"")
        self.assertEqual(replies, b"")

    def test_do_echo_and_sga_get_no_reply(self):
        # We already offered WILL ECHO / WILL SGA.
        _, _, replies = self.p.feed(bytes([IAC, DO, OPT_ECHO, IAC, DO, OPT_SGA]))
        self.assertEqual(replies, b"")

    def test_unknown_do_is_refused_with_wont(self):
        _, _, replies = self.p.feed(bytes([IAC, DO, 99]))
        self.assertEqual(replies, bytes([IAC, WONT, 99]))

    def test_unknown_will_is_refused_with_dont(self):
        _, _, replies = self.p.feed(bytes([IAC, WILL, 77]))
        self.assertEqual(replies, bytes([IAC, DONT, 77]))

    def test_refusals_are_not_answered(self):
        # WONT/DONT are acknowledgements; replying would risk a loop.
        _, _, replies = self.p.feed(bytes([IAC, WONT, 5, IAC, DONT, 6]))
        self.assertEqual(replies, b"")

    def test_naws_subnegotiation_yields_resize(self):
        # NAWS carries width then height, each 2 bytes big-endian.
        # 132 cols x 43 rows.
        sb = bytes([IAC, SB, OPT_NAWS, 0, 132, 0, 43, IAC, SE])
        data, resizes, _ = self.p.feed(sb)
        self.assertEqual(data, b"")
        self.assertEqual(resizes, [(43, 132)])

    def test_naws_with_escaped_ff_dimension(self):
        # A width byte of 0xFF must be sent as IAC IAC inside the subneg; the
        # parser must collapse it and recover width=255.
        sb = bytes([IAC, SB, OPT_NAWS, 0, IAC, IAC, 0, 24, IAC, SE])
        _, resizes, _ = self.p.feed(sb)
        self.assertEqual(resizes, [(24, 255)])

    def test_naws_split_across_feeds(self):
        # An IAC sequence spanning two recv() chunks must still parse.
        part1 = bytes([ord("a"), IAC, SB, OPT_NAWS, 0, 80])
        part2 = bytes([0, 24, IAC, SE, ord("b")])
        d1, r1, _ = self.p.feed(part1)
        d2, r2, _ = self.p.feed(part2)
        self.assertEqual(d1, b"a")
        self.assertEqual(r1, [])
        self.assertEqual(d2, b"b")
        self.assertEqual(r2, [(24, 80)])

    def test_zero_dimension_naws_is_ignored(self):
        # A zero width/height means "unspecified"; don't forward a 0x0 resize.
        sb = bytes([IAC, SB, OPT_NAWS, 0, 0, 0, 24, IAC, SE])
        _, resizes, _ = self.p.feed(sb)
        self.assertEqual(resizes, [])

    def test_data_around_command_is_preserved(self):
        stream = b"echo " + bytes([IAC, DO, 99]) + b"hi\n"
        data, _, replies = self.p.feed(stream)
        self.assertEqual(data, b"echo hi\n")
        self.assertEqual(replies, bytes([IAC, WONT, 99]))


if __name__ == "__main__":
    unittest.main()
