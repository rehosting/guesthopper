#!/usr/bin/env python3
"""Host-side telnet front door for the guest command channel.

Runs OUTSIDE the guest (in the penguin container) and bridges a TCP telnet
client to a vsock ``open-pty`` session on the guest. Users get the familiar
``telnet <host> <port>`` experience while the guest only ever speaks the clean
vsock command channel -- there is no in-guest serial telnet listener and ttyS1
stays free. The internal transport is guesthopper's frame protocol; this module
reuses ``guest_cmd.py``'s codec and pty handshake, so the wire contract lives in
exactly one place.

``ssh_gateway.py`` is the sibling SSH front door: the same bridge with an SSH
terminator in place of the telnet one. Keeping the telnet protocol handling
isolated in ``TelnetInbound`` / ``telnet_escape`` kept that a localized change.

Only the telnet<->frame translation is subtle, so it is factored into pure
functions/classes that are unit-tested without a guest:
  * ``TelnetInbound``  -- client->server: strip IAC negotiation, surface data
    bytes and NAWS (window-size) updates, and produce the minimal negotiation
    replies needed to avoid a client stalling on an unanswered option.
  * ``telnet_escape``  -- server->client: double 0xFF so guest output can't be
    misread as a telnet IAC command.
"""

import argparse
import select
import socket
import sys
import threading

import guest_cmd
from guest_cmd import (
    FRAME_ERROR,
    FRAME_EXIT,
    FRAME_PING,
    FRAME_RESIZE,
    FRAME_STDERR,
    FRAME_STDOUT,
    FRAME_STDIN,
    FRAME_READ_TIMEOUT_S,
    GuestCommandError,
    PING_INTERVAL_S,
    find_vsocket,
    read_frame,
    resize_payload,
    shell_handshake,
    write_frame,
)

# Telnet protocol bytes (RFC 854 / 1073 / 1184).
IAC = 255   # Interpret As Command: prefixes every telnet command.
SE = 240    # End of subnegotiation.
SB = 250    # Begin subnegotiation.
WILL = 251
WONT = 252
DO = 253
DONT = 254

OPT_ECHO = 1    # RFC 857
OPT_SGA = 3     # Suppress Go Ahead (RFC 858): character-at-a-time mode.
OPT_NAWS = 31   # Negotiate About Window Size (RFC 1073).

# We echo and suppress-go-ahead on the server side so the client sends keystrokes
# immediately (no local line editing/echo) and the guest pty does the echoing,
# and we ask the client to report its window size so we can forward RESIZE.
INITIAL_NEGOTIATION = bytes(
    [
        IAC, WILL, OPT_ECHO,
        IAC, WILL, OPT_SGA,
        IAC, DO, OPT_NAWS,
    ]
)


def telnet_escape(data):
    """Server->client: double IAC (0xFF) so a literal 0xFF in guest output is
    not parsed by the client as the start of a telnet command."""
    return data.replace(b"\xff", b"\xff\xff")


class TelnetInbound:
    """Stateful parser for the client->server telnet byte stream.

    ``feed(chunk)`` returns ``(data, resizes, replies)``:
      * ``data``    -- application bytes to forward to the guest as STDIN
                       (IAC IAC already collapsed to a single 0xFF).
      * ``resizes`` -- list of ``(rows, cols)`` from NAWS subnegotiations.
      * ``replies`` -- telnet bytes to send back to the client (option refusals),
                       so an option we don't support can't leave the client
                       waiting for an answer.

    The state survives across calls, so IAC sequences split over recv()
    boundaries are handled correctly.
    """

    def __init__(self):
        self._state = "data"
        self._cmd = None          # pending WILL/WONT/DO/DONT awaiting its option
        self._sb = bytearray()    # subnegotiation payload accumulator

    def feed(self, chunk):
        data = bytearray()
        resizes = []
        replies = bytearray()
        for b in chunk:
            if self._state == "data":
                if b == IAC:
                    self._state = "iac"
                else:
                    data.append(b)
            elif self._state == "iac":
                if b == IAC:
                    data.append(IAC)  # escaped literal 0xFF
                    self._state = "data"
                elif b in (WILL, WONT, DO, DONT):
                    self._cmd = b
                    self._state = "opt"
                elif b == SB:
                    self._sb = bytearray()
                    self._state = "sb"
                else:
                    # NOP/DM/other 2-byte commands: nothing to do.
                    self._state = "data"
            elif self._state == "opt":
                replies.extend(self._negotiate(self._cmd, b))
                self._cmd = None
                self._state = "data"
            elif self._state == "sb":
                if b == IAC:
                    self._state = "sb_iac"
                else:
                    self._sb.append(b)
            elif self._state == "sb_iac":
                if b == SE:
                    self._end_subneg(resizes)
                    self._state = "data"
                elif b == IAC:
                    self._sb.append(IAC)  # escaped 0xFF inside subnegotiation
                    self._state = "sb"
                else:
                    # Malformed (IAC not followed by SE/IAC) -- end the subneg.
                    self._end_subneg(resizes)
                    self._state = "data"
        return bytes(data), resizes, bytes(replies)

    def _negotiate(self, cmd, opt):
        """Minimal, loop-free responses to client option negotiation.

        We proactively offered WILL ECHO / WILL SGA / DO NAWS, so a client
        agreeing to those needs no reply. Anything else we refuse, and we never
        reply to a refusal (WONT/DONT) -- both rules prevent negotiation loops.
        """
        if cmd == DO:
            if opt in (OPT_ECHO, OPT_SGA):
                return b""  # already offered
            return bytes([IAC, WONT, opt])
        if cmd == WILL:
            if opt == OPT_NAWS:
                return b""  # we asked for this
            return bytes([IAC, DONT, opt])
        # WONT / DONT: acknowledgement of a refusal; stay silent.
        return b""

    def _end_subneg(self, resizes):
        sb = self._sb
        # NAWS: option byte, then width (2 bytes, high first) and height.
        if len(sb) >= 5 and sb[0] == OPT_NAWS:
            cols = (sb[1] << 8) | sb[2]
            rows = (sb[3] << 8) | sb[4]
            # A zero dimension means "unspecified"; skip rather than forward 0.
            if rows and cols:
                resizes.append((rows, cols))
        self._sb = bytearray()


def bridge(tcp_conn, unix_socket, port):
    """Bridge one connected telnet client to a fresh guest pty session.

    Runs until either side closes; closing the guest socket makes the guest hang
    up the shell (its read side sees EOF), so a telnet disconnect tears down the
    guest shell like a real terminal HUP.
    """
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as gsock:
        gsock.connect(unix_socket)
        gsock.settimeout(FRAME_READ_TIMEOUT_S)
        # Open the pty at a default size; the client's NAWS (below) resizes it.
        shell_handshake(gsock, port, 24, 80)
        tcp_conn.sendall(INITIAL_NEGOTIATION)

        parser = TelnetInbound()
        watch = [tcp_conn, gsock]
        while True:
            readable, _, _ = select.select(watch, [], [], PING_INTERVAL_S)
            if not readable:
                # Idle: keepalive so the guest agent doesn't time the session out.
                write_frame(gsock, FRAME_PING)
                continue

            if tcp_conn in readable:
                chunk = tcp_conn.recv(4096)
                if not chunk:
                    break  # client hung up -> close gsock -> guest hangs up shell
                data, resizes, replies = parser.feed(chunk)
                if replies:
                    tcp_conn.sendall(replies)
                for rows, cols in resizes:
                    write_frame(gsock, FRAME_RESIZE, resize_payload(rows, cols))
                if data:
                    write_frame(gsock, FRAME_STDIN, data)

            if gsock in readable:
                frame = read_frame(gsock)
                if frame is None:
                    break
                ftype, payload = frame
                if ftype in (FRAME_STDOUT, FRAME_STDERR):
                    tcp_conn.sendall(telnet_escape(payload))
                elif ftype == FRAME_EXIT:
                    break
                elif ftype == FRAME_ERROR:
                    msg = guest_cmd._parse_json(payload).get("message", "")
                    tcp_conn.sendall(
                        telnet_escape(f"\r\n[gateway] guest agent error: {msg}\r\n".encode())
                    )
                    break
                # Unknown frame types: ignored for forward compatibility.


def _serve_client(tcp_conn, addr, unix_socket, port):
    try:
        bridge(tcp_conn, unix_socket, port)
    except (OSError, GuestCommandError) as e:
        try:
            tcp_conn.sendall(
                telnet_escape(f"\r\n[gateway] connection closed: {e}\r\n".encode())
            )
        except OSError:
            pass
    finally:
        try:
            tcp_conn.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        tcp_conn.close()


def serve(listen_host, listen_port, unix_socket, port):
    """Accept telnet clients forever, one guest pty session per connection."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind((listen_host, listen_port))
        srv.listen(8)
        print(
            f"telnet_gateway: listening on {listen_host}:{listen_port} -> "
            f"vsock {unix_socket} port {port}",
            file=sys.stderr,
        )
        while True:
            conn, addr = srv.accept()
            # One thread per client; the guest supports concurrent sessions
            # (bounded by the agent's own session cap).
            t = threading.Thread(
                target=_serve_client,
                args=(conn, addr, unix_socket, port),
                daemon=True,
            )
            t.start()


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Bridge a TCP telnet client to a guest vsock pty session."
    )
    parser.add_argument(
        "--listen-host",
        default="127.0.0.1",
        help="Address to serve telnet on (default 127.0.0.1).",
    )
    parser.add_argument(
        "--listen-port",
        type=int,
        default=2323,
        help="TCP port to serve telnet on (default 2323).",
    )
    parser.add_argument(
        "--socket",
        default=None,
        help="Unix socket made by vhost-device-vsock. Defaults to searching "
        "/tmp for 'vsocket'.",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=12341234,
        help="Vsock port to connect to (default 12341234).",
    )
    args = parser.parse_args(argv)

    try:
        unix_socket = args.socket if args.socket is not None else find_vsocket()
    except GuestCommandError as e:
        print(f"telnet_gateway: {e}", file=sys.stderr)
        return 1

    try:
        serve(args.listen_host, args.listen_port, unix_socket, args.port)
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
