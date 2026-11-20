"""Host-side async client for the guest agent's ``fs`` verb (file service).

The guest agent (guesthopper, src/files.rs) serves plain file operations --
open/read/write/stat/readdir/... -- natively over the command channel. This is
the client for that RPC; ssh_gateway.py builds its SFTP/scp support on it, so
files move in and out of a live guest without the guest running sshd or
sftp-server, or needing any userland tools.

Wire contract (must match src/files.rs): after the vsock handshake and a
``{"verb": "fs"}`` REQUEST, each request is a ``FRAME_FS_REQ`` and each response
a ``FRAME_FS_RESP``, both with payload ``[hdr_len: u32 be][JSON hdr][data]``.
The agent answers strictly in request order, so calls are pipelined: many may be
in flight on one connection and responses are matched first-in first-out.

Paths are ``bytes`` here and hex-encoded on the wire (firmware filenames are not
reliably UTF-8).
"""

import asyncio
import collections
import json

from guest_cmd import (
    FRAME_ERROR,
    FRAME_PING,
    FRAME_REQUEST,
    MAX_FRAME_LEN,
    PING_INTERVAL_S,
    GuestCommandError,
    _parse_json,
)

# Frame types -- must match guesthopper/src/frame.rs.
FRAME_FS_REQ = 6
FRAME_FS_RESP = 20

# Largest read the agent serves in one response (FS_READ_MAX in files.rs), and
# the largest write we send in one frame. The write size is chosen to stay well
# under the agent's inbound frame cap even at its 64 KiB floor.
FS_CHUNK = 32 * 1024

# How many chunk requests of one read()/write() may be in flight at once. Bounds
# what a single large SFTP request can queue in the agent's outbound buffer.
FS_PIPELINE = 16

# Most that one read() returns. The whole result is held in memory, and the
# size ultimately comes from an unauthenticated SSH client, so it must be
# bounded here; callers wanting more read again at the next offset (a short
# read is not end-of-file -- an empty one is).
FS_READ_LIMIT = 4 * 1024 * 1024

# Bound on the vsock handshake, and on sending any one request or waiting for
# its response. The agent
# gives a single file operation 120 s (FS_OP_TIMEOUT in files.rs) before it
# abandons the session, so a response later than that is never coming -- e.g.
# the agent is at its session cap and this connection is parked in its accept
# backlog.
CONNECT_TIMEOUT_S = 30
CALL_TIMEOUT_S = 150


class GuestFsError(GuestCommandError):
    """A file operation the guest refused. ``kind`` is the agent's portable
    error class: noent, perm, exist, invalid, unsupported or fail."""

    def __init__(self, kind, message, errno=0):
        super().__init__(message)
        self.kind = kind
        self.errno = errno


def encode_frame(ftype, payload=b""):
    if len(payload) > MAX_FRAME_LEN:
        raise GuestCommandError(f"frame payload {len(payload)} exceeds {MAX_FRAME_LEN}")
    return bytes([ftype]) + len(payload).to_bytes(4, "big") + payload


def encode_fs_payload(header, data=b""):
    """``[hdr_len: u32 be][JSON hdr][data]``"""
    hdr = json.dumps(header, separators=(",", ":")).encode("utf-8")
    return len(hdr).to_bytes(4, "big") + hdr + data


def decode_fs_payload(payload):
    """Inverse of encode_fs_payload: returns ``(header_dict, data_bytes)``."""
    if len(payload) < 4:
        raise GuestCommandError("fs frame too short")
    n = int.from_bytes(payload[:4], "big")
    if n > len(payload) - 4:
        raise GuestCommandError("fs frame header length exceeds payload")
    return _parse_json(payload[4:4 + n]), payload[4 + n:]


def _path(p):
    return bytes(p).hex()


class GuestFs:
    """One ``fs`` session on the guest. Create with :meth:`connect`.

    Every method is a coroutine that raises :class:`GuestFsError` if the guest
    refuses the operation, or :class:`GuestCommandError` if the channel itself
    fails (after which the session is dead and every later call fails fast).
    """

    def __init__(self, reader, writer):
        self._reader = reader
        self._writer = writer
        self._pending = collections.deque()  # futures, in request order
        self._dead = None  # the exception that killed the session, once dead
        self._tasks = [
            asyncio.ensure_future(self._read_loop()),
            asyncio.ensure_future(self._keepalive()),
        ]

    @classmethod
    async def connect(cls, uds_path, vsock_port):
        """Open a new ``fs`` session through the vhost-device-vsock socket."""
        try:
            reader, writer = await asyncio.open_unix_connection(uds_path)
        except OSError as e:
            raise GuestCommandError(f"cannot reach guest vsock {uds_path}: {e}") from e
        try:
            writer.write(f"CONNECT {vsock_port}\n".encode())
            await writer.drain()
            line = (await asyncio.wait_for(reader.readline(), CONNECT_TIMEOUT_S)).strip()
            if line != f"OK {vsock_port}".encode():
                raise GuestCommandError(f"vsock handshake failed: {line!r}")
            writer.write(encode_frame(FRAME_REQUEST, json.dumps({"verb": "fs"}).encode()))
            await writer.drain()
        except asyncio.TimeoutError:
            writer.close()
            raise GuestCommandError("vsock handshake timed out") from None
        except (OSError, ValueError) as e:
            # ValueError: an over-long handshake line (StreamReader's limit).
            writer.close()
            raise GuestCommandError(f"vsock handshake failed: {e}") from e
        except GuestCommandError:
            writer.close()
            raise
        return cls(reader, writer)

    # -- transport --

    async def _read_loop(self):
        err = GuestCommandError("guest closed the fs session")
        try:
            while True:
                hdr = await self._reader.readexactly(5)
                length = int.from_bytes(hdr[1:5], "big")
                if length > MAX_FRAME_LEN:
                    raise GuestCommandError(f"frame payload {length} exceeds {MAX_FRAME_LEN}")
                payload = await self._reader.readexactly(length) if length else b""
                if hdr[0] == FRAME_FS_RESP:
                    if not self._pending:
                        raise GuestCommandError("unsolicited fs response from guest")
                    fut = self._pending.popleft()
                    if not fut.done():
                        fut.set_result(payload)
                elif hdr[0] == FRAME_ERROR:
                    # Session-fatal (e.g. an agent too old to know the verb).
                    msg = _parse_json(payload or b"{}").get("message", "")
                    raise GuestCommandError(f"guest agent error: {msg}")
                # Unknown frame types ignored for forward compatibility.
        except asyncio.CancelledError:
            err = GuestCommandError("fs session closed")
        except (asyncio.IncompleteReadError, OSError):
            pass
        except GuestCommandError as e:
            err = e
        except Exception as e:  # noqa: BLE001
            # Whatever went wrong, the session must be marked dead: a reader
            # that exits without failing the pending calls would leave them (and
            # every later one) waiting forever while PINGs hold the guest slot.
            err = GuestCommandError(f"fs session reader failed: {e!r}")
        self._fail(err)

    async def _keepalive(self):
        """PING so the agent's idle timeout never trips on a quiet session."""
        while self._dead is None:
            await asyncio.sleep(PING_INTERVAL_S)
            self._writer.write(encode_frame(FRAME_PING))
            if not await self._flush("keepalive"):
                break

    async def _flush(self, what):
        """Wait for buffered frames to reach the socket. A guest that stops
        reading leaves the socket full and this waiting for ever, with no
        response timeout armed yet to notice -- so the wait is bounded, and a
        lapse (or a write error) kills the session. Returns False if it did."""
        try:
            await asyncio.wait_for(self._writer.drain(), CALL_TIMEOUT_S)
        except asyncio.TimeoutError:
            self._fail(GuestCommandError(
                f"guest stopped reading: fs {what} not sent in {CALL_TIMEOUT_S}s"))
        except (OSError, ConnectionError) as e:
            self._fail(GuestCommandError(f"fs session write failed: {e}"))
        return self._dead is None

    def _fail(self, err):
        if self._dead is None:
            self._dead = err
        while self._pending:
            fut = self._pending.popleft()
            if not fut.done():
                fut.set_exception(self._dead)
        try:
            self._writer.close()
        except Exception:  # noqa: BLE001 - best-effort teardown
            pass

    async def close(self):
        """End the session. The guest closes any handles still open."""
        for t in self._tasks:
            t.cancel()
        self._fail(GuestCommandError("fs session closed"))

    async def call(self, op, data=b"", **fields):
        """Send one request and await its response: ``(header, data)``."""
        if self._dead is not None:
            raise self._dead
        fields["op"] = op
        frame = encode_frame(FRAME_FS_REQ, encode_fs_payload(fields, data))
        fut = asyncio.get_running_loop().create_future()
        # Enqueue and write with no await in between: the response order is the
        # write order, so the two must not interleave across concurrent calls.
        self._pending.append(fut)
        self._writer.write(frame)
        # If this fails it has failed `fut` too, so the await below raises.
        await self._flush(f"{op!r} request")
        try:
            payload = await asyncio.wait_for(fut, CALL_TIMEOUT_S)
        except asyncio.TimeoutError:
            # Responses are matched by order, so one that never arrives leaves
            # the stream unusable: fail the whole session, not just this call.
            self._fail(GuestCommandError(f"guest did not answer fs {op!r} in {CALL_TIMEOUT_S}s"))
            raise self._dead from None
        header, body = decode_fs_payload(payload)
        if not header.get("ok"):
            raise GuestFsError(
                header.get("kind", "fail"),
                header.get("message", "guest file operation failed"),
                header.get("errno", 0),
            )
        return header, body

    # -- operations --

    async def stat(self, path, follow=True):
        return (await self.call("stat", path=_path(path), follow=follow))[0]["attrs"]

    async def fstat(self, handle):
        return (await self.call("fstat", h=handle))[0]["attrs"]

    async def open(self, path, read=False, write=False, append=False,
                   create=False, trunc=False, excl=False, mode=None):
        fields = dict(path=_path(path), read=read, write=write, append=append,
                      create=create, trunc=trunc, excl=excl)
        if mode is not None:
            fields["mode"] = mode
        return (await self.call("open", **fields))[0]["h"]

    async def close_handle(self, handle):
        await self.call("close", h=handle)

    async def read(self, handle, offset, size):
        """Read up to ``size`` bytes at ``offset``, and never more than
        FS_READ_LIMIT. Empty means end of file; a short result may also just be
        the limit, so read again from the next offset for more."""
        size = min(size, FS_READ_LIMIT)
        out = []
        while size > 0:
            # Pipeline a window of chunk reads, then stop at the first short one.
            spans = []
            off = offset
            while size > 0 and len(spans) < FS_PIPELINE:
                n = min(size, FS_CHUNK)
                spans.append((off, n))
                off += n
                size -= n
            results = await asyncio.gather(
                *(self.call("read", h=handle, off=o, len=n) for o, n in spans))
            for (_o, n), (_hdr, body) in zip(spans, results):
                if len(body) > n:
                    # A guest must never hand back more than was asked for; a
                    # compromised one could otherwise inflate host memory.
                    raise GuestCommandError(
                        f"guest returned {len(body)} bytes for a {n}-byte read")
                out.append(body)
                if len(body) < n:
                    return b"".join(out)
            offset = off
        return b"".join(out)

    async def write(self, handle, offset, data):
        view = memoryview(data)
        pos = 0
        while pos < len(view):
            calls = []
            while pos < len(view) and len(calls) < FS_PIPELINE:
                chunk = bytes(view[pos:pos + FS_CHUNK])
                calls.append(self.call("write", chunk, h=handle, off=offset + pos))
                pos += len(chunk)
            await asyncio.gather(*calls)

    async def fsync(self, handle):
        await self.call("fsync", h=handle)

    async def setstat(self, path=None, handle=None, follow=True, **attrs):
        """Set any of size/mode/uid/gid/atime/mtime on a path or open handle."""
        fields = {k: v for k, v in attrs.items() if v is not None}
        if handle is not None:
            fields["h"] = handle
        else:
            fields["path"] = _path(path)
            fields["follow"] = follow
        await self.call("setstat", **fields)

    async def scandir(self, path):
        """Yield ``(name_bytes, attrs_or_None)`` for each entry (no . or ..)."""
        handle = (await self.call("opendir", path=_path(path)))[0]["h"]
        try:
            while True:
                hdr, _ = await self.call("readdir", h=handle)
                for ent in hdr.get("entries", []):
                    yield bytes.fromhex(ent["name"]), ent.get("attrs")
                if hdr.get("eof", True):
                    break
        finally:
            try:
                await self.call("close", h=handle)
            except GuestCommandError:
                pass

    async def mkdir(self, path, mode=None):
        fields = {"path": _path(path)}
        if mode is not None:
            fields["mode"] = mode
        await self.call("mkdir", **fields)

    async def rmdir(self, path):
        await self.call("rmdir", path=_path(path))

    async def remove(self, path):
        await self.call("remove", path=_path(path))

    async def rename(self, oldpath, newpath, replace=False):
        await self.call("rename", path=_path(oldpath), to=_path(newpath), replace=replace)

    async def readlink(self, path):
        return bytes.fromhex((await self.call("readlink", path=_path(path)))[0]["target"])

    async def symlink(self, target, linkpath):
        await self.call("symlink", path=_path(target), to=_path(linkpath))

    async def link(self, existing, linkpath):
        await self.call("link", path=_path(existing), to=_path(linkpath))

    async def realpath(self, path):
        return bytes.fromhex((await self.call("realpath", path=_path(path)))[0]["path"])

    async def statvfs(self, path):
        return (await self.call("statvfs", path=_path(path)))[0]

    async def fstatvfs(self, handle):
        return (await self.call("fstatvfs", h=handle))[0]
