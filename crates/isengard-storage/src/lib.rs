//! SQLite-backed inventory storage for the Isengard controller.
//!
//! Phase 2b surface: hosts table only. Containers and journal land in
//! later phases as new migrations.

pub mod acme_account;
pub mod adapter_config;
pub mod agent_cert;
pub mod ca;
pub mod deployment;
pub mod enrollment_token;
pub mod error;
pub mod fleet;
pub mod host;
pub mod host_action;
pub mod inventory;
pub mod journal;
pub mod routing_rule;
pub mod routing_rule_override;
pub mod service;
pub mod setting;
pub mod stack;
pub mod tls_cert;

pub use acme_account::{AcmeAccount, UpsertAcmeAccount};
pub use adapter_config::{AdapterConfig, UpsertAdapterConfig};
pub use agent_cert::AgentCert;
pub use ca::CaRow;
pub use enrollment_token::{EnrollmentTokenRecord, TokenRole};
pub use error::{Error, Result};
pub use fleet::Fleet;
pub use host::{EnrollHost, Host, HostId};
pub use host_action::{HostAction, HostActionId, HostActionKind};
pub use inventory::Inventory;
pub use journal::{EventRow, InsertEvent, Journal};
pub use routing_rule::{
    InsertRoutingRule, RoutingRule, RoutingRuleId, RoutingRuleSource, RoutingRuleState, TlsMode,
};
pub use routing_rule_override::RoutingRuleOverride;
pub use service::{InsertService, Service, ServiceId, ServiceState};
pub use setting::Setting;
pub use stack::{InsertStack, Stack, StackId, StackSource};
pub use tls_cert::{TlsCertMeta, UpsertTlsCertMeta};
