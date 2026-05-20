Direct Docker Engine API access for the `isd` CLI.

`isd-runtime` wraps `bollard` with an SSH tunnel lifecycle so `isd` can
target remote Docker daemons from the operator's laptop. No Isengard
agent or controller has to be installed on the target host for these
primitives to work.

# Shape

Two tiers. [`SshTunnel`] spawns `ssh -L <local-port>:/var/run/docker.sock
<target>` and owns the child process; `Drop` kills it. [`DockerBackend`]
wraps `bollard::Docker` against either the local default socket or a
tunneled remote.

[`controller_discovery::discover`] locates the Isengard controller
container on a docker host by label. [`discovery_labels`] holds the label
name and value constants the controller compose recipe sets, kept in one
place so the recipe and the discovery call site stay in lock-step.

# Trust model

SSH access to a host's docker context equals root on that host. The
docker daemon socket is privileged: anyone who can write to it can
create privileged containers, mount the host root filesystem, and run
arbitrary code as root. This crate's SSH tunnel forwards exactly that
socket.

Read `isd-runtime`'s trust posture as: if you can `ssh user@host`, you
can drive `dockerd` on `host`. The CLI is no more privileged than the
SSH credential it rides on. No separate authentication layer sits
between an `isd` invocation and the daemon.

# Consumers

The `isd` CLI uses [`DockerBackend`] for every container-level command
(`isd ps`, `isd stop`, `isd rm`, `isd logs`). The controller discovery
flow gates every `isd` command that talks to the Isengard controller
REST API.
