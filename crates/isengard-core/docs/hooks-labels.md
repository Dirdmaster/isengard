Parse `isengard.hooks.*` Docker labels into a [`ParsedHooks`] struct.

Pure module: `HashMap<String, String>` in, [`ParsedHooks`] out. The ingest
caller (the controller's `HookLabelIngest`) decides what to upsert.

Recognized labels:

| Label                                | Field             |
|--------------------------------------|-------------------|
| `isengard.hooks.pre_deploy`          | `pre_deploy_url`  |
| `isengard.hooks.post_deploy`         | `post_deploy_url` |
| `isengard.hooks.on_failure`          | `on_failure_url`  |
| `isengard.hooks.secret`              | `secret`          |

Unset / empty values are treated as "no hook for this kind". Malformed
URLs are NOT rejected here: the worker surfaces the parse error in
`last_error` when it tries to dispatch, which is more visible to
operators.
