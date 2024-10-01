# Guest command runner for rehosted systems

The project contains a [client program](guest_cmd.py) that interacts with a [server](src/main.rs) listening in the guest to execute commands locally and propagate stdout/stderr/exit status to the host.
