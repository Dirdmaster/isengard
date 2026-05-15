//! Shared `--format` enum for list commands. One mental model across
//! the CLI: every list takes `--format <table|json>` and defaults to
//! `table`.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Table,
    Json,
}
