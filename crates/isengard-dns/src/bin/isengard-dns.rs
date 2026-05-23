//! Thin binary wrapper around [`isengard_dns::IsengardResolver`].
//!
//! Loads a static zone from a file (TOML or YAML) and serves UDP + TCP on
//! one address. This is the **Step 1** binary: the controller-backed
//! `ZoneSource` lands in Step 2 and replaces the zone loader, leaving the
//! resolver core untouched.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use hickory_proto::rr::rdata::{A, AAAA, CNAME};
use hickory_proto::rr::{Name, RData, Record};
use isengard_dns::{IsengardResolver, StaticZoneSource};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

/// Step 1 isengard DNS binary.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Address to bind (UDP and TCP). Bind to a privileged port (e.g. :53)
    /// requires running with `CAP_NET_BIND_SERVICE`.
    #[arg(long, default_value = "127.0.0.1:53", env = "ISENGARD_DNS_BIND")]
    bind: SocketAddr,

    /// Upstream DNS servers, repeatable. Default: Cloudflare 1.1.1.1 + 1.0.0.1.
    #[arg(
        long,
        env = "ISENGARD_DNS_UPSTREAM",
        value_delimiter = ',',
        default_values_t = vec![
            SocketAddr::from(([1, 1, 1, 1], 53)),
            SocketAddr::from(([1, 0, 0, 1], 53)),
        ]
    )]
    upstream: Vec<SocketAddr>,

    /// Path to a zone file (TOML or YAML). Detection is by extension:
    /// `.toml`, `.yaml`, `.yml`. See module docs for the schema.
    #[arg(long, env = "ISENGARD_DNS_ZONE_FILE")]
    zone_file: Option<PathBuf>,
}

/// On-disk shape: a list of record entries.
///
/// ```yaml
/// records:
///   - { name: "foo.weavers.local.", type: A,     value: "10.0.0.1", ttl: 5 }
///   - { name: "v6.weavers.local.",  type: AAAA,  value: "::1",      ttl: 5 }
///   - { name: "alias.weavers.local.", type: CNAME, value: "foo.weavers.local." }
/// ```
#[derive(Debug, Deserialize)]
struct ZoneFile {
    #[serde(default)]
    records: Vec<ZoneEntry>,
}

#[derive(Debug, Deserialize)]
struct ZoneEntry {
    name: String,
    /// `A`, `AAAA`, `CNAME`. Case-insensitive.
    #[serde(rename = "type")]
    rtype: String,
    value: String,
    #[serde(default = "default_ttl")]
    ttl: u32,
}

const fn default_ttl() -> u32 {
    5
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let source = match &cli.zone_file {
        Some(path) => load_zone_file(path)?,
        None => StaticZoneSource::builder().build(),
    };
    tracing::info!(records = source.len(), "isengard-dns: loaded static zone");

    let resolver = IsengardResolver::new(Arc::new(source), cli.upstream.clone())
        .context("building IsengardResolver")?;

    resolver.serve(cli.bind, cli.bind).await
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

fn load_zone_file(path: &Path) -> Result<StaticZoneSource> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading zone file {}", path.display()))?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    let parsed: ZoneFile = match ext.as_deref() {
        Some("toml") => toml::from_str(&contents)
            .with_context(|| format!("parsing TOML zone file {}", path.display()))?,
        Some("yaml" | "yml") | None => serde_yaml::from_str(&contents)
            .with_context(|| format!("parsing YAML zone file {}", path.display()))?,
        Some(other) => {
            anyhow::bail!("unsupported zone file extension `.{other}`; use .toml, .yaml, or .yml",)
        }
    };

    let mut builder = StaticZoneSource::builder();
    for entry in parsed.records {
        builder = builder.record(record_from_entry(&entry)?);
    }
    Ok(builder.build())
}

fn record_from_entry(entry: &ZoneEntry) -> Result<Record> {
    let name = Name::from_str(&entry.name)
        .with_context(|| format!("parsing record name `{}`", entry.name))?;
    let rtype = entry.rtype.to_ascii_uppercase();
    let rdata = match rtype.as_str() {
        "A" => {
            let ip: std::net::Ipv4Addr = entry
                .value
                .parse()
                .with_context(|| format!("parsing A value `{}`", entry.value))?;
            RData::A(A(ip))
        }
        "AAAA" => {
            let ip: std::net::Ipv6Addr = entry
                .value
                .parse()
                .with_context(|| format!("parsing AAAA value `{}`", entry.value))?;
            RData::AAAA(AAAA(ip))
        }
        "CNAME" => {
            let target = Name::from_str(&entry.value)
                .with_context(|| format!("parsing CNAME target `{}`", entry.value))?;
            RData::CNAME(CNAME(target))
        }
        other => anyhow::bail!(
            "unsupported record type `{other}` (Step 1 binary supports A / AAAA / CNAME)"
        ),
    };
    Ok(Record::from_rdata(name, entry.ttl, rdata))
}
