use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::Manager;

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_WASM_BYTES: usize = 8 * 1024 * 1024;
const MAX_REGISTRY_INDEX_BYTES: usize = 2 * 1024 * 1024;
const MAX_REGISTRY_SIGNATURE_BYTES: usize = 1024;
const REGISTRY_CACHE_KEY: &str = "plugin_registry_cache";
static INSTALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PACKAGE_FILES: &[&str] = &["manifest.json", "plugin.wasm"];
const REGISTRY_PUBLIC_KEY: [u8; 32] = [
    0x6c, 0x25, 0xa1, 0xfd, 0x0c, 0x6d, 0xbc, 0x60, 0xca, 0xb7, 0xa4, 0x8c, 0x23, 0x6a, 0xa9, 0x18,
    0x45, 0x66, 0xa6, 0x57, 0xff, 0x69, 0x72, 0x46, 0xd3, 0x0b, 0xaf, 0xc4, 0x7e, 0x17, 0x6c, 0x00,
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PluginRegistryCacheEnvelope {
    schema_version: u8,
    index_json: String,
    signature: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum PluginRegistryCacheLoad {
    Absent,
    Envelope {
        envelope: PluginRegistryCacheEnvelope,
    },
    Unsafe {
        reason: String,
    },
}

fn unsafe_cache(reason: impl Into<String>) -> PluginRegistryCacheLoad {
    PluginRegistryCacheLoad::Unsafe {
        reason: reason.into(),
    }
}

fn load_plugin_registry_cache_at(path: &Path) -> PluginRegistryCacheLoad {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PluginRegistryCacheLoad::Absent;
        }
        Err(error) => {
            return unsafe_cache(format!("registry cache settings are unreadable: {error}"))
        }
    };
    let root: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) if value.is_object() => value,
        Ok(_) => return unsafe_cache("registry cache settings root is not an object"),
        Err(error) => {
            return unsafe_cache(format!("registry cache settings are malformed: {error}"))
        }
    };
    if let Some(value) = root.get(REGISTRY_CACHE_KEY) {
        let envelope: PluginRegistryCacheEnvelope = match serde_json::from_value(value.clone()) {
            Ok(envelope) => envelope,
            Err(error) => {
                return unsafe_cache(format!("registry cache envelope is malformed: {error}"))
            }
        };
        if envelope.schema_version != 1
            || envelope.index_json.is_empty()
            || envelope.index_json.len() > MAX_REGISTRY_INDEX_BYTES
            || envelope.signature.trim().is_empty()
            || envelope.signature.len() > MAX_REGISTRY_SIGNATURE_BYTES
        {
            return unsafe_cache("registry cache envelope violates its size or schema contract");
        }
        return PluginRegistryCacheLoad::Envelope { envelope };
    }

    PluginRegistryCacheLoad::Absent
}

fn store_plugin_registry_cache_at(
    path: &Path,
    index_json: String,
    signature: String,
) -> Result<(), String> {
    if index_json.is_empty() || index_json.len() > MAX_REGISTRY_INDEX_BYTES {
        return Err("plugin registry index is empty or too large".to_string());
    }
    let signature = signature.trim().to_string();
    if signature.is_empty() || signature.len() > MAX_REGISTRY_SIGNATURE_BYTES {
        return Err("plugin registry signature is empty or too large".to_string());
    }
    verify_plugin_registry(index_json.clone(), signature.clone())?;
    let envelope = PluginRegistryCacheEnvelope {
        schema_version: 1,
        index_json,
        signature,
    };
    crate::settings::update_settings_strict_at(path, |json| {
        json[REGISTRY_CACHE_KEY] =
            serde_json::to_value(&envelope).map_err(|error| error.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub(crate) fn load_plugin_registry_cache(app: tauri::AppHandle) -> PluginRegistryCacheLoad {
    crate::settings::settings_path(&app).map_or_else(
        || unsafe_cache("no app-data dir"),
        |path| load_plugin_registry_cache_at(&path),
    )
}

#[tauri::command]
pub(crate) fn store_plugin_registry_cache(
    index_json: String,
    signature: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let path = crate::settings::settings_path(&app).ok_or("no app-data dir")?;
    store_plugin_registry_cache_at(&path, index_json, signature)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct InstalledPlugin {
    id: String,
    version: String,
    manifest_json: String,
    sha256: String,
    selected: bool,
    enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PluginState {
    version: String,
    enabled: bool,
}

fn safe_component(value: &str, dotted: bool) -> bool {
    if value.is_empty() || value.len() > 64 || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || byte == b'-'
            || (dotted && byte == b'.')
    }) && (!dotted || value.contains('.'))
}

fn safe_version(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    let (core, prerelease) = value
        .split_once('-')
        .map_or((value, None), |(core, prerelease)| (core, Some(prerelease)));
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return false;
    }
    prerelease.is_none_or(|suffix| {
        !suffix.is_empty()
            && !suffix.starts_with('.')
            && !suffix.ends_with('.')
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    })
}

fn unique_transient_name(prefix: &str, id: &str, version: &str) -> String {
    let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(".{prefix}-{id}-{version}-{}-{sequence}", std::process::id())
}

fn manifest_identity(manifest_json: &str) -> Result<(String, String), String> {
    if manifest_json.len() > MAX_MANIFEST_BYTES {
        return Err("plugin manifest is too large".to_string());
    }
    let manifest: serde_json::Value =
        serde_json::from_str(manifest_json).map_err(|_| "plugin manifest is invalid JSON")?;
    let id = manifest
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|value| safe_component(value, true))
        .ok_or("plugin id is invalid")?;
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .filter(|value| safe_version(value))
        .ok_or("plugin version is invalid")?;
    Ok((id.to_string(), version.to_string()))
}

fn plugins_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let root = app
        .path()
        .app_data_dir()
        .map(|dir| dir.join("plugins"))
        .map_err(|_| "no app-data dir".to_string())?;
    recover_plugin_store_at(&root)?;
    Ok(root)
}

fn recover_plugin_store_at(root: &Path) -> Result<(), String> {
    tine_storage::recover_package_store(root, PACKAGE_FILES).map_err(|error| error.to_string())
}

fn package_dir(root: &Path, id: &str, version: &str) -> Result<PathBuf, String> {
    if !safe_component(id, true) || !safe_version(version) {
        return Err("plugin identity is invalid".to_string());
    }
    Ok(root.join(id).join(version))
}

/// Validate one immutable package without following a symlink out of plugin
/// storage. The boolean reports whether this is currently the id's last entry.
fn validate_uninstall_target(
    root: &Path,
    id: &str,
    version: &str,
) -> Result<(PathBuf, PathBuf, bool), String> {
    let target = package_dir(root, id, version)?;
    let id_dir = root.join(id);
    let id_meta = std::fs::symlink_metadata(&id_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "plugin version is not installed".to_string()
        } else {
            error.to_string()
        }
    })?;
    if id_meta.file_type().is_symlink() || !id_meta.is_dir() {
        return Err("installed plugin directory is unsafe".to_string());
    }
    let target_meta = std::fs::symlink_metadata(&target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "plugin version is not installed".to_string()
        } else {
            error.to_string()
        }
    })?;
    if target_meta.file_type().is_symlink() || !target_meta.is_dir() {
        return Err("installed plugin package is unsafe".to_string());
    }
    let manifest_json = std::fs::read_to_string(target.join("manifest.json"))
        .map_err(|_| "installed plugin manifest is unreadable".to_string())?;
    if manifest_identity(&manifest_json).ok().as_ref()
        != Some(&(id.to_string(), version.to_string()))
    {
        return Err("installed plugin manifest identity does not match its directory".to_string());
    }
    let mut last_version = true;
    for entry in std::fs::read_dir(&id_dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.path() != target {
            last_version = false;
            break;
        }
    }
    Ok((id_dir, target, last_version))
}

/// Remove exactly one validated immutable package without ever following a
/// symlink out of plugin storage. Returns true when no versions of this plugin
/// remain and the now-empty id directory was removed too.
fn uninstall_package(root: &Path, id: &str, version: &str) -> Result<bool, String> {
    validate_uninstall_target(root, id, version)?;
    for _ in 0..128 {
        let retired_name = unique_transient_name("retired", id, version);
        match tine_storage::retire_package(root, id, version, &retired_name, PACKAGE_FILES) {
            Ok(last_version) => return Ok(last_version),
            Err(tine_storage::PackageStoreError::TransientNameCollision) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not allocate a private plugin retirement name".to_string())
}

fn install_plugin_package_at(
    root: &Path,
    id: &str,
    version: &str,
    manifest_json: &str,
    wasm: &[u8],
) -> Result<tine_storage::PackagePublishOutcome, String> {
    let files = [
        tine_storage::PackageFile {
            name: "manifest.json",
            bytes: manifest_json.as_bytes(),
        },
        tine_storage::PackageFile {
            name: "plugin.wasm",
            bytes: wasm,
        },
    ];
    for _ in 0..128 {
        let staging_name = unique_transient_name("install", id, version);
        match tine_storage::publish_package_noclobber(root, id, version, &staging_name, &files) {
            Ok(outcome) => return Ok(outcome),
            Err(tine_storage::PackageStoreError::TransientNameCollision) => continue,
            Err(tine_storage::PackageStoreError::ImmutableVersionCollision) => {
                return Err(
                    "that immutable plugin version is already installed with different bytes"
                        .to_string(),
                )
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not allocate a private plugin install name".to_string())
}

fn clear_uninstalled_plugin_settings(
    json: &mut serde_json::Value,
    id: &str,
    version: &str,
    last_version: bool,
) {
    let selected_version = json
        .get("plugin_states")
        .and_then(|states| states.get(id))
        .and_then(|state| state.get("version"))
        .and_then(|value| value.as_str());
    if last_version || selected_version == Some(version) {
        if let Some(states) = json
            .get_mut("plugin_states")
            .and_then(serde_json::Value::as_object_mut)
        {
            states.remove(id);
        }
    }
    if last_version {
        if let Some(root) = json.as_object_mut() {
            root.remove(&format!("plugin-settings:{id}"));
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[tauri::command]
pub(crate) fn verify_plugin_registry(
    index_json: String,
    signature_b64: String,
) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    if index_json.len() > MAX_REGISTRY_INDEX_BYTES {
        return Err("plugin registry index is too large".to_string());
    }
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|_| "plugin registry signature is invalid base64")?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| "plugin registry signature has the wrong length")?;
    let key = VerifyingKey::from_bytes(&REGISTRY_PUBLIC_KEY)
        .map_err(|_| "embedded plugin registry key is invalid")?;
    key.verify(index_json.as_bytes(), &signature)
        .map_err(|_| "plugin registry signature did not verify".to_string())
}

fn plugin_states(app: &tauri::AppHandle) -> std::collections::HashMap<String, PluginState> {
    crate::settings::settings_path(app)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json.get("plugin_states").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Persist an immutable plugin version. Installation never executes the guest and
/// leaves it disabled; enabling is a separate explicit action after the frontend
/// has validated the complete manifest and WebAssembly ABI.
#[tauri::command]
pub(crate) fn install_plugin(
    manifest_json: String,
    wasm_b64: String,
    app: tauri::AppHandle,
) -> Result<InstalledPlugin, String> {
    let (id, version) = manifest_identity(&manifest_json)?;
    let wasm = base64::engine::general_purpose::STANDARD
        .decode(wasm_b64)
        .map_err(|_| "plugin entry is not valid base64")?;
    if wasm.len() > MAX_WASM_BYTES {
        return Err("plugin entry is too large".to_string());
    }
    if !wasm.starts_with(b"\0asm\x01\0\0\0") {
        return Err("plugin entry is not WebAssembly".to_string());
    }
    let digest = sha256(&wasm);
    let root = plugins_dir(&app)?;
    install_plugin_package_at(&root, &id, &version, &manifest_json, &wasm)?;
    Ok(InstalledPlugin {
        id,
        version,
        manifest_json,
        sha256: digest,
        selected: false,
        enabled: false,
    })
}

/// Uninstall removes only the app-local immutable package. It clears a selected
/// version before deleting bytes so a crash cannot leave startup pointing at a
/// half-removed plugin. Per-plugin settings are retained while another version
/// remains and removed with the last version. Graph files are never in scope.
#[tauri::command]
pub(crate) fn uninstall_plugin(
    id: String,
    version: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let root = plugins_dir(&app)?;
    let (_, _, last_version) = validate_uninstall_target(&root, &id, &version)?;
    crate::settings::update_settings(&app, |json| {
        clear_uninstalled_plugin_settings(json, &id, &version, last_version)
    })?;
    uninstall_package(&root, &id, &version)?;
    Ok(())
}

#[tauri::command]
pub(crate) fn list_installed_plugins(app: tauri::AppHandle) -> Vec<InstalledPlugin> {
    let Ok(root) = plugins_dir(&app) else {
        return Vec::new();
    };
    let states = plugin_states(&app);
    let mut installed = Vec::new();
    let Ok(ids) = std::fs::read_dir(root) else {
        return installed;
    };
    for id_entry in ids.flatten().filter(|entry| entry.path().is_dir()) {
        let Some(id) = id_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !safe_component(&id, true) {
            continue;
        }
        let Ok(versions) = std::fs::read_dir(id_entry.path()) else {
            continue;
        };
        for version_entry in versions.flatten().filter(|entry| entry.path().is_dir()) {
            let Some(version) = version_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !safe_version(&version) {
                continue;
            }
            let Ok(manifest_json) =
                std::fs::read_to_string(version_entry.path().join("manifest.json"))
            else {
                continue;
            };
            if manifest_identity(&manifest_json).ok().as_ref()
                != Some(&(id.clone(), version.clone()))
            {
                continue;
            }
            let Ok(wasm) = std::fs::read(version_entry.path().join("plugin.wasm")) else {
                continue;
            };
            let state = states.get(&id);
            let selected = state.is_some_and(|item| item.version == version);
            installed.push(InstalledPlugin {
                id: id.clone(),
                version: version.clone(),
                manifest_json,
                sha256: sha256(&wasm),
                selected,
                enabled: selected && state.is_some_and(|item| item.enabled),
            });
        }
    }
    installed.sort_by(|a, b| a.manifest_json.cmp(&b.manifest_json));
    installed
}

#[tauri::command]
pub(crate) fn read_plugin_entry(
    id: String,
    version: String,
    app: tauri::AppHandle,
) -> Result<tauri::ipc::Response, String> {
    let path = package_dir(&plugins_dir(&app)?, &id, &version)?.join("plugin.wasm");
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_WASM_BYTES || !bytes.starts_with(b"\0asm\x01\0\0\0") {
        return Err("installed plugin entry is invalid".to_string());
    }
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
pub(crate) fn set_plugin_enabled(
    id: String,
    version: String,
    enabled: bool,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let root = plugins_dir(&app)?;
    let settings = crate::settings::settings_path(&app).ok_or("no app-data dir")?;
    set_plugin_enabled_at(&root, &settings, &id, &version, enabled)
}

fn set_plugin_enabled_at(
    plugins_root: &Path,
    settings_path: &Path,
    id: &str,
    version: &str,
    enabled: bool,
) -> Result<(), String> {
    let target = package_dir(plugins_root, id, version)?;
    if !target.join("manifest.json").is_file() || !target.join("plugin.wasm").is_file() {
        return Err("plugin version is not installed".to_string());
    }
    crate::settings::update_settings_strict_at(settings_path, |json| {
        json["plugin_states"][id] = serde_json::json!({ "version": version, "enabled": enabled });
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED_CONTROL_INDEX: &str = "{\n  \"schemaVersion\": 1,\n  \"generatedAt\": \"2026-07-12T00:00:00Z\",\n  \"plugins\": [],\n  \"themes\": [],\n  \"revocations\": []\n}\n";
    const SIGNED_CONTROL_SIGNATURE: &str =
        "2g6EPs5ssf7fkuBH5kYfDNaCEnoTX8PznGPsZ6yzz+xVMggocK5cyYHyE3tnnFGeyuMIBLx6ixPaHWN0FvNdAw==";

    fn test_manifest(id: &str, version: &str) -> String {
        format!(r#"{{"id":"{id}","version":"{version}"}}"#)
    }

    fn write_test_package(root: &Path, id: &str, version: &str) -> PathBuf {
        let package = root.join(id).join(version);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("manifest.json"), test_manifest(id, version)).unwrap();
        std::fs::write(package.join("plugin.wasm"), b"\0asm\x01\0\0\0").unwrap();
        package
    }

    #[test]
    fn plugin_identity_cannot_escape_its_storage_root() {
        assert!(safe_component("dev.tine.example", true));
        assert!(!safe_component("../example", true));
        assert!(!safe_component("Example.Plugin", true));
        assert!(safe_version("0.1.0-beta.1"));
        assert!(!safe_version("0.1.0.4"));
        assert!(!safe_version("01.1.0"));
        assert!(!safe_version("../../outside"));
        assert!(package_dir(Path::new("/plugins"), "dev.tine.example", "0.1.0").is_ok());
    }

    #[test]
    fn manifest_identity_rejects_untrusted_paths_and_oversized_input() {
        let good = r#"{"id":"dev.tine.example","version":"0.1.0"}"#;
        assert_eq!(
            manifest_identity(good).unwrap(),
            ("dev.tine.example".to_string(), "0.1.0".to_string())
        );
        assert!(manifest_identity(r#"{"id":"../bad","version":"0.1.0"}"#).is_err());
        assert!(manifest_identity(&"x".repeat(MAX_MANIFEST_BYTES + 1)).is_err());
    }

    #[test]
    fn registry_public_key_has_the_expected_identity() {
        assert_eq!(REGISTRY_PUBLIC_KEY.len(), 32);
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&REGISTRY_PUBLIC_KEY).is_ok());
        verify_plugin_registry(
            SIGNED_CONTROL_INDEX.to_string(),
            SIGNED_CONTROL_SIGNATURE.to_string(),
        )
        .unwrap();
        assert!(verify_plugin_registry(
            format!("{SIGNED_CONTROL_INDEX} "),
            SIGNED_CONTROL_SIGNATURE.to_string()
        )
        .is_err());
    }

    fn envelope(index_json: &str, signature: &str) -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 1,
            "indexJson": index_json,
            "signature": signature,
        })
    }

    #[test]
    fn registry_cache_store_is_one_atomic_envelope_and_preserves_unrelated_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        let old = serde_json::json!({
            "unrelated": { "keep": true },
            REGISTRY_CACHE_KEY: envelope("old-index", "old-signature"),
        });
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&old).unwrap()),
        )
        .unwrap();

        store_plugin_registry_cache_at(
            &path,
            SIGNED_CONTROL_INDEX.to_string(),
            SIGNED_CONTROL_SIGNATURE.to_string(),
        )
        .unwrap();

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted["unrelated"]["keep"], true);
        assert_eq!(
            persisted[REGISTRY_CACHE_KEY],
            envelope(SIGNED_CONTROL_INDEX, SIGNED_CONTROL_SIGNATURE)
        );
        assert!(matches!(
            load_plugin_registry_cache_at(&path),
            PluginRegistryCacheLoad::Envelope { .. }
        ));
    }

    #[test]
    fn registry_cache_load_distinguishes_absent_torn_and_malformed_states() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        assert_eq!(
            load_plugin_registry_cache_at(&path),
            PluginRegistryCacheLoad::Absent
        );
        std::fs::write(&path, "{}\n").unwrap();
        assert_eq!(
            load_plugin_registry_cache_at(&path),
            PluginRegistryCacheLoad::Absent
        );

        std::fs::write(&path, r#"{"plugin-registry-index":"index"}"#).unwrap();
        assert_eq!(
            load_plugin_registry_cache_at(&path),
            PluginRegistryCacheLoad::Absent,
            "the retired settings-cache shape is disposable and must trigger a refetch"
        );
        std::fs::write(&path, format!(r#"{{"{REGISTRY_CACHE_KEY}":{{"schemaVersion":1,"indexJson":"x","signature":"y","extra":true}}}}"#)).unwrap();
        assert!(matches!(
            load_plugin_registry_cache_at(&path),
            PluginRegistryCacheLoad::Unsafe { .. }
        ));
        std::fs::write(&path, "not-json\n").unwrap();
        assert!(matches!(
            load_plugin_registry_cache_at(&path),
            PluginRegistryCacheLoad::Unsafe { .. }
        ));
    }

    #[test]
    fn invalid_or_unpublishable_registry_cache_never_replaces_last_good_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        std::fs::write(&path, "{\n  \"keep\": true\n}\n").unwrap();
        let before = std::fs::read(&path).unwrap();

        assert!(store_plugin_registry_cache_at(
            &path,
            SIGNED_CONTROL_INDEX.to_string(),
            "invalid".to_string(),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(store_plugin_registry_cache_at(
            &path,
            "x".repeat(MAX_REGISTRY_INDEX_BYTES + 1),
            SIGNED_CONTROL_SIGNATURE.to_string(),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        std::fs::write(&path, "not-json\n").unwrap();
        let malformed = std::fs::read(&path).unwrap();
        assert!(store_plugin_registry_cache_at(
            &path,
            SIGNED_CONTROL_INDEX.to_string(),
            SIGNED_CONTROL_SIGNATURE.to_string(),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
    }

    #[cfg(unix)]
    #[test]
    fn registry_cache_publication_failure_preserves_last_good_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        let old = serde_json::json!({ REGISTRY_CACHE_KEY: envelope("old-index", "old-signature") });
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&old).unwrap()),
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let original_mode = std::fs::metadata(temp.path()).unwrap().permissions().mode();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions, so the cut this test depends on is
        // not established for every user. Probe it rather than assert against a
        // boundary that was never created — a vacuous pass here would stop
        // proving that a failed publication preserves the last good bytes.
        let enforced = {
            let probe = temp.path().join(".write-enforcement-probe");
            let denied = std::fs::write(&probe, b"x").is_err();
            let _ = std::fs::remove_file(&probe);
            denied
        };
        if !enforced {
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(original_mode))
                .unwrap();
            return;
        }
        let result = store_plugin_registry_cache_at(
            &path,
            SIGNED_CONTROL_INDEX.to_string(),
            SIGNED_CONTROL_SIGNATURE.to_string(),
        );
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(original_mode))
            .unwrap();

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn concurrent_registry_cache_readers_observe_complete_old_or_new_envelopes() {
        use std::sync::Arc;
        let temp = tempfile::tempdir().unwrap();
        let path = Arc::new(temp.path().join("tine-settings.json"));
        let old = serde_json::json!({ REGISTRY_CACHE_KEY: envelope("old-index", "old-signature") });
        std::fs::write(
            path.as_ref(),
            format!("{}\n", serde_json::to_string_pretty(&old).unwrap()),
        )
        .unwrap();

        let writer_path = Arc::clone(&path);
        let writer = std::thread::spawn(move || {
            store_plugin_registry_cache_at(
                writer_path.as_ref(),
                SIGNED_CONTROL_INDEX.to_string(),
                SIGNED_CONTROL_SIGNATURE.to_string(),
            )
            .unwrap();
        });
        for _ in 0..500 {
            let text = std::fs::read_to_string(path.as_ref()).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            let cache = &value[REGISTRY_CACHE_KEY];
            let pair = (
                cache["indexJson"].as_str().unwrap(),
                cache["signature"].as_str().unwrap(),
            );
            assert!(
                pair == ("old-index", "old-signature")
                    || pair == (SIGNED_CONTROL_INDEX, SIGNED_CONTROL_SIGNATURE)
            );
        }
        writer.join().unwrap();
    }

    #[test]
    fn revoked_plugin_state_is_durably_disabled_without_opening_guest_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = temp.path().join("plugins");
        write_test_package(&plugins, "page.tine.revoked", "1.0.0");
        let settings = temp.path().join("tine-settings.json");
        std::fs::write(
            &settings,
            r#"{"plugin_states":{"page.tine.revoked":{"version":"1.0.0","enabled":true}}}"#,
        )
        .unwrap();

        set_plugin_enabled_at(&plugins, &settings, "page.tine.revoked", "1.0.0", false).unwrap();

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
        assert_eq!(
            persisted["plugin_states"]["page.tine.revoked"]["enabled"],
            false
        );
        assert_eq!(
            persisted["plugin_states"]["page.tine.revoked"]["version"],
            "1.0.0"
        );
    }

    #[test]
    fn uninstall_removes_only_the_requested_version() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugins");
        let first = write_test_package(&root, "dev.tine.example", "0.1.0");
        let second = write_test_package(&root, "dev.tine.example", "0.2.0");
        let other = write_test_package(&root, "dev.tine.other", "1.0.0");

        assert!(!uninstall_package(&root, "dev.tine.example", "0.1.0").unwrap());
        assert!(!first.exists());
        assert!(second.exists());
        assert!(other.exists());
        assert!(uninstall_package(&root, "dev.tine.example", "0.2.0").unwrap());
        assert!(!root.join("dev.tine.example").exists());
        assert!(other.exists());
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_refuses_symlinked_plugin_directories() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugins");
        let outside = temp.path().join("outside");
        write_test_package(&outside, "dev.tine.example", "0.1.0");
        std::fs::create_dir_all(&root).unwrap();
        symlink(
            outside.join("dev.tine.example"),
            root.join("dev.tine.example"),
        )
        .unwrap();

        assert!(uninstall_package(&root, "dev.tine.example", "0.1.0").is_err());
        assert!(outside.join("dev.tine.example/0.1.0").exists());
    }

    #[test]
    fn transient_names_are_disjoint_from_every_valid_plugin_identity() {
        assert!(!safe_component(".install-dev.tine.example-1.0.0-1-1", true));
        assert!(!safe_component(".retired-dev.tine.example-1.0.0-1-2", true));
        assert!(
            unique_transient_name("install", "dev.tine.example", "1.0.0")
                .starts_with(".install-dev.tine.example-1.0.0-")
        );
        assert!(
            unique_transient_name("retired", "dev.tine.example", "1.0.0")
                .starts_with(".retired-dev.tine.example-1.0.0-")
        );
    }

    #[test]
    fn reopen_reclaims_transient_and_wedged_packages_before_retry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugins");
        std::fs::create_dir_all(root.join(".install-dev.tine.example-1.0.0-7-1")).unwrap();
        std::fs::create_dir_all(root.join(".retired-dev.tine.example-1.0.0-7-2")).unwrap();
        let wedged = root.join("dev.tine.example/1.0.0");
        std::fs::create_dir_all(&wedged).unwrap();
        std::fs::write(wedged.join("plugin.wasm"), b"\0asm\x01\0\0\0").unwrap();

        recover_plugin_store_at(&root).unwrap();

        assert!(!wedged.exists());
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            !name.starts_with(".install-") && !name.starts_with(".retired-")
        }));
        assert_eq!(
            install_plugin_package_at(
                &root,
                "dev.tine.example",
                "1.0.0",
                &test_manifest("dev.tine.example", "1.0.0"),
                b"\0asm\x01\0\0\0",
            )
            .unwrap(),
            tine_storage::PackagePublishOutcome::Published
        );
    }

    #[test]
    fn concurrent_different_installs_leave_one_complete_immutable_winner() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(temp.path().join("plugins"));
        let barrier = Arc::new(Barrier::new(3));
        let manifests = [
            r#"{"id":"dev.tine.example","version":"1.0.0"}"#,
            r#"{ "id": "dev.tine.example", "version": "1.0.0" }"#,
        ];
        let workers = manifests
            .iter()
            .map(|manifest| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                let manifest = manifest.to_string();
                std::thread::spawn(move || {
                    barrier.wait();
                    install_plugin_package_at(
                        &root,
                        "dev.tine.example",
                        "1.0.0",
                        &manifest,
                        b"\0asm\x01\0\0\0",
                    )
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.as_ref().is_err_and(|error| {
                    error == "that immutable plugin version is already installed with different bytes"
                }))
                .count(),
            1
        );
        let stored =
            std::fs::read_to_string(root.join("dev.tine.example/1.0.0/manifest.json")).unwrap();
        assert!(manifests.contains(&stored.as_str()));
        assert_eq!(
            std::fs::read(root.join("dev.tine.example/1.0.0/plugin.wasm")).unwrap(),
            b"\0asm\x01\0\0\0"
        );
    }

    #[test]
    fn settings_clear_cut_leaves_a_recoverable_package_then_retry_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugins");
        install_plugin_package_at(
            &root,
            "dev.tine.example",
            "1.0.0",
            &test_manifest("dev.tine.example", "1.0.0"),
            b"\0asm\x01\0\0\0",
        )
        .unwrap();
        let settings = temp.path().join("tine-settings.json");
        std::fs::write(
            &settings,
            r#"{"plugin_states":{"dev.tine.example":{"version":"1.0.0","enabled":true}},"plugin-settings:dev.tine.example":{"keep":true}}"#,
        )
        .unwrap();

        crate::settings::update_settings_strict_at(&settings, |json| {
            clear_uninstalled_plugin_settings(json, "dev.tine.example", "1.0.0", true);
            Ok(())
        })
        .unwrap();
        // Crash cut: settings are durable, retirement has not started.
        recover_plugin_store_at(&root).unwrap();

        validate_uninstall_target(&root, "dev.tine.example", "1.0.0").unwrap();
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(persisted["plugin_states"].get("dev.tine.example").is_none());
        assert!(persisted.get("plugin-settings:dev.tine.example").is_none());
        assert!(uninstall_package(&root, "dev.tine.example", "1.0.0").unwrap());
        assert!(!root.join("dev.tine.example").exists());
    }

    #[test]
    fn production_plugin_writes_stay_on_named_audited_paths() {
        let source = include_str!("plugins.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap();
        for forbidden in [
            "std::fs::write(",
            "std::fs::rename(",
            "std::fs::remove_dir_all(",
            "std::fs::remove_dir(",
        ] {
            assert!(
                !production.contains(forbidden),
                "I-1/I-2: plugin package writes must use the named tine-storage protocol; see settings.rs::app_private_durable_publications_stay_on_named_audited_paths; found {forbidden}"
            );
        }
        assert!(production.contains("tine_storage::publish_package_noclobber"));
        assert!(production.contains("tine_storage::retire_package"));
        assert!(production.contains("tine_storage::recover_package_store"));
        assert!(production.contains("crate::settings::update_settings"));
    }
}
