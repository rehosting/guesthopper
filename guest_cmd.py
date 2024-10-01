import socket
import argparse
import sys
from glob import glob
import json

BUF_SIZE = 4096


def run_guest(unix_socket, port, command):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(unix_socket)

        # Send CONNECT PORTNUM (for vsock) followed by the actual command
        connect_command = f"CONNECT {port}\n"
        s.sendall(connect_command.encode('utf-8'))
        response = s.recv(4096).decode('utf-8')
        assert f"OK {port}" in response, "OK not received from vsock unix socket"

        s.sendall(command.encode('utf-8'))

        # Receive and parse the response
        output = s.recv(BUF_SIZE)

        received_json = output.decode('utf-8')
        result = json.loads(received_json)

        # Our contract with the runner is STDOUT_BYTES\nSTDOUT\nSTDERR_BYTES\nSTDERR

        print(result["stdout"], end='')
        # Propagate stderr to stderr
        if result["stderr"]:
            print(result["stderr"], file=sys.stderr, end='')
        sys.exit(result["exit_code"])

    except OSError as e:
        if s.error:
            print(f"Socket error: {e}", file=sys.stderr)
        else:
            print(e, file=sys.stderr)
    except SystemExit as e:
        # A little janky, but does the trick
        sys.exit(e.code)
    finally:
        s.close()


if __name__ == "__main__":
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

    args = parser.parse_args()

    if args.socket is None:
        unix_socket = glob("/tmp/*/vsocket")[0]
    else:
        unix_socket = args.socket

    command = ' '.join(args.command)

    run_guest(unix_socket, args.port, command)
