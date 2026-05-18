# Isengard install

Bring up a cluster with one command on your operator machine:

    isd init

This brings up the controller + first agent on the docker host your
current docker context points at. To add more hosts:

    isd join-token        # mints a paste-able join command
    isd join --controller <url> --token <packed> --context <new-host>

See [[2026-05-17-isd-init-and-no-controller-design]] for the design.
