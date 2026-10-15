//! Spike for flotilla#3275: unmodified hudsucker 0.25 + a thin front-end that
//! (a) accepts on many per-vessel listeners (UDS and TCP) and carries the
//! listener identity to the policy, and (b) accepts raw TLS without CONNECT,
//! taking the destination host from SNI (port from SO_ORIGINAL_DST on Linux;
//! from listener config here on macOS), and bridges it into hudsucker by
//! synthesising a CONNECT on a loopback hop.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hudsucker::{
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
    certificate_authority::RcgenAuthority,
    hyper::{Request, Response, StatusCode, body::Frame, header},
    rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose},
    rustls::{self, crypto::aws_lc_rs, pki_types::ServerName},
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
};

type IdentityMap = Arc<Mutex<HashMap<SocketAddr, String>>>;

// ---------------------------------------------------------------- policy ---
#[derive(Clone)]
struct Policy {
    ids: IdentityMap,
}

fn deny(code: StatusCode, msg: &str) -> RequestOrResponse {
    Response::builder().status(code).body(Body::from(msg.to_string())).unwrap().into()
}

impl HttpHandler for Policy {
    async fn handle_request(&mut self, ctx: &HttpContext, req: Request<Body>) -> RequestOrResponse {
        if req.method() == hudsucker::hyper::Method::CONNECT {
            return req.into();
        }
        let Some(vessel) = self.ids.lock().unwrap().get(&ctx.client_addr).cloned() else {
            return deny(StatusCode::FORBIDDEN, "unknown caller");
        };
        let (mut parts, body) = req.into_parts();
        // Header swap: placeholder must match this listener's vessel.
        let want = format!("Bearer placeholder-{vessel}");
        match parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
            Some(v) if v == want => {}
            _ => return deny(StatusCode::FORBIDDEN, "bad placeholder"),
        }
        parts.headers.insert(header::AUTHORIZATION, format!("Bearer REAL-{vessel}").parse().unwrap());

        if parts.uri.path() == "/graphql" {
            // Bounded buffering only for GraphQL; everything else streams.
            let bytes = match http_body_util::Limited::new(body, 1 << 20).collect().await {
                Ok(c) => c.to_bytes(),
                Err(_) => return deny(StatusCode::PAYLOAD_TOO_LARGE, "graphql too large"),
            };
            // Naive check; the real layer would use a GraphQL parser.
            let s = String::from_utf8_lossy(&bytes);
            if s.trim_start_matches(|c: char| c == '{' || c.is_whitespace() || c == '"').contains("mutation") {
                return deny(StatusCode::FORBIDDEN, "graphql mutation refused");
            }
            return Request::from_parts(parts, Body::from(Full::new(bytes))).into();
        }
        Request::from_parts(parts, body).into()
    }
}

// ------------------------------------------------------- front-end bridge ---
/// Read just enough of the stream to parse the ClientHello; return SNI and the
/// bytes consumed (to be replayed).
async fn peek_sni<S: AsyncRead + Unpin>(s: &mut S) -> std::io::Result<(String, Vec<u8>)> {
    let mut acc = rustls::server::Acceptor::default();
    let mut seen = Vec::new();
    loop {
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        seen.extend_from_slice(&buf[..n]);
        acc.read_tls(&mut &buf[..n])?;
        match acc.accept() {
            Ok(Some(accepted)) => {
                let sni = accepted.client_hello().server_name().map(str::to_owned).unwrap_or_default();
                return Ok((sni, seen));
            }
            Ok(None) => continue,
            Err((e, _)) => return Err(std::io::Error::other(e)),
        }
    }
}

async fn bridge<S: AsyncRead + AsyncWrite + Unpin>(
    mut inbound: S,
    vessel: &str,
    orig_port: u16,
    proxy_addr: SocketAddr,
    ids: IdentityMap,
) -> std::io::Result<()> {
    let (sni, prefix) = peek_sni(&mut inbound).await?;
    let mut hop = TcpStream::connect(proxy_addr).await?;
    let local = hop.local_addr()?;
    ids.lock().unwrap().insert(local, vessel.to_string()); // before hudsucker sees a byte
    let connect = format!("CONNECT {sni}:{orig_port} HTTP/1.1\r\nHost: {sni}:{orig_port}\r\n\r\n");
    hop.write_all(connect.as_bytes()).await?;
    let mut resp = Vec::new();
    while !resp.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        hop.read_exact(&mut b).await?;
        resp.push(b[0]);
    }
    hop.write_all(&prefix).await?;
    let r = tokio::io::copy_bidirectional(&mut inbound, &mut hop).await;
    ids.lock().unwrap().remove(&local);
    r.map(|_| ())
}

/// Linux transparent mode: recover the pre-REDIRECT destination. Not compiled on macOS.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn original_dst(s: &TcpStream) -> std::io::Result<std::net::SocketAddrV4> {
    use std::os::fd::AsRawFd;
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(s.as_raw_fd(), libc::SOL_IP, libc::SO_ORIGINAL_DST, &mut addr as *mut _ as *mut _, &mut len)
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(std::net::SocketAddrV4::new(u32::from_be(addr.sin_addr.s_addr).into(), u16::from_be(addr.sin_port)))
}

// -------------------------------------------------------------- upstream ---
async fn upstream(listener: TcpListener, cfg: Arc<rustls::ServerConfig>) {
    let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
    loop {
        let (tcp, _) = listener.accept().await.unwrap();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let tls = acceptor.accept(tcp).await.unwrap();
            let svc = hyper::service::service_fn(|req: Request<hyper::body::Incoming>| async move {
                let auth = req.headers().get(header::AUTHORIZATION).map(|v| v.to_str().unwrap().to_owned());
                let ver = req.version();
                let path = req.uri().path().to_owned();
                let mut body = req.into_body();
                let (mut len, mut first, mut last) = (0usize, None, None);
                while let Some(f) = body.frame().await {
                    if let Ok(d) = f.unwrap().into_data() {
                        let now = Instant::now();
                        first.get_or_insert(now);
                        last = Some(now);
                        len += d.len();
                    }
                }
                let span = match (first, last) {
                    (Some(a), Some(b)) => (b - a).as_millis(),
                    _ => 0,
                };
                let msg = format!("upstream saw path={path} ver={ver:?} auth={auth:?} body_len={len} first_to_last_frame_ms={span}");
                Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(msg))))
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(tls), svc)
                .await;
        });
    }
}

// ---------------------------------------------------------------- client ---
type ClientBody = BoxBody<Bytes, std::convert::Infallible>;

async fn tls_client<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    io: S,
    ca_der: rustls::pki_types::CertificateDer<'static>,
    h2: bool,
) -> tokio_rustls::client::TlsStream<S> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca_der).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = if h2 { vec![b"h2".to_vec()] } else { vec![b"http/1.1".to_vec()] };
    tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(ServerName::try_from("localhost").unwrap(), io).await.unwrap()
}

async fn h1_requests<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(label: &str, io: S, vessel: &str) {
    let (mut tx, conn) = hyper::client::conn::http1::handshake::<_, ClientBody>(TokioIo::new(io)).await.unwrap();
    tokio::spawn(conn);
    let auth = format!("Bearer placeholder-{vessel}");
    let mk = |method: &str, path: &str, body: ClientBody, auth: &str| {
        Request::builder().method(method).uri(path).header("host", "localhost").header(header::AUTHORIZATION, auth).body(body).unwrap()
    };
    let full = |s: &'static str| Full::new(Bytes::from(s)).boxed();
    let cases: Vec<(&str, Request<ClientBody>)> = vec![
        ("GET swap", mk("GET", "/whoami", full(""), &auth)),
        ("wrong placeholder", mk("GET", "/whoami", full(""), "Bearer placeholder-someone-else")),
        ("graphql query", mk("POST", "/graphql", full(r#"{"query":"query { viewer { login } }"}"#), &auth)),
        ("graphql mutation", mk("POST", "/graphql", full(r#"{"query":"mutation { deleteRepo }"}"#), &auth)),
    ];
    for (name, req) in cases {
        let res = tx.send_request(req).await.unwrap();
        let st = res.status();
        let b = res.into_body().collect().await.unwrap().to_bytes();
        println!("[{label}] {name}: {st} {}", String::from_utf8_lossy(&b));
    }
    // Chunked streaming body: 2 chunks 600ms apart; if hudsucker buffered, upstream gap ~0ms.
    let (ctx, crx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::convert::Infallible>>(4);
    tokio::spawn(async move {
        ctx.send(Ok(Frame::data(Bytes::from(vec![b'a'; 1000])))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        ctx.send(Ok(Frame::data(Bytes::from(vec![b'b'; 1000])))).await.unwrap();
    });
    let body = StreamBody::new(tokio_stream_from(crx)).boxed();
    let res = tx.send_request(mk("POST", "/git-receive-pack", body, &auth)).await.unwrap();
    let st = res.status();
    let b = res.into_body().collect().await.unwrap().to_bytes();
    println!("[{label}] chunked stream: {st} {}", String::from_utf8_lossy(&b));
}

fn tokio_stream_from<T: Send + 'static>(mut rx: tokio::sync::mpsc::Receiver<T>) -> impl futures::Stream<Item = T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

async fn h2_request<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(label: &str, io: S, vessel: &str, port: u16) {
    let (mut tx, conn) = hyper::client::conn::http2::handshake::<_, _, ClientBody>(TokioExecutor::new(), TokioIo::new(io)).await.unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri(format!("https://localhost:{port}/whoami-h2"))
        .header(header::AUTHORIZATION, format!("Bearer placeholder-{vessel}"))
        .body(Full::new(Bytes::new()).boxed())
        .unwrap();
    let res = tx.send_request(req).await.unwrap();
    let st = res.status();
    let b = res.into_body().collect().await.unwrap().to_bytes();
    println!("[{label}] h2 client->proxy: {st} {}", String::from_utf8_lossy(&b));
}

// ------------------------------------------------------------------ main ---
#[tokio::main]
async fn main() {
    // Throwaway CA generated in-process (no secrets on disk).
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_der = ca_cert.der().clone();
    let ca = RcgenAuthority::new(Issuer::new(ca_params, ca_key), 100, aws_lc_rs::default_provider());

    // Upstream "github" on localhost with its own self-signed cert.
    let up = hudsucker::rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let up_der = up.cert.der().clone();
    let mut up_cfg = rustls::ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![up_der.clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(up.signing_key.serialize_der().into()))
        .unwrap();
    up_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = up_listener.local_addr().unwrap().port();
    tokio::spawn(upstream(up_listener, Arc::new(up_cfg)));

    // Proxy -> upstream connector trusting the test upstream cert (h1 + h2).
    let mut roots = rustls::RootCertStore::empty();
    roots.add(up_der).unwrap();
    let client_tls = rustls::ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector =
        hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(client_tls).https_or_http().enable_http1().enable_http2().build();

    let ids: IdentityMap = Arc::default();
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy = Proxy::builder()
        .with_listener(proxy_listener)
        .with_ca(ca)
        .with_http_connector(connector)
        .with_http_handler(Policy { ids: ids.clone() })
        .build()
        .unwrap();
    tokio::spawn(proxy.start());

    // Per-vessel listeners: alpha on a UDS, beta on a TCP port.
    let dir = std::env::temp_dir().join(format!("spike3275-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("alpha.sock");
    let uds = UnixListener::bind(&uds_path).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = tcp.local_addr().unwrap();
    {
        let ids = ids.clone();
        tokio::spawn(async move {
            loop {
                let (s, _) = uds.accept().await.unwrap();
                let ids = ids.clone();
                tokio::spawn(async move { bridge(s, "alpha", up_port, proxy_addr, ids).await });
            }
        });
    }
    {
        let ids = ids.clone();
        tokio::spawn(async move {
            loop {
                let (s, _) = tcp.accept().await.unwrap();
                let ids = ids.clone();
                tokio::spawn(async move { bridge(s, "beta", up_port, proxy_addr, ids).await });
            }
        });
    }

    // Raw TLS (no CONNECT), SNI=localhost, verifying the proxy-minted leaf against our CA.
    let s = tls_client(UnixStream::connect(&uds_path).await.unwrap(), ca_der.clone(), false).await;
    h1_requests("alpha/uds", s, "alpha").await;
    let s = tls_client(TcpStream::connect(tcp_addr).await.unwrap(), ca_der.clone(), false).await;
    h1_requests("beta/tcp", s, "beta").await;
    let s = tls_client(TcpStream::connect(tcp_addr).await.unwrap(), ca_der.clone(), true).await;
    h2_request("beta/tcp", s, "beta", up_port).await;
    // Cross-identity: beta's placeholder over alpha's socket must be refused.
    let s = tls_client(UnixStream::connect(&uds_path).await.unwrap(), ca_der.clone(), false).await;
    let (mut tx, conn) = hyper::client::conn::http1::handshake::<_, ClientBody>(TokioIo::new(s)).await.unwrap();
    tokio::spawn(conn);
    let res = tx
        .send_request(
            Request::builder()
                .uri("/whoami")
                .header("host", "localhost")
                .header(header::AUTHORIZATION, "Bearer placeholder-beta")
                .body(Full::new(Bytes::new()).boxed())
                .unwrap(),
        )
        .await
        .unwrap();
    println!("[alpha/uds] beta placeholder on alpha socket: {}", res.status());
    let _ = std::fs::remove_dir_all(dir);
}
