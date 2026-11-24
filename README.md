# Guest command runner for rehosted systems

The project contains a [client program](guest_cmd.py) that interacts with a [server](src/main.rs) listening in the guest to execute commands locally and propagate stdout/stderr/exit status to the host.

## File transfer (scp / sftp)

The SSH front door ([ssh_gateway.py](ssh_gateway.py)) also serves `scp` and `sftp`, so files can be copied in and out of a live guest:

```sh
scp firmware.bin root@<container-ip>:/tmp/      # into the guest
scp -r root@<container-ip>:/etc ./guest-etc     # out of the guest
sftp root@<container-ip>
```

As with the shell, SSH is terminated on the host: the gateway runs the SFTP protocol itself and performs each file operation through the agent's `fs` verb ([src/files.rs](src/files.rs), client in [guest_fs.py](guest_fs.py)). The guest needs no `sshd`, `sftp-server`, `scp` binary or other userland tools. Both modern scp (SFTP subsystem) and legacy `scp -O` work. Relative remote paths resolve against the agent's working directory.

Things to know:

* Access is as open as the shell: anyone who can reach the SSH port can read and write guest files as root.
* An `ssh host "scp ..."` exec is always answered by the gateway's own scp implementation, even if the firmware ships an `scp`. Only the transfer forms (`scp -t` / `scp -f`, i.e. what `scp -O` sends) are understood; to run the guest's own binary for anything else, go through a shell (`ssh host "sh -c 'scp ...'"`).
* Only regular files and seekable devices transfer. FIFOs, ttys and sockets can be listed and stat'ed but not read or written.
* A single read is capped at 4 MiB (the limit advertised to SFTP clients); clients simply ask again for the rest.
* A download whose client takes none of the data sent to it for two minutes is dropped, so that a frozen client cannot hold a guest session open indefinitely. Only that transfer's channel is closed: a shell sharing the same SSH connection (`ControlMaster`) carries on. A slow client is unaffected, since any progress restarts the clock. `--client-stall-timeout` changes the limit (0 disables it).

## Testing

```sh
nix develop -c tests/run_all.sh
```

runs everything that does not need a guest: the Rust agent tests, the Python client and gateway tests, and end-to-end tests that drive real `scp`/`sftp`/`ssh` clients through the gateway. The "guest" in those is `examples/uds_agent.rs`, which serves the real agent session code over a Unix socket on the local machine, so no VM, container or root is needed. CI runs exactly this command.

The flake's dev shell supplies what the tests need beyond the standard library (cargo, OpenSSH's clients, Python with `asyncssh`). Without it, `cargo test` and `python3 -m unittest discover -s tests` still work, and the suites whose prerequisites are missing skip. `run_all.sh` sets `GUESTHOPPER_REQUIRE_ALL_TESTS=1`, which turns such a skip into a failure, so a green CI run means the tests actually ran.

# Distribution

DISTRIBUTION STATEMENT A. Approved for public release. Distribution is unlimited.
 
This material is based upon work supported under Air Force Contract No. FA8702-15-D-0001 or FA8702-25-D-B002. Any opinions, findings, conclusions or recommendations expressed in this material are those of the author(s) and do not necessarily reflect the views of the U.S. Air Force.
 
© 2025 Massachusetts Institute of Technology
 
The software/firmware is provided to you on an As-Is basis.
 
Delivered to the U.S. Government with Unlimited Rights, as defined in DFARS Part 252.227-7013 or 7014 (Feb 2014). Notwithstanding any copyright notice, U.S. Government rights in this work are defined by DFARS 252.227-7013 or DFARS 252.227-7014 as detailed above. Use of this work other than as specifically authorized by the U.S. Government may violate any copyrights that exist in this work.
