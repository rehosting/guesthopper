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

To exercise the whole path without a guest, build the Unix-socket stand-in for the agent and run the end-to-end test (needs `asyncssh` and OpenSSH's `scp`/`sftp`):

```sh
cargo build --example uds_agent
python3 -m unittest tests.test_scp_e2e
```

# Distribution

DISTRIBUTION STATEMENT A. Approved for public release. Distribution is unlimited.
 
This material is based upon work supported under Air Force Contract No. FA8702-15-D-0001 or FA8702-25-D-B002. Any opinions, findings, conclusions or recommendations expressed in this material are those of the author(s) and do not necessarily reflect the views of the U.S. Air Force.
 
© 2025 Massachusetts Institute of Technology
 
The software/firmware is provided to you on an As-Is basis.
 
Delivered to the U.S. Government with Unlimited Rights, as defined in DFARS Part 252.227-7013 or 7014 (Feb 2014). Notwithstanding any copyright notice, U.S. Government rights in this work are defined by DFARS 252.227-7013 or DFARS 252.227-7014 as detailed above. Use of this work other than as specifically authorized by the U.S. Government may violate any copyrights that exist in this work.
