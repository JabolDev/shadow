use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[test]
fn parses_http_and_https_urls() {
    let u = Url::parse("http://127.0.0.1:8080/v1/messages?beta=1").unwrap();
    assert_eq!(
        (u.host.as_str(), u.port, u.path.as_str()),
        ("127.0.0.1", 8080, "/v1/messages?beta=1")
    );
    let u = Url::parse("http://localhost").unwrap();
    assert_eq!(
        (u.port, u.path.as_str(), u.host_header().as_str()),
        (80, "/", "localhost")
    );
    let u = Url::parse("http://[::1]:9/x").unwrap();
    assert_eq!(
        (u.host.as_str(), u.port, u.host_header().as_str()),
        ("::1", 9, "[::1]:9")
    );
    let u = Url::parse("https://api.example.com/v1/messages").unwrap();
    assert_eq!(
        (u.tls, u.port, u.path.as_str(), u.host_header().as_str()),
        (true, 443, "/v1/messages", "api.example.com")
    );
    let u = Url::parse("https://127.0.0.1:8443").unwrap();
    assert_eq!(
        (u.tls, u.port, u.host_header().as_str()),
        (true, 8443, "127.0.0.1:8443")
    );
    assert!(!Url::parse("http://h/").unwrap().tls);
    assert!(Url::parse("ftp://127.0.0.1/x").is_err());
    assert!(Url::parse("http://h:99999/").is_err());
    assert!(Url::parse("http://user@h/").is_err());
}

async fn serve_once(reply: &'static [u8]) -> (Url, tokio::task::JoinHandle<String>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let h = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut buf = vec![0; 4096];
        let n = tokio::io::AsyncReadExt::read(&mut s, &mut buf)
            .await
            .unwrap();
        s.write_all(reply).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    (
        Url::parse(&format!("http://127.0.0.1:{port}/p")).unwrap(),
        h,
    )
}

#[tokio::test]
async fn reads_a_chunked_body() {
    let (url, h) = serve_once(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-A: b\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").await;
    let mut r = post(&url, &[("x-key".into(), "v".into())], b"{}")
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-a"), Some("b"));
    assert_eq!(r.text(100).await.unwrap(), "hello world");
    let sent = h.await.unwrap();
    assert!(
        sent.starts_with("POST /p HTTP/1.1\r\n")
            && sent.contains("x-key: v\r\n")
            && sent.contains("content-length: 2\r\n")
            && sent.ends_with("\r\n\r\n{}"),
        "{sent}"
    );
}

#[tokio::test]
async fn reads_length_and_close_bodies() {
    let (url, _) = serve_once(
        b"HTTP/1.1 429 Too Many\r\nContent-Length: 4\r\nRetry-After: 2\r\n\r\nslowEXTRA",
    )
    .await;
    let mut r = post(&url, &[], b"").await.unwrap();
    assert_eq!((r.status, r.header("retry-after")), (429, Some("2")));
    assert_eq!(r.text(100).await.unwrap(), "slow");
    let (url, _) = serve_once(b"HTTP/1.1 500 X\r\n\r\nuntil close").await;
    let mut r = post(&url, &[], b"").await.unwrap();
    assert_eq!(r.text(5).await.unwrap(), "until");
}

#[tokio::test]
async fn a_cut_chunked_body_is_an_error() {
    let (url, _) =
        serve_once(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\na\r\nonly").await;
    let mut r = post(&url, &[], b"").await.unwrap();
    assert!(r.text(100).await.is_err());
}

#[tokio::test]
async fn header_injection_is_refused() {
    let (url, _) = serve_once(b"HTTP/1.1 200 OK\r\n\r\n").await;
    assert!(
        post(&url, &[("a".into(), "b\r\nc: d".into())], b"")
            .await
            .is_err()
    );
}
