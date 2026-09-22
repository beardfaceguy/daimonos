//! Interactive authorization-code + PKCE login, initiated outside an ACP agent turn.
use super::{canonical_endpoint, write_credential, Credential};
use crate::config::McpOAuthServer;
use reqwest::Url;
use rust_mcp_sdk::auth::{
    generate_pkce_params, metadata_url_fallbacks, parse_www_authenticate_param,
};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const REQUEST_LIMIT: usize = 8192;

#[derive(Deserialize)]
pub(super) struct OAuthMetadata {
    issuer: Url,
    authorization_endpoint: Url,
    pub token_endpoint: Url,
    registration_endpoint: Option<Url>,
    code_challenge_methods_supported: Option<Vec<String>>,
    response_types_supported: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    resource: Url,
    authorization_servers: Vec<Url>,
    scopes_supported: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Registration {
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

fn secure_url(url: &Url) -> Result<(), String> {
    if (url.scheme() != "https"
        && !(cfg!(test)
            && url.scheme() == "http"
            && url.host_str().is_some_and(|host| host == "127.0.0.1")))
        || url.host_str().is_none()
        || url.query().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("OAuth metadata contains an unsafe URL".into());
    }
    Ok(())
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "unable to initialize OAuth HTTP client".into())
}

fn metadata_urls(base: &Url) -> Vec<Url> {
    metadata_url_fallbacks(base.as_str())
        .into_iter()
        .filter_map(|v| Url::parse(&v).ok())
        .collect()
}

pub(super) async fn discover(
    client: &reqwest::Client,
    endpoint: &Url,
) -> Result<(OAuthMetadata, Option<String>), String> {
    let probe = client.post(endpoint.clone())
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"daimonos","version":env!("CARGO_PKG_VERSION")}}}))
        .send().await.map_err(|_| "MCP OAuth discovery probe failed".to_string())?;
    let challenge = probe
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let challenged_scope = parse_www_authenticate_param(challenge, "scope");
    let explicit = parse_www_authenticate_param(challenge, "resource_metadata");
    let origin = endpoint.origin().ascii_serialization();
    let mut candidates = Vec::new();
    if let Some(url) = explicit {
        candidates.push(Url::parse(&url).map_err(|_| "invalid resource metadata URL")?);
    }
    candidates.push(
        Url::parse(&format!(
            "{origin}/.well-known/oauth-protected-resource{}",
            endpoint.path()
        ))
        .map_err(|_| "invalid resource metadata URL")?,
    );
    candidates.push(
        Url::parse(&format!("{origin}/.well-known/oauth-protected-resource"))
            .map_err(|_| "invalid resource metadata URL")?,
    );
    let mut resource = None;
    for url in candidates {
        secure_url(&url)?;
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(parsed) = resp.json::<ProtectedResourceMetadata>().await {
                    resource = Some(parsed);
                    break;
                }
            }
        }
    }
    let resource = resource.ok_or("OAuth protected-resource metadata not found")?;
    if canonical_endpoint(resource.resource.as_str())? != canonical_endpoint(endpoint.as_str())? {
        return Err("OAuth resource metadata does not match configured MCP endpoint".into());
    }
    let scope =
        challenged_scope.or_else(|| resource.scopes_supported.as_ref().map(|v| v.join(" ")));
    for server in resource.authorization_servers {
        secure_url(&server)?;
        for url in metadata_urls(&server) {
            secure_url(&url)?;
            if let Ok(resp) = client.get(url).send().await {
                if resp.status().is_success() {
                    if let Ok(meta) = resp.json::<OAuthMetadata>().await {
                        secure_url(&meta.issuer)?;
                        secure_url(&meta.authorization_endpoint)?;
                        secure_url(&meta.token_endpoint)?;
                        if let Some(url) = &meta.registration_endpoint {
                            secure_url(url)?;
                        }
                        if canonical_endpoint(meta.issuer.as_str())?
                            != canonical_endpoint(server.as_str())?
                        {
                            return Err("OAuth issuer does not match protected-resource authorization server".into());
                        }
                        return Ok((meta, scope));
                    }
                }
            }
        }
    }
    Err("OAuth authorization-server metadata not found".into())
}
fn callback_parameters(target: &str, state: &str) -> Result<String, String> {
    let url =
        Url::parse(&format!("http://localhost{target}")).map_err(|_| "invalid OAuth callback")?;
    if !target.starts_with("/callback?") || url.path() != "/callback" || url.fragment().is_some() {
        return Err("unexpected OAuth callback path".into());
    }
    let mut received_state = None;
    let mut code = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if received_state.replace(value.clone().into_owned()).is_some() => {
                return Err("duplicate OAuth state".into())
            }
            "code" if code.replace(value.clone().into_owned()).is_some() => {
                return Err("duplicate OAuth code".into())
            }
            "error" if error.replace(value.into_owned()).is_some() => {
                return Err("duplicate OAuth error".into())
            }
            _ => {}
        }
    }
    if received_state.as_deref() != Some(state) {
        return Err("OAuth state mismatch".into());
    }
    if error.is_some() {
        return Err("OAuth authorization declined".into());
    }
    code.filter(|v| !v.is_empty())
        .ok_or_else(|| "OAuth callback has no code".into())
}

async fn accept_callback(listener: &TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|_| "OAuth callback listener failed")?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut buf = vec![0; REQUEST_LIMIT];
        let mut count = 0;
        while count < buf.len() && !buf[..count].windows(4).any(|v| v == b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf[count..]))
                .await
                .map_err(|_| "OAuth callback read timeout")?
                .map_err(|_| "OAuth callback read failed")?;
            if n == 0 {
                return Err("incomplete OAuth callback request".into());
            }
            count += n;
        }
        let request = std::str::from_utf8(&buf[..count]).map_err(|_| "invalid OAuth callback")?;
        let line = request
            .lines()
            .next()
            .ok_or("missing OAuth callback request")?;
        let mut parts = line.split(' ');
        if parts.next() != Some("GET") {
            return Err("invalid OAuth callback method".into());
        }
        let target = parts.next().ok_or("missing OAuth callback target")?;
        if !request.contains("\r\n\r\n") {
            return Err("incomplete OAuth callback request".into());
        }
        let host = request
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
                    .map(|(_, value)| value.trim())
            })
            .ok_or("missing OAuth callback host")?;
        let expected_host = format!(
            "127.0.0.1:{}",
            listener
                .local_addr()
                .map_err(|_| "callback port unavailable")?
                .port()
        );
        if host.trim() != expected_host {
            return Err("OAuth callback host mismatch".into());
        }
        let code = callback_parameters(target, state);
        let body = if code.is_ok() {
            "Authorization received. Return to Daimonos."
        } else {
            "Authorization failed. Return to Daimonos."
        };
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = stream.write_all(response.as_bytes()).await;
        return code;
    }
}

fn launch_browser(url: &Url) {
    // An argv invocation (not a shell) prevents URL metacharacters becoming commands.
    match std::process::Command::new("xdg-open")
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => eprintln!("Browser launch requested; if it does not open, use the URL above."),
        Err(_) => eprintln!("Browser unavailable; open the URL above manually."),
    }
}

pub(super) async fn login(root: &Path, name: &str, policy: &McpOAuthServer) -> Result<(), String> {
    let endpoint =
        Url::parse(&canonical_endpoint(&policy.url)?).map_err(|_| "invalid MCP endpoint")?;
    let client = http()?;
    let (metadata, discovered_scope) = discover(&client, &endpoint).await?;
    if metadata
        .code_challenge_methods_supported
        .as_ref()
        .is_some_and(|v| !v.iter().any(|s| s == "S256"))
        || metadata
            .response_types_supported
            .as_ref()
            .is_some_and(|v| !v.iter().any(|s| s == "code"))
    {
        return Err("authorization server does not support code + PKCE S256".into());
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "unable to bind OAuth loopback callback")?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener
            .local_addr()
            .map_err(|_| "callback port unavailable")?
            .port()
    );
    let registration = if let Some(id) = &policy.client_id {
        Registration {
            client_id: id.clone(),
            client_secret: None,
        }
    } else {
        let url = metadata.registration_endpoint.as_ref().ok_or(
            "authorization server requires a configured client_id or registration_endpoint",
        )?;
        let response = client.post(url.clone()).json(&serde_json::json!({
            "client_name": "Daimonos", "redirect_uris": [&redirect], "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"], "token_endpoint_auth_method": "none"
        })).send().await.map_err(|_| "OAuth client registration request failed")?;
        if !response.status().is_success() {
            return Err(format!(
                "OAuth client registration failed: HTTP {}",
                response.status().as_u16()
            ));
        }
        response
            .json::<Registration>()
            .await
            .map_err(|_| "invalid OAuth registration response")?
    };
    if registration.client_id.is_empty() {
        return Err("OAuth registration supplied an empty client ID".into());
    }
    let scope = discovered_scope.as_deref().or(policy.scope.as_deref());
    let pkce = generate_pkce_params();
    let state = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut authorization_url = metadata.authorization_endpoint.clone();
    {
        let mut q = authorization_url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &registration.client_id);
        q.append_pair("redirect_uri", &redirect);
        q.append_pair("code_challenge", &pkce.code_challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &state);
        q.append_pair("resource", endpoint.as_str());
        if let Some(scope) = scope {
            q.append_pair("scope", scope);
        }
    }
    println!("Authorize MCP server '{name}' in the intended workspace, then return here.\nAuthorization URL: {authorization_url}");
    launch_browser(&authorization_url);
    let code = tokio::time::timeout(CALLBACK_TIMEOUT, accept_callback(&listener, &state))
        .await
        .map_err(|_| "OAuth callback timed out")??;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("client_id", registration.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code", code.as_str()),
        ("code_verifier", pkce.code_verifier.as_str()),
        ("resource", endpoint.as_str()),
    ];
    if let Some(scope) = scope {
        form.push(("scope", scope));
    }
    if let Some(secret) = registration.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let response = client
        .post(metadata.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|_| "OAuth token exchange request failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "OAuth token exchange failed: HTTP {}",
            response.status().as_u16()
        ));
    }
    let token: Token = response
        .json()
        .await
        .map_err(|_| "invalid OAuth token response")?;
    if token.access_token.is_empty() || !token.token_type.eq_ignore_ascii_case("bearer") {
        return Err("OAuth token response does not contain a bearer token".into());
    }
    let expiry = token.expires_in.map(|seconds| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(seconds)
    });
    let credential = Credential {
        client_id: registration.client_id,
        client_secret: registration.client_secret,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: expiry,
    };
    write_credential(root, endpoint.as_str(), &policy.profile, &credential)?;
    println!("{name}: OAuth grant saved. Reconnect the ACP session to refresh tool discovery.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn callback_accepts_split_headers_and_checks_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { accept_callback(&listener, "expected").await });
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /callback?state=expected&code=secret HTTP/1.1\r\nho")
            .await
            .unwrap();
        stream
            .write_all(format!("st: 127.0.0.1:{port}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let result = server.await.unwrap().unwrap();
        assert_eq!(result, "secret");
    }

    #[test]
    fn callback_validates_state_and_rejects_duplicates() {
        assert_eq!(
            callback_parameters("/callback?state=ok&code=abc", "ok").unwrap(),
            "abc"
        );
        for bad in [
            "/callback?state=wrong&code=abc",
            "/callback?state=ok&state=ok&code=abc",
            "/callback?state=ok&code=abc&code=def",
            "/bad?state=ok&code=abc",
            "/callback?state=ok&error=denied",
        ] {
            assert!(callback_parameters(bad, "ok").is_err());
        }
    }
}
