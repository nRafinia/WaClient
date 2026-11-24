//! Built-in proxy support (SOCKS5 and HTTP CONNECT) for all WhatsApp traffic.
//!
//! - The chat WebSocket goes through [`ProxyTransportFactory`]: tunnel to
//!   `web.whatsapp.com:443`, TLS on top, then the WebSocket upgrade.
//! - The library's HTTP client (media, version fetch, link previews, avatars)
//!   is a `ureq` agent configured with the same proxy.
//!
//! Privacy: the proxy address and credentials never reach the log at info or
//! above, and the target host name is always sent to the proxy (SOCKS5
//! `DOMAINNAME`), never resolved locally.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use whatsapp_rust::async_trait;
use whatsapp_rust::http::UreqHttpClient;
use whatsapp_rust::transport::{
    Connector, Transport, TransportEvent, TransportFactory, default_tls_connector, from_websocket,
};
use whatsapp_rust::wacore::net::{WHATSAPP_WEB_ORIGIN, WHATSAPP_WEB_WS_URL};

use crate::api::{ProxyConfig, ProxyKind};

type EventReceiver = whatsapp_rust::async_channel::Receiver<TransportEvent>;

const WA_HOST: &str = "web.whatsapp.com";
const WA_PORT: u16 = 443;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest HTTP CONNECT reply head we accept.
const MAX_CONNECT_REPLY: usize = 8 * 1024;
/// Idle time before the OS starts probing the proxied socket, and the gap
/// between probes. Proxies and NAT boxes drop silent flows; probes keep the
/// flow alive and let the OS notice a dead peer (a failed probe ends the
/// connection, which the engine then recovers from).
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

const ERR_UNREACHABLE: &str = "Can't reach the proxy";
const ERR_REFUSED: &str = "Proxy refused the connection";
const ERR_AUTH: &str = "Proxy authentication failed";

impl ProxyConfig {
    fn credentials(&self) -> Option<(&str, &str)> {
        let user = self.username.as_deref().filter(|u| !u.is_empty())?;
        Some((user, self.password.as_deref().unwrap_or("")))
    }

    /// "using SOCKS5 proxy" / "using HTTP proxy": the only thing logged at info.
    pub(crate) fn log_in_use(&self) {
        match self.kind {
            ProxyKind::Socks5 => log::info!("using SOCKS5 proxy"),
            ProxyKind::Http => log::info!("using HTTP proxy"),
        }
        log::debug!("proxy endpoint {}:{}", self.host, self.port);
    }
}

// ---------------------------------------------------------------------------
// Tunnels
// ---------------------------------------------------------------------------

/// Opens a TCP connection to the proxy and asks it to tunnel to
/// `host:port`. The whole exchange is bounded by [`CONNECT_TIMEOUT`].
async fn connect_tunnel(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut stream = TcpStream::connect((bare_host(&cfg.host), cfg.port)).await.map_err(|e| {
            log::debug!("proxy connect failed: {e}");
            anyhow!(ERR_UNREACHABLE)
        })?;
        enable_keepalive(&stream);
        match cfg.kind {
            ProxyKind::Socks5 => socks5_handshake(&mut stream, cfg, host, port).await?,
            ProxyKind::Http => http_connect_handshake(&mut stream, cfg, host, port).await?,
        }
        Ok(stream)
    })
    .await
    .map_err(|_| anyhow!("Proxy timed out"))?
}

/// Turns on TCP keepalive probes. Failing to do so only loses the early
/// detection of half-open tunnels (the WhatsApp-level ping still catches them).
fn enable_keepalive(stream: &TcpStream) {
    let ka = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE_IDLE).with_interval(TCP_KEEPALIVE_INTERVAL);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&ka) {
        log::debug!("could not enable TCP keepalive: {e}");
    }
}

/// The proxy host without IPv6 brackets: `[::1]` and `::1` both become `::1`,
/// which is what `TcpStream::connect` parses as an address.
fn bare_host(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// The proxy host as it must appear in a URL: IPv6 literals in brackets.
fn url_host(host: &str) -> String {
    let bare = bare_host(host);
    if bare.contains(':') { format!("[{bare}]") } else { bare.to_string() }
}

fn io_err(e: std::io::Error) -> anyhow::Error {
    log::debug!("proxy i/o error: {e}");
    anyhow!("Lost the connection to the proxy")
}

/// SOCKS5 (RFC 1928) with optional username/password (RFC 1929). The target is
/// sent as a domain name so the proxy resolves it.
async fn socks5_handshake<S>(s: &mut S, cfg: &ProxyConfig, host: &str, port: u16) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let creds = cfg.credentials();
    let greeting: &[u8] = if creds.is_some() { &[5, 2, 0x00, 0x02] } else { &[5, 1, 0x00] };
    s.write_all(greeting).await.map_err(io_err)?;
    let mut choice = [0u8; 2];
    s.read_exact(&mut choice).await.map_err(io_err)?;
    if choice[0] != 5 {
        bail!("The proxy is not a SOCKS5 proxy");
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let Some((user, pass)) = creds else {
                bail!("{ERR_AUTH}");
            };
            let (user, pass) = (user.as_bytes(), pass.as_bytes());
            if user.len() > 255 || pass.len() > 255 {
                bail!("Proxy username or password is too long");
            }
            let mut msg = Vec::with_capacity(3 + user.len() + pass.len());
            msg.push(1);
            msg.push(user.len() as u8);
            msg.extend_from_slice(user);
            msg.push(pass.len() as u8);
            msg.extend_from_slice(pass);
            s.write_all(&msg).await.map_err(io_err)?;
            let mut reply = [0u8; 2];
            s.read_exact(&mut reply).await.map_err(io_err)?;
            if reply[0] != 1 || reply[1] != 0 {
                bail!("{ERR_AUTH}");
            }
        }
        0xFF => bail!("{ERR_AUTH}"),
        _ => bail!("The proxy asked for an unsupported login method"),
    }

    let name = host.as_bytes();
    if name.is_empty() || name.len() > 255 {
        bail!("Invalid target host name");
    }
    let mut req = Vec::with_capacity(7 + name.len());
    req.extend_from_slice(&[5, 1, 0, 3, name.len() as u8]);
    req.extend_from_slice(name);
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await.map_err(io_err)?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head).await.map_err(io_err)?;
    if head[0] != 5 {
        bail!("The proxy is not a SOCKS5 proxy");
    }
    if head[1] != 0 {
        let why = match head[1] {
            2 => " (not allowed by its rules)",
            3 => " (network unreachable)",
            4 => " (host unreachable)",
            5 => " (connection refused)",
            _ => "",
        };
        bail!("{ERR_REFUSED}{why}");
    }
    // Drain the bound address so the tunnel starts at the first payload byte.
    let addr_len = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await.map_err(io_err)?;
            n[0] as usize
        }
        _ => bail!("The proxy sent an invalid reply"),
    };
    let mut rest = vec![0u8; addr_len + 2];
    s.read_exact(&mut rest).await.map_err(io_err)?;
    Ok(())
}

/// HTTP `CONNECT` with Basic proxy authentication when credentials are set.
async fn http_connect_handshake<S>(s: &mut S, cfg: &ProxyConfig, host: &str, port: u16) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some((user, pass)) = cfg.credentials() {
        let token = base64(format!("{user}:{pass}").as_bytes());
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.map_err(io_err)?;

    // Read byte by byte: anything past the blank line belongs to the tunnel.
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_CONNECT_REPLY {
            bail!("The proxy sent an invalid reply");
        }
        s.read_exact(&mut byte).await.map_err(io_err)?;
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let status = text
        .lines()
        .next()
        .filter(|l| l.starts_with("HTTP/1."))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("The proxy sent an invalid reply"))?;
    match status {
        200..=299 => Ok(()),
        407 => bail!("{ERR_AUTH}"),
        n => bail!("{ERR_REFUSED} (HTTP {n})"),
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |a, (i, b)| a | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

/// The library's own TLS settings (webpki roots, ring), so the proxied path
/// verifies certificates exactly like the direct one.
fn tls_connector() -> Result<TlsConnector> {
    match default_tls_connector() {
        Connector::Rustls(c) => Ok(c),
        _ => bail!("unsupported TLS backend"),
    }
}

/// A TLS stream whose "peer closed the connection without `close_notify`"
/// error (rustls `UnexpectedEof`) reads as a plain end of stream. Proxies and
/// NAT boxes cut tunnels without the TLS goodbye; that is an ordinary
/// connection loss, not a protocol error worth showing. The WebSocket layer
/// then reports "stream ended" and the engine reconnects as for any drop.
pub(crate) struct LenientEof<S>(pub(crate) S);

impl<S: AsyncRead + Unpin> AsyncRead for LenientEof<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                log::debug!("tunnel closed without TLS close_notify");
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for LenientEof<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Tunnels to WhatsApp Web through the proxy and completes the TLS handshake.
async fn dial_tls(cfg: &ProxyConfig, tls: &TlsConnector) -> Result<TlsStream<TcpStream>> {
    let tcp = connect_tunnel(cfg, WA_HOST, WA_PORT).await?;
    let name = ServerName::try_from(WA_HOST).map_err(|e| anyhow!("invalid server name: {e}"))?;
    tokio::time::timeout(CONNECT_TIMEOUT, tls.connect(name, tcp))
        .await
        .map_err(|_| anyhow!("TLS handshake through the proxy timed out"))?
        .map_err(|e| {
            log::debug!("tls handshake through proxy failed: {e}");
            anyhow!("TLS handshake through the proxy failed")
        })
}

/// Dials the WhatsApp chat socket through a proxy.
pub(crate) struct ProxyTransportFactory {
    cfg: ProxyConfig,
    tls: TlsConnector,
}

impl ProxyTransportFactory {
    pub(crate) fn new(cfg: ProxyConfig) -> Result<Self> {
        Ok(Self { cfg, tls: tls_connector()? })
    }

    async fn dial(&self) -> Result<(Arc<dyn Transport>, EventReceiver)> {
        let tls = dial_tls(&self.cfg, &self.tls).await?;
        let uri: http::Uri = WHATSAPP_WEB_WS_URL.parse().map_err(|e| anyhow!("bad WebSocket URL: {e}"))?;
        // Same upgrade request as the library's default factory.
        let origin = http::HeaderValue::from_static(WHATSAPP_WEB_ORIGIN);
        let builder = tokio_websockets::ClientBuilder::from_uri(uri)
            .add_header(http::header::ORIGIN, origin)
            .map_err(|e| anyhow!("Failed to set Origin header: {e}"))?;
        let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, builder.connect_on(LenientEof(tls)))
            .await
            .map_err(|_| anyhow!("WebSocket upgrade through the proxy timed out"))?
            .map_err(|e| anyhow!("WebSocket connect failed: {e}"))?;
        Ok(from_websocket(ws))
    }
}

#[async_trait]
impl TransportFactory for ProxyTransportFactory {
    async fn create_transport(&self) -> Result<(Arc<dyn Transport>, EventReceiver)> {
        self.dial().await
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// The library's HTTP client over a `ureq` agent that uses the proxy.
/// `socks5h` makes the proxy resolve every host name, including CDN hosts.
pub(crate) fn http_client(cfg: &ProxyConfig) -> Result<UreqHttpClient> {
    let protocol = match cfg.kind {
        ProxyKind::Socks5 => ureq::ProxyProtocol::Socks5h,
        ProxyKind::Http => ureq::ProxyProtocol::Http,
    };
    let mut builder = ureq::Proxy::builder(protocol).host(&url_host(&cfg.host)).port(cfg.port);
    if let Some((user, pass)) = cfg.credentials() {
        builder = builder.username(user).password(pass);
    }
    let proxy = builder.build().map_err(|e| anyhow!("Invalid proxy settings: {e}"))?;
    let agent: ureq::Agent = ureq::config::Config::builder()
        .proxy(Some(proxy))
        // Same pooling as the library's default agent (16 KiB buffers).
        .input_buffer_size(16 * 1024)
        .output_buffer_size(16 * 1024)
        .max_idle_connections(3)
        .max_idle_connections_per_host(2)
        .build()
        .into();
    Ok(UreqHttpClient::with_agent(agent))
}

// ---------------------------------------------------------------------------
// Connection test
// ---------------------------------------------------------------------------

/// Tunnels to WhatsApp Web through `cfg` and completes the TLS handshake.
/// Returns how long that took. Blocks; call it from a worker thread.
pub fn test_proxy_blocking(cfg: &ProxyConfig) -> Result<Duration, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Could not start the test: {e}"))?;
    rt.block_on(async {
        let tls = tls_connector().map_err(|e| e.to_string())?;
        let started = Instant::now();
        dial_tls(cfg, &tls).await.map_err(|e| e.to_string())?;
        Ok(started.elapsed())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn cfg(kind: ProxyKind, port: u16, creds: Option<(&str, &str)>) -> ProxyConfig {
        ProxyConfig {
            kind,
            host: "127.0.0.1".into(),
            port,
            username: creds.map(|c| c.0.to_string()),
            password: creds.map(|c| c.1.to_string()),
        }
    }

    /// Runs `script` against the first connection; returns the port.
    async fn fake_proxy<F, Fut>(script: F) -> (u16, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let task = tokio::spawn(async move {
            let (s, _) = listener.accept().await.expect("accept");
            script(s).await;
        });
        (port, task)
    }

    async fn read_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        s.read_exact(&mut b).await.expect("read");
        b
    }

    fn connect_request(host: &str, port: u16) -> Vec<u8> {
        let mut v = vec![5, 1, 0, 3, host.len() as u8];
        v.extend_from_slice(host.as_bytes());
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    #[tokio::test]
    async fn socks5_no_auth_sends_domain_name_and_tunnels() {
        let (port, task) = fake_proxy(|mut s| async move {
            assert_eq!(read_n(&mut s, 3).await, [5, 1, 0]);
            s.write_all(&[5, 0]).await.expect("w");
            let req = connect_request("web.whatsapp.com", 443);
            assert_eq!(read_n(&mut s, req.len()).await, req);
            s.write_all(&[5, 0, 0, 1, 1, 2, 3, 4, 0x1f, 0x90]).await.expect("w");
            // Payload right after the reply must reach the client intact.
            s.write_all(b"hi").await.expect("w");
        })
        .await;
        let mut s = connect_tunnel(&cfg(ProxyKind::Socks5, port, None), "web.whatsapp.com", 443)
            .await
            .expect("tunnel");
        assert_eq!(read_n(&mut s, 2).await, b"hi");
        task.await.expect("proxy task");
    }

    #[tokio::test]
    async fn socks5_user_pass_auth() {
        let (port, task) = fake_proxy(|mut s| async move {
            assert_eq!(read_n(&mut s, 4).await, [5, 2, 0, 2]);
            s.write_all(&[5, 2]).await.expect("w");
            let mut expect = vec![1, 3];
            expect.extend_from_slice(b"bob");
            expect.push(2);
            expect.extend_from_slice(b"pw");
            assert_eq!(read_n(&mut s, expect.len()).await, expect);
            s.write_all(&[1, 0]).await.expect("w");
            let req = connect_request("web.whatsapp.com", 443);
            assert_eq!(read_n(&mut s, req.len()).await, req);
            s.write_all(&[5, 0, 0, 3, 1, b'x', 0, 80]).await.expect("w");
        })
        .await;
        connect_tunnel(&cfg(ProxyKind::Socks5, port, Some(("bob", "pw"))), "web.whatsapp.com", 443)
            .await
            .expect("tunnel");
        task.await.expect("proxy task");
    }

    #[tokio::test]
    async fn socks5_bad_password_is_auth_failure() {
        let (port, _task) = fake_proxy(|mut s| async move {
            read_n(&mut s, 4).await;
            s.write_all(&[5, 2]).await.expect("w");
            read_n(&mut s, 1 + 1 + 1 + 1 + 1).await;
            s.write_all(&[1, 1]).await.expect("w");
        })
        .await;
        let e = connect_tunnel(&cfg(ProxyKind::Socks5, port, Some(("a", "b"))), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), ERR_AUTH);
    }

    #[tokio::test]
    async fn socks5_no_acceptable_method_is_auth_failure() {
        let (port, _task) = fake_proxy(|mut s| async move {
            read_n(&mut s, 3).await;
            s.write_all(&[5, 0xFF]).await.expect("w");
        })
        .await;
        let e = connect_tunnel(&cfg(ProxyKind::Socks5, port, None), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), ERR_AUTH);
    }

    #[tokio::test]
    async fn socks5_connect_failure_codes_are_refusals() {
        for (code, suffix) in [(2u8, "(not allowed by its rules)"), (4, "(host unreachable)"), (5, "(connection refused)"), (1, "")] {
            let (port, _task) = fake_proxy(move |mut s| async move {
                read_n(&mut s, 3).await;
                s.write_all(&[5, 0]).await.expect("w");
                read_n(&mut s, 7 + "web.whatsapp.com".len()).await;
                s.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await.expect("w");
            })
            .await;
            let e = connect_tunnel(&cfg(ProxyKind::Socks5, port, None), "web.whatsapp.com", 443)
                .await
                .expect_err("must fail");
            assert_eq!(e.to_string(), format!("{ERR_REFUSED}{}", if suffix.is_empty() { String::new() } else { format!(" {suffix}") }));
        }
    }

    #[tokio::test]
    async fn socks5_rejects_non_socks_peer() {
        let (port, _task) = fake_proxy(|mut s| async move {
            read_n(&mut s, 3).await;
            s.write_all(b"HT").await.expect("w");
        })
        .await;
        let e = connect_tunnel(&cfg(ProxyKind::Socks5, port, None), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), "The proxy is not a SOCKS5 proxy");
    }

    #[tokio::test]
    async fn closed_proxy_port_is_unreachable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let e = connect_tunnel(&cfg(ProxyKind::Socks5, port, None), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), ERR_UNREACHABLE);
    }

    async fn read_head(s: &mut TcpStream) -> String {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(read_n(s, 1).await[0]);
        }
        String::from_utf8(head).expect("utf8")
    }

    #[tokio::test]
    async fn http_connect_200_with_basic_auth() {
        let (port, task) = fake_proxy(|mut s| async move {
            let head = read_head(&mut s).await;
            assert!(head.starts_with("CONNECT web.whatsapp.com:443 HTTP/1.1\r\n"));
            assert!(head.contains("Host: web.whatsapp.com:443\r\n"));
            // base64("bob:pw")
            assert!(head.contains("Proxy-Authorization: Basic Ym9iOnB3\r\n"));
            s.write_all(b"HTTP/1.1 200 Connection established\r\nX: y\r\n\r\nhi").await.expect("w");
        })
        .await;
        let mut s = connect_tunnel(&cfg(ProxyKind::Http, port, Some(("bob", "pw"))), "web.whatsapp.com", 443)
            .await
            .expect("tunnel");
        assert_eq!(read_n(&mut s, 2).await, b"hi");
        task.await.expect("proxy task");
    }

    #[tokio::test]
    async fn http_connect_without_credentials_sends_no_auth_header() {
        let (port, task) = fake_proxy(|mut s| async move {
            let head = read_head(&mut s).await;
            assert!(!head.contains("Proxy-Authorization"));
            s.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await.expect("w");
        })
        .await;
        connect_tunnel(&cfg(ProxyKind::Http, port, None), "web.whatsapp.com", 443).await.expect("tunnel");
        task.await.expect("proxy task");
    }

    #[tokio::test]
    async fn http_connect_407_is_auth_failure() {
        let (port, _task) = fake_proxy(|mut s| async move {
            read_head(&mut s).await;
            s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").await.expect("w");
        })
        .await;
        let e = connect_tunnel(&cfg(ProxyKind::Http, port, None), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), ERR_AUTH);
    }

    #[tokio::test]
    async fn http_connect_403_is_refusal() {
        let (port, _task) = fake_proxy(|mut s| async move {
            read_head(&mut s).await;
            s.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await.expect("w");
        })
        .await;
        let e = connect_tunnel(&cfg(ProxyKind::Http, port, None), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), format!("{ERR_REFUSED} (HTTP 403)"));
    }

    /// Reads fail with the error rustls reports for a missing close_notify.
    struct CutStream;
    impl AsyncRead for CutStream {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed connection without sending TLS close_notify")))
        }
    }

    #[tokio::test]
    async fn missing_close_notify_reads_as_end_of_stream() {
        let mut s = LenientEof(CutStream);
        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).await.expect("eof, not an error"), 0);
    }

    struct BrokenStream;
    impl AsyncRead for BrokenStream {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset")))
        }
    }

    #[tokio::test]
    async fn other_read_errors_are_kept() {
        let mut s = LenientEof(BrokenStream);
        let mut buf = [0u8; 8];
        let e = s.read(&mut buf).await.expect_err("must stay an error");
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
    }

    #[tokio::test]
    async fn tunnel_socket_has_keepalive_and_a_dropped_tunnel_can_be_redialled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let proxy = tokio::spawn(async move {
            // First tunnel is cut right after the handshake, the second stays up.
            for i in 0..2 {
                let (mut s, _) = listener.accept().await.expect("accept");
                read_n(&mut s, 3).await;
                s.write_all(&[5, 0]).await.expect("w");
                let req = connect_request("web.whatsapp.com", 443);
                read_n(&mut s, req.len()).await;
                s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.expect("w");
                if i == 0 {
                    drop(s);
                } else {
                    s.write_all(b"ok").await.expect("w");
                }
            }
        });
        let c = cfg(ProxyKind::Socks5, port, None);
        let mut first = connect_tunnel(&c, "web.whatsapp.com", 443).await.expect("first tunnel");
        assert!(socket2::SockRef::from(&first).keepalive().expect("query"), "keepalive must be on");
        let mut buf = [0u8; 2];
        assert!(first.read_exact(&mut buf).await.is_err(), "tunnel was cut");
        let mut second = connect_tunnel(&c, "web.whatsapp.com", 443).await.expect("fresh tunnel after the drop");
        assert_eq!(read_n(&mut second, 2).await, b"ok");
        proxy.await.expect("proxy task");
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn debug_redacts_password() {
        let c = cfg(ProxyKind::Socks5, 1080, Some(("bob", "hunter2")));
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn http_client_builds_for_both_kinds() {
        http_client(&cfg(ProxyKind::Socks5, 1080, Some(("bob", "p@ss:w/rd")))).expect("socks");
        http_client(&cfg(ProxyKind::Http, 8080, None)).expect("http");
    }

    #[test]
    fn http_client_builds_for_ipv6_hosts() {
        for host in ["::1", "[::1]", "2001:db8::1"] {
            for kind in [ProxyKind::Socks5, ProxyKind::Http] {
                let mut c = cfg(kind, 1080, None);
                c.host = host.into();
                http_client(&c).unwrap_or_else(|e| panic!("{host}: {e}"));
            }
        }
    }

    #[test]
    fn host_forms_are_normalised() {
        assert_eq!(bare_host("[::1]"), "::1");
        assert_eq!(bare_host("::1"), "::1");
        assert_eq!(bare_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(url_host("::1"), "[::1]");
        assert_eq!(url_host("[::1]"), "[::1]");
        assert_eq!(url_host("proxy.example"), "proxy.example");
    }

    #[tokio::test]
    async fn tunnel_connects_to_ipv6_proxy_in_both_forms() {
        let Ok(listener) = TcpListener::bind("[::1]:0").await else { return };
        let port = listener.local_addr().expect("addr").port();
        let task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.expect("accept");
                read_n(&mut s, 3).await;
                s.write_all(&[5, 0]).await.expect("w");
                let req = connect_request("web.whatsapp.com", 443);
                read_n(&mut s, req.len()).await;
                s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.expect("w");
            }
        });
        for host in ["::1", "[::1]"] {
            let mut c = cfg(ProxyKind::Socks5, port, None);
            c.host = host.into();
            connect_tunnel(&c, "web.whatsapp.com", 443).await.unwrap_or_else(|e| panic!("{host}: {e}"));
        }
        task.await.expect("proxy task");
    }

    #[tokio::test]
    async fn socks5_wrong_auth_reply_version_is_auth_failure() {
        let (port, _task) = fake_proxy(|mut s| async move {
            read_n(&mut s, 4).await;
            s.write_all(&[5, 2]).await.expect("w");
            read_n(&mut s, 5).await;
            s.write_all(&[5, 0]).await.expect("w");
        })
        .await;
        let e = connect_tunnel(&cfg(ProxyKind::Socks5, port, Some(("a", "b"))), "web.whatsapp.com", 443)
            .await
            .expect_err("must fail");
        assert_eq!(e.to_string(), ERR_AUTH);
    }
}
