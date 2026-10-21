use super::*;
use crate::{Client, client::Inner};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

struct Request {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct Step {
    path: &'static str,
    // None simulates a lost response after the service received the request.
    response: Option<(u16, Vec<u8>)>,
}

fn step(path: &'static str, status: u16, body: Vec<u8>) -> Step {
    Step {
        path,
        response: Some((status, body)),
    }
}

async fn read_request(socket: &mut TcpStream) -> Request {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        assert!(raw.len() < 16 * 1024, "test request headers too large");
        raw.push(socket.read_u8().await.unwrap());
    }
    let header = String::from_utf8(raw).unwrap();
    let mut lines = header.split("\r\n");
    let first = lines.next().unwrap();
    assert!(first.starts_with("POST "));
    let path = first.split_whitespace().nth(1).unwrap().to_string();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    assert!(!headers.contains_key("transfer-encoding"));
    let length: usize = headers
        .get("content-length")
        .map(|value| value.parse().unwrap())
        .unwrap_or(0);
    assert!(length < 2 * 1024 * 1024);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    Request {
        path,
        headers,
        body,
    }
}

async fn service(steps: Vec<Step>) -> (String, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let mut requests = Vec::new();
            for step in steps {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                assert_eq!(request.path, step.path);
                requests.push(request);
                if let Some((status, body)) = step.response {
                    let header = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nKb-CompressionType: 1\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(header.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
            }
            // Catch an unbounded recovery or a duplicate publication while the
            // listener is still live, instead of hiding it as connection refused.
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "unexpected additional request"
            );
            requests
        })
        .await
        .expect("control session test timed out")
    });
    (host, server)
}

fn session() -> (tempfile::TempDir, Session, ControlKey) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned(), "session-test".into()).unwrap();
    store.state.uin = 17;
    store.state.consumed.insert(8, 12);
    let old_server = IdentityKey::generate().unwrap();
    store.state.server_key = old_server.public_hex().unwrap();
    store.save().unwrap();
    let mut session = Session::offline_for_test(store).unwrap();
    let key = session
        .identity
        .agree(&session.store.state.server_key)
        .unwrap();
    session.key = Some(key.clone());
    (directory, session, key)
}

fn timestamp(milliseconds: i64) -> Step {
    step(
        "/timestamp",
        200,
        wire::Timestamp {
            milliseconds: Some(milliseconds as u64),
        }
        .encode_to_vec(),
    )
}

fn handshake(server: &IdentityKey) -> Step {
    step(
        "/oauth_pubkey_v2",
        200,
        ControlKey::bootstrap()
            .seal(
                &wire::EcdhResponse {
                    status: Some(0),
                    public_key: Some(server.public_hex().unwrap()),
                    ..Default::default()
                }
                .encode_to_vec(),
            )
            .unwrap(),
    )
}

fn group(key: &ControlKey) -> Step {
    let group = wire::GroupResponse {
        status: Some(0),
        group: Some(wire::Group {
            group_id: Some(8),
            devices: vec![
                wire::BoundInfo {
                    uin: Some(17),
                    ..Default::default()
                },
                wire::BoundInfo {
                    uin: Some(18),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    step(
        "/api_v2",
        200,
        key.seal(
            &wire::BaseResponse {
                field2: Some(0),
                body: Some(group.encode_to_vec()),
            }
            .encode_to_vec(),
        )
        .unwrap(),
    )
}

fn error(path: &'static str, status: u16) -> Step {
    step(
        path,
        status,
        b"service or gateway rejected request".to_vec(),
    )
}

#[tokio::test]
async fn http_400_refreshes_read_with_new_key_timestamp_and_task_without_changing_identity() {
    let (_directory, mut session, old_key) = session();
    let mut before = serde_json::to_value(&session.store.state).unwrap();
    let new_server = IdentityKey::generate().unwrap();
    let key = new_server
        .agree(&session.identity.public_hex().unwrap())
        .unwrap();
    let stamp = now().unwrap() + 60_000;
    let (host, server) = service(vec![
        error("/api_v2", 400),
        timestamp(stamp),
        handshake(&new_server),
        group(&key),
    ])
    .await;
    session.control_host = Some(host);

    assert_eq!(session.group(1).await.unwrap().unwrap().group_id, Some(8));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    let original =
        wire::BaseRequest::decode(old_key.open(&requests[0].body).unwrap().as_slice()).unwrap();
    let retried =
        wire::BaseRequest::decode(key.open(&requests[3].body).unwrap().as_slice()).unwrap();
    assert_eq!(original.body, retried.body);
    assert_eq!(original.subcommand, Some(transport::GROUP));
    assert_eq!(retried.subcommand, original.subcommand);
    assert_ne!(
        requests[0].headers["kb-taskid"],
        requests[3].headers["kb-taskid"]
    );
    assert_ne!(
        requests[0].headers["kb-traceid"],
        requests[3].headers["kb-traceid"]
    );
    let original_time: i64 = requests[0].headers["kb-timestamp"].parse().unwrap();
    let retried_time: i64 = requests[3].headers["kb-timestamp"].parse().unwrap();
    assert!(retried_time - original_time > 50_000);
    assert_ne!(original.header, retried.header);
    let ecdh = wire::EcdhRequest::decode(
        ControlKey::bootstrap()
            .open(&requests[2].body)
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(ecdh.uin, Some(17));
    assert_eq!(ecdh.need_uin, Some(false));
    assert_eq!(
        ecdh.public_key,
        Some(session.identity.public_hex().unwrap())
    );
    assert_eq!(
        ecdh.previous_server_key.as_deref(),
        before["server_key"].as_str()
    );
    before["server_key"] = new_server.public_hex().unwrap().into();
    assert_eq!(serde_json::to_value(&session.store.state).unwrap(), before);
}

#[tokio::test]
async fn persistent_http_400_refreshes_only_once_and_leaves_later_retry_possible() {
    let (_directory, mut session, _) = session();
    let new_server = IdentityKey::generate().unwrap();
    let (host, server) = service(vec![
        error("/api_v2", 400),
        timestamp(now().unwrap()),
        handshake(&new_server),
        error("/api_v2", 400),
    ])
    .await;
    session.control_host = Some(host);
    assert!(matches!(session.group(1).await, Err(Error::Http(400))));
    assert!(session.key.is_none());
    assert_eq!(server.await.unwrap().len(), 4);
}

#[tokio::test]
async fn failed_refresh_is_reinitialized_by_a_later_call() {
    let (_directory, mut session, _) = session();
    let new_server = IdentityKey::generate().unwrap();
    let key = new_server
        .agree(&session.identity.public_hex().unwrap())
        .unwrap();
    let (host, server) = service(vec![
        error("/api_v2", 400),
        timestamp(now().unwrap()),
        error("/oauth_pubkey_v2", 503),
        timestamp(now().unwrap()),
        handshake(&new_server),
        group(&key),
    ])
    .await;
    session.control_host = Some(host);
    assert!(matches!(session.group(1).await, Err(Error::Http(503))));
    assert!(session.key.is_none());
    assert!(session.group(1).await.unwrap().is_some());
    assert!(session.key.is_some());
    assert_eq!(server.await.unwrap().len(), 6);
}

#[tokio::test]
async fn lazy_recovery_does_not_start_a_second_handshake_on_http_400() {
    let (_directory, mut session, _) = session();
    session.key = None;
    let new_server = IdentityKey::generate().unwrap();
    let (host, server) = service(vec![
        timestamp(now().unwrap()),
        handshake(&new_server),
        error("/api_v2", 400),
    ])
    .await;
    session.control_host = Some(host);
    assert!(matches!(session.group(1).await, Err(Error::Http(400))));
    assert!(session.key.is_none());
    assert_eq!(server.await.unwrap().len(), 3);
}

#[tokio::test]
async fn ordinary_http_error_does_not_trigger_an_unrelated_handshake() {
    let (_directory, mut session, _) = session();
    let (host, server) = service(vec![error("/api_v2", 403)]).await;
    session.control_host = Some(host);
    assert!(matches!(session.group(1).await, Err(Error::Http(403))));
    assert!(session.key.is_some());
    assert_eq!(server.await.unwrap().len(), 1);
}

fn client(session: Session) -> Client {
    Client {
        inner: Arc::new(Inner {
            session: Mutex::new(session),
            closed: AtomicBool::new(false),
            lifetime: tokio_util::sync::CancellationToken::new(),
            pairing: AtomicBool::new(false),
            cos: crate::clipboard::cos::Cos::new(None, None).unwrap(),
            proxy: None,
            receive: Mutex::new(crate::clipboard::ReceiveState::default()),
        }),
    }
}

#[tokio::test]
async fn publication_http_400_is_not_replayed_and_remains_outcome_unknown() {
    let (_directory, mut session, key) = session();
    let (host, server) = service(vec![group(&key), group(&key), error("/api_v2", 400)]).await;
    session.control_host = Some(host);
    let client = client(session);
    assert!(matches!(
        client.send_text("latest clipboard").await,
        Err(Error::OutcomeUnknown)
    ));
    assert!(client.session().await.unwrap().key.is_none());
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].headers["kb-subcmdid"],
        transport::SEND_CLIPBOARD.to_string()
    );
}

#[tokio::test]
async fn lost_publication_response_is_not_replayed_and_remains_outcome_unknown() {
    let (_directory, mut session, key) = session();
    let (host, server) = service(vec![
        group(&key),
        group(&key),
        Step {
            path: "/api_v2",
            response: None,
        },
    ])
    .await;
    session.control_host = Some(host);
    let client = client(session);
    assert!(matches!(
        client.send_text("latest clipboard").await,
        Err(Error::OutcomeUnknown)
    ));
    assert_eq!(server.await.unwrap().len(), 3);
}

#[tokio::test]
async fn refresh_keeps_the_configured_http_proxy_for_every_request() {
    let (_directory, mut session, _) = session();
    let new_server = IdentityKey::generate().unwrap();
    let key = new_server
        .agree(&session.identity.public_hex().unwrap())
        .unwrap();
    let mut steps = vec![
        error("/api_v2", 400),
        timestamp(now().unwrap()),
        handshake(&new_server),
        group(&key),
    ];
    for step in &mut steps {
        step.path = match step.path {
            "/timestamp" => "http://control.invalid/timestamp",
            "/oauth_pubkey_v2" => "http://control.invalid/oauth_pubkey_v2",
            "/api_v2" => "http://control.invalid/api_v2",
            _ => unreachable!(),
        };
    }
    let (proxy, server) = service(steps).await;
    let proxy = ProxyConfig::parse(&proxy.replace("http://", "http://test:secret@")).unwrap();
    session.http = network::http_client(Some(&proxy), Duration::from_secs(1)).unwrap();
    // This origin deliberately cannot be reached directly. The same HTTP client
    // must carry the failed request, handshake, and recovered request via proxy.
    session.control_host = Some("http://control.invalid".into());
    assert!(session.group(1).await.unwrap().is_some());
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.headers.contains_key("proxy-authorization"))
    );
}

#[test]
fn replay_is_limited_to_known_safe_commands() {
    for command in [
        transport::GROUP,
        transport::COS_CREDENTIALS,
        transport::FETCH_BIND,
        transport::POLL_CODE,
    ] {
        assert!(can_replay(Endpoint::Api(command)));
    }
    for command in [
        transport::SEND_CLIPBOARD,
        transport::JOIN,
        transport::UNBIND,
        transport::CONFIRM_BIND,
        transport::REQUEST_CODE,
        transport::RELEASE_BIND,
        999,
    ] {
        assert!(!can_replay(Endpoint::Api(command)));
    }
    assert!(!can_replay(Endpoint::Register));
}
