#!/usr/bin/env python3
"""Host-side SSH front door for the guest command channel.

The second front door (after telnet_gateway.py): runs OUTSIDE the guest (in the
penguin container) and bridges an SSH client to a vsock ``open-pty`` session on
the guest. Same idea as the telnet gateway -- a real interactive pty (job
control, echo, window resize, TUI apps) -- but terminated with SSH instead of
telnet, so the SSH client's pty request/window-change map onto our RESIZE frame.

The guest never runs sshd and holds no key material: SSH is terminated entirely
here, on the host side, and only the clean vsock frame protocol crosses into the
guest. The wire contract (frame types, handshake) is shared with guest_cmd.py.

Auth is OPEN (any username, no password/key), mirroring the telnet door and the
serial/telnet console it replaces: access is gated by being able to reach the
listener. penguin_run binds it on the container's network (0.0.0.0) but publishes
nothing to the host unless the run adds -p, so it answers on the container IP
like a real device. There is deliberately no key-auth path yet; if a keyed
lockdown is ever wanted, add an --authorized-keys arg and have begin_auth return
True with a validate_public_key implementation.

asyncssh provides the SSH protocol. It is imported lazily/guarded so this module
(and its frame codec, which is unit-tested) still imports on a host without
asyncssh; the SSH server itself needs it at run time.
"""

import argparse
import asyncio
import json
import sys

from guest_cmd import (
    FRAME_ERROR,
    FRAME_EXIT,
    FRAME_PING,
    FRAME_RESIZE,
    FRAME_REQUEST,
    FRAME_STDERR,
    FRAME_STDIN,
    FRAME_STDIN_EOF,
    FRAME_STDOUT,
    MAX_FRAME_LEN,
    PING_INTERVAL_S,
    GuestCommandError,
    _parse_json,
    find_vsocket,
)

# SSH->guest inbound backpressure: when the queue of not-yet-forwarded client
# events reaches the high-water mark (the guest/vsock side has stalled and isn't
# draining) we pause reading the SSH channel, and resume once it drains below the
# low-water mark. This bounds gateway memory instead of buffering a flooding
# client without limit.
INBOUND_HIGH_WATER = 1024
INBOUND_LOW_WATER = 256

try:
    import asyncssh
except ImportError:  # pragma: no cover - exercised only where asyncssh is absent
    asyncssh = None


# --- Frame codec over asyncio streams (mirrors guest_cmd.py's sync codec) ----

def encode_frame(ftype, payload=b""):
    """Serialize one frame: [type:u8][len:u32-be][payload]. Same wire format as
    guest_cmd.write_frame -- kept in lockstep so both clients interoperate."""
    if isinstance(payload, str):
        payload = payload.encode("utf-8")
    if len(payload) > MAX_FRAME_LEN:
        raise GuestCommandError(f"frame payload {len(payload)} exceeds {MAX_FRAME_LEN}")
    return bytes([ftype]) + len(payload).to_bytes(4, "big") + payload


async def write_frame(writer, ftype, payload=b""):
    writer.write(encode_frame(ftype, payload))
    await writer.drain()


async def read_frame(reader):
    """Read one frame from an asyncio StreamReader, or None at a clean EOF."""
    try:
        hdr = await reader.readexactly(5)
    except asyncio.IncompleteReadError:
        return None
    ftype = hdr[0]
    length = int.from_bytes(hdr[1:5], "big")
    if length > MAX_FRAME_LEN:
        raise GuestCommandError(f"frame payload {length} exceeds maximum {MAX_FRAME_LEN}")
    if length == 0:
        return ftype, b""
    try:
        payload = await reader.readexactly(length)
    except asyncio.IncompleteReadError as e:
        raise GuestCommandError("connection closed mid-frame") from e
    return ftype, payload


def open_pty_request(rows, cols):
    return json.dumps({"verb": "open-pty", "rows": rows, "cols": cols}).encode("utf-8")


def exec_request(command):
    return json.dumps({"verb": "exec", "cmd": command}).encode("utf-8")


def resize_payload(rows, cols):
    return json.dumps({"rows": rows, "cols": cols}).encode("utf-8")


# --- SSH glue (needs asyncssh; validation-pending -- no asyncssh/guest here) --
#
# Defined only when asyncssh is importable so this module loads for unit tests
# of the codec above on hosts without asyncssh.

if asyncssh is not None:

    class VsockShellSession(asyncssh.SSHServerSession):
        """One SSH session <-> one guest vsock pty (or exec) session.

        asyncssh drives the SSH side (pty request, data, window-change, EOF); we
        open the vsock session and shuttle bytes/frames between them. A periodic
        PING keeps the guest agent's idle timeout from tripping while the user
        sits at the prompt.
        """

        def __init__(self, uds_path, vsock_port):
            self._uds_path = uds_path
            self._vsock_port = vsock_port
            self._chan = None
            self._want_pty = False
            self._command = None            # set if the client requested `ssh host CMD`
            self._term_size = (80, 24)       # (cols, rows)
            self._reader = None
            self._writer = None
            # SSH-side events can arrive before the vsock is connected; queue them.
            self._inbound = asyncio.Queue()
            self._tasks = []
            self._reading_paused = False
            # Set == the SSH client is keeping up with guest output. asyncssh
            # clears it via pause_writing() when its outbound buffer to the client
            # fills, so _pump_outbound stops reading guest frames rather than
            # buffering without bound when the client stops reading.
            self._write_resumed = asyncio.Event()
            self._write_resumed.set()

        # -- asyncssh callbacks --
        def connection_made(self, chan):
            self._chan = chan

        def pty_requested(self, term_type, term_size, term_modes):
            self._want_pty = True
            cols, rows = term_size[0], term_size[1]
            self._term_size = (cols or 80, rows or 24)
            return True

        def shell_requested(self):
            return True

        def exec_requested(self, command):
            # With encoding=None (a binary channel) asyncssh delivers the exec
            # command as bytes; decode it for the JSON exec request. Lossy so a
            # non-UTF-8 command can't crash the session with a TypeError in
            # json.dumps.
            if isinstance(command, (bytes, bytearray)):
                command = bytes(command).decode("utf-8", "replace")
            self._command = command
            return True

        def terminal_size_changed(self, width, height, pixwidth, pixheight):
            self._enqueue(("resize", (height, width)))

        def data_received(self, data, datatype):
            self._enqueue(("data", data))

        def eof_received(self):
            self._enqueue(("eof", None))
            return True  # keep the channel open to keep reading guest output

        def _enqueue(self, item):
            """Queue an SSH-side event, applying backpressure if we're behind."""
            self._inbound.put_nowait(item)
            if (not self._reading_paused
                    and self._chan is not None
                    and self._inbound.qsize() >= INBOUND_HIGH_WATER):
                try:
                    self._chan.pause_reading()
                    self._reading_paused = True
                except Exception:  # noqa: BLE001
                    pass

        def pause_writing(self):
            # asyncssh's outbound buffer to the SSH client is full: stop pulling
            # guest output until it drains (resume_writing), so a client that
            # stops reading can't make us buffer guest output without bound.
            self._write_resumed.clear()

        def resume_writing(self):
            self._write_resumed.set()

        def session_started(self):
            self._tasks.append(asyncio.ensure_future(self._run()))

        def connection_lost(self, exc):
            for t in self._tasks:
                t.cancel()
            # Closing the vsock writer makes the guest see read-EOF and hang up
            # the pty shell -- an SSH disconnect thus tears down the guest shell.
            if self._writer is not None:
                try:
                    self._writer.close()
                except Exception:  # noqa: BLE001
                    pass

        # -- bridge --
        async def _run(self):
            try:
                self._reader, self._writer = await asyncio.open_unix_connection(self._uds_path)
            except OSError as e:
                self._chan.write(f"ssh_gateway: cannot reach guest vsock: {e}\r\n".encode())
                self._chan.exit(1)
                return

            # vhost hybrid handshake, then request a pty shell (or exec).
            self._writer.write(f"CONNECT {self._vsock_port}\n".encode())
            await self._writer.drain()
            line = (await self._reader.readline()).strip()
            if line != f"OK {self._vsock_port}".encode():
                self._chan.write(b"ssh_gateway: vsock handshake failed\r\n")
                self._chan.exit(1)
                return

            if self._command is not None and not self._want_pty:
                await write_frame(self._writer, FRAME_REQUEST, exec_request(self._command))
            else:
                cols, rows = self._term_size
                await write_frame(self._writer, FRAME_REQUEST, open_pty_request(rows, cols))

            self._tasks.append(asyncio.ensure_future(self._pump_inbound()))
            self._tasks.append(asyncio.ensure_future(self._keepalive()))
            try:
                await self._pump_outbound()
            finally:
                # The guest session ended: stop the sibling pumps and close the
                # vsock now, rather than leaking them (a keepalive PINGing a dead
                # guest, an open vsock socket) until the SSH channel closes.
                current = asyncio.current_task()
                for t in self._tasks:
                    if t is not current:
                        t.cancel()
                if self._writer is not None:
                    try:
                        self._writer.close()
                    except Exception:  # noqa: BLE001
                        pass

        async def _pump_inbound(self):
            """SSH -> guest: stdin, window resize, EOF."""
            while True:
                kind, val = await self._inbound.get()
                if (self._reading_paused
                        and self._inbound.qsize() <= INBOUND_LOW_WATER):
                    try:
                        self._chan.resume_reading()
                        self._reading_paused = False
                    except Exception:  # noqa: BLE001
                        pass
                try:
                    if kind == "data":
                        await write_frame(self._writer, FRAME_STDIN, val)
                    elif kind == "resize":
                        rows, cols = val
                        await write_frame(self._writer, FRAME_RESIZE, resize_payload(rows, cols))
                    elif kind == "eof":
                        await write_frame(self._writer, FRAME_STDIN_EOF)
                except (OSError, ConnectionError):
                    break

        async def _keepalive(self):
            """Send a PING every PING_INTERVAL_S so the guest agent's idle timeout
            never trips while the session is live but quiet."""
            while True:
                await asyncio.sleep(PING_INTERVAL_S)
                try:
                    await write_frame(self._writer, FRAME_PING)
                except (OSError, ConnectionError):
                    break

        async def _pump_outbound(self):
            """guest -> SSH: stream stdout/stderr, propagate EXIT/ERROR."""
            try:
                while True:
                    # If the SSH client has stopped reading (asyncssh's write
                    # buffer is full), wait rather than pulling more guest output
                    # into an unbounded buffer.
                    if not self._write_resumed.is_set():
                        await self._write_resumed.wait()
                    frame = await read_frame(self._reader)
                    if frame is None:
                        break
                    ftype, payload = frame
                    if ftype == FRAME_STDOUT:
                        self._chan.write(payload)
                    elif ftype == FRAME_STDERR:
                        # A pty has a single tty stream, so fold stderr into it;
                        # for an exec (non-pty) session keep it on the SSH stderr
                        # stream so `ssh host cmd 2>err` separates correctly.
                        if self._want_pty:
                            self._chan.write(payload)
                        else:
                            self._chan.write_stderr(payload)
                    elif ftype == FRAME_EXIT:
                        # _parse_json (not raw json.loads) so a malformed EXIT
                        # from a buggy guest raises GuestCommandError -- caught
                        # below -- instead of an unhandled ValueError/AttributeError
                        # that would kill this task and leak the siblings.
                        code = _parse_json(payload or b"{}").get("code", 0)
                        self._chan.exit(code if isinstance(code, int) else 0)
                        return
                    elif ftype == FRAME_ERROR:
                        msg = _parse_json(payload or b"{}").get("message", "")
                        self._chan.write(f"\r\nssh_gateway: guest agent error: {msg}\r\n".encode())
                        self._chan.exit(1)
                        return
                    # Unknown frame types ignored for forward compatibility.
            except (GuestCommandError, OSError, ConnectionError) as e:
                try:
                    self._chan.write(f"\r\nssh_gateway: {e}\r\n".encode())
                except Exception:  # noqa: BLE001
                    pass
            self._chan.exit(0)

    class _GatewayServer(asyncssh.SSHServer):
        def __init__(self, uds_path, vsock_port):
            self._uds_path = uds_path
            self._vsock_port = vsock_port

        def begin_auth(self, username):
            # Returning False means "no authentication required" for this user --
            # mirrors the telnet door's open, reachability-gated access model.
            return False

        def session_requested(self):
            return VsockShellSession(self._uds_path, self._vsock_port)

    async def _serve(listen_host, listen_port, uds_path, vsock_port, host_keys):
        await asyncssh.create_server(
            lambda: _GatewayServer(uds_path, vsock_port),
            listen_host,
            listen_port,
            server_host_keys=host_keys,
            # Binary channels: the pty stream carries arbitrary bytes, not text.
            encoding=None,
        )
        print(
            f"ssh_gateway: listening on {listen_host}:{listen_port} -> "
            f"vsock {uds_path} port {vsock_port}",
            file=sys.stderr,
        )
        await asyncio.Event().wait()  # serve forever

    def _load_host_keys(path):
        if path:
            return [asyncssh.read_private_key(path)]
        # Ephemeral per-process host key. Clients reaching a fresh run get a
        # host-key-changed prompt; fine for a dev console reached via docker exec
        # (use StrictHostKeyChecking=no / UserKnownHostsFile=/dev/null).
        return [asyncssh.generate_private_key("ssh-ed25519")]


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Bridge an SSH client to a guest vsock pty session."
    )
    parser.add_argument("--listen-host", default="127.0.0.1",
                        help="Address to serve SSH on (default 127.0.0.1).")
    parser.add_argument("--listen-port", type=int, default=2222,
                        help="TCP port to serve SSH on (default 2222).")
    parser.add_argument("--socket", default=None,
                        help="Unix socket made by vhost-device-vsock. Defaults to "
                        "searching /tmp for 'vsocket'.")
    parser.add_argument("--port", type=int, default=12341234,
                        help="Vsock port to connect to (default 12341234).")
    parser.add_argument("--host-key", default=None,
                        help="Path to an SSH host private key. Default: ephemeral.")
    args = parser.parse_args(argv)

    if asyncssh is None:
        print("ssh_gateway: asyncssh is not installed", file=sys.stderr)
        return 1

    try:
        uds_path = args.socket if args.socket is not None else find_vsocket()
    except GuestCommandError as e:
        print(f"ssh_gateway: {e}", file=sys.stderr)
        return 1

    host_keys = _load_host_keys(args.host_key)
    try:
        asyncio.run(_serve(args.listen_host, args.listen_port, uds_path, args.port, host_keys))
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
