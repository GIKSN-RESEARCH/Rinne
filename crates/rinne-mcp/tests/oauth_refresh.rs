//! `oauth::refresh` posts the right grant to the token endpoint and parses the
//! new tokens — exercised against a raw-TCP mock so no real IdP is needed.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use rinne_mcp::{oauth, OAuthSession};

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

#[tokio::test]
async fn refresh_exchanges_the_refresh_token() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || {
        let mut s = listener.incoming().next().unwrap().unwrap();
        let req = read_request(&mut s);
        // The refresh grant and the resource indicator must be present.
        assert!(req.contains("grant_type=refresh_token"), "req: {req}");
        assert!(req.contains("refresh_token=old-refresh"));
        assert!(req.contains("resource=https"));
        let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"token_type":"Bearer"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).unwrap();
        s.flush().unwrap();
    });

    let session = OAuthSession {
        access_token: "old-access".into(),
        refresh_token: Some("old-refresh".into()),
        expires_at: Some(0),
        token_endpoint: format!("http://127.0.0.1:{port}/token"),
        client_id: "client-1".into(),
        client_secret: None,
        resource: "https://mcp.example.com".into(),
        scope: None,
    };

    let refreshed = oauth::refresh(&session, 1000).await.unwrap();
    server.join().unwrap();

    assert_eq!(refreshed.access_token, "new-access");
    assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(refreshed.expires_at, Some(1000 + 3600));
    assert!(!refreshed.is_expired(1000));
}
