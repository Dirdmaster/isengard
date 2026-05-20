SSH tunnel that forwards a local TCP port to a remote docker socket.

[`SshTunnel`] spawns `ssh -L <local-port>:/var/run/docker.sock <target>`
so a local TCP port routes to the remote dockerd's Unix socket. The
struct owns the ssh child; `Drop` kills it.

Requires OpenSSH 7+ on `PATH` (modern dev machines). The forward syntax
`-L <port>:/var/run/docker.sock` routes local TCP to a remote UNIX
socket; supported since OpenSSH 6.7 (released 2014).

# Trust model

This crate operates on trust-the-transport. SSH access to the docker
context host equals controller access. The docker daemon socket is
privileged: anyone who can write to it can create privileged
containers, mount the host root filesystem, and run arbitrary code as
root. The tunnel forwards exactly that socket.

The implication: there is no separate authentication layer between
`isd` and the docker daemon. The SSH credential the operator already
has against the host is the entire access-control story. If the
operator's SSH key authenticates as a user in the `docker` group on
the remote host, that operator can drive everything on the host.

The trust posture is deliberate. `isd` does not invent a parallel
auth scheme; the operator already has SSH, the operator already trusts
SSH, the operator's existing SSH posture is the access control.

# Multiplexing

Uses ssh `ControlMaster` multiplexing so back-to-back invocations
(e.g. `isd ps; isd ps`) reuse one ssh connection instead of paying the
handshake cost every call. First call: ~1 to 2 seconds (handshake).
Subsequent calls within the `ControlPersist` window: ~100 ms.

The per-target `ControlPath` is a stable hash of the target string
under the user's temp dir. Two `SshTunnel::open` calls for the same
target share the master socket; the second one returns the moment the
forwarded port becomes ready.

# Cleanup

`Drop` calls `start_kill` on the owned ssh child. The kernel reaps the
process. The master ssh connection survives via `ControlPersist` so a
subsequent `open()` against the same target can reuse it without a
fresh handshake.
