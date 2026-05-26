import socket
import argparse
import sys
import json
import os


class GuestCommandError(RuntimeError):
    pass


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


def recv_all(sock):
    output = bytearray()
    while True:
        chunk = sock.recv(4096)
        if not chunk:
            break
        output.extend(chunk)
    return bytes(output)


def decode_response(payload):
    if not payload:
        raise GuestCommandError("No response received from guest command server")

    try:
        received_json = payload.decode("utf-8")
    except UnicodeDecodeError as e:
        raise GuestCommandError(f"Guest command response was not valid UTF-8: {e}") from e

    try:
        result = json.loads(received_json)
    except json.JSONDecodeError as e:
        raise GuestCommandError(f"Guest command response was not valid JSON: {e}") from e

    if not isinstance(result, dict):
        raise GuestCommandError("Guest command response was not a JSON object")

    for key in ("stdout", "stderr", "exit_code"):
        if key not in result:
            raise GuestCommandError(f"Guest command response missing {key!r}")

    if not isinstance(result["stdout"], str):
        raise GuestCommandError("Guest command response field 'stdout' was not a string")
    if not isinstance(result["stderr"], str):
        raise GuestCommandError("Guest command response field 'stderr' was not a string")
    if not isinstance(result["exit_code"], int):
        raise GuestCommandError("Guest command response field 'exit_code' was not an integer")

    return result


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
    # Send CONNECT PORTNUM (for vsock) followed by the actual command.
    try:
        connect_command = f"CONNECT {port}\n"
        sock.sendall(connect_command.encode("utf-8"))
        response = sock.recv(4096).decode("utf-8", errors="replace").strip()
    except OSError as e:
        raise GuestCommandError(f"Failed to connect to vsock port {port}: {e}") from e

    expected = f"OK {port}"
    if response != expected:
        raise GuestCommandError(
            f"Unexpected response from vsock unix socket: expected {expected!r}, got {response!r}"
        )

    try:
        sock.sendall(prepare_command(command).encode("utf-8"))
        return decode_response(recv_all(sock))
    except OSError as e:
        raise GuestCommandError(f"Failed while running guest command: {e}") from e


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

    parser.add_argument("command",
                        nargs=argparse.REMAINDER,
                        help="The command to run on the server.")

    args = parser.parse_args(argv)

    if not args.command:
        parser.error("command is required")

    try:
        unix_socket = args.socket if args.socket is not None else find_vsocket()
        command = " ".join(args.command)
        run_guest(unix_socket, args.port, command)
    except GuestCommandError as e:
        print(f"guest_cmd: {e}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
