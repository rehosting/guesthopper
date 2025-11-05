import socket
import argparse
import sys
import json
import os
import time


def run_guest(unix_socket, port, command, use_stdio=True, max_retries=3, retry_delay=1):
    attempt = 0
    while attempt < max_retries:
        s = None
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(unix_socket)
            s.settimeout(None) # Disable timeout for long-running commands

            # Send CONNECT PORTNUM (for vsock) followed by the actual command
            connect_command = f"CONNECT {port}\n"
            s.sendall(connect_command.encode('utf-8'))
            response = s.recv(4096).decode('utf-8')
            assert f"OK {port}" in response, "OK not received from vsock unix socket"

            s.sendall(command.encode('utf-8'))

            output = b""
            while True:
                chunk = s.recv(4096)
                if not chunk:
                    break
                output += chunk

            received_json = output.decode('utf-8')
            result = json.loads(received_json)

            if not use_stdio:
                return result["stdout"]
            print(result["stdout"], end='')
            # Propagate stderr to stderr
            if result["stderr"]:
                print(result["stderr"], file=sys.stderr, end='')
            sys.exit(result["exit_code"])

        except ConnectionResetError as e:
            print(f"Connection reset by peer (attempt {attempt+1}/{max_retries}): {e}", file=sys.stderr)
            attempt += 1
            time.sleep(retry_delay)
        except OSError as e:
            print(f"Socket error: {e}", file=sys.stderr)
            break
        except SystemExit as e:
            sys.exit(e.code)
        finally:
            if s is not None:
                s.close()
    else:
        print("Failed to connect after multiple attempts.", file=sys.stderr)
        sys.exit(1)


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
        for root, dirs, files in os.walk('/tmp'):
            for file in files:
                if 'vsocket' in file:
                    unix_socket = os.path.join(root, file)
                    break
    else:
        unix_socket = args.socket

    command = ' '.join(args.command)

    run_guest(unix_socket, args.port, command)
