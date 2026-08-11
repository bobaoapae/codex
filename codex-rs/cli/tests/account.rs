// FORK: integration tests for `codex account` (multi-account vault).

use std::path::Path;

use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::encode_id_token;
use app_test_support::write_chatgpt_auth;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_login::vault::AccountVault;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

fn write_file_auth_config(codex_home: &Path) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    Ok(())
}

fn write_active_account(codex_home: &Path, email: &str, account_id: &str) -> Result<()> {
    write_chatgpt_auth(
        codex_home,
        ChatGptAuthFixture::new(format!("access-{account_id}"))
            .refresh_token(format!("refresh-{account_id}"))
            .account_id(account_id)
            .email(email)
            .plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )
}

fn chatgpt_payload(email: &str, account_id: &str) -> Result<AuthDotJson> {
    let claims = ChatGptIdTokenClaims::new()
        .email(email)
        .plan_type("pro")
        .chatgpt_user_id("user-1")
        .chatgpt_account_id(account_id);
    let id_token_raw = encode_id_token(&claims)?;
    Ok(AuthDotJson {
        auth_mode: Some(codex_protocol::auth::AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token_raw)?,
            access_token: format!("access-{account_id}"),
            refresh_token: format!("refresh-{account_id}"),
            account_id: Some(account_id.to_string()),
        }),
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    })
}

fn list_accounts_json(codex_home: &Path) -> Result<Vec<Value>> {
    let output = codex_command(codex_home)?
        .args(["account", "list", "--json", "--no-usage"])
        .output()?;
    assert!(
        output.status.success(),
        "account list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed.as_array().cloned().unwrap_or_default())
}

#[test]
fn list_shows_active_and_stored_accounts() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;
    write_active_account(codex_home.path(), "a@example.com", "acct-a")?;

    let vault = AccountVault::new(codex_home.path());
    vault.upsert(
        chatgpt_payload("work@example.com", "acct-b")?,
        Some("work".to_string()),
    )?;

    let rows = list_accounts_json(codex_home.path())?;
    assert_eq!(rows.len(), 2);

    let active: Vec<&Value> = rows
        .iter()
        .filter(|row| row["active"].as_bool() == Some(true))
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["email"].as_str(), Some("a@example.com"));
    assert!(
        active[0]["label"]
            .as_str()
            .is_some_and(|label| label.contains("not stored"))
    );

    let stored = rows
        .iter()
        .find(|row| row["label"].as_str() == Some("work"))
        .expect("stored account row");
    assert_eq!(stored["active"].as_bool(), Some(false));
    assert_eq!(stored["email"].as_str(), Some("work@example.com"));
    assert_eq!(stored["usage"]["status"].as_str(), Some("not_applicable"));
    Ok(())
}

#[test]
fn switch_swaps_active_slot_and_preserves_outgoing_account() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;
    write_active_account(codex_home.path(), "a@example.com", "acct-a")?;

    let vault = AccountVault::new(codex_home.path());
    vault.upsert(
        chatgpt_payload("work@example.com", "acct-b")?,
        Some("work".to_string()),
    )?;

    codex_command(codex_home.path())?
        .args(["account", "switch", "work"])
        .assert()
        .success()
        .stdout(contains("Switched to \"work\""));

    let auth_json: Value = serde_json::from_str(&std::fs::read_to_string(
        codex_home.path().join("auth.json"),
    )?)?;
    assert_eq!(
        auth_json["tokens"]["account_id"].as_str(),
        Some("acct-b"),
        "auth.json should now hold the target account"
    );

    let rows = list_accounts_json(codex_home.path())?;
    assert_eq!(rows.len(), 2, "outgoing account should have been stored");
    let work = rows
        .iter()
        .find(|row| row["label"].as_str() == Some("work"))
        .expect("work row");
    assert_eq!(work["active"].as_bool(), Some(true));
    let outgoing = rows
        .iter()
        .find(|row| row["email"].as_str() == Some("a@example.com"))
        .expect("outgoing row");
    assert_eq!(outgoing["active"].as_bool(), Some(false));
    assert_eq!(outgoing["label"].as_str(), Some("a"));

    codex_command(codex_home.path())?
        .args(["account", "switch", "work"])
        .assert()
        .success()
        .stdout(contains("already the active account"));
    Ok(())
}

#[test]
fn remove_blocks_active_account_without_force() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;
    write_active_account(codex_home.path(), "a@example.com", "acct-a")?;

    let vault = AccountVault::new(codex_home.path());
    vault.upsert(
        chatgpt_payload("work@example.com", "acct-b")?,
        Some("work".to_string()),
    )?;

    // Removing a non-active stored account works.
    codex_command(codex_home.path())?
        .args(["account", "remove", "work"])
        .assert()
        .success()
        .stdout(contains("Removed stored account \"work\""));
    assert_eq!(list_accounts_json(codex_home.path())?.len(), 1);

    // Removing the entry matching the active account requires --force.
    vault.upsert(chatgpt_payload("a@example.com", "acct-a")?, None)?;
    codex_command(codex_home.path())?
        .args(["account", "remove", "a"])
        .assert()
        .failure()
        .stderr(contains("currently active"));

    codex_command(codex_home.path())?
        .args(["account", "remove", "a", "--force"])
        .assert()
        .success();
    assert!(codex_home.path().join("auth.json").exists());
    Ok(())
}

#[test]
fn switch_without_name_and_without_terminal_cancels_gracefully() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;
    write_active_account(codex_home.path(), "a@example.com", "acct-a")?;

    let vault = AccountVault::new(codex_home.path());
    vault.upsert(
        chatgpt_payload("work@example.com", "acct-b")?,
        Some("work".to_string()),
    )?;

    // Empty stdin on the numbered fallback cancels without changing anything.
    codex_command(codex_home.path())?
        .args(["account", "switch", "--no-usage"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(contains("Switch cancelled"));

    let auth_json: Value = serde_json::from_str(&std::fs::read_to_string(
        codex_home.path().join("auth.json"),
    )?)?;
    assert_eq!(auth_json["tokens"]["account_id"].as_str(), Some("acct-a"));
    Ok(())
}
