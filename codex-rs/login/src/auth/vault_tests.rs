use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pretty_assertions::assert_eq;
use serial_test::serial;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;

use super::*;
use crate::auth::manager::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use crate::test_support::transport_default_auth_route_config;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

fn fake_jwt(email: &str, account_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = serde_json::json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": "user-1",
        },
        "exp": 4_102_444_800_i64,
    });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("serialize payload"));
    format!("{header}.{payload}.signature")
}

fn chatgpt_auth(email: &str, account_id: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&fake_jwt(email, account_id)).expect("parse jwt"),
            access_token: format!("access-{account_id}"),
            refresh_token: format!("refresh-{account_id}"),
            account_id: Some(account_id.to_string()),
        }),
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
fn upsert_creates_entries_and_dedupes_by_identity() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    let first = vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert first");
    let second = vault
        .upsert(chatgpt_auth("work@example.com", "acct-2"), None)
        .expect("upsert second");
    assert_eq!(vault.list().expect("list").len(), 2);
    assert_ne!(first.session_id, second.session_id);

    let mut updated_auth = chatgpt_auth("joao@example.com", "acct-1");
    if let Some(tokens) = updated_auth.tokens.as_mut() {
        tokens.access_token = "rotated-access".to_string();
    }
    let updated = vault.upsert(updated_auth, None).expect("re-upsert");

    assert_eq!(vault.list().expect("list").len(), 2);
    assert_eq!(updated.session_id, first.session_id);
    assert_eq!(updated.label, first.label);
    assert_eq!(updated.added_at, first.added_at);
    assert_eq!(
        updated
            .auth
            .tokens
            .as_ref()
            .expect("tokens")
            .access_token
            .as_str(),
        "rotated-access"
    );
    assert!(updated.last_used_at >= first.last_used_at);
}

#[test]
fn default_labels_derive_from_email_and_disambiguate() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    let first = vault
        .upsert(chatgpt_auth("joao@personal.com", "acct-1"), None)
        .expect("upsert first");
    let second = vault
        .upsert(chatgpt_auth("joao@work.com", "acct-2"), None)
        .expect("upsert second");

    assert_eq!(first.label, "joao");
    assert_eq!(second.label, "joao-2");
}

#[test]
fn resolve_matches_label_email_and_session_prefix() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    let entry = vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert");
    vault
        .upsert(
            chatgpt_auth("other@example.com", "acct-2"),
            Some("work".to_string()),
        )
        .expect("upsert second");

    assert_eq!(vault.resolve("JOAO").expect("by label").session_id, entry.session_id);
    assert_eq!(
        vault
            .resolve("joao@example.com")
            .expect("by email")
            .session_id,
        entry.session_id
    );
    assert_eq!(
        vault
            .resolve(&entry.session_id[..8])
            .expect("by prefix")
            .session_id,
        entry.session_id
    );

    let missing = vault.resolve("nobody").expect_err("missing should fail");
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn resolve_rejects_ambiguous_names() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    vault
        .upsert(
            chatgpt_auth("a@example.com", "acct-1"),
            Some("same".to_string()),
        )
        .expect("upsert first");
    vault
        .upsert(
            chatgpt_auth("b@example.com", "acct-2"),
            Some("same".to_string()),
        )
        .expect("upsert second");

    let err = vault.resolve("same").expect_err("ambiguous should fail");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn list_skips_corrupt_entries() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert");
    std::fs::write(vault.dir().join("corrupt.json"), b"not json").expect("write corrupt");

    let entries = vault.list().expect("list");
    assert_eq!(entries.len(), 1);
}

#[test]
fn remove_reports_whether_entry_existed() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    let entry = vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert");
    assert!(vault.remove(&entry.session_id).expect("remove"));
    assert!(!vault.remove(&entry.session_id).expect("second remove"));
    assert!(vault.list().expect("list").is_empty());
}

#[test]
fn write_active_slot_writes_auth_json_and_leaves_no_temp_files() {
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());

    let entry = vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert");
    vault
        .write_active_slot(
            &entry,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )
        .expect("write active slot");

    let written: AuthDotJson = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("auth.json")).expect("read auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(written, entry.auth);

    let leftovers: Vec<_> = std::fs::read_dir(home.path())
        .expect("read home")
        .chain(std::fs::read_dir(vault.dir()).expect("read vault dir"))
        .map(|dir_entry| dir_entry.expect("dir entry").file_name())
        .filter(|name| name.to_string_lossy().contains(".tmp"))
        .collect();
    assert_eq!(leftovers, Vec::<std::ffi::OsString>::new());
}

#[test]
fn active_identity_matches_vault_entry() {
    let auth = chatgpt_auth("joao@example.com", "acct-1");
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());
    let entry = vault.upsert(auth.clone(), None).expect("upsert");

    assert_eq!(account_identity(&auth), entry.identity());
    assert_eq!(
        session_id_for_identity(&account_identity(&auth).expect("identity")),
        entry.session_id
    );
}

#[test]
fn api_key_payloads_are_storable_and_identifiable() {
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-test-123".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    };
    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());
    let entry = vault.upsert(auth, None).expect("upsert");

    assert!(entry.label.starts_with("account-"));
    assert!(!entry.is_chatgpt());
}

#[tokio::test]
#[serial(codex_refresh_token_url)]
async fn refresh_entry_persists_rotation_into_vault_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id_token": fake_jwt("joao@example.com", "acct-1"),
            "access_token": "refreshed-access",
            "refresh_token": "refreshed-refresh",
        })))
        .mount(&server)
        .await;
    let _guard = EnvVarGuard::set(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, &server.uri());

    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());
    let entry = vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert");

    let refreshed = vault
        .refresh_entry(&entry.session_id, &transport_default_auth_route_config())
        .await
        .expect("refresh entry");

    let tokens = refreshed.auth.tokens.as_ref().expect("tokens");
    assert_eq!(tokens.access_token, "refreshed-access");
    assert_eq!(tokens.refresh_token, "refreshed-refresh");
    assert_eq!(refreshed.label, entry.label);
    assert!(refreshed.auth.last_refresh.is_some());
    assert!(
        !home.path().join("auth.json").exists(),
        "refreshing a vault entry must not create or touch auth.json"
    );
}

#[tokio::test]
#[serial(codex_refresh_token_url)]
async fn refresh_entry_surfaces_permanent_failures() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": { "code": "refresh_token_expired" },
        })))
        .mount(&server)
        .await;
    let _guard = EnvVarGuard::set(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, &server.uri());

    let home = TempDir::new().expect("tempdir");
    let vault = AccountVault::new(home.path());
    let entry = vault
        .upsert(chatgpt_auth("joao@example.com", "acct-1"), None)
        .expect("upsert");

    let err = vault
        .refresh_entry(&entry.session_id, &transport_default_auth_route_config())
        .await
        .expect_err("expired refresh token should fail permanently");
    assert!(matches!(err, RefreshTokenError::Permanent(_)));

    let unchanged = vault.resolve(&entry.label).expect("entry still present");
    assert_eq!(
        unchanged.auth.tokens.expect("tokens").refresh_token,
        format!("refresh-{}", "acct-1")
    );
}
