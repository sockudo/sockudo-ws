//! WAMP serialization fixtures from the pinned Autobahn 0.10.9 reference.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// One WAMP message represented as a raw message and two wire encodings.
#[derive(Debug, Deserialize, Serialize)]
pub struct Vector {
    /// Reference message description.
    pub name: String,
    /// Raw WAMP message array.
    pub rmsg: Value,
    /// JSON wire encoding.
    #[serde(default)]
    pub json: String,
    /// Hex-encoded MessagePack wire encoding.
    #[serde(default)]
    pub msgpack: String,
}
/// Generate all 40 reference messages using native Rust serializers.
pub fn vectors() -> Result<Vec<Vector>> {
    let mut vectors: Vec<Vector> = serde_json::from_str(include_str!("../catalog/serializer.json"))
        .map_err(|e| Error::Config(e.to_string()))?;
    for vector in &mut vectors {
        vector.json =
            serde_json::to_string(&vector.rmsg).map_err(|e| Error::Config(e.to_string()))?;
        let encoded = rmp_serde::to_vec(&vector.rmsg).map_err(|e| Error::Config(e.to_string()))?;
        vector.msgpack = encoded.iter().map(|b| format!("{b:02x}")).collect();
    }
    Ok(vectors)
}
/// Write upstream-shaped JSON containing all generated serializer vectors.
pub fn write(path: impl AsRef<Path>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&vectors()?).map_err(|e| Error::Config(e.to_string()))?;
    std::fs::write(path, bytes)?;
    Ok(())
}
