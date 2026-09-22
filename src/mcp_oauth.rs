//! Explicit, per-endpoint OAuth policy and private credential storage for outbound MCP.
//! Never serialize tokens into the ACP protocol, Zed settings, diagnostics or pool keys.
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::McpOAuthServer;

mod login;
mod proxy;
use login::login;
pub use proxy::{start_proxy, OAuthProxy};

pub fn canonical_endpoint(input: &str) -> Result<String, String> {
    let mut url = Url::parse(input).map_err(|_| "invalid server URL")?;
    if (url.scheme() != "https"
        && !(cfg!(test)
            && url.scheme() == "http"
            && url.host_str().is_some_and(|host| host == "127.0.0.1")))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "OAuth server must be an HTTPS URL without credentials, query or fragment".into(),
        );
    }
    // Require absolute origin-rooted paths. URL parsers normalize dot segments,
    // but an encoded traversal component must not be accepted for auth identity.
    if input.to_ascii_lowercase().contains("%2e") || input.to_ascii_lowercase().contains("%2f") {
        return Err("ambiguous encoded path in OAuth endpoint".into());
    }
    // Trailing slash is not significant for configured endpoint identity.
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.into())
}

pub fn policy_for<'a>(
    servers: &'a std::collections::HashMap<String, McpOAuthServer>,
    url: &str,
) -> Option<(&'a str, &'a McpOAuthServer)> {
    let canonical = canonical_endpoint(url).ok()?;
    servers.iter().find_map(|(name, server)| {
        (canonical_endpoint(&server.url).ok().as_deref() == Some(canonical.as_str()))
            .then_some((name.as_str(), server))
    })
}

#[cfg(test)]
pub fn rejects_static_authorization(headers: &std::collections::HashMap<String, String>) -> bool {
    headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
}

#[derive(Serialize, Deserialize)]
pub struct Credential {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl Credential {
    pub fn usable(&self) -> bool {
        !self.access_token.is_empty()
            && self.expires_at.is_none_or(|expiry| {
                expiry
                    > SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        + 30
            })
    }
}

/// Digest of canonical URL + profile; neither token nor account identifier appears in the filename.
pub fn credential_path(root: &Path, url: &str, profile: &str) -> Result<PathBuf, String> {
    if profile.is_empty() || profile.contains('/') || profile.contains('\\') {
        return Err("invalid OAuth profile".into());
    }
    let endpoint = canonical_endpoint(url)?;
    let digest = Sha256::digest(format!("{endpoint}\0{profile}").as_bytes());
    Ok(root.join(format!("{}.json", hex::encode(digest))))
}

pub fn default_store_dir() -> Result<PathBuf, String> {
    let home = crate::paths::home_dir().ok_or("HOME required for OAuth credential storage")?;
    let parent = home.join(".blue_rose");
    if parent.exists() {
        check_private(&parent, true)?;
    }
    Ok(parent.join("mcp-oauth"))
}

#[cfg(unix)]
fn check_private(path: &Path, directory: bool) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(|_| "OAuth credential path unavailable")?;
    let valid_type = if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    if !valid_type
        || meta.file_type().is_symlink()
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return Err("OAuth credential path must be user-owned, private, and not a symlink".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path, _directory: bool) -> Result<(), String> {
    Err("OAuth file credential storage requires Unix permissions".into())
}

pub fn read_credential(
    root: &Path,
    url: &str,
    profile: &str,
) -> Result<Option<Credential>, String> {
    let path = credential_path(root, url, profile)?;
    if std::fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(None);
    }
    check_private(root, true)?;
    check_private(&path, false)?;
    let raw = std::fs::read(path).map_err(|_| "unable to read OAuth credential")?;
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|_| "invalid OAuth credential file".into())
}

#[cfg(unix)]
pub fn write_credential(
    root: &Path,
    url: &str,
    profile: &str,
    credential: &Credential,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = credential_path(root, url, profile)?;
    // Never follow a symlink masquerading as the credential directory.
    if let Some(parent) = root.parent() {
        if parent
            == crate::paths::home_dir()
                .unwrap_or_default()
                .join(".blue_rose")
            && !parent.exists()
        {
            std::fs::create_dir(parent).map_err(|_| "unable to create OAuth credential parent")?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "unable to protect OAuth credential parent")?;
        }
        if parent
            == crate::paths::home_dir()
                .unwrap_or_default()
                .join(".blue_rose")
        {
            check_private(parent, true)?;
        }
    }
    if std::fs::symlink_metadata(root).is_ok() {
        check_private(root, true)?;
    } else {
        std::fs::create_dir_all(root).map_err(|_| "unable to create OAuth credential directory")?;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "unable to set OAuth credential directory permissions")?;
    }
    check_private(root, true)?;
    if std::fs::symlink_metadata(&path).is_ok() {
        check_private(&path, false)?;
    }
    let temp = root.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| "unable to create private OAuth credential file")?;
        let encoded =
            serde_json::to_vec(credential).map_err(|_| "unable to serialize OAuth credential")?;
        file.write_all(&encoded)
            .and_then(|_| file.sync_all())
            .map_err(|_| "unable to persist OAuth credential")?;
        std::fs::rename(&temp, &path).map_err(|_| "unable to replace OAuth credential")?;
        let directory =
            std::fs::File::open(root).map_err(|_| "unable to open OAuth credential directory")?;
        directory
            .sync_all()
            .map_err(|_| "unable to sync OAuth credential directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn remove_credential(root: &Path, url: &str, profile: &str) -> Result<(), String> {
    let path = credential_path(root, url, profile)?;
    if std::fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(());
    }
    check_private(root, true)?;
    check_private(&path, false)?;
    std::fs::remove_file(path).map_err(|_| "unable to delete OAuth credential".into())
}

pub async fn run_command(
    command: crate::cli::McpAuthCommand,
    cfg: &crate::config::AcpMcpConfig,
) -> anyhow::Result<()> {
    let server_name = match &command {
        crate::cli::McpAuthCommand::Login { server }
        | crate::cli::McpAuthCommand::Status { server }
        | crate::cli::McpAuthCommand::Logout { server } => server,
    };
    let policy = cfg.oauth_servers.get(server_name).ok_or_else(|| {
        anyhow::anyhow!("no OAuth policy configured for MCP server '{server_name}'")
    })?;
    let root = default_store_dir().map_err(anyhow::Error::msg)?;
    match command {
        crate::cli::McpAuthCommand::Login { .. } => {
            login(&root, server_name, policy)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        crate::cli::McpAuthCommand::Status { .. } => {
            let status = match read_credential(&root, &policy.url, &policy.profile)
                .map_err(anyhow::Error::msg)?
            {
                None => "auth_required",
                Some(credential) if credential.usable() => "authenticated",
                Some(credential) if credential.refresh_token.is_some() => "refresh_required",
                Some(_) => "auth_required",
            };
            println!("{server_name}: {status} (profile: {})", policy.profile);
        }
        crate::cli::McpAuthCommand::Logout { .. } => {
            remove_credential(&root, &policy.url, &policy.profile).map_err(anyhow::Error::msg)?;
            println!("{server_name}: local OAuth grant removed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_identity_is_strict() {
        assert_eq!(
            canonical_endpoint("https://MCP.Notion.com/mcp/").unwrap(),
            "https://mcp.notion.com/mcp"
        );
        for url in [
            "http://example.com/mcp",
            "https://example.com/mcp/%2e%2e/other",
            "https://user@example.com/mcp",
            "https://example.com/mcp?token=secret",
            "https://example.com/mcp#frag",
        ] {
            assert!(canonical_endpoint(url).is_err());
        }
    }
    #[test]
    fn conflicting_header_is_case_insensitive() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("aUtHoRiZaTiOn".into(), "secret".into());
        assert!(rejects_static_authorization(&headers));
    }
    #[cfg(unix)]
    #[test]
    fn credential_store_rejects_symlinks_and_world_readable_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let url = "https://example.com/mcp";
        let credential = Credential {
            client_id: "client".into(),
            client_secret: None,
            access_token: "hidden".into(),
            refresh_token: None,
            expires_at: None,
        };
        write_credential(&root, url, "default", &credential).unwrap();
        let path = credential_path(&root, url, "default").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_credential(&root, url, "default").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let other = tmp.path().join("other");
        std::fs::rename(&path, &other).unwrap();
        symlink(&other, &path).unwrap();
        assert!(read_credential(&root, url, "default").is_err());
        assert!(write_credential(&root, url, "default", &credential).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&root, tmp.path().join("actual")).unwrap();
        symlink(tmp.path().join("actual"), &root).unwrap();
        assert!(write_credential(&root, url, "default", &credential).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private_and_no_tokens_in_paths() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let url = "https://example.com/mcp";
        let c = Credential {
            client_id: "client".into(),
            client_secret: None,
            access_token: "very-secret".into(),
            refresh_token: None,
            expires_at: None,
        };
        write_credential(&root, url, "default", &c).unwrap();
        let path = credential_path(&root, url, "default").unwrap();
        assert!(!path.to_string_lossy().contains("very-secret"));
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(read_credential(&root, url, "default")
            .unwrap()
            .unwrap()
            .usable());
        remove_credential(&root, url, "default").unwrap();
        assert!(!path.exists());
    }
}
