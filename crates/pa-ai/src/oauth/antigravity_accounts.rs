//! The Antigravity multi-account store: a private `antigravity-accounts.json`
//! under the agent directory that remembers every logged-in account and which
//! one is active, and backs account switching plus the HTTP 429 quota failover.
//! Ported from the `pi-antigravity` extension's `src/auth/accounts.ts`.
//!
//! The store is the port's slot for the two fields the shared credential type
//! (`pa_core::auth::types::AuthCredential::Oauth`) has no room for: the account
//! `email` and the Cloud Code Assist `projectId`. `pa-core` is left untouched;
//! the active account's credential is mirrored into the `antigravity` entry of
//! `auth.json`, the file the provider already reads, so switching and failover
//! keep working through the existing path.
//!
//! The store is populated opportunistically: every provider request syncs the
//! credential `auth.json` currently holds, so each login (which writes
//! `auth.json`) is remembered on the next request, mirroring the TS
//! `rememberAccount` call in its login flow. A sync never drops the store's
//! `email`/`projectId` when `auth.json` (which has no slot for them) omits them.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::antigravity::{default_project_id, load_code_assist, refresh_antigravity_token};
use super::provider_http::ProviderHttp;

/// The account store's file name (the TS `ACCOUNTS_FILE`).
pub const ACCOUNTS_FILE: &str = "antigravity-accounts.json";
/// The shared auth file the active credential is mirrored into (the TS `AUTH_FILE`).
pub const AUTH_FILE: &str = "auth.json";
/// The credential expiry skew (the TS `5 * 60 * 1000`).
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
/// Fresh temporary names per write; `std::process::id` alone can collide.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One remembered account: the credentials plus the identity and timestamps the
/// shared credential type cannot carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAccount {
    /// The account id: the lowercased email, or `account-<sha256(refresh) prefix>`.
    pub account_id: String,
    /// The account email, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The bearer access token.
    pub access: String,
    /// The refresh token.
    pub refresh: String,
    /// Wall-clock epoch milliseconds the access token expires at.
    pub expires: i64,
    /// The Cloud Code Assist project id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Wall-clock epoch milliseconds the account was first added.
    pub added_at: i64,
    /// Wall-clock epoch milliseconds the account was last used.
    pub last_used_at: i64,
}

/// The `antigravity-accounts.json` document (the TS `AccountsFile`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStore {
    /// The store format version (always `1`).
    pub version: u32,
    /// The active account's id, when one is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_id: Option<String>,
    /// The accounts, keyed by account id.
    pub accounts: BTreeMap<String, StoredAccount>,
}

/// One `list_accounts` row (the TS `AccountSummary`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// The account id.
    pub account_id: String,
    /// The account email, when known.
    pub email: Option<String>,
    /// Whether this account is the active one.
    pub active: bool,
    /// Wall-clock epoch milliseconds the account was last used.
    pub last_used_at: i64,
}

/// The fields one account write carries: the shared credential fields plus the
/// store-only `projectId`/`email`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountCredentials {
    /// The bearer access token.
    pub access: String,
    /// The refresh token.
    pub refresh: String,
    /// Wall-clock epoch milliseconds the access token expires at.
    pub expires: i64,
    /// The Cloud Code Assist project id, when known.
    pub project_id: Option<String>,
    /// The account email, when known.
    pub email: Option<String>,
}

impl From<&super::antigravity::AntigravityCredentials> for AccountCredentials {
    fn from(credentials: &super::antigravity::AntigravityCredentials) -> Self {
        Self {
            access: credentials.access.clone(),
            refresh: credentials.refresh.clone(),
            expires: credentials.expires,
            // The login flow discovers the project id, but the shared credential
            // type does not carry it back; discovery happens on the next refresh.
            project_id: None,
            email: credentials.account_id.clone(),
        }
    }
}

/// A selected account's bearer token plus its still-known project id and email;
/// the provider resolves the project id when one is absent, like
/// `resolve_credentials` does for the primary credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountApiKey {
    /// The bearer access token.
    pub token: String,
    /// The Cloud Code Assist project id, when the account carries one.
    pub project_id: Option<String>,
    /// The account email, when known.
    pub email: Option<String>,
}

impl AccountApiKey {
    /// Serialize as the JSON `{"token":"...","projectId":"..."}` expected by `parse_api_key`.
    #[must_use]
    pub fn to_json_key(&self) -> String {
        serde_json::json!({
            "token": self.token,
            "projectId": self.project_id.as_deref().unwrap_or(""),
        })
        .to_string()
    }
}

/// The account-store failure.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// No account matched the selector.
    #[error("Antigravity account not found: {0}")]
    NotFound(String),
    /// The agent directory could not be resolved.
    #[error("the Antigravity account store is unavailable: no agent directory")]
    NoAgentDirectory,
    /// The account's refresh failed and the store was left untouched.
    #[error("Antigravity account refresh failed: {0}")]
    Refresh(String),
    /// `auth.json` exists but cannot be parsed; the write refuses to clobber it.
    #[error("refusing to overwrite unreadable Pi auth.json")]
    UnreadableAuth,
    /// A store or auth-file write failed.
    #[error("Antigravity account store io error: {0}")]
    Io(#[from] std::io::Error),
}

// Epoch millis fit i64 for ~292 million years.
#[allow(clippy::cast_possible_truncation)]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |elapsed| elapsed.as_millis() as i64)
}

fn agent_dir_or_none() -> Option<PathBuf> {
    pa_types::platform::agent_dir()
}

fn accounts_path(dir: &Path) -> PathBuf {
    dir.join(ACCOUNTS_FILE)
}

fn auth_path(dir: &Path) -> PathBuf {
    dir.join(AUTH_FILE)
}

fn empty_store() -> AccountStore {
    AccountStore {
        version: 1,
        active_account_id: None,
        accounts: BTreeMap::new(),
    }
}

/// A JSON object at `path`, or `None` when the file is missing, unreadable, not
/// JSON, or not an object (the TS `readJsonObject`).
fn read_json_object(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.is_object().then_some(value)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(unix)]
fn set_private_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Write `value` as pretty JSON with a trailing newline to a `0600` temp file,
/// then rename it over `path` (the TS `writePrivateJson`).
fn write_private_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let temp = temp_path(path);
    {
        let mut file = create_private_file(&temp)?;
        file.write_all(body.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    set_private_mode(&temp)?;
    std::fs::rename(&temp, path)?;
    set_private_mode(path)?;
    Ok(())
}

fn save_account_store(dir: &Path, store: &AccountStore) -> std::io::Result<()> {
    write_private_json(&accounts_path(dir), store)
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn normalized_email(email: Option<&str>) -> Option<String> {
    email
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_lowercase)
}

/// One stored-account entry from the on-disk object (the TS `isStoredAccount` +
/// prioritizing email as the account identifier).
fn normalize_account(key: &str, raw: &Value) -> Option<StoredAccount> {
    let map = raw.as_object()?;
    let refresh = map
        .get("refresh")
        .and_then(Value::as_str)
        .filter(|refresh| !refresh.is_empty())?;
    let email = optional_string(map.get("email"))
        .as_deref()
        .and_then(|e| normalized_email(Some(e)))
        .or_else(|| {
            if key.contains('@') {
                normalized_email(Some(key))
            } else {
                None
            }
        });
    let account_id = if let Some(ref email) = email {
        email.clone()
    } else {
        map.get("accountId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(key)
            .to_string()
    };
    Some(StoredAccount {
        account_id,
        email,
        access: map
            .get("access")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        refresh: refresh.to_string(),
        expires: map.get("expires").and_then(Value::as_i64).unwrap_or(0),
        project_id: optional_string(map.get("projectId")),
        added_at: map.get("addedAt").and_then(Value::as_i64).unwrap_or(0),
        last_used_at: map.get("lastUsedAt").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Merge two accounts representing the same identity.
fn merge_stored_accounts(mut base: StoredAccount, mut other: StoredAccount) -> StoredAccount {
    let other_is_newer = other.last_used_at > base.last_used_at;

    let (access, refresh, expires) = if other_is_newer {
        if !other.access.is_empty() {
            (other.access.clone(), other.refresh.clone(), other.expires)
        } else {
            (base.access.clone(), base.refresh.clone(), base.expires)
        }
    } else {
        if !base.access.is_empty() {
            (base.access.clone(), base.refresh.clone(), base.expires)
        } else {
            (other.access.clone(), other.refresh.clone(), other.expires)
        }
    };

    let email = normalized_email(base.email.as_deref())
        .or_else(|| normalized_email(other.email.as_deref()));
    let project_id = base.project_id.take().or(other.project_id);

    let added_at = if base.added_at == 0 {
        other.added_at
    } else if other.added_at == 0 {
        base.added_at
    } else {
        base.added_at.min(other.added_at)
    };
    let last_used_at = base.last_used_at.max(other.last_used_at);

    // Prefer email as account_id
    let account_id = if let Some(ref e) = email {
        e.clone()
    } else if !base.account_id.starts_with("account-") {
        base.account_id
    } else if !other.account_id.starts_with("account-") {
        other.account_id
    } else {
        base.account_id
    };

    StoredAccount {
        account_id,
        email,
        access,
        refresh,
        expires,
        project_id,
        added_at,
        last_used_at,
    }
}

/// Whether two stored accounts represent the same identity.
fn are_same_identity(a: &StoredAccount, b: &StoredAccount) -> bool {
    let email_a = normalized_email(a.email.as_deref()).or_else(|| {
        if a.account_id.contains('@') {
            normalized_email(Some(&a.account_id))
        } else {
            None
        }
    });
    let email_b = normalized_email(b.email.as_deref()).or_else(|| {
        if b.account_id.contains('@') {
            normalized_email(Some(&b.account_id))
        } else {
            None
        }
    });

    if let (Some(ea), Some(eb)) = (&email_a, &email_b) {
        if ea == eb {
            return true;
        }
    }
    if !a.refresh.is_empty() && a.refresh == b.refresh {
        return true;
    }
    false
}

/// Migrate any legacy `account-<hash>` entries to email keys when email is known,
/// and merge duplicate entries representing the same identity.
/// Returns true if the store was modified.
fn migrate_and_deduplicate_accounts(store: &mut AccountStore) -> bool {
    let original_count = store.accounts.len();
    let original_keys: Vec<String> = store.accounts.keys().cloned().collect();

    let mut alias_map: HashMap<String, String> = HashMap::new();
    let mut merged: Vec<StoredAccount> = Vec::new();
    let old_accounts = std::mem::take(&mut store.accounts);

    for (key, mut acc) in old_accounts {
        let email = normalized_email(acc.email.as_deref()).or_else(|| {
            if key.contains('@') {
                normalized_email(Some(&key))
            } else {
                None
            }
        });
        if let Some(ref e) = email {
            acc.email = Some(e.clone());
            acc.account_id = e.clone();
        }

        let mut matches: Vec<StoredAccount> = Vec::new();
        let mut rest: Vec<StoredAccount> = Vec::new();

        for m in merged {
            if are_same_identity(&m, &acc) {
                matches.push(m);
            } else {
                rest.push(m);
            }
        }

        let mut current = acc;
        for m in matches {
            alias_map.insert(m.account_id.clone(), current.account_id.clone());
            current = merge_stored_accounts(m, current);
        }

        alias_map.insert(key, current.account_id.clone());
        rest.push(current);
        merged = rest;
    }

    for acc in merged {
        store.accounts.insert(acc.account_id.clone(), acc);
    }

    if let Some(ref active) = store.active_account_id {
        if let Some(canonical) = alias_map.get(active) {
            store.active_account_id = Some(canonical.clone());
        }
    }

    if let Some(ref active) = store.active_account_id {
        if !store.accounts.contains_key(active) {
            store.active_account_id = None;
        }
    }

    let new_keys: Vec<String> = store.accounts.keys().cloned().collect();
    original_count != store.accounts.len() || original_keys != new_keys
}

/// Normalize a parsed file into a store (the TS `normalizeStore`): the schema
/// version must be 1, invalid entries drop, and the active id must exist.
fn normalize_store(value: Option<&Value>) -> AccountStore {
    let Some(Value::Object(map)) = value else {
        return empty_store();
    };
    if map.get("version").and_then(Value::as_i64) != Some(1) {
        return empty_store();
    }
    let Some(Value::Object(raw_accounts)) = map.get("accounts") else {
        return empty_store();
    };
    let mut accounts: BTreeMap<String, StoredAccount> = BTreeMap::new();
    for (key, raw) in raw_accounts {
        if let Some(account) = normalize_account(key, raw) {
            accounts.insert(key.clone(), account);
        }
    }
    let active_account_id = map
        .get("activeAccountId")
        .and_then(Value::as_str)
        .or_else(|| map.get("activeEmail").and_then(Value::as_str))
        .map(str::to_string);

    let mut store = AccountStore {
        version: 1,
        active_account_id,
        accounts,
    };
    migrate_and_deduplicate_accounts(&mut store);
    store
}

fn load_account_store_in(dir: &Path) -> AccountStore {
    normalize_store(read_json_object(&accounts_path(dir)).as_ref())
}

/// The account id for fresh credentials (the TS `accountIdFor`): the lowercased
/// email, else a `account-<sha256(refresh) prefix>` surrogate.
fn account_id_for(credentials: &AccountCredentials) -> String {
    let email = normalized_email(credentials.email.as_deref()).filter(|email| !email.is_empty());
    if let Some(email) = email {
        return email;
    }
    let digest = hex::encode(Sha256::digest(credentials.refresh.as_bytes()));
    let short = &digest[..16];
    format!("account-{short}")
}

/// Build a stored account from credentials, keeping identity/timestamps from an
/// existing entry (the TS `accountFromCredentials`). The store-only `email` and
/// `projectId` additionally fall back to the existing entry: the shared
/// credential type has no slot for them, and a `pa-core` token refresh rewrites
/// `auth.json` without them, so the store must not drop what it already knows.
fn account_from_credentials(
    credentials: &AccountCredentials,
    existing: Option<&StoredAccount>,
) -> StoredAccount {
    let now = now_ms();
    let email = normalized_email(credentials.email.as_deref())
        .or_else(|| existing.and_then(|account| normalized_email(account.email.as_deref())));
    let account_id = match (&email, existing) {
        (Some(email), _) => email.clone(),
        (None, Some(existing)) => existing.account_id.clone(),
        (None, None) => account_id_for(credentials),
    };
    StoredAccount {
        account_id,
        email,
        access: credentials.access.clone(),
        refresh: credentials.refresh.clone(),
        expires: credentials.expires,
        project_id: credentials
            .project_id
            .clone()
            .or_else(|| existing.and_then(|account| account.project_id.clone())),
        added_at: existing.map_or(now, |account| account.added_at),
        last_used_at: now,
    }
}

/// Accounts in insertion order (`addedAt`, then id), the TS `sortedAccounts`.
fn sorted_accounts(store: &AccountStore) -> Vec<StoredAccount> {
    let mut accounts: Vec<StoredAccount> = store.accounts.values().cloned().collect();
    accounts.sort_by(|a, b| {
        a.added_at
            .cmp(&b.added_at)
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    accounts
}

/// The store entry matching credentials by email (case-insensitive) or refresh token
/// (the TS `findAccount`).
fn find_account<'a>(
    store: &'a AccountStore,
    credentials: &AccountCredentials,
) -> Option<&'a StoredAccount> {
    let cred_email = normalized_email(credentials.email.as_deref());
    let cred_refresh = credentials.refresh.trim();

    // (b) email yang sama (case-insensitive)
    if let Some(ref email) = cred_email {
        if let Some(matched) = store.accounts.values().find(|account| {
            normalized_email(account.email.as_deref()).as_ref() == Some(email)
                || normalized_email(Some(&account.account_id)).as_ref() == Some(email)
        }) {
            return Some(matched);
        }
    }

    // (a) refresh_token yang sama
    // (c) jika credentials baru tidak punya email tapi punya refresh yang sama dengan akun manapun
    if !cred_refresh.is_empty() {
        if let Some(matched) = store
            .accounts
            .values()
            .find(|account| account.refresh.trim() == cred_refresh)
        {
            return Some(matched);
        }
    }

    None
}

/// Resolve a selector to an account id: a 1-based index into the sorted
/// accounts, else a case-insensitive account id or email (the TS
/// `findAccountId`).
fn find_account_id(store: &AccountStore, selector: &str) -> Option<String> {
    let normalized = selector.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let entries = sorted_accounts(store);
    if normalized.bytes().all(|byte| byte.is_ascii_digit()) {
        if let Ok(index) = normalized.parse::<usize>() {
            if index >= 1 && index <= entries.len() {
                return Some(entries[index - 1].account_id.clone());
            }
        }
        return None;
    }
    entries
        .iter()
        .find(|account| {
            account.account_id.to_lowercase() == normalized
                || normalized_email(account.email.as_deref()).as_deref()
                    == Some(normalized.as_str())
        })
        .map(|account| account.account_id.clone())
}

/// The `antigravity` credential `auth.json` currently holds (the TS
/// `currentAuthCredentials`).
fn current_auth_credentials(dir: &Path) -> Option<AccountCredentials> {
    let auth = read_json_object(&auth_path(dir))?;
    let current = auth.get("antigravity")?;
    let map = current.as_object()?;
    let refresh = map
        .get("refresh")
        .and_then(Value::as_str)
        .filter(|refresh| !refresh.is_empty())?;
    Some(AccountCredentials {
        access: map
            .get("access")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        refresh: refresh.to_string(),
        expires: map.get("expires").and_then(Value::as_i64).unwrap_or(0),
        project_id: optional_string(map.get("projectId")),
        // The TS writer stores `email`; Prime Agent's `AuthCredential::Oauth`
        // stores the same value as `accountId`, so accept either.
        email: optional_string(map.get("email")).or_else(|| optional_string(map.get("accountId"))),
    })
}

/// Read `auth.json` for a credential write; an unreadable file is refused
/// rather than clobbered (the TS `readAuthFile`).
fn read_auth_file(dir: &Path) -> Result<Value, AccountError> {
    let path = auth_path(dir);
    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let text = std::fs::read_to_string(&path).map_err(|_| AccountError::UnreadableAuth)?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(AccountError::UnreadableAuth),
    }
}

/// Mirror the active account into `auth.json`'s `antigravity` entry, preserving
/// every other provider (the TS `writeActiveCredential`); `None` clears it.
fn write_active_credential(
    dir: &Path,
    account: Option<&StoredAccount>,
) -> Result<(), AccountError> {
    let mut auth = read_auth_file(dir)?;
    let map = auth
        .as_object_mut()
        .expect("read_auth_file returns a JSON object");
    match account {
        Some(account) => {
            let mut entry = serde_json::Map::new();
            entry.insert("type".to_string(), Value::String("oauth".to_string()));
            if let Some(email) = &account.email {
                // `email` is the TS key; `accountId` is the same value under
                // `AuthCredential::Oauth`, so `pa-core` keeps its account id.
                entry.insert("email".to_string(), Value::String(email.clone()));
                entry.insert("accountId".to_string(), Value::String(email.clone()));
            }
            entry.insert("access".to_string(), Value::String(account.access.clone()));
            entry.insert(
                "refresh".to_string(),
                Value::String(account.refresh.clone()),
            );
            entry.insert("expires".to_string(), Value::Number(account.expires.into()));
            if let Some(project_id) = &account.project_id {
                entry.insert("projectId".to_string(), Value::String(project_id.clone()));
            }
            map.insert("antigravity".to_string(), Value::Object(entry));
        }
        None => {
            map.remove("antigravity");
        }
    }
    write_private_json(&auth_path(dir), &auth)?;
    Ok(())
}

/// Fields that make a remembered account genuinely different (ignoring the
/// `lastUsedAt` churn of every sync).
fn account_differs(previous: &StoredAccount, account: &StoredAccount) -> bool {
    previous.account_id != account.account_id
        || previous.email != account.email
        || previous.access != account.access
        || previous.refresh != account.refresh
        || previous.expires != account.expires
        || previous.project_id != account.project_id
        || previous.added_at != account.added_at
}

/// Import the credential `auth.json` holds into the store and make it active
/// (the TS `syncCurrentAuth`), writing only when something changed.
fn sync_current_auth_in(dir: &Path) {
    let Some(credentials) = current_auth_credentials(dir) else {
        return;
    };
    let mut store = load_account_store_in(dir);

    let cred_email = normalized_email(credentials.email.as_deref());
    let existing = if cred_email.is_none() {
        if let Some(active_id) = store.active_account_id.as_deref() {
            if let Some(active_account) = store.accounts.get(active_id).cloned() {
                Some(active_account)
            } else {
                find_account(&store, &credentials).cloned()
            }
        } else if store.accounts.len() == 1 {
            store.accounts.values().next().cloned()
        } else {
            find_account(&store, &credentials).cloned()
        }
    } else {
        find_account(&store, &credentials).cloned()
    };

    let account = account_from_credentials(&credentials, existing.as_ref());
    let changed = match &existing {
        Some(previous) => {
            store.active_account_id.as_deref() != Some(account.account_id.as_str())
                || account_differs(previous, &account)
        }
        None => true,
    };

    if let Some(ref prev) = existing {
        if prev.account_id != account.account_id {
            store.accounts.remove(&prev.account_id);
        }
    }

    store
        .accounts
        .insert(account.account_id.clone(), account.clone());
    store.active_account_id = Some(account.account_id.clone());

    let migrated = migrate_and_deduplicate_accounts(&mut store);
    if changed || migrated {
        let _ = save_account_store(dir, &store);
    }
}

fn remember_account_in(dir: &Path, credentials: &AccountCredentials) -> StoredAccount {
    sync_current_auth_in(dir);
    let mut store = load_account_store_in(dir);

    let cred_email = normalized_email(credentials.email.as_deref());
    let existing = if cred_email.is_none() {
        if let Some(active_id) = store.active_account_id.as_deref() {
            if let Some(active_account) = store.accounts.get(active_id).cloned() {
                Some(active_account)
            } else {
                find_account(&store, credentials).cloned()
            }
        } else if store.accounts.len() == 1 {
            store.accounts.values().next().cloned()
        } else {
            find_account(&store, credentials).cloned()
        }
    } else {
        find_account(&store, credentials).cloned()
    };

    let account = account_from_credentials(credentials, existing.as_ref());
    if let Some(ref prev) = existing {
        if prev.account_id != account.account_id {
            store.accounts.remove(&prev.account_id);
        }
    }
    store
        .accounts
        .insert(account.account_id.clone(), account.clone());
    store.active_account_id = Some(account.account_id.clone());
    migrate_and_deduplicate_accounts(&mut store);
    let _ = save_account_store(dir, &store);
    account
}

fn update_remembered_account_in(
    dir: &Path,
    previous: &AccountCredentials,
    credentials: &AccountCredentials,
) {
    let mut store = load_account_store_in(dir);
    let Some(existing) = find_account(&store, previous).cloned() else {
        return;
    };
    let account = account_from_credentials(credentials, Some(&existing));
    store.accounts.remove(&existing.account_id);
    store
        .accounts
        .insert(account.account_id.clone(), account.clone());
    if store.active_account_id.as_deref() == Some(existing.account_id.as_str()) {
        store.active_account_id = Some(account.account_id.clone());
    }
    migrate_and_deduplicate_accounts(&mut store);
    let _ = save_account_store(dir, &store);
}

fn list_accounts_in(dir: &Path) -> Vec<AccountSummary> {
    sync_current_auth_in(dir);
    let store = load_account_store_in(dir);
    sorted_accounts(&store)
        .into_iter()
        .map(|account| AccountSummary {
            active: store.active_account_id.as_deref() == Some(account.account_id.as_str()),
            account_id: account.account_id,
            email: account.email,
            last_used_at: account.last_used_at,
        })
        .collect()
}

/// Refresh an account that is missing or past its expiry skew (the TS
/// `ensureFresh`), preserving its identity and discovering a missing project id.
async fn ensure_fresh(
    http: &dyn ProviderHttp,
    account: &StoredAccount,
) -> Result<StoredAccount, String> {
    if !account.access.is_empty() && account.expires > now_ms().saturating_add(REFRESH_SKEW_MS) {
        return Ok(account.clone());
    }
    let refreshed = refresh_antigravity_token(http, &account.refresh).await?;
    let project_id = match &account.project_id {
        Some(project) => Some(project.clone()),
        None => {
            let discovered = load_code_assist(http, &refreshed.access).await;
            Some(discovered.unwrap_or_else(|| {
                default_project_id(account.email.as_deref().unwrap_or("antigravity-default"))
            }))
        }
    };
    Ok(StoredAccount {
        account_id: account.account_id.clone(),
        email: account.email.clone(),
        access: refreshed.access,
        refresh: refreshed.refresh,
        expires: refreshed.expires,
        project_id,
        added_at: account.added_at,
        last_used_at: now_ms(),
    })
}

fn api_key_for(account: &StoredAccount) -> AccountApiKey {
    AccountApiKey {
        token: account.access.clone(),
        project_id: account.project_id.clone(),
        email: account.email.clone(),
    }
}

async fn activate_account_in(
    http: &dyn ProviderHttp,
    dir: &Path,
    selector: &str,
) -> Result<StoredAccount, AccountError> {
    sync_current_auth_in(dir);
    let mut store = load_account_store_in(dir);
    let account_id = find_account_id(&store, selector)
        .ok_or_else(|| AccountError::NotFound(selector.to_string()))?;
    let existing = store
        .accounts
        .get(&account_id)
        .cloned()
        .ok_or_else(|| AccountError::NotFound(selector.to_string()))?;
    let mut account = ensure_fresh(http, &existing)
        .await
        .map_err(AccountError::Refresh)?;
    account.last_used_at = now_ms();
    store
        .accounts
        .insert(account.account_id.clone(), account.clone());
    store.active_account_id = Some(account.account_id.clone());
    save_account_store(dir, &store)?;
    write_active_credential(dir, Some(&account))?;
    Ok(account)
}

async fn remove_account_in(
    http: &dyn ProviderHttp,
    dir: &Path,
    selector: &str,
) -> Result<Option<StoredAccount>, AccountError> {
    sync_current_auth_in(dir);
    let mut store = load_account_store_in(dir);
    let account_id = find_account_id(&store, selector)
        .ok_or_else(|| AccountError::NotFound(selector.to_string()))?;
    store.accounts.remove(&account_id);
    let mut next: Option<StoredAccount> = None;
    if store.active_account_id.as_deref() == Some(account_id.as_str()) {
        let candidate = sorted_accounts(&store)
            .into_iter()
            .max_by_key(|account| account.last_used_at);
        if let Some(candidate) = candidate {
            let mut refreshed = ensure_fresh(http, &candidate)
                .await
                .map_err(AccountError::Refresh)?;
            refreshed.last_used_at = now_ms();
            store
                .accounts
                .insert(refreshed.account_id.clone(), refreshed.clone());
            next = Some(refreshed);
        }
        store.active_account_id = next.as_ref().map(|account| account.account_id.clone());
        write_active_credential(dir, next.as_ref())?;
    }
    save_account_store(dir, &store)?;
    Ok(next)
}

async fn failover_to_next_account_in(
    http: &dyn ProviderHttp,
    dir: &Path,
    tried_access_tokens: &[String],
) -> Option<AccountApiKey> {
    sync_current_auth_in(dir);
    let mut store = load_account_store_in(dir);
    let candidates: Vec<StoredAccount> = sorted_accounts(&store)
        .into_iter()
        .filter(|account| {
            !tried_access_tokens
                .iter()
                .any(|token| token == &account.access)
        })
        .collect();
    for candidate in candidates {
        let Ok(mut account) = ensure_fresh(http, &candidate).await else {
            // Skip accounts whose refresh token is no longer valid.
            continue;
        };
        if account.access.is_empty()
            || tried_access_tokens
                .iter()
                .any(|token| token == &account.access)
        {
            continue;
        }
        account.last_used_at = now_ms();
        store
            .accounts
            .insert(account.account_id.clone(), account.clone());
        store.active_account_id = Some(account.account_id.clone());
        let _ = save_account_store(dir, &store);
        let _ = write_active_credential(dir, Some(&account));
        return Some(api_key_for(&account));
    }
    None
}

/// Load the account store; empty when the file is missing, malformed, or the
/// agent directory cannot be resolved.
#[must_use]
pub fn load_account_store() -> AccountStore {
    agent_dir_or_none().map_or_else(empty_store, |dir| load_account_store_in(&dir))
}

/// Import the credential `auth.json` holds into the store and make it active.
/// No-op when `auth.json` has no `antigravity` credential.
pub fn sync_current_auth() {
    if let Some(dir) = agent_dir_or_none() {
        sync_current_auth_in(&dir);
    }
}

/// Remember credentials and make their account active; returns the stored
/// account (also without an agent directory, where persistence is skipped).
#[must_use]
pub fn remember_account(credentials: &AccountCredentials) -> StoredAccount {
    agent_dir_or_none().map_or_else(
        || account_from_credentials(credentials, None),
        |dir| remember_account_in(&dir, credentials),
    )
}

/// Replace a remembered account's credentials after a token rotation, keyed by
/// the previous credentials; no-op when the account was never remembered.
pub fn update_remembered_account(previous: &AccountCredentials, credentials: &AccountCredentials) {
    if let Some(dir) = agent_dir_or_none() {
        update_remembered_account_in(&dir, previous, credentials);
    }
}

/// The stored accounts in insertion order, with the active one flagged.
#[must_use]
pub fn list_accounts() -> Vec<AccountSummary> {
    agent_dir_or_none().map_or_else(Vec::new, |dir| list_accounts_in(&dir))
}

/// Activate the account a selector names (a 1-based index into the sorted
/// accounts, an account id, or an email), refreshing it when needed and
/// mirroring it into `auth.json`.
///
/// # Errors
///
/// Returns [`AccountError::NoAgentDirectory`] without a resolvable agent
/// directory, [`AccountError::NotFound`] when no account matches, and
/// [`AccountError::Refresh`] or [`AccountError::Io`] when the refresh or a
/// write fails.
pub async fn activate_account(
    http: &dyn ProviderHttp,
    selector: &str,
) -> Result<StoredAccount, AccountError> {
    match agent_dir_or_none() {
        Some(dir) => activate_account_in(http, &dir, selector).await,
        None => Err(AccountError::NoAgentDirectory),
    }
}

/// Remove the account a selector names. When it was the active one, the
/// remaining account used most recently becomes active and is refreshed; `None`
/// when no account remains.
///
/// # Errors
///
/// The same failures as [`activate_account`], plus a refresh failure on the
/// promoted account (the store is left untouched then).
pub async fn remove_account(
    http: &dyn ProviderHttp,
    selector: &str,
) -> Result<Option<StoredAccount>, AccountError> {
    match agent_dir_or_none() {
        Some(dir) => remove_account_in(http, &dir, selector).await,
        None => Err(AccountError::NoAgentDirectory),
    }
}

/// Switch to the next stored account whose access token is not in
/// `tried_access_tokens`, refreshing it and making it active. Used when
/// Antigravity answers a hard quota wall (HTTP 429).
pub async fn failover_to_next_account(
    http: &dyn ProviderHttp,
    tried_access_tokens: &[String],
) -> Option<AccountApiKey> {
    let dir = agent_dir_or_none()?;
    failover_to_next_account_in(http, &dir, tried_access_tokens).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{ProviderHttp, ProviderHttpRequest, ProviderHttpResponse};
    use std::future::Future;
    use std::pin::Pin;

    /// The refresh seam never runs in these tests (every account is unexpired).
    struct NoHttp;

    impl ProviderHttp for NoHttp {
        fn request(
            &self,
            _request: ProviderHttpRequest,
            _timeout_ms: u64,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderHttpResponse, String>> + Send + '_>>
        {
            Box::pin(async {
                Err::<ProviderHttpResponse, String>("no network in account tests".to_string())
            })
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        #[must_use]
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "pi-antigravity-accounts-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("the temp dir is created");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn account(email: &str, refresh: &str) -> AccountCredentials {
        AccountCredentials {
            access: format!("access-{email}"),
            refresh: refresh.to_string(),
            expires: now_ms() + 10 * 60_000,
            project_id: Some(format!("project-{email}")),
            email: Some(email.to_string()),
        }
    }

    fn names(dir: &Path) -> Vec<Option<String>> {
        list_accounts_in(dir)
            .into_iter()
            .map(|entry| entry.email)
            .collect()
    }

    /// `addedAt` reads through millisecond resolution; separate insertions that
    /// must sort apart need a real gap.
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    #[tokio::test]
    async fn stores_and_switches_accounts() {
        let dir = TempDir::new("switch");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        remember_account_in(dir.path(), &account("b@example.com", "refresh-b"));
        assert_eq!(
            names(dir.path()),
            vec![
                Some("a@example.com".to_string()),
                Some("b@example.com".to_string())
            ]
        );
        assert!(
            list_accounts_in(dir.path())
                .iter()
                .find(|entry| entry.email.as_deref() == Some("b@example.com"))
                .expect("the remembered account is listed")
                .active
        );

        activate_account_in(&NoHttp, dir.path(), "a@example.com")
            .await
            .unwrap();
        let store = load_account_store_in(dir.path());
        assert_eq!(store.active_account_id.as_deref(), Some("a@example.com"));
        let auth: Value =
            serde_json::from_str(&std::fs::read_to_string(auth_path(dir.path())).unwrap()).unwrap();
        assert_eq!(auth["antigravity"]["email"], "a@example.com");
    }

    #[test]
    fn keeps_rotated_refresh_tokens() {
        let dir = TempDir::new("rotate");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        update_remembered_account_in(
            dir.path(),
            &account("a@example.com", "refresh-a"),
            &account("a@example.com", "refresh-a2"),
        );
        assert_eq!(
            load_account_store_in(dir.path()).accounts["a@example.com"].refresh,
            "refresh-a2"
        );
    }

    #[tokio::test]
    async fn numeric_selector_ignores_digit_emails() {
        let dir = TempDir::new("numeric");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        tick();
        remember_account_in(dir.path(), &account("b@example.com", "refresh-b"));
        tick();
        remember_account_in(dir.path(), &account("1@example.com", "refresh-1"));
        let switched = activate_account_in(&NoHttp, dir.path(), "1@example.com")
            .await
            .unwrap();
        assert_eq!(switched.email.as_deref(), Some("1@example.com"));
        let by_index = activate_account_in(&NoHttp, dir.path(), "1").await.unwrap();
        assert_eq!(by_index.email.as_deref(), Some("a@example.com"));
    }

    #[tokio::test]
    async fn preserves_other_auth_providers() {
        let dir = TempDir::new("providers");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        remember_account_in(dir.path(), &account("b@example.com", "refresh-b"));
        let auth = serde_json::json!({
            "antigravity": {
                "type": "oauth",
                "email": "a@example.com",
                "access": "access-a@example.com",
                "refresh": "refresh-a",
                "expires": now_ms() + 10 * 60_000,
                "projectId": "project-a@example.com",
            },
            "other": { "type": "api", "key": "keep-me" },
        });
        std::fs::write(
            auth_path(dir.path()),
            serde_json::to_string_pretty(&auth).unwrap(),
        )
        .unwrap();

        activate_account_in(&NoHttp, dir.path(), "b@example.com")
            .await
            .unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(auth_path(dir.path())).unwrap()).unwrap();
        assert_eq!(written["other"]["key"], "keep-me");
        assert_eq!(written["antigravity"]["email"], "b@example.com");
    }

    #[test]
    fn preserves_store_only_fields_when_auth_drops_them() {
        let dir = TempDir::new("retain");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        // A `pa-core` refresh rewrites auth.json with only the shared fields.
        let auth = serde_json::json!({
            "antigravity": {
                "type": "oauth",
                "refresh": "refresh-a",
                "access": "access-a@example.com",
                "expires": now_ms() + 10 * 60_000,
            },
        });
        std::fs::write(
            auth_path(dir.path()),
            serde_json::to_string_pretty(&auth).unwrap(),
        )
        .unwrap();
        sync_current_auth_in(dir.path());
        let store = load_account_store_in(dir.path());
        let entry = &store.accounts["a@example.com"];
        assert_eq!(entry.email.as_deref(), Some("a@example.com"));
        assert_eq!(entry.project_id.as_deref(), Some("project-a@example.com"));
    }

    #[tokio::test]
    async fn failover_skips_tried_tokens() {
        let dir = TempDir::new("failover");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        tick();
        remember_account_in(dir.path(), &account("b@example.com", "refresh-b"));
        let next =
            failover_to_next_account_in(&NoHttp, dir.path(), &["access-b@example.com".to_string()])
                .await
                .expect("failover returns another account");
        assert_eq!(next.token, "access-a@example.com");
    }

    #[tokio::test]
    async fn removes_account_and_promotes_another() {
        let dir = TempDir::new("remove");
        remember_account_in(dir.path(), &account("a@example.com", "refresh-a"));
        tick();
        remember_account_in(dir.path(), &account("b@example.com", "refresh-b"));
        let removed = remove_account_in(&NoHttp, dir.path(), "b@example.com")
            .await
            .unwrap();
        assert_eq!(
            removed.as_ref().unwrap().email.as_deref(),
            Some("a@example.com")
        );
        let remaining = list_accounts_in(dir.path());
        assert!(remaining
            .iter()
            .all(|entry| entry.email.as_deref() != Some("b@example.com")));
        assert!(remaining.iter().any(|entry| entry.active));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(accounts_path(dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "account store permissions are not owner-only");
        }
    }

    #[test]
    fn normalizes_the_store_document() {
        let dir = TempDir::new("normalize");
        let raw = serde_json::json!({
            "version": 1,
            "activeEmail": "a@example.com",
            "accounts": {
                "a@example.com": {
                    "refresh": "refresh-a",
                    "access": "tok",
                    "expires": 5,
                    "addedAt": 1,
                    "lastUsedAt": 2,
                },
                "bad": { "access": "no-refresh" },
            },
        });
        std::fs::write(
            accounts_path(dir.path()),
            serde_json::to_string(&raw).unwrap(),
        )
        .unwrap();
        let store = load_account_store_in(dir.path());
        assert_eq!(store.active_account_id.as_deref(), Some("a@example.com"));
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.accounts["a@example.com"].access, "tok");
    }

    #[test]
    fn derives_an_account_id_without_email() {
        let credentials = AccountCredentials {
            access: "tok".to_string(),
            refresh: "refresh-x".to_string(),
            expires: 0,
            project_id: None,
            email: None,
        };
        let account = account_from_credentials(&credentials, None);
        assert_eq!(account.account_id.len(), "account-".len() + 16);
        assert!(account.account_id.starts_with("account-"));
    }

    #[test]
    fn sync_current_auth_without_email_updates_active_account_without_duplicate() {
        let dir = TempDir::new("no-email-sync");
        // Initially, account is remembered with email
        remember_account_in(dir.path(), &account("user@example.com", "refresh-orig"));
        let store_before = load_account_store_in(dir.path());
        assert_eq!(store_before.accounts.len(), 1);
        assert_eq!(
            store_before.active_account_id.as_deref(),
            Some("user@example.com")
        );

        // pa-core refresh rewrites auth.json WITHOUT email and with rotated tokens
        let auth = serde_json::json!({
            "antigravity": {
                "type": "oauth",
                "access": "access-updated",
                "refresh": "refresh-rotated",
                "expires": now_ms() + 20 * 60_000,
            },
        });
        std::fs::write(
            auth_path(dir.path()),
            serde_json::to_string_pretty(&auth).unwrap(),
        )
        .unwrap();

        // Run sync
        sync_current_auth_in(dir.path());

        let store_after = load_account_store_in(dir.path());
        // Must NOT create duplicate account-<hash>!
        assert_eq!(
            store_after.accounts.len(),
            1,
            "must not create duplicate account entries"
        );
        assert_eq!(
            store_after.active_account_id.as_deref(),
            Some("user@example.com")
        );
        assert!(store_after.accounts.contains_key("user@example.com"));
        assert!(
            !store_after.accounts.keys().any(|k| k.starts_with("account-")),
            "must not contain synthetic account-<hash> keys"
        );

        let updated = &store_after.accounts["user@example.com"];
        assert_eq!(updated.account_id, "user@example.com");
        assert_eq!(updated.email.as_deref(), Some("user@example.com"));
        assert_eq!(updated.access, "access-updated");
        assert_eq!(updated.refresh, "refresh-rotated");
        assert_eq!(
            updated.project_id.as_deref(),
            Some("project-user@example.com")
        );
    }

    #[test]
    fn account_with_email_saved_under_email_key_and_migrates_synthetic_id() {
        let dir = TempDir::new("email-key-migration");

        // (1) Remembering an account with email stores under email key and sets account_id to email
        let acc = remember_account_in(dir.path(), &account("User.New@Example.COM", "refresh-new"));
        assert_eq!(acc.account_id, "user.new@example.com");
        let store = load_account_store_in(dir.path());
        assert!(store.accounts.contains_key("user.new@example.com"));
        assert_eq!(
            store.active_account_id.as_deref(),
            Some("user.new@example.com")
        );

        // (2) If a legacy file has a synthetic account-<hash> entry with known email,
        // loading or syncing migrates and merges it to the email key
        let legacy_json = serde_json::json!({
            "version": 1,
            "activeAccountId": "account-legacy123456",
            "accounts": {
                "account-legacy123456": {
                    "accountId": "account-legacy123456",
                    "email": "legacy@example.com",
                    "access": "tok-leg",
                    "refresh": "ref-leg",
                    "expires": 12345,
                    "projectId": "proj-leg",
                    "addedAt": 100,
                    "lastUsedAt": 200,
                }
            }
        });
        std::fs::write(
            accounts_path(dir.path()),
            serde_json::to_string_pretty(&legacy_json).unwrap(),
        )
        .unwrap();

        let store_migrated = load_account_store_in(dir.path());
        assert_eq!(store_migrated.accounts.len(), 1);
        assert!(
            store_migrated.accounts.contains_key("legacy@example.com"),
            "entry must be keyed by email"
        );
        assert!(
            !store_migrated.accounts.contains_key("account-legacy123456"),
            "legacy synthetic key must be removed"
        );
        assert_eq!(
            store_migrated.active_account_id.as_deref(),
            Some("legacy@example.com")
        );
        assert_eq!(
            store_migrated.accounts["legacy@example.com"].account_id,
            "legacy@example.com"
        );
    }

    #[tokio::test]
    async fn failover_selects_next_accounts_sequentially() {
        let dir = TempDir::new("failover-seq");
        remember_account_in(dir.path(), &account("first@example.com", "refresh-1"));
        tick();
        remember_account_in(dir.path(), &account("second@example.com", "refresh-2"));
        tick();
        remember_account_in(dir.path(), &account("third@example.com", "refresh-3"));

        // Activate first so test starts with first as active
        activate_account_in(&NoHttp, dir.path(), "first@example.com")
            .await
            .unwrap();
        assert_eq!(
            load_account_store_in(dir.path()).active_account_id.as_deref(),
            Some("first@example.com")
        );

        // Failover 1: first@example.com failed (tried: access-first)
        let tried = vec!["access-first@example.com".to_string()];
        let next1 = failover_to_next_account_in(&NoHttp, dir.path(), &tried)
            .await
            .expect("failover 1 should return next candidate");
        assert_eq!(next1.email.as_deref(), Some("second@example.com"));
        assert_eq!(
            load_account_store_in(dir.path()).active_account_id.as_deref(),
            Some("second@example.com")
        );

        // Failover 2: second@example.com also failed (tried: first, second)
        let tried2 = vec![
            "access-first@example.com".to_string(),
            "access-second@example.com".to_string(),
        ];
        let next2 = failover_to_next_account_in(&NoHttp, dir.path(), &tried2)
            .await
            .expect("failover 2 should return next candidate");
        assert_eq!(next2.email.as_deref(), Some("third@example.com"));
        assert_eq!(
            load_account_store_in(dir.path()).active_account_id.as_deref(),
            Some("third@example.com")
        );

        // Failover 3: third@example.com also failed (all tried)
        let tried3 = vec![
            "access-first@example.com".to_string(),
            "access-second@example.com".to_string(),
            "access-third@example.com".to_string(),
        ];
        let next3 = failover_to_next_account_in(&NoHttp, dir.path(), &tried3).await;
        assert!(next3.is_none(), "no candidates remaining");
    }
}
