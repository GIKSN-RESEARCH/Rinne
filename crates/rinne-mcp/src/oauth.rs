//! OAuth 2.1 for remote MCP servers (`MCP_SKILLS.md` §10; MCP auth spec
//! 2025-06-18). The spec-mandated path for public remote servers: discover the
//! authorization server (RFC 9728 protected-resource metadata → RFC 8414
//! authorization-server metadata), register a client dynamically (RFC 7591),
//! run the authorization-code + PKCE flow through the browser with a local
//! callback, exchange the code for tokens bound to the server via the `resource`
//! indicator (RFC 8707), and refresh them as they expire.
//!
//! Tokens live in the OS keychain, never in config. The non-interactive pieces
//! (PKCE, discovery parsing, URL building, token parsing, refresh) are unit- and
//! integration-tested; the browser hop is orchestrated in [`login`].

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use rinne_types::{Result, RinneError};

fn err(msg: impl Into<String>) -> RinneError {
    RinneError::Mcp(msg.into())
}

/// A stored OAuth session for one server. Persisted (as JSON) in the keychain;
/// the config only records `auth = "oauth"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthSession {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds at which the access token expires, if known.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Where to refresh (and the client identity for it).
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// The canonical server URI the tokens are bound to (RFC 8707).
    pub resource: String,
    #[serde(default)]
    pub scope: Option<String>,
}

impl OAuthSession {
    /// Whether the access token is expired or within a 60s refresh margin.
    pub fn is_expired(&self, now: u64) -> bool {
        match self.expires_at {
            Some(exp) => now + 60 >= exp,
            None => false, // no expiry known → assume valid; a 401 will re-trigger login
        }
    }
}

// ---- PKCE ------------------------------------------------------------------

/// A PKCE verifier + its S256 challenge (RFC 7636).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        let verifier = random_token(32)?; // 43-char base64url, within the 43–128 range
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// A URL-safe random token from `n` bytes of OS randomness.
fn random_token(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| err(format!("randomness unavailable: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(&buf))
}

// ---- Discovery (RFC 9728 + RFC 8414) ---------------------------------------

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// The authorization endpoints + the resource identifier discovered for a server.
#[derive(Debug, Clone)]
pub struct Discovered {
    meta: AuthServerMetadata,
    /// The canonical resource URI to bind tokens to.
    resource: String,
    scope: Option<String>,
}

/// Extract the `resource_metadata` URL from a `WWW-Authenticate: Bearer …` header.
pub fn resource_metadata_url(www_authenticate: &str) -> Option<String> {
    // e.g. `Bearer resource_metadata="https://x/.well-known/oauth-protected-resource"`
    let key = "resource_metadata";
    let start = www_authenticate.find(key)? + key.len();
    let rest = www_authenticate[start..].trim_start_matches([' ', '=']);
    let rest = rest.trim_start_matches('"');
    let end = rest.find('"').unwrap_or(rest.len());
    let url = rest[..end].trim();
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// The origin (`scheme://host[:port]`) of a URL.
fn origin(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let mut o = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
            if let Some(p) = u.port() {
                o.push_str(&format!(":{p}"));
            }
            o
        }
        Err(_) => url.trim_end_matches('/').to_string(),
    }
}

/// Discover the authorization server for `server_url`. Tries the protected-
/// resource metadata pointed to by a 401's `WWW-Authenticate` (if given), then
/// the well-known locations, then treats the server's own origin as the auth
/// server as a last resort.
pub async fn discover(server_url: &str, www_authenticate: Option<&str>) -> Result<Discovered> {
    let client = http();

    // 1. Protected-resource metadata → the authorization server(s) + resource.
    let prm_urls = {
        let mut v = Vec::new();
        if let Some(h) = www_authenticate {
            if let Some(u) = resource_metadata_url(h) {
                v.push(u);
            }
        }
        v.push(format!(
            "{}/.well-known/oauth-protected-resource",
            origin(server_url)
        ));
        v
    };
    let mut resource = server_url.to_string();
    let mut auth_servers: Vec<String> = Vec::new();
    for url in prm_urls {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(prm) = resp.json::<ProtectedResourceMetadata>().await {
                    if let Some(r) = prm.resource {
                        resource = r;
                    }
                    auth_servers = prm.authorization_servers;
                    break;
                }
            }
        }
    }
    // If no PRM, assume the server is (or fronts) its own authorization server.
    if auth_servers.is_empty() {
        auth_servers.push(origin(server_url));
    }

    // 2. Authorization-server metadata for the first server that resolves.
    for as_url in &auth_servers {
        for wk in [
            "/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration",
        ] {
            let url = format!("{}{}", origin(as_url), wk);
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    if let Ok(meta) = resp.json::<AuthServerMetadata>().await {
                        let scope = if meta.scopes_supported.is_empty() {
                            None
                        } else {
                            Some(meta.scopes_supported.join(" "))
                        };
                        return Ok(Discovered {
                            meta,
                            resource,
                            scope,
                        });
                    }
                }
            }
        }
    }
    Err(err(
        "could not discover an OAuth authorization server for this MCP server",
    ))
}

// ---- Dynamic client registration (RFC 7591) --------------------------------

#[derive(Debug, Deserialize)]
struct ClientRegistration {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<ClientRegistration> {
    let body = serde_json::json!({
        "client_name": "Rinne",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = http()
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| err(format!("dynamic client registration failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(err(format!(
            "client registration HTTP {status}: {}",
            snippet(&text)
        )));
    }
    resp.json::<ClientRegistration>()
        .await
        .map_err(|e| err(format!("bad client registration response: {e}")))
}

// ---- Authorization URL + token exchange ------------------------------------

/// Build the browser authorization URL for the code+PKCE flow.
pub fn authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    resource: &str,
    scope: Option<&str>,
) -> Result<String> {
    let mut url = reqwest::Url::parse(authorization_endpoint)
        .map_err(|e| err(format!("bad authorization endpoint: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("state", state);
        q.append_pair("code_challenge", challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("resource", resource);
        if let Some(s) = scope {
            q.append_pair("scope", s);
        }
    }
    Ok(url.to_string())
}

/// The raw token endpoint response.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

/// POST a form to the token endpoint and parse the response.
async fn token_request(token_endpoint: &str, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = http()
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .map_err(|e| err(format!("token request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(err(format!(
            "token endpoint HTTP {status}: {}",
            snippet(&text)
        )));
    }
    resp.json::<TokenResponse>()
        .await
        .map_err(|e| err(format!("bad token response: {e}")))
}

fn session_from(
    token: TokenResponse,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    resource: &str,
    now: u64,
    prev_refresh: Option<String>,
) -> OAuthSession {
    OAuthSession {
        expires_at: token.expires_in.map(|s| now + s),
        // A refresh response may omit a new refresh_token; keep the old one.
        refresh_token: token.refresh_token.or(prev_refresh),
        access_token: token.access_token,
        token_endpoint: token_endpoint.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.map(String::from),
        resource: resource.to_string(),
        scope: token.scope,
    }
}

/// Exchange a refresh token for a fresh session (RFC 6749 §6).
pub async fn refresh(session: &OAuthSession, now: u64) -> Result<OAuthSession> {
    let refresh = session
        .refresh_token
        .as_deref()
        .ok_or_else(|| err("session has no refresh token; run `rinne mcp login` again"))?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", session.client_id.as_str()),
        ("resource", session.resource.as_str()),
    ];
    if let Some(secret) = &session.client_secret {
        form.push(("client_secret", secret.as_str()));
    }
    let token = token_request(&session.token_endpoint, &form).await?;
    Ok(session_from(
        token,
        &session.token_endpoint,
        &session.client_id,
        session.client_secret.as_deref(),
        &session.resource,
        now,
        session.refresh_token.clone(),
    ))
}

// ---- Local callback server -------------------------------------------------

/// Bind an ephemeral loopback listener and return it with its `redirect_uri`.
async fn bind_callback() -> Result<(TcpListener, String)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| err(format!("could not open the local callback listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| err(format!("callback listener has no address: {e}")))?
        .port();
    Ok((listener, format!("http://127.0.0.1:{port}/callback")))
}

/// Wait for the browser redirect, returning the `code` once `state` matches.
async fn await_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| err(format!("callback connection failed: {e}")))?;

    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    // First line: `GET /callback?code=…&state=… HTTP/1.1`
    let target = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    let (code, state) = parse_callback_query(target);

    let (status, msg) = if code.is_some() && state.as_deref() == Some(expected_state) {
        ("200 OK", "Rinne is now connected. You can close this tab.")
    } else {
        (
            "400 Bad Request",
            "Authorization failed. Return to Rinne and try again.",
        )
    };
    let body = format!("<html><body><p>{msg}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;

    match (code, state) {
        (Some(c), Some(s)) if s == expected_state => Ok(c),
        _ => Err(err(
            "authorization was denied or the callback state did not match",
        )),
    }
}

/// Pull `code` and `state` out of a `/callback?…` request target.
fn parse_callback_query(target: &str) -> (Option<String>, Option<String>) {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let val = percent_decode(v);
            match k {
                "code" => code = Some(val),
                "state" => state = Some(val),
                _ => {}
            }
        }
    }
    (code, state)
}

/// Minimal percent-decoding for callback query values.
fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- The full login flow ---------------------------------------------------

/// Run the interactive OAuth login for `server_url` and return a session with
/// tokens. Opens the browser (also printing the URL as a fallback) and waits up
/// to five minutes for the redirect. `preset_client_id` skips dynamic
/// registration for servers that require a pre-registered client.
pub async fn login(
    server_url: &str,
    www_authenticate: Option<&str>,
    preset_client_id: Option<String>,
    now: u64,
) -> Result<OAuthSession> {
    login_inner(server_url, www_authenticate, preset_client_id, now, |url| {
        open_browser(url)
    })
    .await
}

/// [`login`] with an injectable opener (the browser hop), so the full flow can
/// be exercised against a mock in tests.
async fn login_inner<F: FnOnce(&str)>(
    server_url: &str,
    www_authenticate: Option<&str>,
    preset_client_id: Option<String>,
    now: u64,
    open: F,
) -> Result<OAuthSession> {
    let discovered = discover(server_url, www_authenticate).await?;
    let (listener, redirect_uri) = bind_callback().await?;

    // Obtain a client id: a caller-provided one, else dynamic registration.
    let (client_id, client_secret) = match preset_client_id {
        Some(id) => (id, None),
        None => {
            let reg_endpoint = discovered
                .meta
                .registration_endpoint
                .clone()
                .ok_or_else(|| {
                    err("server needs a pre-registered client — pass one with --client-id")
                })?;
            let reg = register_client(&reg_endpoint, &redirect_uri).await?;
            (reg.client_id, reg.client_secret)
        }
    };

    let pkce = Pkce::generate()?;
    let state = random_token(16)?;
    let url = authorize_url(
        &discovered.meta.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &state,
        &pkce.challenge,
        &discovered.resource,
        discovered.scope.as_deref(),
    )?;

    open(&url);

    // Wait for the redirect (bounded — the user may never finish).
    let code = tokio::time::timeout(Duration::from_secs(300), await_code(listener, &state))
        .await
        .map_err(|_| err("timed out waiting for the browser authorization"))??;

    // Exchange the code for tokens, bound to this resource.
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", pkce.verifier.as_str()),
        ("resource", discovered.resource.as_str()),
    ];
    if let Some(secret) = &client_secret {
        form.push(("client_secret", secret.as_str()));
    }
    let token = token_request(&discovered.meta.token_endpoint, &form).await?;
    Ok(session_from(
        token,
        &discovered.meta.token_endpoint,
        &client_id,
        client_secret.as_deref(),
        &discovered.resource,
        now,
        None,
    ))
}

/// Open a URL in the user's default browser (best-effort, non-blocking).
fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program).args(args).spawn();
}

fn snippet(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = Pkce::generate().unwrap();
        assert!(p.verifier.len() >= 43, "verifier within RFC 7636 length");
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        // No padding / URL-unsafe chars.
        assert!(
            !p.challenge.contains('=') && !p.challenge.contains('+') && !p.challenge.contains('/')
        );
    }

    #[test]
    fn parses_resource_metadata_from_www_authenticate() {
        let h = r#"Bearer error="invalid_token", resource_metadata="https://s/.well-known/oauth-protected-resource""#;
        assert_eq!(
            resource_metadata_url(h).as_deref(),
            Some("https://s/.well-known/oauth-protected-resource")
        );
        assert_eq!(resource_metadata_url("Bearer realm=\"x\"").as_deref(), None);
    }

    #[test]
    fn builds_authorize_url_with_all_params() {
        let u = authorize_url(
            "https://auth.example.com/authorize",
            "client123",
            "http://127.0.0.1:9000/callback",
            "st4te",
            "chal",
            "https://mcp.example.com",
            Some("read write"),
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "client123");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["resource"], "https://mcp.example.com");
        assert_eq!(q["scope"], "read write");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:9000/callback");
    }

    #[test]
    fn parses_callback_query() {
        let (code, state) = parse_callback_query("/callback?code=abc123&state=xyz");
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("xyz"));
        let (code, _) = parse_callback_query("/callback?error=access_denied");
        assert_eq!(code, None);
    }

    #[test]
    fn percent_decodes_callback_values() {
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn origin_strips_path() {
        assert_eq!(
            origin("https://api.example.com/mcp/v1"),
            "https://api.example.com"
        );
        assert_eq!(origin("http://127.0.0.1:8080/x"), "http://127.0.0.1:8080");
    }

    #[tokio::test]
    async fn full_login_flow_against_mock_idp() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let server_url = format!("{base}/mcp");

        // A mock server for the discovery/register/authorize/token endpoints.
        // `/authorize` 302-redirects to the local callback with a code (as a
        // real IdP would after the user approves).
        let b = base.clone();
        let mock = std::thread::spawn(move || {
            for _ in 0..5 {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = req.lines().next().unwrap_or("").to_string();
                let json = |s: &mut std::net::TcpStream, body: String| {
                    let r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    s.write_all(r.as_bytes()).unwrap();
                    s.flush().unwrap();
                };
                if line.contains("/.well-known/oauth-protected-resource") {
                    json(
                        &mut s,
                        format!(r#"{{"resource":"{b}","authorization_servers":["{b}"]}}"#),
                    );
                } else if line.contains("/.well-known/oauth-authorization-server") {
                    json(
                        &mut s,
                        format!(
                            r#"{{"authorization_endpoint":"{b}/authorize","token_endpoint":"{b}/token","registration_endpoint":"{b}/register"}}"#
                        ),
                    );
                } else if line.starts_with("POST /register") {
                    json(&mut s, r#"{"client_id":"client-xyz"}"#.to_string());
                } else if line.starts_with("GET /authorize") {
                    let target = line.split_whitespace().nth(1).unwrap_or("");
                    let q: std::collections::HashMap<String, String> = target
                        .split_once('?')
                        .map(|(_, q)| q)
                        .unwrap_or("")
                        .split('&')
                        .filter_map(|p| p.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    let redirect =
                        percent_decode(q.get("redirect_uri").map(|s| s.as_str()).unwrap_or(""));
                    let state = q.get("state").cloned().unwrap_or_default();
                    let loc = format!("{redirect}?code=test-code&state={state}");
                    let r = format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    s.write_all(r.as_bytes()).unwrap();
                    s.flush().unwrap();
                } else if line.starts_with("POST /token") {
                    json(
                        &mut s,
                        r#"{"access_token":"acc-token","refresh_token":"ref-token","expires_in":3600}"#.to_string(),
                    );
                }
            }
        });

        // The "browser": follow the authorize URL; the 302 lands on the local
        // callback, completing the flow.
        let opener = |url: &str| {
            let url = url.to_string();
            tokio::spawn(async move {
                let _ = reqwest::Client::new().get(&url).send().await;
            });
        };

        let session = login_inner(&server_url, None, None, 1000, opener)
            .await
            .unwrap();
        let _ = mock.join();

        assert_eq!(session.access_token, "acc-token");
        assert_eq!(session.refresh_token.as_deref(), Some("ref-token"));
        assert_eq!(session.client_id, "client-xyz");
        assert_eq!(session.resource, base);
        assert_eq!(session.expires_at, Some(1000 + 3600));
    }

    #[test]
    fn expiry_uses_refresh_margin() {
        let s = OAuthSession {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(1000),
            token_endpoint: "t".into(),
            client_id: "c".into(),
            client_secret: None,
            resource: "r".into(),
            scope: None,
        };
        assert!(!s.is_expired(900)); // 900+60 < 1000
        assert!(s.is_expired(950)); // 950+60 >= 1000
    }
}
