#!/bin/sh
# Run every host-side test, with nothing allowed to skip.
#
# Needs cargo, OpenSSH's clients (ssh/scp/sftp) and Python with asyncssh. The
# flake's dev shell provides all of them, which is how CI runs this:
#
#     nix develop -c tests/run_all.sh
#
# No guest, VM, container or root is involved: the "guest" in the end-to-end
# tests is the uds_agent example, a process on this machine.
set -eu
cd "$(dirname "$0")/.."

echo "== Rust: agent session, frame and fs-verb tests"
cargo test

echo "== Rust: build the Unix-socket agent stand-in for the end-to-end tests"
cargo build --example uds_agent

echo "== Python: client, gateway and end-to-end tests (prerequisites required)"
GUESTHOPPER_REQUIRE_ALL_TESTS=1 python3 -m unittest discover -s tests -v
