use super::*;
use futures_util::{SinkExt, StreamExt};
use std::{
    future::Future,
    net::{Ipv4Addr, Ipv6Addr},
};
use tokio::net::TcpListener;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::server::{Request as ServerRequest, Response as ServerResponse},
        http::HeaderValue,
    },
};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("loopback test timed out")
}

async fn listener() -> TcpListener {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap()
}

fn proxy(listener: &TcpListener, scheme: &str, credentials: &str) -> ProxyConfig {
    ProxyConfig::parse(&format!(
        "{scheme}://{credentials}{}",
        listener.local_addr().unwrap()
    ))
    .unwrap()
}

async fn socks_greeting(socket: &mut TcpStream, credentials: Option<(&str, &str)>, accept: bool) {
    assert_eq!(socket.read_u8().await.unwrap(), 5);
    let mut methods = vec![0; socket.read_u8().await.unwrap() as usize];
    socket.read_exact(&mut methods).await.unwrap();
    let method = if credentials.is_some() { 2 } else { 0 };
    assert!(methods.contains(&method));
    socket.write_all(&[5, method]).await.unwrap();
    if let Some((username, password)) = credentials {
        assert_eq!(socket.read_u8().await.unwrap(), 1);
        let mut user = vec![0; socket.read_u8().await.unwrap() as usize];
        socket.read_exact(&mut user).await.unwrap();
        let mut pass = vec![0; socket.read_u8().await.unwrap() as usize];
        socket.read_exact(&mut pass).await.unwrap();
        assert_eq!(user, username.as_bytes());
        assert_eq!(pass, password.as_bytes());
        socket
            .write_all(&[1, if accept { 0 } else { 1 }])
            .await
            .unwrap();
    }
}

async fn socks_target(socket: &mut TcpStream) -> (u8, String, u16) {
    let mut prefix = [0; 3];
    socket.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix, [5, 1, 0]);
    let kind = socket.read_u8().await.unwrap();
    let host = match kind {
        1 => {
            let mut raw = [0; 4];
            socket.read_exact(&mut raw).await.unwrap();
            Ipv4Addr::from(raw).to_string()
        }
        4 => {
            let mut raw = [0; 16];
            socket.read_exact(&mut raw).await.unwrap();
            Ipv6Addr::from(raw).to_string()
        }
        3 => {
            let mut raw = vec![0; socket.read_u8().await.unwrap() as usize];
            socket.read_exact(&mut raw).await.unwrap();
            String::from_utf8(raw).unwrap()
        }
        _ => panic!("invalid SOCKS address type"),
    };
    let port = socket.read_u16().await.unwrap();
    (kind, host, port)
}

async fn socks_ok(socket: &mut TcpStream) {
    socket
        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
        .await
        .unwrap();
}

#[test]
fn proxy_urls_are_validated_and_credentials_are_redacted() {
    for scheme in ["http", "socks5", "socks5h"] {
        let config =
            ProxyConfig::parse(&format!("{scheme}://user:p%40ss%3Aword@localhost:7890")).unwrap();
        let credentials = config.credentials.as_ref().unwrap();
        assert_eq!(credentials.username.as_str(), "user");
        assert_eq!(credentials.password.as_str(), "p@ss:word");
        for printed in [format!("{config:?}"), config.to_string()] {
            assert!(!printed.contains("user"));
            assert!(!printed.contains("word"));
            assert!(!printed.contains("p%40ss"));
        }
    }
    assert_eq!(ProxyConfig::parse("http://localhost").unwrap().port(), 80);
    assert_eq!(ProxyConfig::parse("socks5h://[::1]").unwrap().port(), 1080);
    assert_eq!(ProxyConfig::parse("socks5h://[::1]").unwrap().host(), "::1");
    for invalid in [
        "https://localhost:7890",
        "socks4://localhost:7890",
        "http://localhost/path",
        "http://localhost?password=secret",
        "http://localhost#secret",
        "http://localhost:0",
        "http://user:se%zzcret@localhost",
        "http://user:secret word@localhost",
        "socks5://user:@localhost",
        "http://user%3Aname:secret@localhost",
        "http://",
    ] {
        let error = ProxyConfig::parse(invalid).unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn direct_tcp_works_without_proxy() {
    bounded(async {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"direct").await.unwrap();
        });
        let mut socket = connect_tcp("127.0.0.1", address.port(), None, CONNECT_TIMEOUT)
            .await
            .unwrap();
        let mut bytes = [0; 6];
        socket.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"direct");
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn http_connect_auth_and_fragmented_headers_preserve_tunnel_bytes() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "http", "user:p%40ss%3Aword@");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request =
                String::from_utf8(read_connect_header(&mut socket).await.unwrap()).unwrap();
            assert!(request.starts_with("CONNECT origin.invalid:443 HTTP/1.1\r\n"));
            assert!(request.contains("Host: origin.invalid:443\r\n"));
            assert!(request.contains(&format!(
                "Proxy-Authorization: Basic {}\r\n",
                STANDARD.encode("user:p@ss:word")
            )));
            socket
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Con")
                .await
                .unwrap();
            tokio::task::yield_now().await;
            socket
                .write_all(b"nection established\r\n\r\ntunnel")
                .await
                .unwrap();
        });
        // .invalid can only succeed if the proxy, not this process, receives the hostname.
        let mut socket = connect_tcp("origin.invalid", 443, Some(&proxy), CONNECT_TIMEOUT)
            .await
            .unwrap();
        let mut bytes = [0; 6];
        socket.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"tunnel");
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn rejected_http_connect_never_falls_back_to_origin() {
    bounded(async {
        let origin = listener().await;
        let origin_port = origin.local_addr().unwrap().port();
        let listener = listener().await;
        let proxy = proxy(&listener, "http", "user:password@");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_connect_header(&mut socket).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });
        let error = connect_tcp("127.0.0.1", origin_port, Some(&proxy), CONNECT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::ProxyHttp(407)));
        assert!(!format!("{error:?}").contains("password"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), origin.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn http_connect_headers_and_wait_are_bounded() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "http", "");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_connect_header(&mut socket).await.unwrap();
            let _ = socket.write_all(&vec![b'x'; MAX_CONNECT_HEADER + 1]).await;
        });
        assert!(matches!(
            connect_tcp("origin.invalid", 443, Some(&proxy), CONNECT_TIMEOUT).await,
            Err(Error::Proxy(_))
        ));
        server.await.unwrap();

        let listener = self::listener().await;
        let proxy = self::proxy(&listener, "http", "");
        let (release, hold) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            let _ = hold.await;
        });
        assert!(matches!(
            connect_tcp(
                "origin.invalid",
                443,
                Some(&proxy),
                Duration::from_millis(100)
            )
            .await,
            Err(Error::Timeout(_))
        ));
        let _ = release.send(());
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn socks5h_sends_hostname_and_percent_decoded_password_to_proxy() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "socks5h", "user:p%40ss%3Aword@");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socks_greeting(&mut socket, Some(("user", "p@ss:word")), true).await;
            assert_eq!(
                socks_target(&mut socket).await,
                (3, "origin.invalid".to_string(), 443)
            );
            socks_ok(&mut socket).await;
            socket.write_all(b"socks").await.unwrap();
        });
        let mut socket = connect_tcp("origin.invalid", 443, Some(&proxy), CONNECT_TIMEOUT)
            .await
            .unwrap();
        let mut bytes = [0; 5];
        socket.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"socks");
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn socks5_resolves_hostname_locally_and_sends_ip_address() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "socks5", "");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socks_greeting(&mut socket, None, true).await;
            let (kind, host, port) = socks_target(&mut socket).await;
            assert!(matches!(kind, 1 | 4));
            assert!(host.parse::<IpAddr>().unwrap().is_loopback());
            assert_eq!(port, 443);
            socks_ok(&mut socket).await;
        });
        connect_tcp("localhost", 443, Some(&proxy), CONNECT_TIMEOUT)
            .await
            .unwrap();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn socks_auth_failure_is_reported_without_credentials() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "socks5h", "user:secret@");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socks_greeting(&mut socket, Some(("user", "secret")), false).await;
        });
        let error = connect_tcp("origin.invalid", 443, Some(&proxy), CONNECT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Proxy("SOCKS5 authentication rejected")
        ));
        assert!(!format!("{error:?}").contains("secret"));
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn reqwest_https_uses_authenticated_connect_to_http_proxy() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "http", "us%2565r:p%40ss%2540@");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request =
                String::from_utf8(read_connect_header(&mut socket).await.unwrap()).unwrap();
            assert!(request.starts_with("CONNECT origin.invalid:443 HTTP/1.1\r\n"));
            assert!(
                request.to_ascii_lowercase().contains(
                    &format!(
                        "proxy-authorization: basic {}",
                        STANDARD.encode("us%65r:p@ss%40")
                    )
                    .to_ascii_lowercase()
                )
            );
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            // TLS must still start inside the tunnel instead of sending plaintext HTTP.
            assert_eq!(socket.read_u8().await.unwrap(), 0x16);
        });
        let client = http_client(Some(&proxy), Duration::from_secs(2)).unwrap();
        assert!(client.get("https://origin.invalid/").send().await.is_err());
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn reqwest_socks5h_auth_and_remote_dns_complete_an_http_request() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "socks5h", "us%2565r:p%40ss%2540@");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socks_greeting(&mut socket, Some(("us%65r", "p@ss%40")), true).await;
            assert_eq!(
                socks_target(&mut socket).await,
                (3, "origin.invalid".to_string(), 80)
            );
            socks_ok(&mut socket).await;
            let request =
                String::from_utf8(read_connect_header(&mut socket).await.unwrap()).unwrap();
            assert!(request.starts_with("GET /sample HTTP/1.1\r\n"));
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let client = http_client(Some(&proxy), Duration::from_secs(2)).unwrap();
        let response = client
            .get("http://origin.invalid/sample")
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn websocket_upgrade_uses_tunnel_and_keeps_origin_and_subprotocol() {
    bounded(async {
        let listener = listener().await;
        let proxy = proxy(&listener, "http", "");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request =
                String::from_utf8(read_connect_header(&mut socket).await.unwrap()).unwrap();
            assert!(request.starts_with("CONNECT origin.invalid:80 HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |request: &ServerRequest, mut response: ServerResponse| {
                    assert_eq!(request.headers().get("Host").unwrap(), "origin.invalid");
                    assert_eq!(
                        request.headers().get("Sec-WebSocket-Protocol").unwrap(),
                        "wxws_pb"
                    );
                    response.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        HeaderValue::from_static("wxws_pb"),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            websocket
                .send(Message::Binary(b"via-proxy".to_vec()))
                .await
                .unwrap();
        });
        let mut request = "ws://origin.invalid/".into_client_request().unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static("wxws_pb"),
        );
        let (mut websocket, response) = connect_websocket(request, Some(&proxy)).await.unwrap();
        assert_eq!(response.status(), 101);
        assert_eq!(
            websocket.next().await.unwrap().unwrap().into_data(),
            b"via-proxy"
        );
        server.await.unwrap();
    })
    .await;
}
