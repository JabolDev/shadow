//! A plain-HTTP/1.1 POST with a streamed response body — all the model
//! client needs. No TLS in this version: the endpoint is `http://…`.
//! One connection per request (`connection: close`).

use std::io;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Clone, Debug)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path and query, starting with "/".
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("endpoint.url must start with http:// (got {url:?})"))?;
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
                Some((h, "")) => (h, 80),
                Some((h, p)) if p.starts_with(':') => (h, port_of(&p[1..])?),
                _ => return Err(format!("bad host in endpoint.url {url:?}")),
            },
            None => match authority.split_once(':') {
                Some((h, p)) => (h, port_of(p)?),
                None => (authority, 80),
            },
        };
        if host.is_empty() {
            return Err(format!("no host in endpoint.url {url:?}"));
        }
        Ok(Url {
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
        if self.port == 80 {
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

struct Body {
    r: BufReader<TcpStream>,
    framing: Framing,
}

/// Sends `POST url` with `body` and returns once the response head is read.
pub async fn post(url: &Url, headers: &[(String, String)], body: &[u8]) -> io::Result<Response> {
    let stream = TcpStream::connect((url.host.as_str(), url.port)).await?;
    stream.set_nodelay(true)?;
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

async fn read_line(r: &mut BufReader<TcpStream>) -> io::Result<String> {
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
