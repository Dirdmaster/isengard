//! Pull orchestration. Implementation lands in dispatch B step 4.

#![allow(dead_code)]

use std::path::Path;

use crate::error::WispImageError;
use crate::reference::ImageRef;
use crate::store::ContentStore;

/// One layer in a pulled image. Carries enough for the layer extractor
/// (dispatch C) to fetch the blob from the store and decompress it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerRef {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

/// Result of a successful pull. The manifest, config, and every layer
/// blob are persisted to the store; this struct is just the in-memory
/// summary that callers index by.
#[derive(Debug, Clone)]
pub struct PulledImage {
    pub r: ImageRef,
    pub manifest_digest: String,
    pub config: oci_spec::image::ImageConfiguration,
    pub layers: Vec<LayerRef>,
}

/// Stub: dispatch B step 4 will populate this with the real pull
/// state machine.
pub struct Client {
    #[allow(dead_code)]
    store: ContentStore,
    #[allow(dead_code)]
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(store_dir: &Path) -> Result<Self, WispImageError> {
        let store = ContentStore::new(store_dir)?;
        let http = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| WispImageError::Network(format!("http client: {e}")))?;
        Ok(Self { store, http })
    }
}
