//! FORK: on-disk vault for additional signed-in accounts.
//!
//! Upstream stores exactly one account in `$CODEX_HOME/auth.json`. The vault
//! keeps any number of `AuthDotJson` payloads under
//! `$CODEX_HOME/auth_accounts/<session_id>.json`, one self-describing file per
//! account, so `codex account add/list/switch/remove` can rotate which payload
//! occupies the active slot without ever touching sessions, history, or
//! memories.
//!
//! There is deliberately no manifest and no notion of "active" stored here:
//! the active account is whatever `auth.json` holds, and callers derive it by
//! identity-matching against vault entries. That keeps upstream `codex login`
//! and `codex logout` coherent with the vault without hooking either.
//!
//! The schema mirrors the field names of upstream's dormant `AccountSession`
//! protocol types (`app-server-protocol/src/protocol/v2/account.rs`) so a
//! future official multi-account implementation merges with minimal friction.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;

use super::manager::CodexAuth;
use super::manager::RefreshTokenError;
use super::manager::chatgpt_auth_from_parts;
use super::manager::refresh_chatgpt_tokens_in_storage;
use super::manager::save_auth;
use super::revoke::revoke_auth_tokens;
use super::storage::AuthDotJson;
use super::storage::AuthKeyringBackendKind;
use super::storage::AuthStorageBackend;
use super::storage::get_auth_file;
use crate::outbound_proxy::AuthRouteConfig;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;

pub const VAULT_DIR_NAME: &str = "auth_accounts";
const VAULT_ENTRY_VERSION: u32 = 1;

/// One stored account: the full auth payload plus the metadata needed to pick
/// it from a list. `session_id` is derived deterministically from the account
/// identity, so re-adding the same account updates its entry in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub version: u32,
    pub session_id: String,
    pub label: String,
    pub added_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub auth: AuthDotJson,
}

impl VaultEntry {
    pub fn email(&self) -> Option<&str> {
        self.auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.email.as_deref())
    }

    pub fn plan_type(&self) -> Option<String> {
        self.auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.get_chatgpt_plan_type())
    }

    pub fn identity(&self) -> Option<String> {
        account_identity(&self.auth)
    }

    pub fn is_chatgpt(&self) -> bool {
        resolved_auth_mode(&self.auth) == AuthMode::Chatgpt && self.auth.tokens.is_some()
    }
}

/// Stable identity of an auth payload, used both to dedupe vault entries and
/// to recognize which entry currently occupies the active slot. ChatGPT
/// payloads are keyed by workspace/account id (falling back to user id, then
/// email); bare API keys by a key fingerprint.
pub fn account_identity(auth: &AuthDotJson) -> Option<String> {
    if let Some(tokens) = &auth.tokens {
        if let Some(id) = non_empty(tokens.account_id.as_deref()) {
            return Some(format!("chatgpt|{id}"));
        }
        let info = &tokens.id_token;
        if let Some(id) = non_empty(info.chatgpt_account_id.as_deref()) {
            return Some(format!("chatgpt|{id}"));
        }
        if let Some(id) = non_empty(info.chatgpt_user_id.as_deref()) {
            return Some(format!("chatgpt-user|{id}"));
        }
        if let Some(email) = non_empty(info.email.as_deref()) {
            return Some(format!("email|{}", email.to_ascii_lowercase()));
        }
    }
    if let Some(api_key) = non_empty(auth.openai_api_key.as_deref()) {
        return Some(format!("api-key|{}", short_hash(api_key)));
    }
    None
}

pub fn session_id_for_identity(identity: &str) -> String {
    short_hash(&format!("codex-account-vault|{identity}"))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    hex[..16].to_string()
}

fn resolved_auth_mode(auth: &AuthDotJson) -> AuthMode {
    if let Some(mode) = auth.auth_mode {
        return mode;
    }
    if auth.openai_api_key.is_some() {
        return AuthMode::ApiKey;
    }
    AuthMode::Chatgpt
}

#[derive(Debug, Clone)]
pub struct AccountVault {
    codex_home: PathBuf,
}

impl AccountVault {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            codex_home: codex_home.to_path_buf(),
        }
    }

    pub fn dir(&self) -> PathBuf {
        self.codex_home.join(VAULT_DIR_NAME)
    }

    fn entry_path(&self, session_id: &str) -> PathBuf {
        self.dir().join(format!("{session_id}.json"))
    }

    /// All stored entries, most recently used first. Unreadable files are
    /// skipped with a warning rather than failing the whole listing.
    pub fn list(&self) -> std::io::Result<Vec<VaultEntry>> {
        let dir = self.dir();
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut entries = Vec::new();
        for dir_entry in read_dir {
            let path = dir_entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match read_entry(&path) {
                Ok(entry) => entries.push(entry),
                Err(err) => warn!("skipping unreadable account vault entry {path:?}: {err}"),
            }
        }
        entries.sort_by(|a, b| {
            b.last_used_at
                .cmp(&a.last_used_at)
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(entries)
    }

    /// Resolve a user-supplied name against labels, emails, and session-id
    /// prefixes. Fails when nothing matches or the match is ambiguous.
    pub fn resolve(&self, needle: &str) -> std::io::Result<VaultEntry> {
        let needle_trimmed = needle.trim();
        if needle_trimmed.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "account name is empty",
            ));
        }
        let needle_lower = needle_trimmed.to_ascii_lowercase();

        let entries = self.list()?;
        let matches: Vec<&VaultEntry> = entries
            .iter()
            .filter(|entry| {
                entry.label.to_ascii_lowercase() == needle_lower
                    || entry
                        .email()
                        .is_some_and(|email| email.to_ascii_lowercase() == needle_lower)
                    || (needle_trimmed.len() >= 6 && entry.session_id.starts_with(&needle_lower))
            })
            .collect();

        match matches.as_slice() {
            [] => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no stored account matches \"{needle_trimmed}\""),
            )),
            [entry] => Ok((*entry).clone()),
            multiple => {
                let labels: Vec<&str> = multiple.iter().map(|entry| entry.label.as_str()).collect();
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "\"{needle_trimmed}\" matches more than one stored account: {}",
                        labels.join(", ")
                    ),
                ))
            }
        }
    }

    /// Insert or update the entry for this payload's account. Re-adding an
    /// existing account replaces its tokens but keeps label and `added_at`
    /// unless a new label is given.
    pub fn upsert(&self, auth: AuthDotJson, label: Option<String>) -> std::io::Result<VaultEntry> {
        let identity = account_identity(&auth).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "auth payload has no identifiable account to store",
            )
        })?;
        let session_id = session_id_for_identity(&identity);
        let now = Utc::now();

        let entry = match read_entry(&self.entry_path(&session_id)) {
            Ok(mut existing) => {
                existing.version = VAULT_ENTRY_VERSION;
                if let Some(label) = label {
                    existing.label = label;
                }
                existing.last_used_at = now;
                existing.auth = auth;
                existing
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => VaultEntry {
                version: VAULT_ENTRY_VERSION,
                session_id: session_id.clone(),
                label: match label {
                    Some(label) => label,
                    None => self.default_label(&auth, &session_id)?,
                },
                added_at: now,
                last_used_at: now,
                auth,
            },
            Err(err) => return Err(err),
        };

        write_entry(&self.entry_path(&entry.session_id), &entry)?;
        Ok(entry)
    }

    pub fn remove(&self, session_id: &str) -> std::io::Result<bool> {
        match std::fs::remove_file(self.entry_path(session_id)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub fn touch_last_used(&self, session_id: &str) -> std::io::Result<()> {
        let path = self.entry_path(session_id);
        let mut entry = read_entry(&path)?;
        entry.last_used_at = Utc::now();
        write_entry(&path, &entry)
    }

    /// Copy this entry's payload into the active slot (`auth.json` or the
    /// configured keyring). This is the "switch": running processes keep the
    /// previous account (`AuthManager` pins its account id) until restarted.
    pub fn write_active_slot(
        &self,
        entry: &VaultEntry,
        mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> std::io::Result<()> {
        match mode {
            AuthCredentialsStoreMode::File => {
                atomic_write_json(&get_auth_file(&self.codex_home), &entry.auth)
            }
            _ => save_auth(&self.codex_home, &entry.auth, mode, keyring_backend_kind),
        }
    }

    /// Build a usable `CodexAuth` for a stored entry. ChatGPT entries get a
    /// storage handle pointing back at their vault file, so any token refresh
    /// (refresh tokens are single-use) persists the rotation into the vault
    /// instead of clobbering the active slot.
    pub fn codex_auth_for(
        &self,
        entry: &VaultEntry,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<CodexAuth> {
        match resolved_auth_mode(&entry.auth) {
            AuthMode::ApiKey => {
                let api_key = entry.auth.openai_api_key.as_deref().ok_or_else(|| {
                    std::io::Error::other("stored API-key account is missing its key")
                })?;
                Ok(CodexAuth::from_api_key(api_key))
            }
            AuthMode::Chatgpt => {
                let storage: Arc<dyn AuthStorageBackend> = Arc::new(VaultEntryStorage {
                    path: self.entry_path(&entry.session_id),
                });
                chatgpt_auth_from_parts(entry.auth.clone(), storage, auth_route_config)
            }
            other => Err(std::io::Error::other(format!(
                "stored account with auth mode {other:?} cannot be used from the vault"
            ))),
        }
    }

    /// Refresh a stored ChatGPT entry's tokens, persisting the rotation into
    /// its vault file, and return the updated entry.
    pub async fn refresh_entry(
        &self,
        session_id: &str,
        auth_route_config: &AuthRouteConfig,
    ) -> Result<VaultEntry, RefreshTokenError> {
        let path = self.entry_path(session_id);
        let storage: Arc<dyn AuthStorageBackend> =
            Arc::new(VaultEntryStorage { path: path.clone() });
        refresh_chatgpt_tokens_in_storage(&storage, auth_route_config).await?;
        read_entry(&path).map_err(RefreshTokenError::Transient)
    }

    /// Best-effort server-side revocation of a stored entry's tokens.
    pub async fn revoke_entry_tokens(
        &self,
        entry: &VaultEntry,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<()> {
        revoke_auth_tokens(Some(&entry.auth), auth_route_config).await
    }

    fn default_label(&self, auth: &AuthDotJson, session_id: &str) -> std::io::Result<String> {
        let base = auth
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.id_token.email.as_deref())
            .and_then(|email| email.split('@').next())
            .map(sanitize_label)
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| format!("account-{}", &session_id[..6.min(session_id.len())]));

        let existing: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|entry| entry.session_id != session_id)
            .map(|entry| entry.label.to_ascii_lowercase())
            .collect();
        if !existing.contains(&base.to_ascii_lowercase()) {
            return Ok(base);
        }
        for suffix in 2..100 {
            let candidate = format!("{base}-{suffix}");
            if !existing.contains(&candidate.to_ascii_lowercase()) {
                return Ok(candidate);
            }
        }
        Ok(format!("{base}-{session_id}"))
    }
}

fn sanitize_label(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn read_entry(path: &Path) -> std::io::Result<VaultEntry> {
    let contents = std::fs::read_to_string(path)?;
    let entry: VaultEntry = serde_json::from_str(&contents)?;
    Ok(entry)
}

fn write_entry(path: &Path, entry: &VaultEntry) -> std::io::Result<()> {
    atomic_write_json(path, entry)
}

/// Temp-file + rename so a crash mid-write can never corrupt an entry (the
/// upstream `FileAuthStorage::save` truncates in place; the vault does not
/// inherit that).
fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("vault path has no parent directory"))?;
    std::fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry.json");
    let tmp_path = parent.join(format!(".{}.tmp-{}", file_name, std::process::id()));

    let json = serde_json::to_string_pretty(value)?;
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options.open(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.flush()?;
    }

    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(err)
        }
    }
}

/// `AuthStorageBackend` view of a single vault entry file: loads and saves the
/// inner `AuthDotJson` while preserving the entry's metadata. This is what
/// makes upstream's refresh persistence write rotated tokens back into the
/// vault atomically.
#[derive(Debug)]
struct VaultEntryStorage {
    path: PathBuf,
}

impl AuthStorageBackend for VaultEntryStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match read_entry(&self.path) {
            Ok(entry) => Ok(Some(entry.auth)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let mut entry = read_entry(&self.path).map_err(|err| {
            std::io::Error::other(format!(
                "cannot persist refreshed tokens: vault entry {:?} is unreadable: {err}",
                self.path
            ))
        })?;
        entry.auth = auth.clone();
        write_entry(&self.path, &entry)
    }

    fn delete(&self) -> std::io::Result<bool> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
#[path = "vault_tests.rs"]
mod tests;
