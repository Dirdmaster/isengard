The state of a single placement row.

The lifecycle is:

```text
pending -> active -> draining -> (deleted)
                 \-> failed (terminal until operator clears)
```

`Pending` means the scheduler wrote the row; the agent has not yet
confirmed the replica is running. `Active` is the steady state
populated by heartbeat reports. `Draining` covers rolling updates and
operator-initiated tear-downs. `Failed` is terminal: the scheduler
stops touching the row until an operator clears it, so a recurring
placement bug cannot silently churn the table.
