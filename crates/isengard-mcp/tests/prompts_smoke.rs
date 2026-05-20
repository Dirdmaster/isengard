//! End-to-end smoke test for the embedded skills catalogue.
//!
//! `skills/` is empty in v1 (only a `README.md` placeholder with
//! no front-matter), so the catalogue is expected to be empty until
//! Phase 6 of the docs+AI plan lands real skills. These tests assert
//! the shape holds:
//!
//! - `list_skills` does not panic on the empty placeholder.
//! - Every listed skill has a parseable front-matter.
//! - Argument substitution works end-to-end via `render_prompt`.

use std::collections::BTreeMap;

use isengard_mcp::{ParsedSkill, list_skills, render_prompt};

#[test]
fn list_skills_handles_the_placeholder_readme() {
    // `skills/README.md` carries no YAML front-matter and must be
    // dropped silently rather than panicking.
    let skills = list_skills();
    for skill in &skills {
        assert!(!skill.name.is_empty(), "skill name must be non-empty");
    }
}

#[test]
fn render_prompt_substitutes_arguments() {
    let skill = ParsedSkill {
        name: "test".into(),
        title: Some("Test".into()),
        parameters: Vec::new(),
        body: "Container is {service}; host is {host}.".into(),
    };
    let mut args = BTreeMap::new();
    args.insert("service".into(), "plex".into());
    args.insert("host".into(), "morgul.iso".into());
    let rendered = render_prompt(&skill, &args);
    assert_eq!(rendered, "Container is plex; host is morgul.iso.");
}
