Merge compose YAML files into a single document.

A stack declares an ordered list of compose files. This module merges
them into one [`serde_yaml::Value`] tree and serializes the result back
to text. The base file is the first entry; subsequent entries (plus any
selected overlay) merge on top of it.

# Merge rules

| Field shape              | Behavior                                  |
|--------------------------|-------------------------------------------|
| Scalar (string, number)  | Last write wins.                          |
| Nested map               | Recurse with the same rules.              |
| List with `K=V` strings  | Append. Dedupe on the first component.    |
| List with `k:v` strings  | Append. Dedupe on the first component.    |
| List of `{name|source|target|published}` maps | Append. Dedupe on the named field. |

The list-merge rule applies to these compose fields:
`volumes`, `environment`, `ports`, `depends_on`, `cap_add`, `cap_drop`,
`networks`, `expose`, `labels`, `profiles`, `dns`, `extra_hosts`,
`external_links`, `links`, `tmpfs`, `security_opt`,
`device_cgroup_rules`, `devices`, `sysctls`.

# Why YAML, not the reconciler shape

Merging happens over `serde_yaml::Value` rather than the controller's
[`DesiredCompose`] struct to keep this crate dep-light: only `serde_yaml`,
not the entire reconciler. The reconciler stays downstream of the merge.

[`DesiredCompose`]: ../../isengard-controller/src/reconcile.rs
