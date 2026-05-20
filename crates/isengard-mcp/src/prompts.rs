//! `prompts/list` and `prompts/get` over the embedded skills tree.
//!
//! Each `skills/<name>.md` becomes one MCP prompt. The file's YAML
//! front-matter declares the parameters the host should solicit
//! before calling `prompts/get`; the markdown body below the
//! front-matter is the prompt body.
//!
//! Front-matter shape (matches the operator's Superpowers skills):
//!
//! ```yaml
//! ---
//! title: Add a route
//! parameters:
//!   service_name:
//!     description: Container name or compose service.
//!     required: true
//!   public_hostname:
//!     description: Hostname the route exposes.
//!     required: false
//! returns: The created routing rule id.
//! ---
//! ```
//!
//! Files without front-matter are skipped with a warning log. The
//! placeholder `skills/README.md` falls into that bucket: it carries
//! no front-matter, so it never surfaces as a prompt. Phase 6 of the
//! docs+AI plan fills `skills/` with real playbooks.

use std::collections::BTreeMap;

use include_dir::DirEntry;
use serde::Deserialize;

use crate::embedded::SKILLS;

/// One parsed skill. Built from `skills/<name>.md` at startup; the
/// `name` matches the file stem.
#[derive(Debug, Clone)]
pub struct ParsedSkill {
    /// File stem. Becomes the prompt name (e.g. `add-a-route`).
    pub name: String,
    /// Front-matter `title` field. Used as the prompt description.
    pub title: Option<String>,
    /// Parameters declared in the front-matter, in declaration order.
    pub parameters: Vec<SkillParameter>,
    /// Markdown body below the front-matter. This is the raw
    /// playbook text the host hands to the LLM.
    pub body: String,
}

/// One declared parameter on a skill.
#[derive(Debug, Clone)]
pub struct SkillParameter {
    /// Argument name passed in `prompts/get` arguments.
    pub name: String,
    /// Human-readable description for the host.
    pub description: Option<String>,
    /// Whether the host must provide a value. Defaults to `false`
    /// when the front-matter omits the field.
    pub required: bool,
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    parameters: Option<BTreeMap<String, FrontMatterParam>>,
}

#[derive(Debug, Deserialize)]
struct FrontMatterParam {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required: Option<bool>,
}

/// Walk `skills/`. Returns one [`ParsedSkill`] per file with valid
/// front-matter. Skills without front-matter are dropped (a `warn!`
/// log records each skip).
pub fn list_skills() -> Vec<ParsedSkill> {
    let mut out = Vec::new();
    for entry in SKILLS.entries() {
        let DirEntry::File(file) = entry else {
            continue;
        };
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let raw = match file.contents_utf8() {
            Some(s) => s,
            None => continue,
        };
        match parse(raw) {
            Some((fm, body)) => {
                let parameters = front_matter_params(&fm);
                out.push(ParsedSkill {
                    name,
                    title: fm.title,
                    parameters,
                    body: body.to_string(),
                });
            }
            None => {
                tracing::warn!(
                    skill = %name,
                    "skill markdown has no YAML front-matter; skipped"
                );
            }
        }
    }
    out
}

/// Render a skill's body for `prompts/get`. The host can pass a
/// `name` and `arguments` map; arguments are spliced into the body
/// by simple `{name}` substitution. Unsubstituted placeholders are
/// left intact so the LLM can fill them.
pub fn render_prompt(skill: &ParsedSkill, arguments: &BTreeMap<String, String>) -> String {
    let mut out = skill.body.clone();
    for (key, value) in arguments {
        let needle = format!("{{{key}}}");
        out = out.replace(&needle, value);
    }
    out
}

fn parse(raw: &str) -> Option<(FrontMatter, &str)> {
    // Front-matter is delimited by lines of exactly `---`. We accept
    // an optional leading BOM and any line endings.
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let trimmed = trimmed.trim_start_matches('\n');
    let after_open = trimmed.strip_prefix("---\n")?;
    let close_idx = after_open.find("\n---\n").or_else(|| {
        // Allow a closing `---` at EOF (no trailing newline).
        if after_open.ends_with("\n---") {
            Some(after_open.len() - 4)
        } else {
            None
        }
    })?;
    let yaml = &after_open[..close_idx];
    let body_start = close_idx + "\n---\n".len();
    let body = if body_start <= after_open.len() {
        // Common case: closing `---` followed by `\n`.
        &after_open[body_start.min(after_open.len())..]
    } else {
        ""
    };
    let fm: FrontMatter = match serde_yaml::from_str(yaml) {
        Ok(fm) => fm,
        Err(err) => {
            tracing::warn!(error = %err, "skill front-matter failed to parse as YAML");
            return None;
        }
    };
    Some((fm, body.trim_start_matches('\n')))
}

fn front_matter_params(fm: &FrontMatter) -> Vec<SkillParameter> {
    let Some(map) = fm.parameters.as_ref() else {
        return Vec::new();
    };
    map.iter()
        .map(|(name, raw)| SkillParameter {
            name: name.clone(),
            description: raw.description.clone(),
            required: raw.required.unwrap_or(false),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_front_matter() {
        let raw = "---\n\
title: Add a route\n\
parameters:\n  service_name:\n    description: A container.\n    required: true\n\
---\n\
# body\n\
hello\n";
        let (fm, body) = parse(raw).expect("front-matter parses");
        assert_eq!(fm.title.as_deref(), Some("Add a route"));
        assert!(body.starts_with("# body"));
        let params = front_matter_params(&fm);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "service_name");
        assert!(params[0].required);
    }

    #[test]
    fn returns_none_when_front_matter_missing() {
        let raw = "# bare heading\nnothing fancy\n";
        assert!(parse(raw).is_none());
    }

    #[test]
    fn render_substitutes_named_arguments() {
        let skill = ParsedSkill {
            name: "demo".into(),
            title: None,
            parameters: Vec::new(),
            body: "Hello, {who}!".into(),
        };
        let mut args = BTreeMap::new();
        args.insert("who".into(), "world".into());
        assert_eq!(render_prompt(&skill, &args), "Hello, world!");
    }

    #[test]
    fn render_leaves_unknown_placeholders_intact() {
        let skill = ParsedSkill {
            name: "demo".into(),
            title: None,
            parameters: Vec::new(),
            body: "Need {a} and {b}".into(),
        };
        let mut args = BTreeMap::new();
        args.insert("a".into(), "one".into());
        assert_eq!(render_prompt(&skill, &args), "Need one and {b}");
    }
}
