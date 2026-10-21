//! One explicit routing policy for control HTTP, COS and WebSocket traffic.
//! Proxy failures never fall back to a direct connection. Destination TLS still
//! uses the original hostname and normal certificate verification inside a tunnel.

use crate::{Error, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::{fmt, net::IpAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, lookup_host},
};
use tokio_socks::{TargetAddr, tcp::Socks5Stream};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    tungstenite::handshake::client::{Request, Response},
};
use url::Url;
use zeroize::Zeroizing;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WEBSOCKET_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECT_HEADER: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyKind {
    Http,
    Socks5,
    Socks5h,
}

#[derive(Clone)]
struct Credentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

/// An explicitly configured HTTP CONNECT or SOCKS5 proxy.
///
/// Supported URLs: `http://host:port`, `socks5://host:port` (local DNS), and
/// `socks5h://host:port` (proxy DNS), optionally with percent-encoded user info.
/// Debug and Display output omit credentials. System proxy settings and PAC
/// scripts are deliberately not consulted.
#[derive(Clone)]
pub struct ProxyConfig {
    // The endpoint never contains user info, even internally.
    endpoint: Url,
    kind: ProxyKind,
    credentials: Option<Credentials>,
}

impl ProxyConfig {
    pub fn parse(value: &str) -> Result<Self> {
        if value.chars().any(char::is_whitespace) {
            return Err(Error::Invalid(
                "proxy URL contains whitespace; percent-encode credentials",
            ));
        }
        let mut endpoint = Url::parse(value).map_err(|_| Error::Invalid("invalid proxy URL"))?;
        let kind = match endpoint.scheme() {
            "http" => ProxyKind::Http,
            "socks5" => ProxyKind::Socks5,
            "socks5h" => ProxyKind::Socks5h,
            _ => {
                return Err(Error::Invalid(
                    "proxy scheme must be http, socks5 or socks5h",
                ));
            }
        };
        if endpoint.host_str().is_none_or(str::is_empty)
            || !matches!(endpoint.path(), "" | "/")
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.port() == Some(0)
        {
            return Err(Error::Invalid(
                "proxy URL requires a host and valid port, with no path, query or fragment",
            ));
        }
        let credentials = if !endpoint.username().is_empty() || endpoint.password().is_some() {
            let username = decode_credential(endpoint.username())?;
            let password = decode_credential(endpoint.password().unwrap_or_default())?;
            if kind == ProxyKind::Http && username.contains(':') {
                return Err(Error::Invalid(
                    "HTTP proxy username must not contain a colon",
                ));
            }
            if kind != ProxyKind::Http
                && (!(1..=255).contains(&username.len()) || !(1..=255).contains(&password.len()))
            {
                return Err(Error::Invalid(
                    "SOCKS5 username and password must each be 1..255 UTF-8 bytes",
                ));
            }
            Some(Credentials { username, password })
        } else {
            None
        };
        endpoint
            .set_username("")
            .map_err(|_| Error::Invalid("invalid proxy authority"))?;
        endpoint
            .set_password(None)
            .map_err(|_| Error::Invalid("invalid proxy authority"))?;
        Ok(Self {
            endpoint,
            kind,
            credentials,
        })
    }

    fn host(&self) -> &str {
        let host = self.endpoint.host_str().expect("validated proxy host");
        // Url includes brackets for an IPv6 literal; Tokio's tuple address does not.
        host.strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host)
    }

    fn port(&self) -> u16 {
        self.endpoint.port().unwrap_or(match self.kind {
            ProxyKind::Http => 80,
            ProxyKind::Socks5 | ProxyKind::Socks5h => 1080,
        })
    }

    fn reqwest_proxy(&self) -> Result<reqwest::Proxy> {
        let mut endpoint = self.endpoint.clone();
        if let Some(credentials) = &self.credentials {
            // reqwest 0.12's basic_auth calls Url's user-info setters, which
            // preserve '%' escapes. Encode the complete decoded credentials
            // first, including literal '%', so the proxy matcher decodes once.
            let username = Zeroizing::new(
                utf8_percent_encode(&credentials.username, NON_ALPHANUMERIC).to_string(),
            );
            let password = Zeroizing::new(
                utf8_percent_encode(&credentials.password, NON_ALPHANUMERIC).to_string(),
            );
            endpoint
                .set_username(&username)
                .map_err(|_| Error::Invalid("invalid proxy authority"))?;
            endpoint
                .set_password(Some(&password))
                .map_err(|_| Error::Invalid("invalid proxy authority"))?;
        }
        // Proxy::http only matches HTTP destinations, whereas our API/COS use HTTPS.
        reqwest::Proxy::all(endpoint).map_err(|_| Error::Invalid("invalid proxy configuration"))
    }
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("endpoint", &self.endpoint.as_str())
            .field("authenticated", &self.credentials.is_some())
            .finish()
    }
}

impl fmt::Display for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.endpoint.as_str())
    }
}

fn decode_credential(value: &str) -> Result<Zeroizing<String>> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return Err(Error::Invalid(
                "proxy credentials contain invalid percent encoding",
            ));
        }
    }
    percent_decode_str(value)
        .decode_utf8()
        .map(|s| Zeroizing::new(s.into_owned()))
        .map_err(|_| Error::Invalid("proxy credentials must be valid UTF-8"))
}

pub(crate) fn http_client(
    proxy: Option<&ProxyConfig>,
    timeout: Duration,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout);
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy.reqwest_proxy()?);
    }
    builder.build().map_err(|_| Error::Transport)
}

pub(crate) async fn connect_websocket(
    request: Request,
    proxy: Option<&ProxyConfig>,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response)> {
    let host = request
        .uri()
        .host()
        .ok_or(Error::Invalid("WebSocket host missing"))?;
    let host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
        .to_string();
    let default_port = match request.uri().scheme_str() {
        Some("wss") => 443,
        Some("ws") => 80,
        _ => return Err(Error::Invalid("WebSocket scheme must be ws or wss")),
    };
    let port = request.uri().port_u16().unwrap_or(default_port);
    tokio::time::timeout(WEBSOCKET_TIMEOUT, async {
        let socket = connect_tcp(&host, port, proxy, CONNECT_TIMEOUT).await?;
        // Keep the origin URI, including its hostname for SNI/certificate verification.
        client_async_tls_with_config(request, socket, None, None)
            .await
            .map_err(|_| Error::Transport)
    })
    .await
    .map_err(|_| Error::Timeout("WebSocket TLS/upgrade handshake"))?
}

async fn connect_tcp(
    host: &str,
    port: u16,
    proxy: Option<&ProxyConfig>,
    timeout: Duration,
) -> Result<TcpStream> {
    tokio::time::timeout(timeout, async {
        let Some(proxy) = proxy else {
            return TcpStream::connect((host, port))
                .await
                .map_err(|_| Error::Transport);
        };
        match proxy.kind {
            ProxyKind::Http => {
                let mut socket = proxy_socket(proxy).await?;
                http_connect(&mut socket, host, port, proxy.credentials.as_ref()).await?;
                Ok(socket)
            }
            ProxyKind::Socks5 | ProxyKind::Socks5h => socks_connect(host, port, proxy).await,
        }
    })
    .await
    .map_err(|_| Error::Timeout("TCP/proxy connection"))?
}

async fn proxy_socket(proxy: &ProxyConfig) -> Result<TcpStream> {
    TcpStream::connect((proxy.host(), proxy.port()))
        .await
        .map_err(|_| Error::Proxy("cannot connect to proxy server"))
}

async fn socks_connect(host: &str, port: u16, proxy: &ProxyConfig) -> Result<TcpStream> {
    let targets = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![TargetAddr::Ip((ip, port).into())]
    } else if proxy.kind == ProxyKind::Socks5h {
        vec![TargetAddr::Domain(host.to_owned().into(), port)]
    } else {
        // Resolve before creating the SOCKS request so `socks5` and `socks5h`
        // have identical DNS semantics in reqwest and the WebSocket transport.
        lookup_host((host, port))
            .await
            .map_err(|_| Error::Proxy("cannot resolve SOCKS5 destination locally"))?
            .map(TargetAddr::Ip)
            .collect::<Vec<_>>()
    };
    if targets.is_empty() {
        return Err(Error::Proxy("SOCKS5 destination has no IP addresses"));
    }
    let mut last_error = Error::Proxy("SOCKS5 connection failed");
    for target in targets {
        let socket = proxy_socket(proxy).await?;
        let connected = match &proxy.credentials {
            Some(credentials) => {
                Socks5Stream::connect_with_password_and_socket(
                    socket,
                    target,
                    &credentials.username,
                    &credentials.password,
                )
                .await
            }
            None => Socks5Stream::connect_with_socket(socket, target).await,
        };
        match connected {
            Ok(stream) => return Ok(stream.into_inner()),
            Err(
                tokio_socks::Error::PasswordAuthFailure(_)
                | tokio_socks::Error::NoAcceptableAuthMethods
                | tokio_socks::Error::AuthorizationRequired
                | tokio_socks::Error::UnknownAuthMethod,
            ) => {
                return Err(Error::Proxy("SOCKS5 authentication rejected"));
            }
            Err(tokio_socks::Error::ConnectionNotAllowedByRuleset) => {
                return Err(Error::Proxy("SOCKS5 destination denied by proxy rules"));
            }
            Err(_) => {
                last_error = Error::Proxy("SOCKS5 handshake or destination connection failed")
            }
        }
    }
    Err(last_error)
}

async fn http_connect(
    socket: &mut TcpStream,
    host: &str,
    port: u16,
    credentials: Option<&Credentials>,
) -> Result<()> {
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Invalid("invalid CONNECT destination"));
    }
    let authority = if matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = Zeroizing::new(format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"
    ));
    if let Some(credentials) = credentials {
        let plain = Zeroizing::new(format!(
            "{}:{}",
            credentials.username.as_str(),
            credentials.password.as_str()
        ));
        let encoded = Zeroizing::new(STANDARD.encode(plain.as_bytes()));
        request.push_str("Proxy-Authorization: Basic ");
        request.push_str(&encoded);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Proxy("cannot write HTTP CONNECT request"))?;

    // A small bounded header is read without buffering past its terminator; any
    // bytes already sent through the tunnel must remain available to TLS/WS.
    for _ in 0..5 {
        let raw = read_connect_header(socket).await?;
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        if !matches!(response.parse(&raw), Ok(httparse::Status::Complete(_))) {
            return Err(Error::Proxy("malformed HTTP CONNECT response"));
        }
        let status = response
            .code
            .ok_or(Error::Proxy("missing HTTP CONNECT status"))?;
        match status {
            200..=299 => return Ok(()),
            100..=199 if status != 101 => continue,
            _ => return Err(Error::ProxyHttp(status)),
        }
    }
    Err(Error::Proxy("too many interim HTTP CONNECT responses"))
}

async fn read_connect_header(socket: &mut TcpStream) -> Result<Vec<u8>> {
    let mut raw = Vec::with_capacity(256);
    while raw.len() < MAX_CONNECT_HEADER {
        raw.push(
            socket
                .read_u8()
                .await
                .map_err(|_| Error::Proxy("HTTP proxy closed during CONNECT handshake"))?,
        );
        if raw.ends_with(b"\r\n\r\n") {
            return Ok(raw);
        }
    }
    Err(Error::Proxy("HTTP CONNECT response headers exceed 16 KiB"))
}

#[cfg(test)]
mod tests;
