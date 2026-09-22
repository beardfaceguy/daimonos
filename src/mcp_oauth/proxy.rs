//! In-process loopback proxy: the SDK never sees a bearer token.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::{
    canonical_endpoint, default_store_dir, read_credential, remove_credential, write_credential,
    Credential,
};

const MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;

struct ProxyState {
    endpoint: Url,
    profile: String,
    root: PathBuf,
    capability: String,
    http: Client,
    refresh_lock: Arc<Mutex<()>>,
}

pub struct OAuthProxy {
    pub url: String,
    pub capability: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for OAuthProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

fn response(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut reply = Response::new(Body::from(message));
    *reply.status_mut() = status;
    reply
}

async fn refresh(state: &ProxyState, old: &Credential) -> Result<Credential, String> {
    let refresh_token = old
        .refresh_token
        .as_deref()
        .ok_or("auth_required: no refresh grant")?;
    let (metadata, _) = super::login::discover(&state.http, &state.endpoint).await?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", old.client_id.as_str()),
        ("resource", state.endpoint.as_str()),
    ];
    if let Some(secret) = old.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let result = state
        .http
        .post(metadata.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|_| "OAuth refresh request failed")?;
    if !result.status().is_success() {
        if result.status() == reqwest::StatusCode::BAD_REQUEST
            || result.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            // Only invalid_grant proves the saved refresh token is unusable.
            // Other 400 responses (e.g. invalid_client) must not destroy it.
            let invalid_grant = result
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|body| {
                    body.get("error")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("invalid_grant");
            if invalid_grant {
                remove_credential(&state.root, state.endpoint.as_str(), &state.profile)?;
                return Err("auth_required: OAuth grant expired or revoked".into());
            }
            return Err("OAuth refresh rejected; grant retained".into());
        }
        return Err("OAuth refresh temporarily unavailable".into());
    }
    let token: TokenResponse = result
        .json()
        .await
        .map_err(|_| "invalid OAuth refresh response")?;
    if token.access_token.is_empty() || !token.token_type.eq_ignore_ascii_case("bearer") {
        return Err("invalid OAuth refresh token type".into());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rotated = Credential {
        client_id: old.client_id.clone(),
        client_secret: old.client_secret.clone(),
        access_token: token.access_token,
        refresh_token: token.refresh_token.or_else(|| old.refresh_token.clone()),
        expires_at: token
            .expires_in
            .map(|duration| now.saturating_add(duration)),
    };
    write_credential(
        &state.root,
        state.endpoint.as_str(),
        &state.profile,
        &rotated,
    )?;
    Ok(rotated)
}

async fn current_credential(
    state: &ProxyState,
    failed_token: Option<&str>,
) -> Result<Credential, String> {
    // In-process mutex plus a private advisory file lock protect refresh
    // rotation across all Daimonos processes sharing this grant.
    let _guard = state.refresh_lock.lock().await;
    let lock_path = state.root.join(format!(
        "{}.lock",
        super::credential_path(&state.root, state.endpoint.as_str(), &state.profile)?
            .file_stem()
            .unwrap()
            .to_string_lossy()
    ));
    let _file_lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File, String> {
        use fs2::FileExt;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
            if meta.file_type().is_symlink() {
                return Err("OAuth refresh lock must not be a symlink".into());
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|_| "OAuth refresh lock unavailable")?;
        let meta = file
            .metadata()
            .map_err(|_| "OAuth refresh lock metadata unavailable")?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.mode() & 0o077 != 0
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err("OAuth refresh lock is not private".into());
        }
        file.lock_exclusive()
            .map_err(|_| "OAuth refresh lock unavailable")?;
        Ok(file)
    })
    .await
    .map_err(|_| "OAuth refresh lock task failed")??;
    let credential = read_credential(&state.root, state.endpoint.as_str(), &state.profile)?
        .ok_or("auth_required: no OAuth grant")?;
    // Another request may have rotated this grant while we waited on the lock.
    if credential.usable() && failed_token.is_none_or(|old| credential.access_token != old) {
        return Ok(credential);
    }
    refresh(state, &credential).await
}

fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in [
        CONTENT_TYPE,
        HeaderName::from_static("mcp-session-id"),
        HeaderName::from_static("mcp-protocol-version"),
        HeaderName::from_static("retry-after"),
    ] {
        if let Some(value) = upstream.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    headers
}

async fn forward(
    State(state): State<Arc<ProxyState>>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    if !matches!(method, Method::POST | Method::GET | Method::DELETE) {
        return response(StatusCode::METHOD_NOT_ALLOWED, "invalid MCP method");
    }
    // Only the SDK's own loopback client knows this random per-process capability.
    if headers
        .get("x-daimonos-proxy-key")
        .map(HeaderValue::as_bytes)
        != Some(state.capability.as_bytes())
    {
        return response(StatusCode::FORBIDDEN, "invalid local proxy capability");
    }
    let data = match axum::body::to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(data) => data,
        Err(_) => return response(StatusCode::PAYLOAD_TOO_LARGE, "MCP request too large"),
    };
    let mut credential = match current_credential(&state, None).await {
        Ok(credential) => credential,
        Err(_) => return response(StatusCode::UNAUTHORIZED, "auth_required"),
    };
    let mut refreshed = false;
    loop {
        let mut request = state
            .http
            .request(method.clone(), state.endpoint.clone())
            .header(AUTHORIZATION, format!("Bearer {}", credential.access_token))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(data.clone());
        for name in [
            HeaderName::from_static("mcp-session-id"),
            HeaderName::from_static("mcp-protocol-version"),
            HeaderName::from_static("last-event-id"),
        ] {
            if let Some(value) = headers.get(&name) {
                request = request.header(name, value.clone());
            }
        }
        let upstream = match request.send().await {
            Ok(value) => value,
            Err(_) => return response(StatusCode::BAD_GATEWAY, "MCP upstream unavailable"),
        };
        if upstream.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
            credential = match current_credential(&state, Some(&credential.access_token)).await {
                Ok(credential) => credential,
                Err(_) => return response(StatusCode::UNAUTHORIZED, "auth_required"),
            };
            refreshed = true;
            continue;
        }
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if status == StatusCode::UNAUTHORIZED {
            return response(status, "auth_required");
        }
        let safe_headers = response_headers(upstream.headers());
        let stream = upstream
            .bytes_stream()
            .map(|result| result.map_err(std::io::Error::other));
        let mut outbound = Response::new(Body::from_stream(stream));
        *outbound.status_mut() = status;
        *outbound.headers_mut() = safe_headers;
        return outbound;
    }
}

fn shared_refresh_lock(endpoint: &str, profile: &str) -> Arc<Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Mutex as StdMutex, OnceLock, Weak};
    type RefreshLocks = StdMutex<HashMap<(String, String), Weak<Mutex<()>>>>;
    static LOCKS: OnceLock<RefreshLocks> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = (endpoint.to_owned(), profile.to_owned());
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// Each bridge gets its own random capability and listener. The capability is
/// never exposed in ACP or tool schemas; dropping the bridge closes the proxy.
pub async fn start_proxy(endpoint: &str, profile: &str) -> Result<OAuthProxy, String> {
    start_proxy_with_root(endpoint, profile, default_store_dir()?).await
}

async fn start_proxy_with_root(
    endpoint: &str,
    profile: &str,
    root: PathBuf,
) -> Result<OAuthProxy, String> {
    let endpoint =
        Url::parse(&canonical_endpoint(endpoint)?).map_err(|_| "invalid OAuth MCP endpoint")?;
    if read_credential(&root, endpoint.as_str(), profile)?.is_none() {
        return Err(
            "auth_required: OAuth grant not found; run daimonos mcp auth login <server>".into(),
        );
    }
    let http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "unable to create OAuth MCP HTTP client")?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "unable to bind OAuth MCP proxy")?;
    let capability = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let refresh_lock = shared_refresh_lock(endpoint.as_str(), profile);
    let state = Arc::new(ProxyState {
        endpoint,
        profile: profile.to_string(),
        root,
        capability: capability.clone(),
        http,
        refresh_lock,
    });
    let app = Router::new().route("/mcp", any(forward)).with_state(state);
    let address = listener
        .local_addr()
        .map_err(|_| "OAuth proxy address unavailable")?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(OAuthProxy {
        url: format!("http://127.0.0.1:{}/mcp", address.port()),
        capability,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_selected_response_headers_are_forwarded() {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", HeaderValue::from_static("session"));
        headers.insert("set-cookie", HeaderValue::from_static("secret"));
        let safe = response_headers(&headers);
        assert_eq!(safe.get("mcp-session-id").unwrap(), "session");
        assert!(safe.get("set-cookie").is_none());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use axum::http::StatusCode as Status;

    async fn mock_mcp(
        headers: HeaderMap,
        method: Method,
        State(count): State<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> (Status, HeaderMap, &'static str) {
        let authorization = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
        let mut reply = HeaderMap::new();
        reply.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        reply.insert(
            "set-cookie",
            HeaderValue::from_static("upstream-session-secret"),
        );
        reply.insert("mcp-session-id", HeaderValue::from_static("session-123"));
        if method == Method::GET {
            return (Status::METHOD_NOT_ALLOWED, reply, "");
        }
        if authorization != Some("Bearer valid-token") {
            return (Status::UNAUTHORIZED, reply, "unauthorized");
        }
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (
            Status::OK,
            reply,
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        )
    }

    #[tokio::test]
    async fn shared_refresh_lock_is_per_endpoint_profile() {
        let one = shared_refresh_lock("https://example.com/mcp", "a");
        let two = shared_refresh_lock("https://example.com/mcp", "a");
        let other = shared_refresh_lock("https://example.com/mcp", "b");
        assert!(Arc::ptr_eq(&one, &two));
        assert!(!Arc::ptr_eq(&one, &other));
    }

    #[tokio::test]
    async fn proxy_requires_capability_forwards_only_safe_headers_and_never_redirects() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("secrets");
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Router::new()
            .route("/mcp", any(mock_mcp))
            .with_state(Arc::clone(&count));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        write_credential(
            &store,
            &endpoint,
            "p",
            &Credential {
                client_id: "test-client".into(),
                client_secret: None,
                access_token: "valid-token".into(),
                refresh_token: None,
                expires_at: None,
            },
        )
        .unwrap();
        let proxy = start_proxy_with_root(&endpoint, "p", store).await.unwrap();
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let forbidden = http.post(&proxy.url).body("{}").send().await.unwrap();
        assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);
        let accepted = http
            .post(&proxy.url)
            .header("x-daimonos-proxy-key", &proxy.capability)
            .header("authorization", "Bearer attacker-token")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), reqwest::StatusCode::OK);
        assert!(accepted.headers().get("set-cookie").is_none());
        assert_eq!(
            accepted.headers().get("mcp-session-id").unwrap(),
            "session-123"
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
        task.abort();
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use axum::Json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct Mock {
        token_calls: Arc<AtomicUsize>,
    }

    async fn resource(State(state): State<(Mock, String)>) -> Json<serde_json::Value> {
        Json(
            serde_json::json!({"resource":format!("{}/mcp",state.1),"authorization_servers":[state.1]}),
        )
    }
    async fn as_metadata(State(state): State<(Mock, String)>) -> Json<serde_json::Value> {
        Json(
            serde_json::json!({"issuer":state.1,"authorization_endpoint":format!("{}/authorize",state.1),"token_endpoint":format!("{}/token",state.1)}),
        )
    }
    async fn token(
        State(state): State<(Mock, String)>,
        body: String,
    ) -> (StatusCode, Json<serde_json::Value>) {
        state.0.token_calls.fetch_add(1, Ordering::Relaxed);
        if body.contains("refresh_token=bad") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"invalid_grant"})),
            );
        }
        (
            StatusCode::OK,
            Json(
                serde_json::json!({"access_token":"new-token","token_type":"Bearer","refresh_token":"rotated-token","expires_in":3600}),
            ),
        )
    }
    async fn mcp(headers: HeaderMap) -> StatusCode {
        if headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some("Bearer new-token")
        {
            StatusCode::OK
        } else {
            StatusCode::UNAUTHORIZED
        }
    }

    async fn fixture() -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let calls = Arc::new(AtomicUsize::new(0));
        let state = (
            Mock {
                token_calls: Arc::clone(&calls),
            },
            base.clone(),
        );
        let app = Router::new()
            .route("/mcp", any(mcp))
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                axum::routing::get(resource),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                axum::routing::get(as_metadata),
            )
            .route("/token", axum::routing::post(token))
            .with_state(state);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("{base}/mcp"), task, calls)
    }
    fn credential(token: &str) -> Credential {
        Credential {
            client_id: "test-client".into(),
            client_secret: None,
            access_token: token.into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: Some(1),
        }
    }
    #[tokio::test]
    async fn expired_grant_refreshes_and_rotates_atomically() {
        let (endpoint, task, calls) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("auth");
        write_credential(&root, &endpoint, "account", &credential("old-token")).unwrap();
        let proxy = start_proxy_with_root(&endpoint, "account", root.clone())
            .await
            .unwrap();
        let http = Client::new();
        let response = http
            .post(&proxy.url)
            .header("x-daimonos-proxy-key", &proxy.capability)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let saved = read_credential(&root, &endpoint, "account")
            .unwrap()
            .unwrap();
        assert_eq!(saved.access_token, "new-token");
        assert_eq!(saved.refresh_token.as_deref(), Some("rotated-token"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        task.abort();
    }
    #[tokio::test]
    async fn invalid_grant_requires_login_without_leaking_tokens() {
        let (endpoint, task, _) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("auth");
        let mut saved = credential("old-token");
        saved.refresh_token = Some("bad".into());
        write_credential(&root, &endpoint, "account", &saved).unwrap();
        let proxy = start_proxy_with_root(&endpoint, "account", root.clone())
            .await
            .unwrap();
        let response = Client::new()
            .post(&proxy.url)
            .header("x-daimonos-proxy-key", &proxy.capability)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(response.text().await.unwrap(), "auth_required");
        assert!(read_credential(&root, &endpoint, "account")
            .unwrap()
            .is_none());
        task.abort();
    }
}
