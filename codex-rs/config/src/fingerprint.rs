use crate::ConfigLayerMetadata;
use crate::merge::is_structured_feature_path;
use codex_features::FEATURES;
use codex_features::Features;
use codex_features::legacy_feature_keys;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use toml::Value as TomlValue;

pub(super) fn record_origins(
    value: &TomlValue,
    meta: &ConfigLayerMetadata,
    path: &mut Vec<String>,
    origins: &mut HashMap<String, ConfigLayerMetadata>,
) {
    match value {
        TomlValue::Table(table) => {
            for (key, val) in table {
                path.push(key.clone());
                record_origins(val, meta, path, origins);
                path.pop();
            }
        }
        TomlValue::Array(items) => {
            for (idx, item) in (0_i32..).zip(items.iter()) {
                path.push(idx.to_string());
                record_origins(item, meta, path, origins);
                path.pop();
            }
        }
        _ => {
            if !path.is_empty() {
                if matches!(value, TomlValue::Boolean(_)) && is_structured_feature_path(path) {
                    if path
                        .last()
                        .is_some_and(|feature| feature == "network_proxy")
                    {
                        origins.insert(path.join("."), meta.clone());
                    }
                    path.push("enabled".to_string());
                    origins.insert(path.join("."), meta.clone());
                    path.pop();
                    return;
                }
                origins.insert(path.join("."), meta.clone());
            }
        }
    }
}

/// Return a deterministic digest for a parsed config layer.
///
/// Canonicalization sorts object keys before hashing the complete parsed
/// representation. The digest is used for trust and configuration identity:
/// secret-bearing values therefore participate in the identity just like every
/// other effective value, while the function never returns or logs the values.
pub fn version_for_toml(value: &TomlValue) -> String {
    let json = serde_json::to_value(value).unwrap_or(JsonValue::Null);
    let canonical = canonical_json(&json);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    sha256_revision(&serialized)
}

/// Returns a canonical revision for a set of effective, non-secret config
/// layers. Disabled layers are deliberately omitted: changing a layer that
/// cannot affect runtime behavior must not change the effective revision.
pub(super) fn revision_for_layers(layers: impl Iterator<Item = (String, String)>) -> String {
    let layers = layers
        .map(|(source, version)| {
            let mut layer = JsonMap::new();
            layer.insert("source".to_string(), JsonValue::String(source));
            layer.insert("version".to_string(), JsonValue::String(version));
            JsonValue::Object(layer)
        })
        .collect::<Vec<_>>();
    let serialized =
        serde_json::to_vec(&canonical_json(&JsonValue::Array(layers))).unwrap_or_default();
    sha256_revision(&serialized)
}

/// Returns the revision of the effective runtime feature enablement set.
///
/// This is intentionally separate from [`version_for_toml`]: feature defaults,
/// legacy aliases, and dependency normalization can affect runtime behavior
/// without the layer's other settings changing. Only canonical feature names
/// and booleans enter the digest; feature configuration payloads are omitted.
pub(super) fn runtime_feature_revision(value: &TomlValue) -> String {
    let mut features = Features::with_defaults();

    if let Some(feature_table) = value.get("features").and_then(TomlValue::as_table) {
        let mut configured = BTreeMap::new();
        for (key, value) in feature_table {
            let enabled = value
                .as_bool()
                .or_else(|| value.get("enabled").and_then(TomlValue::as_bool));
            if let Some(enabled) = enabled {
                configured.insert(key.clone(), enabled);
            }
        }
        features.apply_map(&configured);
    }

    let mut legacy = BTreeMap::new();
    for key in legacy_feature_keys() {
        if let Some(enabled) = value.get(key).and_then(TomlValue::as_bool) {
            legacy.insert(key.to_string(), enabled);
        }
    }
    features.apply_map(&legacy);
    features.normalize_dependencies();

    let enabled = FEATURES
        .iter()
        .map(|spec| (spec.key, features.enabled(spec.id)))
        .collect::<BTreeMap<_, _>>();
    let serialized = serde_json::to_vec(&enabled).unwrap_or_default();
    sha256_revision(&serialized)
}

fn sha256_revision(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    let hex = hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(val) = map.get(&key) {
                    sorted.insert(key, canonical_json(val));
                }
            }
            JsonValue::Object(sorted)
        }
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}
