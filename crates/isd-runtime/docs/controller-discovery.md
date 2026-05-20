Discovers the Isengard controller container on a docker host.

Every `isd` command that talks to the controller REST API calls
[`discover`] first to locate the controller and confirm version
compatibility. The operator never types a controller URL; the URL is
derived from a label query against the docker daemon already on the
other end of the operator's docker context.

# Flow

The flow that runs from `isd::Session::open`:

1. Connect to docker via the operator's docker context URL.
2. `docker ps --filter label=io.isengard.role=controller`.
3. Read the matching container's published port for 9418/tcp.
4. Verify the container's `io.isengard.api.version` label matches
   [`crate::discovery_labels::API_VERSION`].
5. Return the reachable URL (the caller handles SSH-LocalForward when
   the docker context is SSH-backed).

Label name and value constants live in [`crate::discovery_labels`]. This
module imports them rather than redeclaring so the compose recipe and
the discovery call site stay in lock-step.

# Failure modes

The errors in [`DiscoveryError`] map one-to-one onto the failure modes
the operator can hit:

- No controller on the host: [`DiscoveryError::NotFound`].
- More than one controller on the host: [`DiscoveryError::Multiple`].
  Multi-controller-per-host is unsupported in v1.
- Controller version mismatch: [`DiscoveryError::VersionSkew`]. The
  error names which side to upgrade.
- Controller's REST port not published to the host:
  [`DiscoveryError::NoPublishedPort`].

# Trust

SSH access to the docker context's host equals controller access. There
is no separate authentication between `isd` and the controller beyond
the SSH credential that opens the docker socket forward. The discovery
flow returns a host-local endpoint (typically `127.0.0.1:9418`) because
the controller binds loopback only; the SSH-LocalForward in the caller
is what makes that endpoint reachable from the operator's laptop.
