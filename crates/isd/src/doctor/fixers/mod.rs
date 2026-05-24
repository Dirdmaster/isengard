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
