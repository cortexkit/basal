//! A local HTTPS server for the `net.fetch` tests.
//!
//! It listens on 127.0.0.1 with a self-signed certificate for the test host
//! names. Nothing trusts that certificate except a `net.fetch` configured
//! with [`TestServer::trust_der`] as its only trust anchor, which only
//! tests do. Each connection carries one request; the server answers from
//! a fixed table of paths and records every request it read.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// The host names the certificate covers.
pub const TEST_HOSTS: [&str; 3] = ["api.test", "other.test", "evil.test"];

/// One answer the server gives.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Send the body chunked instead of with a Content-Length.
    pub chunked: bool,
    /// Bytes to send instead of a response, for a server that answers
    /// with something that is not HTTP.
    pub raw: Option<Vec<u8>>,
}

impl Reply {
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: body.as_bytes().to_vec(),
            chunked: false,
            raw: None,
        }
    }

    /// Answers with `bytes` exactly as given, then closes.
    pub fn raw(bytes: &[u8]) -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            chunked: false,
            raw: Some(bytes.to_vec()),
        }
    }

    pub fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
            chunked: false,
            raw: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A request the server read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The running server. Dropping it stops the accept loop.
pub struct TestServer {
    addr: SocketAddr,
    cert_der: Vec<u8>,
    seen: Arc<Mutex<Vec<Seen>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    #[cfg(unix)]
    blocking_accept: bool,
}

impl TestServer {
    /// Starts a server answering the routes `routes` builds from the
    /// server's port (path, query included, to reply); any other path
    /// answers 404.
    pub fn start(routes: impl FnOnce(u16) -> BTreeMap<String, Reply>) -> Self {
        let names: Vec<String> = TEST_HOSTS.iter().map(|h| (*h).to_owned()).collect();
        let certified = rcgen::generate_simple_self_signed(names).expect("certificate");
        let cert_der = certified.cert.der().to_vec();
        let key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert_der.clone())], key)
            .expect("server certificate");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        #[cfg(all(test, unix))]
        let blocking_accept = {
            use std::os::fd::AsRawFd;
            // SAFETY: listener owns this live socket; F_GETFL only reads flags.
            let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            flags & libc::O_NONBLOCK == 0
        };
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(routes(addr.port()));
        let thread = {
            let (seen, connections, stop) = (seen.clone(), connections.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((tcp, _)) => {
                            // Drop wakes accept with a local connection, not an
                            // HTTPS request. Do not count or serve that wakeup.
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            connections.fetch_add(1, Ordering::SeqCst);
                            let (config, routes, seen) =
                                (config.clone(), routes.clone(), seen.clone());
                            thread::spawn(move || {
                                let _ = serve(tcp, config, &routes, &seen);
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => panic!("accept test HTTPS connection: {e}"),
                    }
                }
            })
        };
        Self {
            addr,
            cert_der,
            seen,
            connections,
            stop,
            thread: Some(thread),
            #[cfg(test)]
            #[cfg(unix)]
            blocking_accept,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The server's certificate, to trust in a test's `net.fetch`.
    pub fn trust_der(&self) -> Vec<u8> {
        self.cert_der.clone()
    }

    /// A production-form HTTPS URL; the static resolver maps port 443 to this
    /// server's ephemeral socket without weakening URL approval checks.
    pub fn url(&self, host: &str, path: &str) -> String {
        format!("https://{host}{path}")
    }

    /// Every request read so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// TCP connections accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(
    tcp: TcpStream,
    config: Arc<ServerConfig>,
    routes: &BTreeMap<String, Reply>,
    seen: &Mutex<Vec<Seen>>,
) -> std::io::Result<()> {
    tcp.set_nonblocking(false)?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let conn = ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut reader = BufReader::new(StreamOwned::new(conn, tcp));
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((n, v)) = line.trim_end().split_once(':') {
            headers.push((n.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    if let Ok(mut s) = seen.lock() {
        s.push(Seen {
            method: method.clone(),
            path: path.clone(),
            headers,
            body,
        });
    }
    let reply = routes
        .get(&path)
        .cloned()
        .unwrap_or_else(|| Reply::text(404, "not found"));
    let mut stream = reader.into_inner();
    if let Some(raw) = &reply.raw {
        stream.write_all(raw)?;
        stream.flush()?;
        stream.conn.send_close_notify();
        let _ = stream.conn.complete_io(&mut stream.sock);
        return Ok(());
    }
    let mut head = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status);
    for (n, v) in &reply.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    if reply.chunked {
        head.push_str("Transfer-Encoding: chunked\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        if method != "HEAD" {
            for chunk in reply.body.chunks(64 * 1024) {
                stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())?;
                stream.write_all(chunk)?;
                stream.write_all(b"\r\n")?;
            }
            stream.write_all(b"0\r\n\r\n")?;
        }
    } else {
        head.push_str(&format!("Content-Length: {}\r\n\r\n", reply.body.len()));
        stream.write_all(head.as_bytes())?;
        if method != "HEAD" {
            stream.write_all(&reply.body)?;
        }
    }
    stream.flush()?;
    stream.conn.send_close_notify();
    let _ = stream.conn.complete_io(&mut stream.sock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_https_accept_blocks_and_shutdown_wakes_it_without_a_request() {
        let server = TestServer::start(|_| BTreeMap::new());
        let connections = server.connections.clone();
        let seen = server.seen.clone();
        #[cfg(unix)]
        assert!(
            server.blocking_accept,
            "the accept loop must block instead of polling"
        );
        drop(server);
        assert_eq!(connections.load(Ordering::SeqCst), 0);
        assert!(seen.lock().unwrap().is_empty());
    }
}
