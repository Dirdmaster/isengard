//! Shared `--format` enum for list commands. One mental model across
//! the CLI: every list takes `--format <table|json>` and defaults to
//! `table`.

use clap::ValueEnum;

/// Output format for list-style subcommands (`ps`, `hosts list`,
/// `secret list`, ...). Defaults to [`Format::Table`] for a TTY-friendly
/// box-drawn render; [`Format::Json`] emits a parseable structure
/// downstream tools can consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Format {
    /// Boxed, ANSI-styled table for interactive use.
    #[default]
    Table,
    /// Pretty-printed JSON for scripts and pipelines.
    Json,
}
