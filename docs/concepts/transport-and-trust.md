---
title: Transport And Trust
description: Enrollment, mTLS, SSH certs, and operator context trust.
---

# Transport And Trust

Isengard uses short-lived enrollment tokens to bootstrap long-lived trust between the controller and agents.

## Enrollment

`isd init` starts the controller and first agent on the current Docker context. To add another host, run:

```sh
isd join-token
```

The command prints an `isd join ...` command. The token embeds the controller CA fingerprint so the new agent can verify the controller it is joining.

## Agent transport

After enrollment, the agent uses mTLS for controller RPC. The token is only for first contact. The enrolled agent stores its certificate material in the agent state volume and renews before expiry.

## Operator transport

`isd` targets Docker contexts. For SSH-backed Docker contexts, the CLI reuses Docker's SSH transport and opens local access to the controller instead of maintaining a separate login flow.

## SSH host access

The SSH bastion feature is separate from controller mTLS. The controller mints short-lived SSH user certificates, and enrolled Linux agents install the controller SSH CA into sshd. Operators use `isd ssh <host>` to mint and use a cert.
