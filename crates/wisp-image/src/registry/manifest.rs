//! Manifest + index deserialisation with arch selection.
//!
//! OCI registries return one of three shapes from
//! `/v2/<repo>/manifests/<ref>`:
//!
//!   - `application/vnd.oci.image.manifest.v1+json` (single arch)
//!   - `application/vnd.oci.image.index.v1+json` (multi-arch index)
//!   - `application/vnd.docker.distribution.manifest.v2+json` (Docker v2,
//!     identical schema to OCI image manifest, different mediaType)
//!
//! The Content-Type header is the canonical signal, but registries
//! sometimes elide it (or return `application/json`). We fall back to
//! inspecting the JSON body's own `mediaType` field, and finally to
//! shape-detection (`manifests` vs. `layers`) so we can still parse
//! schemaless responses from older registries.
//!
//! Arch selection follows containerd's GOARCH/GOOS naming. The local
//! host's `std::env::consts::ARCH` is mapped to OCI canonical names
//! (`x86_64` -> `amd64`, `aarch64` -> `arm64`); other archs pass
//! through unchanged.

use oci_spec::image::{Descriptor, ImageIndex, ImageManifest};

use crate::error::WispImageError;

/// Either flavour of manifest a registry can return at the
/// `/manifests/<ref>` endpoint. The variant determines whether the
/// caller needs to recurse via `select_arch_entry` (Index) or proceed
/// to fetching config + layers (Image).
///
/// Variants are boxed: `oci_spec::image::ImageManifest` is ~800 bytes
/// (it carries the full Descriptor + Vec<Descriptor> + annotations),
/// and clippy rightly complains when the smaller variant ends up
/// padded to match. Boxing keeps the enum to a single pointer-width
/// regardless of which arm is populated.
#[derive(Debug)]
pub enum Manifest {
    Image(Box<ImageManifest>),
    Index(Box<ImageIndex>),
}

/// OCI media type for an image manifest.
pub const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// OCI media type for an image index.
pub const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
/// Docker v2 manifest (same shape as OCI manifest).
pub const MT_DOCKER_V2_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
/// Docker manifest list (same shape as OCI index).
pub const MT_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";

/// Parse a manifest response body. The `content_type_header` is what
/// the server returned (e.g. via `Content-Type:`); pass `None` when
/// the header was missing or the value was a generic `application/json`.
pub fn parse(body: &[u8], content_type_header: Option<&str>) -> Result<Manifest, WispImageError> {
    // 1. Authoritative source: the response Content-Type header.
    if let Some(ct) = content_type_header {
        // `application/vnd.oci.image.manifest.v1+json; charset=utf-8`:
        // strip params before matching.
        let primary = ct.split(';').next().unwrap_or("").trim();
        match primary {
            MT_OCI_MANIFEST | MT_DOCKER_V2_MANIFEST => {
                return decode_image_manifest(body);
            }
            MT_OCI_INDEX | MT_DOCKER_MANIFEST_LIST => {
                return decode_image_index(body);
            }
            _ => {
                // Fall through to body inspection.
            }
        }
    }

    // 2. Body's own mediaType field.
    #[derive(serde::Deserialize)]
    struct Probe<'a> {
        #[serde(default, borrow, rename = "mediaType")]
        media_type: Option<&'a str>,
        // Index documents always carry a `manifests` array; image
        // manifests carry `layers`. Used as the third-line fallback
        // when both header and body mediaType are absent.
        #[serde(default)]
        manifests: Option<serde_json::Value>,
        #[serde(default)]
        layers: Option<serde_json::Value>,
    }
    let probe: Probe<'_> = serde_json::from_slice(body)
        .map_err(|e| WispImageError::Manifest(format!("body is not JSON: {e}")))?;

    if let Some(mt) = probe.media_type {
        match mt {
            MT_OCI_MANIFEST | MT_DOCKER_V2_MANIFEST => return decode_image_manifest(body),
            MT_OCI_INDEX | MT_DOCKER_MANIFEST_LIST => return decode_image_index(body),
            _ => {
                // fall through to shape detection
            }
        }
    }

    // 3. Shape detection: an Index has a `manifests` array; an
    // Image manifest has a `layers` array.
    if probe.manifests.is_some() {
        return decode_image_index(body);
    }
    if probe.layers.is_some() {
        return decode_image_manifest(body);
    }

    Err(WispImageError::Manifest(
        "manifest body has no recognisable mediaType, manifests, or layers field".into(),
    ))
}

fn decode_image_manifest(body: &[u8]) -> Result<Manifest, WispImageError> {
    let m = ImageManifest::from_reader(body)
        .map_err(|e| WispImageError::Manifest(format!("ImageManifest parse: {e}")))?;
    Ok(Manifest::Image(Box::new(m)))
}

fn decode_image_index(body: &[u8]) -> Result<Manifest, WispImageError> {
    let i = ImageIndex::from_reader(body)
        .map_err(|e| WispImageError::Manifest(format!("ImageIndex parse: {e}")))?;
    Ok(Manifest::Index(Box::new(i)))
}

/// Pick the descriptor in `index` whose `platform.architecture +
/// platform.os` matches the requested values. Returns `None` when no
/// entry matches; the caller (orchestrator) treats that as a hard
/// error since pulling an unknown-arch image isn't useful.
///
/// Both `target_arch` and `target_os` are compared via the Display
/// impl of `Arch` / `Os`, which produces the canonical OCI / GOARCH
/// strings (`amd64`, `arm64`, `linux`, ...).
pub fn select_arch_entry(
    index: &ImageIndex,
    target_arch: &str,
    target_os: &str,
) -> Option<Descriptor> {
    for descriptor in index.manifests() {
        let Some(platform) = descriptor.platform() else {
            continue;
        };
        let arch = platform.architecture().to_string();
        let os = platform.os().to_string();
        if arch == target_arch && os == target_os {
            return Some(descriptor.clone());
        }
    }
    None
}

/// Map `std::env::consts::ARCH` to the canonical OCI / GOARCH name
/// the manifest format uses. Only the two archs we actually run on
/// (Mac dev box for arm64, Linux CI / hosts for amd64) need explicit
/// mapping; the rest pass through unchanged so the function stays
/// truthful for whatever target the caller is compiled for.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        // Other archs pass through verbatim. Most match (`riscv64`,
        // `mips64`); ones that don't will fail `select_arch_entry` at
        // pull time, which is the correct behaviour: we'd rather
        // refuse than silently pull the wrong arch.
        other => other,
    }
}

/// Wisp targets Linux containers exclusively in 0.2 (the runtime is
/// Linux-namespace-based). This helper exists so `host_arch` has a
/// matching `host_os` for symmetry.
pub fn host_os() -> &'static str {
    "linux"
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::image::{
        Arch, DescriptorBuilder, ImageIndexBuilder, MediaType, Os, PlatformBuilder, Sha256Digest,
    };
    use std::path::PathBuf;
    use std::str::FromStr;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn read_fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixtures_dir().join(name))
            .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
    }

    #[test]
    fn parses_oci_image_manifest() {
        let body = read_fixture("oci_image_manifest.json");
        let m = parse(&body, Some(MT_OCI_MANIFEST)).expect("parse");
        match m {
            Manifest::Image(m) => {
                assert_eq!(m.layers().len(), 2);
            }
            Manifest::Index(_) => panic!("expected Image"),
        }
    }

    #[test]
    fn parses_oci_image_index() {
        let body = read_fixture("oci_image_index.json");
        let m = parse(&body, Some(MT_OCI_INDEX)).expect("parse");
        match m {
            Manifest::Index(idx) => {
                assert_eq!(idx.manifests().len(), 3);
            }
            Manifest::Image(_) => panic!("expected Index"),
        }
    }

    #[test]
    fn parses_docker_v2_manifest() {
        let body = read_fixture("docker_v2_manifest.json");
        let m = parse(&body, Some(MT_DOCKER_V2_MANIFEST)).expect("parse");
        match m {
            Manifest::Image(m) => {
                assert_eq!(m.layers().len(), 1);
            }
            Manifest::Index(_) => panic!("expected Image"),
        }
    }

    #[test]
    fn parses_with_content_type_charset_param() {
        // Some registries append `; charset=utf-8`. The parser must
        // strip params before matching the media type.
        let body = read_fixture("oci_image_manifest.json");
        let header = format!("{MT_OCI_MANIFEST}; charset=utf-8");
        let m = parse(&body, Some(&header)).expect("parse");
        assert!(matches!(m, Manifest::Image(_)));
    }

    #[test]
    fn parse_falls_back_to_field_inspection_when_content_type_missing() {
        // No header: parser inspects the JSON body's own mediaType.
        let body = read_fixture("oci_image_index.json");
        let m = parse(&body, None).expect("parse");
        assert!(matches!(m, Manifest::Index(_)));

        let body = read_fixture("oci_image_manifest.json");
        let m = parse(&body, None).expect("parse");
        assert!(matches!(m, Manifest::Image(_)));
    }

    #[test]
    fn parse_falls_back_to_shape_when_media_type_missing_everywhere() {
        // Manually-built body without a top-level mediaType. Shape
        // detection picks up the `manifests` array.
        let body = br#"{
            "schemaVersion": 2,
            "manifests": []
        }"#;
        let m = parse(body, None).expect("parse");
        assert!(matches!(m, Manifest::Index(_)));
    }

    #[test]
    fn parse_errors_on_unrecognisable_body() {
        let body = b"{}";
        let res = parse(body, None);
        assert!(res.is_err());
    }

    #[test]
    fn select_arch_entry_picks_matching_arch() {
        let body = read_fixture("oci_image_index.json");
        let m = parse(&body, Some(MT_OCI_INDEX)).expect("parse");
        let Manifest::Index(idx) = m else {
            panic!("expected Index");
        };
        let descriptor =
            select_arch_entry(&idx, "arm64", "linux").expect("expected linux/arm64 entry");
        assert_eq!(
            descriptor.digest().to_string(),
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn select_arch_entry_returns_none_when_no_match() {
        // Build an index programmatically with only a windows entry,
        // then ask for linux/arm64.
        let descriptor = DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .digest(
                Sha256Digest::from_str(
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap(),
            )
            .size(1u64)
            .platform(
                PlatformBuilder::default()
                    .architecture(Arch::from("amd64"))
                    .os(Os::from("windows"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let idx = ImageIndexBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageIndex)
            .manifests(vec![descriptor])
            .build()
            .unwrap();
        assert!(select_arch_entry(&idx, "arm64", "linux").is_none());
    }

    #[test]
    fn host_arch_maps_canonical_names() {
        // We can't assert a single value (the test runs on whatever
        // arch the developer has), but we can assert the mapping is
        // sane for the two we care about.
        let mapped = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        assert_eq!(host_arch(), mapped);
        assert_eq!(host_os(), "linux");
    }
}
