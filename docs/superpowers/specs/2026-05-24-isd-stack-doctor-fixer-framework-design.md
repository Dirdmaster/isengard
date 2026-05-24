# isd Stack Doctor Fixer Framework Design

## Problem

`isd stack doctor` can find missing Isengard idioms, but it cannot repair them. That leaves the operator in an awkward split-brain path: doctor explains what is wrong, then the operator has to use `isd route add`, `isd stack edit`, or a manual compose edit to act on the finding.

That is wrong for the product direction. Compose is the source of truth. If doctor knows the stack is missing routing labels, the repair path should update compose and deploy the corrected stack.

The first live need is `servarr`: `plex` should be exposed through a hostname without falling back to an imperative route-only write. The broader need is bigger than Plex. Doctor will gain more checks over time, so the first fixer should establish a reusable foundation instead of hardcoding one special case into `doctor::run`.

## Decision

Build a small fixer framework behind `isd stack doctor --fix`.

The framework separates three responsibilities:

1. Checks detect findings against parsed compose documents.
2. Fixers turn selected findings into compose document mutations.
3. Apply targets persist the mutated document to either a local file or the controller.

`EXPOSE_HOST_MISSING` is the first fixer. It asks the operator for a hostname, adds `isengard.expose` labels to the affected service, shows the resulting diff, and applies the compose update after confirmation.

## CLI Surface

Extend `isd stack doctor` with:

```text
isd stack doctor [--fix] [--yes] [--force] [target]
```

- `--fix`: enter the interactive fixer flow. Without this flag, doctor remains read-only.
- `--yes`: skip only the final apply confirmation. It does not invent missing fixer inputs such as hostnames.
- `--force`: for controller targets, bypass optimistic concurrency in the same sense as `isd stack deploy --force`.

Examples:

```bash
isd stack doctor servarr
isd stack doctor --fix servarr
isd stack doctor --fix ./compose.yaml
isd stack doctor --fix --force servarr
```

## Syntax Boundary

Doctor fixers must work with both Isengard stack layouts:

- Compose-only stacks: `compose.yaml`, `compose.yml`, or `compose.toml`.
- Manifest stacks: `stack.toml` with one or more compose entries, including TOML compose files.

Do not introduce a new `.isd` file or syntax in this pass. `stack.toml` already is the Isengard-native orchestration manifest, and `isengard.toml` already exists as the optional repo or fleet-level configuration file. A third syntax would split the model before there is evidence that `stack.toml` cannot carry the needed semantics.

The implementation should treat TOML compose as an input syntax that normalizes into the same in-memory service model used by YAML compose. Fixers operate on that model, then serialize back to the original syntax where practical.

If exact TOML round-trip preservation is too large for this pass, the minimum acceptable behavior is:

1. Local `compose.toml`: mutate and write TOML back.
2. `stack.toml` manifest: resolve the declared compose files, mutate the relevant compose source, and apply through the existing manifest-aware deploy path.
3. Controller stored compose: continue to mutate stored YAML, because the controller stores the merged deployment compose, not the original local TOML sources.

## Data Model

### Finding

Keep findings human-readable, but add structured fields so fixers do not parse prose.

```rust
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub hint: Option<String>,
    pub target: Option<FindingTarget>,
    pub fix: Option<FixSpec>,
}

pub enum FindingTarget {
    Service { name: String },
}

pub enum FixSpec {
    ExposeService { service: String, inferred_port: Option<u16> },
}
```

The exact Rust names can change during implementation, but the boundary matters: a check may attach machine-readable fix data, and the fixer consumes only that data plus the YAML tree.

### Fixer Registry

Add a registry with one entry per fix kind.

```rust
trait Fixer {
    fn id(&self) -> &'static str;
    fn supports(&self, finding: &Finding) -> bool;
    fn prompt(&self, finding: &Finding, compose: &Value) -> Result<Option<FixInput>>;
    fn apply(&self, compose: &mut Value, input: FixInput) -> Result<FixOutcome>;
}
```

The registry is intentionally small and static. There is no plugin system, dynamic loading, or generic workflow engine in this pass.

## Flow

### Read-only mode

Unchanged:

1. Resolve target.
2. Load the compose document.
3. Run `audit`.
4. Print findings.

### Fix mode

`isd stack doctor --fix <target>` runs:

1. Resolve target into an `ApplyTarget`.
2. Load the compose document and source metadata.
3. Run `audit`.
4. Filter findings to those with registered fixers.
5. For each fixable finding, prompt apply or skip.
6. Run the fixer prompt for required input.
7. Mutate an in-memory compose document.
8. Re-run `audit` on the mutated document.
9. Print a compact diff from original to proposed document.
10. Confirm apply unless `--yes` was passed.
11. Persist through the apply target.
12. Print a verification command.

If there are no fixable findings, print the current findings and say no registered fixers are available.

## Apply Targets

### Local File Target

Used when the target exists on disk.

Resolution stays compatible with current doctor behavior:

- file path: use that file
- directory: use `stack.toml`, then `compose.toml`, then `compose.yaml`, then `compose.yml`
- omitted target: use current directory compose file

Apply writes the proposed document back to the same file after confirmation.

When the resolved target is `stack.toml`, apply should preserve the manifest and update the referenced compose source. If multiple compose files contribute to the same service, the first version may stop with a clear error telling the operator to pass the specific compose file. Silent mutation of the wrong overlay is worse than asking for a narrower target.

### Controller Stack Target

Used when the target does not exist on disk and resolves as a controller stack name.

Controller mode needs more metadata than read-only doctor currently returns:

- stack id
- stack name
- stored compose YAML
- stored compose SHA-256

Apply uses:

```text
PUT /api/v1/stacks/<id>/compose
Content-Type: application/yaml
If-Match: <sha256>
```

When `--force` is set, append `?force=true` and allow the controller to bypass the hash conflict.

On success, print:

```text
Applied fixes to stack "servarr". Run `isd stack doctor servarr` to verify.
```

On conflict, print a clear retry message. The operator should re-run doctor to reload current compose, or use `--force` if they intentionally want to overwrite concurrent edits.

## First Fixer: EXPOSE_HOST_MISSING

The first checker already detects services that publish HTTP-ish ports but have no `isengard.expose` label.

The checker should attach:

- target service name
- inferred internal port, if available
- `FixSpec::ExposeService`

The fixer prompts:

```text
Expose services.plex through which hostname?
```

Validation:

- empty hostname is rejected
- whitespace is trimmed
- the first version does not guess domains

Mutation rules:

- Existing label map: insert `isengard.expose` and `isengard.expose.port`.
- Existing label list: append `isengard.expose=<hostname>` and `isengard.expose.port=<port>`.
- Missing labels: create a map under `labels`.
- Preserve unrelated labels.
- Do not overwrite an existing `isengard.expose*` label. If it exists, skip and report that the finding went stale.

Port rule:

- If exactly one web-relevant internal port is inferred, write `isengard.expose.port`.
- If no safe port is inferred, write only `isengard.expose` and let existing label parser defaults or future checks surface any ambiguity.

For Plex this should produce the shape:

```yaml
services:
  plex:
    labels:
      isengard.expose: plex.vallee.casa
      isengard.expose.port: "32400"
```

For TOML compose, the equivalent should preserve TOML syntax:

```toml
[services.plex.labels]
"isengard.expose" = "plex.vallee.casa"
"isengard.expose.port" = "32400"
```

## Error Handling

- No findings: print healthy and exit zero.
- Findings without registered fixers: print them, then say no fixer is available.
- Controller stack has no stored compose: tell the operator doctor cannot mutate missing YAML and suggest waiting for auto-adopt or running the adopt refresh path if live synth is available.
- Non-TTY with prompts required: fail and explain which input must be provided interactively.
- Non-TTY with `--yes`: only allowed if no fixer needs interactive input.
- YAML mutation fails because the service path is missing: treat as stale finding and skip that fix.
- Apply conflict: fail without retrying automatically.

## Testing

Unit tests:

- `Finding` carries structured service target and fix spec for `EXPOSE_HOST_MISSING`.
- Registry dispatch returns `ExposeHostFixer` for the first finding.
- Expose fixer mutates label maps.
- Expose fixer mutates label lists.
- Expose fixer creates labels when absent.
- Expose fixer mutates TOML compose without converting the local file to YAML.
- Expose fixer preserves unrelated labels.
- Expose fixer does not overwrite existing expose labels.
- Audit is re-run after mutation and the original finding disappears.
- Clap parses `doctor --fix servarr --yes --force`.

Apply tests:

- Local file target writes the proposed document after confirmation is bypassed.
- Local TOML target writes TOML after confirmation is bypassed.
- Manifest target resolves `stack.toml` and refuses ambiguous multi-compose overlays with a clear error.
- Controller apply builds a `PUT /api/v1/stacks/<id>/compose` request with `If-Match`.
- Controller apply appends `?force=true` when requested.

Manual smoke:

```bash
cargo fmt --check
cargo test -p isd doctor --lib
cargo test -p isd --test cli_test doctor
isd stack doctor --fix ./compose.toml
isd stack doctor --fix servarr
isd stack doctor servarr
```

## Out of Scope

- Route-only repairs that create routing rules without editing compose.
- A new `.isd` syntax. Revisit only if `stack.toml` and `isengard.toml` become clearly insufficient.
- A generic TUI workflow engine.
- Auto-generating hostnames from service names and base domains.
- Full YAML style preservation beyond map-vs-list label shape.
- Fixers for secrets, hooks, policies, or DNS. The framework should make them easy later, but this pass only ships expose-label repair.

## Success Criteria

- `isd stack doctor` remains read-only by default.
- `isd stack doctor --fix servarr` can add missing expose labels and deploy the corrected stored compose.
- Local compose files can be fixed without a controller.
- TOML compose and manifest-backed stacks are first-class doctor inputs.
- Adding the next fixer requires registering a new fixer and attaching a `FixSpec`, not rewriting doctor control flow.
- The operator never has to choose between doctor and compose-as-truth.
