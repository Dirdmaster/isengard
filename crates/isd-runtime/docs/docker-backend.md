Talks to a docker daemon, local or tunneled.

`DockerBackend` is the handle the rest of `isd` uses to drive a docker
daemon. It owns the optional [`crate::SshTunnel`] so the connection's
lifetime is bounded by the backend's lifetime: drop the backend, the
tunnel closes, the ssh child exits.

# Construction

Three constructors map to the three docker context shapes `isd`
supports:

- [`DockerBackend::from_local`] talks to the local socket
  (Unix socket on Linux, named pipe on Windows, `DOCKER_HOST` when set).
- [`DockerBackend::from_tunnel`] talks to an already-opened
  [`crate::SshTunnel`].
- [`DockerBackend::from_uri`] dispatches on a docker endpoint URI:
  `ssh://user@host` opens a tunnel, `unix://<path>` or `local` uses the
  local socket, anything else errors with [`crate::Error::InvalidEndpoint`].

# Trust

The backend has the privileges of whatever opened the docker socket.
For a local backend that is the user running `isd`. For a tunneled
backend that is whoever the SSH credential authenticates as on the
remote host. Either way, the docker daemon socket grants root: anyone
holding this handle can create privileged containers, mount the host
root filesystem, and run code as root on the target.

# Debug

`bollard::Docker` does not implement `Debug`. The manual `Debug` impl
on `DockerBackend` reports whether the backend is tunneled so test
harness output is readable without leaking the daemon connection
state.
