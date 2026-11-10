//! An HTTP/1.1 POST with a streamed response body — all the model client
//! needs — over plain TCP (`http://…`) or TLS (`https://…`: rustls with the
//! ring provider and the bundled web root certificates). One connection per
//! request (`connection: close`).

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use std::io;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Tests only: a PEM file of extra certificates to trust.
const TEST_CA_VAR: &str = "SHADOW_TEST_CA";

#[derive(Clone, Debug)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with "/".
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let (tls, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
            (Some(rest), _) => (true, rest),
            (_, Some(rest)) => (false, rest),
            _ => {
                return Err(format!(
                    "endpoint.url must start with http:// or https:// (got {url:?})"
                ));
            }
        };
        let default_port = if tls { 443 } else { 80 };
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        if authority.contains('@') {
            return Err("endpoint.url must not carry user info".into());
        }
        let port_of = |p: &str| {
            p.parse::<u16>()
                .map_err(|_| format!("bad port in endpoint.url {url:?}"))
        };
        let (host, port) = match authority.strip_prefix('[') {
            Some(v6) => match v6.split_once(']') {
                Some((h, "")) => (h, default_port),
                Some((h, p)) if p.starts_with(':') => (h, port_of(&p[1..])?),
                _ => return Err(format!("bad host in endpoint.url {url:?}")),
            },
            None => match authority.split_once(':') {
                Some((h, p)) => (h, port_of(p)?),
                None => (authority, default_port),
            },
        };
        if host.is_empty() {
            return Err(format!("no host in endpoint.url {url:?}"));
        }
        Ok(Url {
            tls,
            host: host.to_string(),
            port,
            path,
        })
    }

    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == if self.tls { 443 } else { 80 } {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    body: Body,
}

enum Framing {
    Chunked { left: usize, done: bool },
    Length(u64),
    Close,
}

/// The connection: TCP, or TLS over TCP.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

struct Body {
    r: BufReader<Box<dyn Io>>,
    framing: Framing,
}

/// Sends `POST url` with `body` and returns once the response head is read.
pub async fn post(url: &Url, headers: &[(String, String)], body: &[u8]) -> io::Result<Response> {
    let tcp = TcpStream::connect((url.host.as_str(), url.port)).await?;
    tcp.set_nodelay(true)?;
    let stream: Box<dyn Io> = if url.tls {
        let name = ServerName::try_from(url.host.clone()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad TLS host name {:?}", url.host),
            )
        })?;
        Box::new(TlsConnector::from(tls_config()?).connect(name, tcp).await?)
    } else {
        Box::new(tcp)
    };
    let mut head = format!(
        "POST {} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\naccept: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n",
        url.path,
        url.host_header(),
        body.len()
    );
    for (k, v) in headers {
        if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad header {k:?}"),
            ));
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut r = BufReader::new(stream);
    r.get_mut().write_all(head.as_bytes()).await?;
    r.get_mut().write_all(body).await?;
    r.get_mut().flush().await?;

    let status_line = read_line(&mut r).await?;
    let mut parts = status_line.splitn(3, ' ');
    let status = match (parts.next(), parts.next()) {
        (Some(v), Some(code)) if v.starts_with("HTTP/") => {
            code.parse::<u16>().map_err(|_| bad("bad status line"))?
        }
        _ => return Err(bad("bad status line")),
    };
    let mut headers = Vec::new();
    loop {
        let line = read_line(&mut r).await?;
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let framing =
        if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            Framing::Chunked {
                left: 0,
                done: false,
            }
        } else if let Some(n) = get("content-length") {
            Framing::Length(n.parse().map_err(|_| bad("bad content-length"))?)
        } else {
            Framing::Close
        };
    Ok(Response {
        status,
        headers,
        body: Body { r, framing },
    })
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The next piece of the body; None at its end.
    pub async fn chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let b = &mut self.body;
        let want = match &mut b.framing {
            Framing::Chunked { done: true, .. } | Framing::Length(0) => return Ok(None),
            Framing::Chunked { left, done } => {
                if *left == 0 {
                    let line = read_line(&mut b.r).await?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    *left = usize::from_str_radix(size, 16).map_err(|_| bad("bad chunk size"))?;
                    if *left == 0 {
                        while !read_line(&mut b.r).await?.is_empty() {}
                        *done = true;
                        return Ok(None);
                    }
                }
                (*left).min(64 * 1024)
            }
            Framing::Length(n) => (*n).min(64 * 1024) as usize,
            Framing::Close => 64 * 1024,
        };
        let mut buf = vec![0; want];
        let n = b.r.read(&mut buf).await?;
        buf.truncate(n);
        match &mut b.framing {
            Framing::Close if n == 0 => return Ok(None),
            _ if n == 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the response ended early",
                ));
            }
            Framing::Chunked { left, .. } => {
                *left -= n;
                if *left == 0 && !read_line(&mut b.r).await?.is_empty() {
                    return Err(bad("chunk without CRLF"));
                }
            }
            Framing::Length(left) => *left -= n as u64,
            Framing::Close => {}
        }
        Ok(Some(buf))
    }

    /// The whole remaining body, at most `max` bytes kept.
    pub async fn text(&mut self, max: usize) -> io::Result<String> {
        let mut out = Vec::new();
        while let Some(c) = self.chunk().await? {
            if out.len() < max {
                out.extend_from_slice(&c[..c.len().min(max - out.len())]);
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// The TLS settings, made once: the bundled web roots, plus the test
/// certificates named by SHADOW_TEST_CA when it is set.
fn tls_config() -> io::Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let made = CONFIG.get_or_init(|| {
        let mut roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        if let Some(path) = std::env::var_os(TEST_CA_VAR).filter(|p| !p.is_empty()) {
            let bad = |e: &dyn std::fmt::Display| format!("{TEST_CA_VAR}: {e}");
            for cert in CertificateDer::pem_file_iter(&path).map_err(|e| bad(&e))? {
                roots.add(cert.map_err(|e| bad(&e))?).map_err(|e| bad(&e))?;
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    });
    made.clone().map_err(io::Error::other)
}

async fn read_line(r: &mut BufReader<Box<dyn Io>>) -> io::Result<String> {
    let mut line = Vec::new();
    let n = (&mut *r)
        .take(64 * 1024)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 || line.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the connection closed mid-response",
        ));
    }
    while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| bad("response head is not UTF-8"))
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
#[path = "http_test.rs"]
mod tests;
