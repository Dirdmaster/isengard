//! Audit checks for `isd stack doctor`. One file per check; the
//! parent [`crate::doctor::audit`] runs them in order.

pub mod expose_host;
