# Isengard install

Install `isd` on the machine you use to operate Docker hosts, then bootstrap
the controller and first agent on your current Docker context:

```sh
isd init
```

`isd init` discovers the active Docker context, starts the controller, and
enrolls the first agent on that host. Use Docker contexts to choose a different
target before running the command.

To add another host, mint a join command from the controller and run it against
the new Docker context:

```sh
isd join-token
isd join --controller <url> --token <packed> --context <new-host>
```

`isd join-token` creates a short-lived enrollment token. `isd join` uses that
token to enroll an agent without an extra authentication ceremony.
