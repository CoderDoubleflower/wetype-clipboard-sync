//! Real loopback HTTP CONNECT / SOCKS tunnels carrying certificate-verified TLS
//! and binary COS PUT/GET bodies. No DNS or service traffic leaves this process.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    ssl::{SslAcceptor, SslMethod},
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
    },
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use wx_ime_protocol::{clipboard::ImageReference, crypto::ImageKey};

const HOST: &str = "clipboard-loopback.invalid.myqcloud.com";
const OBJECT: &str = "https://clipboard-loopback.invalid.myqcloud.com/image?q-signature=local-test";

struct LocalDns(Arc<AtomicUsize>);
impl Resolve for LocalDns {
    fn resolve(&self, name: Name) -> Resolving {
        // A strict synthetic resolver makes even a routing regression incapable
        // of contacting a real host. SOCKS5h/CONNECT should never resolve HOST.
        assert!(matches!(name.as_str(), HOST | "127.0.0.1"));
        if name.as_str() == HOST {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        let addresses: Addrs =
            Box::new(std::iter::once(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))));
        Box::pin(async { Ok(addresses) })
    }
}

fn certificate(name: &str, key: &PKey<Private>, issuer: Option<(&X509, &PKey<Private>)>) -> X509 {
    let mut subject = X509NameBuilder::new().unwrap();
    subject.append_entry_by_text("CN", name).unwrap();
    let subject = subject.build();
    let mut certificate = X509::builder().unwrap();
    certificate.set_version(2).unwrap();
    certificate
        .set_serial_number(
            &BigNum::from_u32(if issuer.is_some() { 2 } else { 1 })
                .unwrap()
                .to_asn1_integer()
                .unwrap(),
        )
        .unwrap();
    certificate.set_subject_name(&subject).unwrap();
    certificate
        .set_issuer_name(issuer.map_or(subject.as_ref(), |(ca, _)| ca.subject_name()))
        .unwrap();
    certificate.set_pubkey(key).unwrap();
    certificate
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    certificate
        .set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    if let Some((ca, _)) = issuer {
        certificate
            .append_extension(BasicConstraints::new().critical().build().unwrap())
            .unwrap();
        certificate
            .append_extension(KeyUsage::new().digital_signature().build().unwrap())
            .unwrap();
        certificate
            .append_extension(ExtendedKeyUsage::new().server_auth().build().unwrap())
            .unwrap();
        let alternative = SubjectAlternativeName::new()
            .dns(name)
            .build(&certificate.x509v3_context(Some(ca), None))
            .unwrap();
        certificate.append_extension(alternative).unwrap();
    } else {
        certificate
            .append_extension(BasicConstraints::new().critical().ca().build().unwrap())
            .unwrap();
        certificate
            .append_extension(
                KeyUsage::new()
                    .critical()
                    .key_cert_sign()
                    .crl_sign()
                    .build()
                    .unwrap(),
            )
            .unwrap();
    }
    certificate
        .sign(issuer.map_or(key, |(_, key)| key), MessageDigest::sha256())
        .unwrap();
    certificate.build()
}

fn local_tls() -> (SslAcceptor, reqwest::Certificate) {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let root_key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let root = certificate("COS loopback test CA", &root_key, None);
    let server_key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let server = certificate(HOST, &server_key, Some((&root, &root_key)));
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&server_key).unwrap();
    acceptor.set_certificate(&server).unwrap();
    acceptor.check_private_key().unwrap();
    (
        acceptor.build(),
        reqwest::Certificate::from_pem(&root.to_pem().unwrap()).unwrap(),
    )
}

fn listener() -> TcpListener {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "proxy connection timed out");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("proxy accept failed: {error}"),
        }
    }
}

fn header(socket: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16 * 1024);
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn byte(socket: &mut impl Read) -> u8 {
    let mut value = [0];
    socket.read_exact(&mut value).unwrap();
    value[0]
}

fn proxy_handshake(socket: &mut TcpStream, scheme: &str, authenticated: bool) {
    if scheme == "http" {
        let request = header(socket);
        assert!(request.starts_with(&format!("CONNECT {HOST}:443 HTTP/1.1\r\n")));
        let authorization = request
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("proxy-authorization")
                    .then(|| value.trim())
            })
            .unwrap();
        assert_eq!(
            authorization,
            format!("Basic {}", STANDARD.encode("us%65r:p@ss%40"))
        );
        socket
            .write_all(if authenticated {
                b"HTTP/1.1 200 Connection established\r\n\r\n"
            } else {
                b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n"
            })
            .unwrap();
        return;
    }
    assert_eq!(byte(socket), 5);
    let mut methods = vec![0; byte(socket) as usize];
    socket.read_exact(&mut methods).unwrap();
    assert!(methods.contains(&2));
    socket.write_all(&[5, 2]).unwrap();
    assert_eq!(byte(socket), 1);
    let mut user = vec![0; byte(socket) as usize];
    socket.read_exact(&mut user).unwrap();
    let mut password = vec![0; byte(socket) as usize];
    socket.read_exact(&mut password).unwrap();
    assert_eq!(user, b"us%65r");
    assert_eq!(password, b"p@ss%40");
    socket.write_all(&[1, u8::from(!authenticated)]).unwrap();
    if !authenticated {
        return;
    }
    let mut prefix = [0; 3];
    socket.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix, [5, 1, 0]);
    if scheme == "socks5h" {
        assert_eq!(byte(socket), 3);
        let mut name = vec![0; byte(socket) as usize];
        socket.read_exact(&mut name).unwrap();
        assert_eq!(name, HOST.as_bytes());
    } else {
        assert_eq!(byte(socket), 1);
        let mut address = [0; 4];
        socket.read_exact(&mut address).unwrap();
        assert_eq!(address, Ipv4Addr::LOCALHOST.octets());
    }
    let mut port = [0; 2];
    socket.read_exact(&mut port).unwrap();
    assert_eq!(u16::from_be_bytes(port), 443);
    socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
}

fn proxy(listener: &TcpListener, scheme: &str) -> ProxyConfig {
    ProxyConfig::parse(&format!(
        "{scheme}://us%2565r:p%40ss%2540@{}",
        listener.local_addr().unwrap()
    ))
    .unwrap()
}

async fn image_roundtrip(scheme: &'static str) {
    let listener = listener();
    let proxy = proxy(&listener, scheme);
    let (tls, root) = local_tls();
    let dns_calls = Arc::new(AtomicUsize::new(0));
    let cos = Cos {
        http: network::http_client_builder(Some(&proxy), Duration::from_secs(5))
            .unwrap()
            .add_root_certificate(root)
            .dns_resolver(Arc::new(LocalDns(dns_calls.clone())))
            .build()
            .unwrap(),
        capture: None,
    };
    let plain = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a9V0AAAAASUVORK5CYII=").unwrap();
    let key = ImageKey::new([0x72; 16]);
    let encrypted = key.apply(&plain).unwrap();
    assert_ne!(encrypted, plain);
    let expected = encrypted.clone();
    let server = tokio::task::spawn_blocking(move || {
        let mut stored = Vec::new();
        for index in 0..3 {
            let mut socket = accept(&listener);
            proxy_handshake(&mut socket, scheme, true);
            let mut socket = tls.accept(socket).unwrap();
            let request = header(&mut socket);
            // Proxy credentials belong outside the TLS origin request.
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            if index == 0 {
                assert!(request.starts_with("PUT /image?q-signature=local-test HTTP/1.1\r\n"));
                let length: usize = request
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                stored.resize(length, 0);
                socket.read_exact(&mut stored).unwrap();
                assert_eq!(stored, expected);
                socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                assert!(request.starts_with("GET /image?q-signature=local-test HTTP/1.1\r\n"));
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            stored.len()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                // The final request deliberately supplies a smaller receive
                // limit, so its client may close immediately after the headers.
                if index == 1 {
                    for fragment in stored.chunks(7) {
                        socket.write_all(fragment).unwrap();
                    }
                } else {
                    let _ = socket.write_all(&stored);
                }
            }
        }
    });
    let url = CosUrl::parse(OBJECT).unwrap();
    cos.put(&url, encrypted).await.unwrap();
    // Round-trip the actual wire reference, not an in-memory shortcut.
    let reference = ImageReference::new(&plain, url, key).unwrap();
    let received_reference = ImageReference::decode(&reference.encode().unwrap()).unwrap();
    let mut downloaded = cos
        .get(&received_reference.url, MAX_IMAGE_BYTES)
        .await
        .unwrap();
    assert_eq!(received_reference.decrypt(&downloaded).unwrap(), plain);
    downloaded[0] ^= 0x80;
    assert!(received_reference.decrypt(&downloaded).is_err());
    assert!(matches!(
        cos.get(&received_reference.url, plain.len() - 1).await,
        Err(Error::Invalid("COS download exceeds limit"))
    ));
    server.await.unwrap();
    if scheme == "socks5" {
        assert!(dns_calls.load(Ordering::SeqCst) > 0);
    } else {
        assert_eq!(dns_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn binary_image_roundtrip_uses_http_connect_and_verified_tls() {
    tokio::time::timeout(Duration::from_secs(15), image_roundtrip("http"))
        .await
        .unwrap();
}

#[tokio::test]
async fn binary_image_roundtrip_uses_socks5_local_dns_and_verified_tls() {
    tokio::time::timeout(Duration::from_secs(15), image_roundtrip("socks5"))
        .await
        .unwrap();
}

#[tokio::test]
async fn binary_image_roundtrip_uses_socks5h_remote_dns_and_verified_tls() {
    tokio::time::timeout(Duration::from_secs(15), image_roundtrip("socks5h"))
        .await
        .unwrap();
}

#[tokio::test]
async fn rejected_cos_proxy_auth_never_resolves_or_connects_to_the_origin() {
    for scheme in ["http", "socks5h"] {
        let listener = listener();
        let proxy = proxy(&listener, scheme);
        let dns_calls = Arc::new(AtomicUsize::new(0));
        let cos = Cos {
            http: network::http_client_builder(Some(&proxy), Duration::from_secs(2))
                .unwrap()
                .dns_resolver(Arc::new(LocalDns(dns_calls.clone())))
                .build()
                .unwrap(),
            capture: None,
        };
        let server = tokio::task::spawn_blocking(move || {
            for _ in 0..2 {
                proxy_handshake(&mut accept(&listener), scheme, false);
            }
        });
        let url = CosUrl::parse(OBJECT).unwrap();
        for error in [
            cos.put(&url, vec![0, 255, 128]).await.unwrap_err(),
            cos.get(&url, MAX_IMAGE_BYTES).await.unwrap_err(),
        ] {
            assert!(matches!(error, Error::CosTransport));
            assert!(!format!("{error:?}").contains("p@ss"));
        }
        assert_eq!(
            dns_calls.load(Ordering::SeqCst),
            0,
            "proxy rejection attempted direct routing"
        );
        server.await.unwrap();
    }
}
