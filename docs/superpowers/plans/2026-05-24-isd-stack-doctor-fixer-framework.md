# isd Stack Doctor Fixer Framework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `isd stack doctor --fix` with a reusable fixer registry, local YAML/TOML mutation, manifest-aware target resolution, and controller apply for stored compose.

**Architecture:** Keep doctor checks pure and add structured fix metadata to findings. Add focused fixer modules that mutate a `ComposeDocument`, then persist through an `ApplyTarget` abstraction. Start with `EXPOSE_HOST_MISSING`, but make the registry and document layer reusable for future doctor repairs.

**Tech Stack:** Rust, clap, anyhow, serde_yaml, toml, similar, reqwest, wiremock, inquire.

---

## File Structure

- Modify `crates/isd/src/doctor/mod.rs`: CLI flags, `Finding` metadata, read-only vs fix dispatch, target resolution entry points.
- Modify `crates/isd/src/doctor/checks/expose_host.rs`: attach `FindingTarget`, `FixSpec`, and inferred port to findings.
- Create `crates/isd/src/doctor/document.rs`: parse/serialize YAML and TOML compose documents, expose service lookup/mutation helpers.
- Create `crates/isd/src/doctor/fixers/mod.rs`: registry and shared fixer data types.
- Create `crates/isd/src/doctor/fixers/expose_host.rs`: `EXPOSE_HOST_MISSING` fixer implementation.
- Create `crates/isd/src/doctor/apply.rs`: local file, manifest, and controller apply targets.
- Modify `crates/isd/src/compose_cmd.rs`: expose stack id/compose fetch/apply helpers for doctor.
- Modify `crates/isd/Cargo.toml`: add dev/test dependencies only if an integration test needs a new one. Prefer existing `wiremock`.
- Add tests in the new modules and existing `doctor` tests. Keep tests package-scoped with `cargo test -p isd doctor --lib`.

## Implementation Notes

- Keep commits small. Commit after each task.
- Preserve the no-em-dash rule in code, comments, docs, and commit messages.
- Do not introduce `.isd` syntax.
- For TOML labels with dotted keys, always quote keys: `"isengard.expose"` and `"isengard.expose.port"`.
- Controller targets mutate stored YAML only. Local TOML stays TOML.
- For `stack.toml` with multiple compose files, refuse the first version with a clear error unless the service exists in exactly one referenced compose file.

### Task 1: Add Structured Finding Metadata

**Files:**
- Modify: `crates/isd/src/doctor/mod.rs`
- Modify: `crates/isd/src/doctor/checks/expose_host.rs`

- [ ] **Step 1: Write failing tests for metadata on `EXPOSE_HOST_MISSING`**

Add this test to `crates/isd/src/doctor/checks/expose_host.rs` inside `mod tests`:

```rust
#[test]
fn bare_http_service_carries_structured_fix_data() {
    let v = parse(
        r#"
services:
  plex:
    image: lscr.io/linuxserver/plex
    ports:
      - "32400:32400"
"#,
    );
    let f = check(&v);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].id, "EXPOSE_HOST_MISSING");
    assert_eq!(
        f[0].target,
        Some(crate::doctor::FindingTarget::Service {
            name: "plex".to_string()
        })
    );
    assert_eq!(
        f[0].fix,
        Some(crate::doctor::FixSpec::ExposeService {
            service: "plex".to_string(),
            inferred_port: Some(32400),
        })
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isd doctor::checks::expose_host::tests::bare_http_service_carries_structured_fix_data --lib`

Expected: FAIL because `FindingTarget` and `FixSpec` do not exist.

- [ ] **Step 3: Add metadata types and fields**

In `crates/isd/src/doctor/mod.rs`, update `Finding` and add the enums near it:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingTarget {
    Service { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixSpec {
    ExposeService {
        service: String,
        inferred_port: Option<u16>,
    },
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub hint: Option<String>,
    pub target: Option<FindingTarget>,
    pub fix: Option<FixSpec>,
}
```

Update every existing `Finding { ... }` construction to include `target` and `fix`.

- [ ] **Step 4: Infer exposed port and attach fix metadata**

In `crates/isd/src/doctor/checks/expose_host.rs`, replace `exposes_http_port` with a function that returns the first matching port:

```rust
fn inferred_http_port(svc: &serde_yaml::Mapping) -> Option<u16> {
    let ports = svc.get("ports").and_then(Value::as_sequence)?;
    ports.iter().find_map(http_ish_port)
}

fn http_ish_port(spec: &Value) -> Option<u16> {
    let port = if let Some(s) = spec.as_str() {
        let container = s.rsplit(':').next().unwrap_or(s);
        let container = container.split('/').next().unwrap_or(container);
        container.parse::<u16>().ok()?
    } else if let Some(m) = spec.as_mapping() {
        let n = m.get("target")?.as_u64()?;
        u16::try_from(n).ok()?
    } else {
        return None;
    };
    HTTP_ISH_PORTS.contains(&port).then_some(port)
}

fn port_is_http_ish(spec: &Value) -> bool {
    http_ish_port(spec).is_some()
}
```

Then update `check`:

```rust
let Some(inferred_port) = inferred_http_port(svc) else {
    continue;
};
```

and add metadata to the finding:

```rust
target: Some(crate::doctor::FindingTarget::Service {
    name: name.to_string(),
}),
fix: Some(crate::doctor::FixSpec::ExposeService {
    service: name.to_string(),
    inferred_port: Some(inferred_port),
}),
```

- [ ] **Step 5: Run focused tests**

Run: `cargo test -p isd doctor::checks::expose_host --lib`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/checks/expose_host.rs
git commit -m "feat: add doctor finding fix metadata"
```

### Task 2: Add ComposeDocument Parsing and Serialization

**Files:**
- Create: `crates/isd/src/doctor/document.rs`
- Modify: `crates/isd/src/doctor/mod.rs`

- [ ] **Step 1: Write failing document tests**

Create `crates/isd/src/doctor/document.rs` with only tests first:

```rust
use anyhow::Result;
use serde_yaml::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeSyntax {
    Yaml,
    Toml,
}

pub struct ComposeDocument {
    pub syntax: ComposeSyntax,
    pub value: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_document_round_trips_services_shape() {
        let doc = ComposeDocument::parse_path(
            Path::new("compose.yaml"),
            "services:\n  web:\n    image: nginx\n",
        )
        .unwrap();
        assert_eq!(doc.syntax, ComposeSyntax::Yaml);
        assert!(doc.value.get("services").is_some());
        assert!(doc.to_string().unwrap().contains("services:"));
    }

    #[test]
    fn toml_document_normalizes_to_yaml_value_and_serializes_toml() {
        let doc = ComposeDocument::parse_path(
            Path::new("compose.toml"),
            "[services.web]\nimage = \"nginx\"\nports = [\"8080:80\"]\n",
        )
        .unwrap();
        assert_eq!(doc.syntax, ComposeSyntax::Toml);
        assert!(doc.value.get("services").is_some());
        let rendered = doc.to_string().unwrap();
        assert!(rendered.contains("[services.web]"), "{rendered}");
        assert!(rendered.contains("image = \"nginx\""), "{rendered}");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isd doctor::document --lib`

Expected: FAIL because methods are missing and module is not exported.

- [ ] **Step 3: Export document module**

In `crates/isd/src/doctor/mod.rs`, add:

```rust
pub mod document;
```

- [ ] **Step 4: Implement parser and serializer**

Replace the stub in `document.rs` with:

```rust
use anyhow::{Context as _, Result, anyhow};
use serde_yaml::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeSyntax {
    Yaml,
    Toml,
}

pub struct ComposeDocument {
    pub syntax: ComposeSyntax,
    pub value: Value,
}

impl ComposeDocument {
    pub fn parse_path(path: &Path, content: &str) -> Result<Self> {
        let syntax = syntax_for_path(path)?;
        match syntax {
            ComposeSyntax::Yaml => Ok(Self {
                syntax,
                value: serde_yaml::from_str(content)
                    .with_context(|| format!("parsing YAML compose at {}", path.display()))?,
            }),
            ComposeSyntax::Toml => {
                let value: toml::Value = toml::from_str(content)
                    .with_context(|| format!("parsing TOML compose at {}", path.display()))?;
                Ok(Self {
                    syntax,
                    value: toml_value_to_yaml(value)?,
                })
            }
        }
    }

    pub fn parse_controller_yaml(content: &str) -> Result<Self> {
        Ok(Self {
            syntax: ComposeSyntax::Yaml,
            value: serde_yaml::from_str(content).context("parsing controller compose YAML")?,
        })
    }

    pub fn to_string(&self) -> Result<String> {
        match self.syntax {
            ComposeSyntax::Yaml => serde_yaml::to_string(&self.value).context("serializing YAML compose"),
            ComposeSyntax::Toml => {
                let toml_value = yaml_value_to_toml(&self.value)?;
                toml::to_string_pretty(&toml_value).context("serializing TOML compose")
            }
        }
    }
}

fn syntax_for_path(path: &Path) -> Result<ComposeSyntax> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("yaml" | "yml") => Ok(ComposeSyntax::Yaml),
        Some("toml") => Ok(ComposeSyntax::Toml),
        other => Err(anyhow!("unsupported compose extension {:?}; expected yaml, yml, or toml", other)),
    }
}

fn toml_value_to_yaml(value: toml::Value) -> Result<Value> {
    let json = toml_to_json(value)?;
    serde_yaml::to_value(json).context("converting TOML compose to YAML value")
}

fn toml_to_json(value: toml::Value) -> Result<serde_json::Value> {
    use serde_json::Value as Json;
    Ok(match value {
        toml::Value::String(s) => Json::String(s),
        toml::Value::Integer(i) => Json::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map(Json::Number).unwrap_or(Json::Null),
        toml::Value::Boolean(b) => Json::Bool(b),
        toml::Value::Datetime(dt) => Json::String(dt.to_string()),
        toml::Value::Array(items) => Json::Array(items.into_iter().map(toml_to_json).collect::<Result<_>>()?),
        toml::Value::Table(table) => {
            let mut out = serde_json::Map::new();
            for (key, value) in table {
                out.insert(key, toml_to_json(value)?);
            }
            Json::Object(out)
        }
    })
}

fn yaml_value_to_toml(value: &Value) -> Result<toml::Value> {
    Ok(match value {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                return Err(anyhow!("unsupported numeric YAML value: {n}"));
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Sequence(items) => toml::Value::Array(
            items.iter().map(yaml_value_to_toml).collect::<Result<Vec<_>>>()?,
        ),
        Value::Mapping(map) => {
            let mut table = toml::map::Map::new();
            for (key, value) in map {
                let key = key.as_str().ok_or_else(|| anyhow!("TOML compose keys must be strings"))?;
                table.insert(key.to_string(), yaml_value_to_toml(value)?);
            }
            toml::Value::Table(table)
        }
        Value::Tagged(_) => return Err(anyhow!("tagged YAML values are not supported in compose")),
    })
}
```

- [ ] **Step 5: Run document tests**

Run: `cargo test -p isd doctor::document --lib`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/document.rs
git commit -m "feat: add doctor compose document model"
```

### Task 3: Implement ExposeHost Fixer and Registry

**Files:**
- Create: `crates/isd/src/doctor/fixers/mod.rs`
- Create: `crates/isd/src/doctor/fixers/expose_host.rs`
- Modify: `crates/isd/src/doctor/mod.rs`

- [ ] **Step 1: Write failing fixer tests**

Create `crates/isd/src/doctor/fixers/expose_host.rs`:

```rust
use anyhow::Result;
use serde_yaml::Value;

pub struct ExposeHostInput {
    pub service: String,
    pub hostname: String,
    pub port: Option<u16>,
}

pub fn apply_expose_host(_compose: &mut Value, _input: &ExposeHostInput) -> Result<bool> {
    todo!("implemented in Task 3")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn creates_label_map_when_missing() {
        let mut v = parse("services:\n  plex:\n    image: plex\n    ports: [\"32400:32400\"]\n");
        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "plex".into(),
                hostname: "plex.vallee.casa".into(),
                port: Some(32400),
            },
        )
        .unwrap();
        assert!(changed);
        let labels = &v["services"]["plex"]["labels"];
        assert_eq!(labels["isengard.expose"].as_str(), Some("plex.vallee.casa"));
        assert_eq!(labels["isengard.expose.port"].as_str(), Some("32400"));
    }

    #[test]
    fn appends_to_label_list() {
        let mut v = parse("services:\n  web:\n    image: nginx\n    ports: [\"8080:80\"]\n    labels:\n      - \"foo=bar\"\n");
        apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "web.test".into(),
                port: Some(80),
            },
        )
        .unwrap();
        let labels = v["services"]["web"]["labels"].as_sequence().unwrap();
        assert!(labels.iter().any(|v| v.as_str() == Some("foo=bar")));
        assert!(labels.iter().any(|v| v.as_str() == Some("isengard.expose=web.test")));
        assert!(labels.iter().any(|v| v.as_str() == Some("isengard.expose.port=80")));
    }

    #[test]
    fn refuses_to_overwrite_existing_expose_label() {
        let mut v = parse("services:\n  web:\n    labels:\n      isengard.expose: old.test\n");
        let changed = apply_expose_host(
            &mut v,
            &ExposeHostInput {
                service: "web".into(),
                hostname: "new.test".into(),
                port: Some(80),
            },
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(v["services"]["web"]["labels"]["isengard.expose"].as_str(), Some("old.test"));
    }
}
```

- [ ] **Step 2: Export fixer modules and run failing tests**

In `crates/isd/src/doctor/mod.rs`, add:

```rust
pub mod fixers;
```

Create `crates/isd/src/doctor/fixers/mod.rs`:

```rust
pub mod expose_host;
```

Run: `cargo test -p isd doctor::fixers::expose_host --lib`

Expected: FAIL because `todo!` panics.

- [ ] **Step 3: Implement YAML mutation helper**

Replace `apply_expose_host` in `expose_host.rs` with:

```rust
pub fn apply_expose_host(compose: &mut Value, input: &ExposeHostInput) -> Result<bool> {
    let services = compose
        .get_mut("services")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("compose has no services mapping"))?;
    let service = services
        .get_mut(Value::String(input.service.clone()))
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("service {:?} not found", input.service))?;

    let labels_key = Value::String("labels".to_string());
    if !service.contains_key(&labels_key) {
        let mut labels = serde_yaml::Mapping::new();
        labels.insert(Value::String("isengard.expose".into()), Value::String(input.hostname.clone()));
        if let Some(port) = input.port {
            labels.insert(Value::String("isengard.expose.port".into()), Value::String(port.to_string()));
        }
        service.insert(labels_key, Value::Mapping(labels));
        return Ok(true);
    }

    let labels = service.get_mut(&labels_key).expect("checked above");
    if let Some(map) = labels.as_mapping_mut() {
        if map.keys().filter_map(Value::as_str).any(|k| k.starts_with("isengard.expose")) {
            return Ok(false);
        }
        map.insert(Value::String("isengard.expose".into()), Value::String(input.hostname.clone()));
        if let Some(port) = input.port {
            map.insert(Value::String("isengard.expose.port".into()), Value::String(port.to_string()));
        }
        return Ok(true);
    }
    if let Some(seq) = labels.as_sequence_mut() {
        if seq.iter().filter_map(Value::as_str).any(|entry| {
            entry.split('=').next().unwrap_or(entry).starts_with("isengard.expose")
        }) {
            return Ok(false);
        }
        seq.push(Value::String(format!("isengard.expose={}", input.hostname)));
        if let Some(port) = input.port {
            seq.push(Value::String(format!("isengard.expose.port={port}")));
        }
        return Ok(true);
    }
    Err(anyhow::anyhow!("services.{}.labels must be a map or list", input.service))
}
```

- [ ] **Step 4: Add registry types**

In `crates/isd/src/doctor/fixers/mod.rs`, replace the module-only file with:

```rust
use crate::doctor::{Finding, FixSpec};

pub mod expose_host;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixInput {
    ExposeHost(expose_host::ExposeHostInput),
}

pub fn fixer_id_for(finding: &Finding) -> Option<&'static str> {
    match finding.fix {
        Some(FixSpec::ExposeService { .. }) => Some("EXPOSE_HOST_MISSING"),
        None => None,
    }
}
```

Add `#[derive(Debug, Clone, PartialEq, Eq)]` to `ExposeHostInput`.

- [ ] **Step 5: Run focused tests**

Run: `cargo test -p isd doctor::fixers --lib`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/fixers
git commit -m "feat: add doctor expose host fixer"
```

### Task 4: Wire Read-Only Doctor to ComposeDocument Without Behavior Change

**Files:**
- Modify: `crates/isd/src/doctor/mod.rs`
- Modify: `crates/isd/src/compose_cmd.rs`

- [ ] **Step 1: Write failing tests for local path resolution order**

Add to `crates/isd/src/doctor/mod.rs` tests:

```rust
#[test]
fn local_directory_prefers_stack_then_toml_then_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("compose.yaml"), "services:\n").unwrap();
    std::fs::write(tmp.path().join("compose.toml"), "[services.web]\nimage = \"nginx\"\n").unwrap();
    std::fs::write(tmp.path().join("stack.toml"), "name = \"demo\"\ncompose = [\"compose.toml\"]\n").unwrap();
    let resolved = resolve_local_path(tmp.path()).unwrap();
    assert_eq!(resolved.file_name().and_then(|s| s.to_str()), Some("stack.toml"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isd doctor::tests::local_directory_prefers_stack_then_toml_then_yaml --lib`

Expected: FAIL because current resolver ignores `stack.toml` and `compose.toml`.

- [ ] **Step 3: Update local resolver**

In `resolve_local_path`, replace directory probing with:

```rust
for name in ["stack.toml", "compose.toml", "compose.yaml", "compose.yml"] {
    let candidate = target.join(name);
    if candidate.exists() {
        return Ok(candidate);
    }
}
return Err(anyhow::anyhow!(
    "no stack.toml, compose.toml, compose.yaml, or compose.yml in {}",
    target.display()
));
```

- [ ] **Step 4: Parse local documents through `ComposeDocument`**

Update `run` so local targets use `ComposeDocument::parse_path` and controller targets use `ComposeDocument::parse_controller_yaml`.

Introduce a small resolved struct in `doctor/mod.rs`:

```rust
struct ResolvedCompose {
    body: String,
    document: document::ComposeDocument,
    source: ComposeSource,
}
```

Change `resolve_compose` to return `ResolvedCompose` instead of `(String, ComposeSource)`.

For local paths:

```rust
let body = std::fs::read_to_string(&resolved)
    .with_context(|| format!("reading compose file {}", resolved.display()))?;
let document = document::ComposeDocument::parse_path(&resolved, &body)?;
return Ok(ResolvedCompose { body, document, source: ComposeSource::Local(resolved) });
```

For controller:

```rust
let document = document::ComposeDocument::parse_controller_yaml(&yaml)?;
Ok(ResolvedCompose { body: yaml, document, source: ComposeSource::Controller(t.to_string()) })
```

Then in `run`:

```rust
let resolved = resolve_compose(args.target.as_deref(), context).await?;
let findings = audit(&resolved.document.value);
```

- [ ] **Step 5: Keep `load_compose` compatible for deploy preflight**

Update `load_compose` to use `ComposeDocument::parse_path(path, content)` and return `document.value`.

- [ ] **Step 6: Run doctor tests**

Run: `cargo test -p isd doctor --lib`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/isd/src/doctor/mod.rs
git commit -m "feat: load doctor compose documents"
```

### Task 5: Add Local Apply Targets

**Files:**
- Create: `crates/isd/src/doctor/apply.rs`
- Modify: `crates/isd/src/doctor/mod.rs`

- [ ] **Step 1: Write failing local apply tests**

Create `crates/isd/src/doctor/apply.rs`:

```rust
use anyhow::Result;
use std::path::PathBuf;

pub enum ApplyTarget {
    LocalFile { path: PathBuf },
}

impl ApplyTarget {
    pub async fn apply(&self, _body: &str, _force: bool) -> Result<()> {
        todo!("implemented in Task 5")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_file_apply_overwrites_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("compose.yaml");
        std::fs::write(&path, "old").unwrap();
        let target = ApplyTarget::LocalFile { path: path.clone() };
        target.apply("new", false).await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }
}
```

- [ ] **Step 2: Export apply module and run failing test**

In `doctor/mod.rs`, add:

```rust
pub mod apply;
```

Run: `cargo test -p isd doctor::apply --lib`

Expected: FAIL because `todo!` panics.

- [ ] **Step 3: Implement local apply**

Replace `apply` with:

```rust
pub async fn apply(&self, body: &str, _force: bool) -> Result<()> {
    match self {
        ApplyTarget::LocalFile { path } => {
            std::fs::write(path, body)
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Add manifest resolver test**

Add to `apply.rs`:

```rust
pub fn resolve_manifest_compose_path(stack_toml: &std::path::Path) -> Result<PathBuf> {
    let text = std::fs::read_to_string(stack_toml)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", stack_toml.display()))?;
    let manifest = isengard_manifest::StackManifest::from_str(&text, stack_toml.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", stack_toml.display()))?;
    if manifest.compose.len() != 1 {
        return Err(anyhow::anyhow!(
            "stack.toml has {} compose entries; pass the specific compose file to doctor --fix",
            manifest.compose.len()
        ));
    }
    Ok(stack_toml.parent().unwrap_or_else(|| std::path::Path::new(".")).join(&manifest.compose[0]))
}

#[test]
fn manifest_with_multiple_compose_files_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("stack.toml");
    std::fs::write(&path, "name = \"demo\"\ncompose = [\"base.toml\", \"prod.toml\"]\n").unwrap();
    let err = resolve_manifest_compose_path(&path).unwrap_err().to_string();
    assert!(err.contains("pass the specific compose file"), "{err}");
}
```

If `StackManifest::from_str` signature differs, use the existing signature from `crates/isengard-manifest/tests/parse_tests.rs`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p isd doctor::apply --lib`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/apply.rs
git commit -m "feat: add doctor local apply targets"
```

### Task 6: Wire Interactive `--fix` for Local Documents

**Files:**
- Modify: `crates/isd/src/doctor/mod.rs`
- Modify: `crates/isd/src/doctor/fixers/mod.rs`
- Modify: `crates/isd/src/doctor/fixers/expose_host.rs`

- [ ] **Step 1: Add clap flag parsing test**

In `doctor/mod.rs` tests, add:

```rust
#[test]
fn doctor_args_parse_fix_yes_force() {
    use clap::Parser;
    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: DoctorArgs,
    }
    let w = Wrap::try_parse_from(["x", "--fix", "--yes", "--force", "servarr"]).unwrap();
    assert!(w.args.fix);
    assert!(w.args.yes);
    assert!(w.args.force);
    assert_eq!(w.args.target.as_deref(), Some("servarr"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p isd doctor::tests::doctor_args_parse_fix_yes_force --lib`

Expected: FAIL because flags do not exist.

- [ ] **Step 3: Add flags to `DoctorArgs`**

In `doctor/mod.rs`:

```rust
#[arg(long)]
pub fix: bool,
#[arg(long)]
pub yes: bool,
#[arg(long)]
pub force: bool,
```

- [ ] **Step 4: Add prompt input collector**

In `fixers/expose_host.rs`, add:

```rust
pub fn input_from_finding(finding: &crate::doctor::Finding) -> Option<ExposeHostInput> {
    let crate::doctor::FixSpec::ExposeService { service, inferred_port } = finding.fix.clone()?;
    Some(ExposeHostInput {
        service,
        hostname: String::new(),
        port: inferred_port,
    })
}
```

In `doctor/mod.rs`, implement a helper:

```rust
fn prompt_hostname(service: &str) -> Result<String> {
    let hostname = inquire::Text::new(&format!("Expose services.{service} through which hostname?"))
        .with_validator(|input: &str| {
            if input.trim().is_empty() {
                Ok(inquire::validator::Validation::Invalid("hostname cannot be empty".into()))
            } else {
                Ok(inquire::validator::Validation::Valid)
            }
        })
        .prompt()
        .map_err(|e| anyhow::anyhow!("hostname prompt cancelled: {e}"))?;
    Ok(hostname.trim().to_string())
}
```

- [ ] **Step 5: Implement local fix loop**

In `run`, branch early:

```rust
if args.fix {
    return run_fix(args, context).await;
}
```

Add `run_fix`:

```rust
async fn run_fix(args: DoctorArgs, context: Option<&str>) -> Result<()> {
    let mut resolved = resolve_compose(args.target.as_deref(), context).await?;
    let original = resolved.body.clone();
    let findings = audit(&resolved.document.value);
    let mut changed = false;
    for finding in findings {
        if finding.id != "EXPOSE_HOST_MISSING" {
            continue;
        }
        let Some(mut input) = fixers::expose_host::input_from_finding(&finding) else {
            continue;
        };
        input.hostname = prompt_hostname(&input.service)?;
        changed |= fixers::expose_host::apply_expose_host(&mut resolved.document.value, &input)?;
    }
    if !changed {
        println!("No fixes applied.");
        return Ok(());
    }
    let proposed = resolved.document.to_string()?;
    print_compact_diff(&original, &proposed);
    if !args.yes && !confirm_apply()? {
        println!("Aborted.");
        return Ok(());
    }
    match resolved.source {
        ComposeSource::Local(path) => {
            apply::ApplyTarget::LocalFile { path }.apply(&proposed, args.force).await?;
            println!("Applied fixes locally.");
        }
        ComposeSource::Controller(_) => anyhow::bail!("controller apply lands in Task 7"),
    }
    Ok(())
}
```

Add `print_compact_diff` using `similar::TextDiff` like `compose_cmd.rs`.

Add `confirm_apply` with the same non-TTY behavior as `compose_cmd::confirm`:

```rust
fn confirm_apply() -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("no TTY for confirmation; pass --yes to apply fixes");
    }
    print!("Apply fixes? [y/N] ");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}
```

- [ ] **Step 6: Run local non-interactive test manually with `--yes` skipped**

This task cannot fully automate prompt input without changing the design. Verify compile and unit coverage only here.

Run: `cargo test -p isd doctor --lib`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/fixers/mod.rs crates/isd/src/doctor/fixers/expose_host.rs
git commit -m "feat: wire local doctor fixer flow"
```

### Task 7: Add Controller Fetch and Apply Support

**Files:**
- Modify: `crates/isd/src/compose_cmd.rs`
- Modify: `crates/isd/src/doctor/apply.rs`
- Modify: `crates/isd/src/doctor/mod.rs`

- [ ] **Step 1: Expose compose response and helpers**

In `compose_cmd.rs`, make these public within crate:

```rust
pub(crate) struct ComposeResponse {
    pub compose_yaml: String,
    pub sha256: String,
}

pub(crate) async fn fetch_compose(session: &Session, stack_id: &str) -> Result<Option<ComposeResponse>>

pub(crate) async fn put_compose(
    session: &Session,
    stack_id: &str,
    body: &str,
    expected_sha256: &str,
    force: bool,
) -> Result<PutOk>
```

Make `PutOk.written_sha256` visible if doctor prints it:

```rust
pub(crate) struct PutOk {
    pub written_sha256: String,
}
```

- [ ] **Step 2: Add controller apply target**

In `doctor/apply.rs`, extend enum:

```rust
pub enum ApplyTarget {
    LocalFile { path: PathBuf },
    ControllerStack {
        session: crate::session::Session,
        stack_id: String,
        stack_name: String,
        sha256: String,
    },
}
```

If `Session` is not cloneable or storable, change `apply` to accept `&Session` and store only metadata:

```rust
pub enum ApplyTarget {
    LocalFile { path: PathBuf },
    ControllerStack { stack_id: String, stack_name: String, sha256: String },
}
```

Then implement controller branch:

```rust
ApplyTarget::ControllerStack { stack_id, sha256, .. } => {
    let session = session.ok_or_else(|| anyhow::anyhow!("controller apply requires a session"))?;
    crate::compose_cmd::put_compose(session, stack_id, body, sha256, force).await?;
    Ok(())
}
```

Pick one signature and keep it consistent.

- [ ] **Step 3: Resolve controller metadata in doctor**

Update `ComposeSource::Controller` to carry stack id and sha:

```rust
Controller {
    name: String,
    id: String,
    sha256: String,
}
```

In `resolve_compose`, replace `fetch_compose_yaml_by_name` with:

```rust
let session = Session::open(context).await?;
let id = crate::compose_cmd::resolve_stack_id(&session, t).await?;
let current = crate::compose_cmd::fetch_compose(&session, &id).await?;
match current {
    Some(row) => {
        let document = document::ComposeDocument::parse_controller_yaml(&row.compose_yaml)?;
        Ok(ResolvedCompose {
            body: row.compose_yaml,
            document,
            source: ComposeSource::Controller { name: t.to_string(), id, sha256: row.sha256 },
        })
    }
    None => Err(anyhow::anyhow!("stack {t:?} has no stored compose; doctor --fix cannot mutate missing compose")),
}
```

- [ ] **Step 4: Apply controller changes from `run_fix`**

Update the controller match arm:

```rust
ComposeSource::Controller { name, id, sha256 } => {
    let session = Session::open(context).await?;
    apply::ApplyTarget::ControllerStack {
        stack_id: id,
        stack_name: name.clone(),
        sha256,
    }
    .apply_with_session(&session, &proposed, args.force)
    .await?;
    println!("Applied fixes to stack {:?}. Run `isd stack doctor {}` to verify.", name, name);
}
```

Use the exact method signature chosen in Step 2.

- [ ] **Step 5: Add wiremock test for apply target**

If `Session` can be constructed in tests from an explicit URL, add a focused test in `apply.rs` using `wiremock`. If not, skip wiremock and rely on `compose_cmd::put_compose` existing coverage.

The test should assert a PUT to `/api/v1/stacks/42/compose` with `If-Match: abc` and body `services: {}` returns success.

- [ ] **Step 6: Run tests**

Run: `cargo test -p isd doctor --lib`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/isd/src/compose_cmd.rs crates/isd/src/doctor/mod.rs crates/isd/src/doctor/apply.rs
git commit -m "feat: apply doctor fixes to controller stacks"
```

### Task 8: Add TOML and Manifest End-to-End Unit Coverage

**Files:**
- Modify: `crates/isd/src/doctor/document.rs`
- Modify: `crates/isd/src/doctor/apply.rs`
- Modify: `crates/isd/src/doctor/mod.rs`

- [ ] **Step 1: Add TOML mutation round-trip test**

In `document.rs` tests, add:

```rust
#[test]
fn toml_expose_fix_serializes_quoted_label_keys() {
    let mut doc = ComposeDocument::parse_path(
        Path::new("compose.toml"),
        "[services.plex]\nimage = \"plex\"\nports = [\"32400:32400\"]\n",
    )
    .unwrap();
    crate::doctor::fixers::expose_host::apply_expose_host(
        &mut doc.value,
        &crate::doctor::fixers::expose_host::ExposeHostInput {
            service: "plex".into(),
            hostname: "plex.vallee.casa".into(),
            port: Some(32400),
        },
    )
    .unwrap();
    let rendered = doc.to_string().unwrap();
    assert!(rendered.contains("[services.plex.labels]"), "{rendered}");
    assert!(rendered.contains("\"isengard.expose\" = \"plex.vallee.casa\""), "{rendered}");
    assert!(rendered.contains("\"isengard.expose.port\" = \"32400\""), "{rendered}");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p isd doctor::document::tests::toml_expose_fix_serializes_quoted_label_keys --lib`

Expected: PASS, or FAIL if `toml::to_string_pretty` does not quote dotted keys correctly.

- [ ] **Step 3: Fix quoted TOML keys if needed**

If Step 2 fails because dotted keys serialize as nested tables, switch TOML serialization for labels to preserve dotted keys by post-processing only labels maps.

Add a helper in `document.rs` before `yaml_value_to_toml` inserts map keys:

```rust
fn toml_key(key: &str) -> String {
    key.to_string()
}
```

If `toml` crate still treats dotted keys as dotted paths, add `toml_edit` as a dependency only after confirming the existing `toml` crate cannot serialize the required shape. Prefer avoiding the dependency until proven necessary.

- [ ] **Step 4: Add manifest single-compose resolver test**

In `apply.rs` tests, add:

```rust
#[test]
fn manifest_with_one_compose_resolves_path() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = tmp.path().join("stack.toml");
    std::fs::write(&stack, "name = \"demo\"\ncompose = [\"compose.toml\"]\n").unwrap();
    let resolved = resolve_manifest_compose_path(&stack).unwrap();
    assert_eq!(resolved, tmp.path().join("compose.toml"));
}
```

- [ ] **Step 5: Run all doctor tests**

Run: `cargo test -p isd doctor --lib`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/isd/src/doctor/document.rs crates/isd/src/doctor/apply.rs crates/isd/src/doctor/mod.rs
git commit -m "test: cover doctor toml and manifest fixes"
```

### Task 9: Polish CLI Output and Verification

**Files:**
- Modify: `crates/isd/src/doctor/mod.rs`
- Modify: `crates/isd/src/doctor/apply.rs`

- [ ] **Step 1: Update stale v0.2 copy**

Replace the read-only footer:

```rust
println!(
    "{} finding(s). Re-run with `isd stack doctor --fix <target>` to repair fixable findings.",
    findings.len()
);
```

- [ ] **Step 2: Add no-fixable-findings output**

In `run_fix`, if findings exist but none have `fixer_id_for`, print:

```rust
println!("{} finding(s), but none have registered fixers yet.", findings.len());
```

- [ ] **Step 3: Add post-apply audit**

After mutation and before diff, run:

```rust
let remaining = audit(&resolved.document.value);
if remaining.iter().any(|f| f.id == "EXPOSE_HOST_MISSING") {
    eprintln!("warning: some expose findings remain after selected fixes");
}
```

- [ ] **Step 4: Run fmt and focused tests**

Run: `cargo fmt --check && cargo test -p isd doctor --lib`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/isd/src/doctor/mod.rs crates/isd/src/doctor/apply.rs
git commit -m "fix: polish doctor fixer output"
```

### Task 10: Final Verification and PR Prep

**Files:**
- No code changes expected.

- [ ] **Step 1: Run package verification**

Run: `cargo fmt --check`

Expected: PASS.

- [ ] **Step 2: Run doctor tests**

Run: `cargo test -p isd doctor --lib`

Expected: PASS.

- [ ] **Step 3: Run isd package checks**

Run: `cargo clippy -p isd --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 4: Run fast CI lane**

Run: `just ci-fast`

Expected: PASS.

- [ ] **Step 5: Commit verification note if docs changed**

If no files changed, do not commit. If verification caused formatting changes, commit them:

```bash
git add crates/isd/src/doctor crates/isd/src/compose_cmd.rs
git commit -m "chore: format doctor fixer implementation"
```

- [ ] **Step 6: Open PR**

Run:

```bash
git status --short
git log --oneline origin/next..HEAD
git push -u origin doctor-fixer-framework
gh pr create --base next --head doctor-fixer-framework --title "feat: add stack doctor fixer framework" --body "## Summary
- add structured doctor findings and fixer registry
- add local YAML/TOML expose-label fixer
- apply controller stack fixes through stored compose

## Tests
- cargo fmt --check
- cargo test -p isd doctor --lib
- cargo clippy -p isd --all-targets -- -D warnings
- just ci-fast"
```

Expected: PR URL printed.

## Self-Review

Spec coverage:

- Read-only doctor preserved: Tasks 4 and 9.
- `--fix`, `--yes`, `--force`: Task 6 and Task 7.
- Structured findings: Task 1.
- Fixer registry: Task 3.
- YAML and TOML local mutation: Tasks 2, 3, 8.
- Manifest layout support and ambiguity guard: Tasks 5 and 8.
- Controller stored compose apply with hash: Task 7.
- No `.isd` syntax: explicitly in implementation notes and no task adds one.

Placeholder scan:

- The only `todo!` appears inside a failing-test scaffold and is replaced in the same task.
- No task says to add unspecified validation or tests.

Type consistency:

- `FindingTarget`, `FixSpec`, `ComposeDocument`, `ApplyTarget`, and `ExposeHostInput` names are introduced before later tasks reference them.
- Controller helper visibility is changed before doctor apply uses it.
