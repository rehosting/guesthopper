import socket
import argparse
import struct
import sys
import json
import os


class GuestCommandError(RuntimeError):
    pass


# Frame types -- must match guesthopper/src/frame.rs.
FRAME_REQUEST = 1
FRAME_STDIN = 2
FRAME_STDIN_EOF = 3
FRAME_PING = 4  # client->agent liveness keepalive (no payload, no reply)
FRAME_RESIZE = 5
FRAME_STDOUT = 16
FRAME_STDERR = 17
FRAME_EXIT = 18
FRAME_ERROR = 19

MAX_FRAME_LEN = 16 * 1024 * 1024

# Send a keepalive PING after this many idle seconds so the agent's idle-timeout
# (default 30s) never trips on a live-but-quiet session. Well under that budget.
PING_INTERVAL_S = 5


def prepare_command(command):
    return f"export PATH=/igloo/utils:$PATH; {command}"


def find_vsocket(search_root="/tmp"):
    matches = []
    for root, _dirs, files in os.walk(search_root):
        for filename in files:
            if "vsocket" in filename:
                matches.append(os.path.join(root, filename))

    if not matches:
        raise GuestCommandError(f"No vsocket found under {search_root}")

    matches.sort()
    return matches[0]


def write_frame(sock, ftype, payload=b""):
    if isinstance(payload, str):
        payload = payload.encode("utf-8")
    sock.sendall(bytes([ftype]) + struct.pack(">I", len(payload)) + payload)


def _recv_line(sock, limit=4096):
    """Read bytes up to and including the first newline (handshake line)."""
    buf = bytearray()
    while len(buf) < limit:
        chunk = sock.recv(1)
        if not chunk:
            break
        buf.extend(chunk)
        if chunk == b"\n":
            break
    return bytes(buf)


def _recv_exact(sock, n):
    """Read exactly n bytes, or return None on a clean EOF at a boundary."""
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            if not buf:
                return None
            raise GuestCommandError("connection closed mid-frame")
        buf.extend(chunk)
    return bytes(buf)


def read_frame(sock):
    """Read one frame. Returns (ftype, payload) or None at clean EOF."""
    hdr = _recv_exact(sock, 5)
    if hdr is None:
        return None
    ftype = hdr[0]
    (length,) = struct.unpack(">I", hdr[1:5])
    if length > MAX_FRAME_LEN:
        raise GuestCommandError(f"frame payload {length} exceeds maximum {MAX_FRAME_LEN}")
    if length == 0:
        return (ftype, b"")
    payload = _recv_exact(sock, length)
    if payload is None:
        raise GuestCommandError("connection closed before frame payload")
    return (ftype, payload)


def run_guest(unix_socket, port, command, use_stdio=True):
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.connect(unix_socket)
            # Disable timeout for long-running commands.
            sock.settimeout(None)

            result = run_guest_with_socket(sock, port, command)
    except OSError as e:
        raise GuestCommandError(f"Socket error while talking to {unix_socket}: {e}") from e

    if not use_stdio:
        return result["stdout"]

    print(result["stdout"], end="")
    if result["stderr"]:
        print(result["stderr"], file=sys.stderr, end="")
    sys.exit(result["exit_code"])


def run_guest_with_socket(sock, port, command):
    # vhost-device-vsock hybrid handshake: CONNECT <port> / expect OK <port>.
    # This is the transport's, not guesthopper's -- unchanged by the framing
    # rework. The guest agent sends no bytes until it receives our REQUEST, so
    # the line-oriented OK read cannot swallow frame data.
    try:
        connect_command = f"CONNECT {port}\n"
        sock.sendall(connect_command.encode("utf-8"))
        # Read exactly the OK line (up to the newline) so we never consume the
        # first frame's bytes if the transport coalesces them into one read.
        response = _recv_line(sock).decode("utf-8", errors="replace").strip()
    except OSError as e:
        raise GuestCommandError(f"Failed to connect to vsock port {port}: {e}") from e

    expected = f"OK {port}"
    if response != expected:
        raise GuestCommandError(
            f"Unexpected response from vsock unix socket: expected {expected!r}, got {response!r}"
        )

    try:
        request = {"verb": "exec", "cmd": prepare_command(command), "deadline": None}
        write_frame(sock, FRAME_REQUEST, json.dumps(request).encode("utf-8"))
        return _collect_result(sock)
    except OSError as e:
        raise GuestCommandError(f"Failed while running guest command: {e}") from e


def _collect_result(sock):
    """Drain frames until EXIT, returning the legacy {stdout,stderr,exit_code} dict."""
    import select

    stdout = bytearray()
    stderr = bytearray()
    exit_code = None

    while True:
        # Keep the session alive on the agent side during a long, quiet command:
        # send a PING whenever no output has arrived for PING_INTERVAL_S. A
        # socket without a real fileno (e.g. an in-memory test fake) can't be
        # polled -- fall back to a plain blocking read (no keepalive needed).
        try:
            readable, _, _ = select.select([sock], [], [], PING_INTERVAL_S)
        except (TypeError, OSError, ValueError):
            readable = True
        if not readable:
            write_frame(sock, FRAME_PING)
            continue
        frame = read_frame(sock)
        if frame is None:
            break
        ftype, payload = frame
        if ftype == FRAME_STDOUT:
            stdout.extend(payload)
        elif ftype == FRAME_STDERR:
            stderr.extend(payload)
        elif ftype == FRAME_EXIT:
            exit_code = _parse_json(payload).get("code")
            if not isinstance(exit_code, int):
                raise GuestCommandError("EXIT frame missing integer 'code'")
            break
        elif ftype == FRAME_ERROR:
            msg = _parse_json(payload).get("message", "")
            raise GuestCommandError(f"Guest agent error: {msg}")
        # Ignore unknown frame types for forward compatibility.

    if exit_code is None:
        raise GuestCommandError("Guest command closed without an EXIT frame")

    return {
        "stdout": stdout.decode("utf-8", errors="replace"),
        "stderr": stderr.decode("utf-8", errors="replace"),
        "exit_code": exit_code,
    }


def _parse_json(payload):
    try:
        obj = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as e:
        raise GuestCommandError(f"Guest control frame was not valid JSON: {e}") from e
    if not isinstance(obj, dict):
        raise GuestCommandError("Guest control frame was not a JSON object")
    return obj


def open_pty_request(rows, cols):
    return json.dumps({"verb": "open-pty", "rows": rows, "cols": cols}).encode("utf-8")


def resize_payload(rows, cols):
    return json.dumps({"rows": rows, "cols": cols}).encode("utf-8")


def _term_size(fd):
    import fcntl
    import termios
    try:
        data = fcntl.ioctl(fd, termios.TIOCGWINSZ, b"\x00" * 8)
        rows, cols, _, _ = struct.unpack("HHHH", data)
        return (rows or 24, cols or 80)
    except OSError:
        return (24, 80)


def shell_handshake(sock, port, rows, cols):
    """CONNECT/OK handshake, then request an interactive pty."""
    try:
        sock.sendall(f"CONNECT {port}\n".encode("utf-8"))
        response = _recv_line(sock).decode("utf-8", errors="replace").strip()
    except OSError as e:
        raise GuestCommandError(f"Failed to connect to vsock port {port}: {e}") from e
    expected = f"OK {port}"
    if response != expected:
        raise GuestCommandError(
            f"Unexpected response from vsock unix socket: expected {expected!r}, got {response!r}"
        )
    write_frame(sock, FRAME_REQUEST, open_pty_request(rows, cols))


def run_shell(unix_socket, port):
    """Open an interactive pty session on the guest over the command channel.

    Puts the local terminal in raw mode and shuttles bytes both directions;
    SIGWINCH is forwarded as a RESIZE frame. Requires a tty on stdin for the
    full interactive experience (its behavior against a live guest is validated
    on a real run -- there's no guest to boot here).
    """
    import select
    import signal
    import termios
    import tty

    stdin_fd = sys.stdin.fileno()
    is_tty = os.isatty(stdin_fd)
    rows, cols = _term_size(stdin_fd) if is_tty else (24, 80)

    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.connect(unix_socket)
            sock.settimeout(None)
            shell_handshake(sock, port, rows, cols)

            old_attrs = termios.tcgetattr(stdin_fd) if is_tty else None
            if is_tty:
                tty.setraw(stdin_fd)
                signal.signal(
                    signal.SIGWINCH,
                    lambda *_: _safe_resize(sock, *_term_size(stdin_fd)),
                )

            exit_code = 0
            try:
                sock_fd = sock.fileno()
                # Watch stdin until it hits EOF, then drop it: send STDIN_EOF
                # exactly once and keep reading frames until the guest sends
                # EXIT. Continuing to poll a closed stdin would busy-loop and
                # spam STDIN_EOF frames into a socket the guest has stopped
                # reading once its shell exits -- the unconsumed inbound data
                # makes the guest RST the connection, which can beat the EXIT
                # frame to us (lost exit code + spurious "connection reset").
                watch = [stdin_fd, sock_fd]
                while True:
                    readable, _, _ = select.select(watch, [], [], PING_INTERVAL_S)
                    if not readable:
                        # Idle: keepalive so the agent doesn't time the session
                        # out while the user is just sitting at the prompt.
                        write_frame(sock, FRAME_PING)
                        continue
                    if stdin_fd in readable:
                        data = os.read(stdin_fd, 4096)
                        if not data:
                            write_frame(sock, FRAME_STDIN_EOF)
                            watch = [sock_fd]
                        else:
                            write_frame(sock, FRAME_STDIN, data)
                    if sock_fd in readable:
                        frame = read_frame(sock)
                        if frame is None:
                            break
                        ftype, payload = frame
                        if ftype == FRAME_STDOUT:
                            os.write(sys.stdout.fileno(), payload)
                        elif ftype == FRAME_STDERR:
                            os.write(sys.stderr.fileno(), payload)
                        elif ftype == FRAME_EXIT:
                            exit_code = _parse_json(payload).get("code", 0)
                            break
                        elif ftype == FRAME_ERROR:
                            msg = _parse_json(payload).get("message", "")
                            sys.stderr.write(f"guest_cmd: guest agent error: {msg}\n")
                            exit_code = 1
                            break
            finally:
                if is_tty and old_attrs is not None:
                    termios.tcsetattr(stdin_fd, termios.TCSADRAIN, old_attrs)
    except OSError as e:
        raise GuestCommandError(f"Socket error while talking to {unix_socket}: {e}") from e

    return exit_code


def _safe_resize(sock, rows, cols):
    try:
        write_frame(sock, FRAME_RESIZE, resize_payload(rows, cols))
    except OSError:
        pass


def main(argv=None):
    parser = argparse.ArgumentParser(description="Run a command in a rehosted guest")

    parser.add_argument("--socket",
                        help="Unix socket made by `vhost-device-vsock`." +
                        "\nDefaults to searching for 'vsocket' in /tmp/*/",
                        default=None)

    parser.add_argument("--port",
                        type=int,
                        help="Vsock port number to connect to. Defaults to 12341234",
                        default=12341234)

    parser.add_argument("--shell",
                        action="store_true",
                        help="Open an interactive pty shell on the guest instead of "
                        "running a one-shot command.")

    parser.add_argument("command",
                        nargs=argparse.REMAINDER,
                        help="The command to run on the server.")

    args = parser.parse_args(argv)

    if not args.shell and not args.command:
        parser.error("command is required")

    try:
        unix_socket = args.socket if args.socket is not None else find_vsocket()
        if args.shell:
            return run_shell(unix_socket, args.port)
        command = " ".join(args.command)
        run_guest(unix_socket, args.port, command)
    except GuestCommandError as e:
        print(f"guest_cmd: {e}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
