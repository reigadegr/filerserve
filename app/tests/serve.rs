#![allow(clippy::unwrap_used)]

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use lanfile::{AccessLog, build_router, serve};
use salvo::{
    prelude::*,
    test::{ResponseExt, TestClient},
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lanfile-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn api_router(root: PathBuf) -> Arc<Router> {
    Arc::new(build_router(root, 8000, Arc::new(AccessLog::Tracing)))
}

// ---- JSON API tests ----

#[tokio::test]
async fn api_list_returns_json_for_root() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("a.txt"), "abc").unwrap();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));

    let body = res.take_string().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["path"], "/");
    assert_eq!(json["port"], 8000);
    assert!(json["lan_ip"].is_null() || json["lan_ip"].is_string());
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);

    let by_name: std::collections::HashMap<&str, &serde_json::Value> = entries
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e))
        .collect();
    let a = by_name.get("a.txt").unwrap();
    assert_eq!(a["type"], "file");
    assert_eq!(a["size"], 3);
    assert!(a["modified"].as_str().is_some());

    let sub = by_name.get("sub").unwrap();
    assert_eq!(sub["type"], "dir");
    assert!(sub["size"].is_null());
}

#[tokio::test]
async fn api_list_shows_dot_files() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join(".hidden"), "secret").unwrap();
    std::fs::write(dir.root().join("visible.txt"), "abc").unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));

    let body = res.take_string().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);

    let by_name: std::collections::HashMap<&str, &serde_json::Value> = entries
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e))
        .collect();
    assert!(by_name.contains_key(".hidden"));
    assert!(by_name.contains_key("visible.txt"));
}

#[cfg(unix)]
#[tokio::test]
async fn api_list_hides_symlinks() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("real.txt"), "real").unwrap();
    std::os::unix::fs::symlink(dir.root().join("real.txt"), dir.root().join("alias.txt")).unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));

    let body = res.take_string().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "real.txt");
    assert!(!body.contains("alias.txt"));
}

#[tokio::test]
async fn api_list_lists_subdirectory() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();
    std::fs::write(dir.root().join("sub").join("inner.txt"), "xyz").unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list/sub")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));

    let body = res.take_string().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["path"], "/sub");
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "inner.txt");
    assert_eq!(entries[0]["type"], "file");
}

#[tokio::test]
async fn api_list_returns_404_for_missing_directory() {
    let dir = TestDir::new();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/api/list/nope")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

// ---- File download tests ----

#[tokio::test]
async fn files_endpoint_serves_file() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/files/hello.txt")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    assert_eq!(res.take_string().await.unwrap(), "hello world");
}

/// 第二次请求同一个文件会走命中缓存的路径：缓存里复用的 `ETag` 与 `Content-Disposition`
/// 必须与未命中时现算的一模一样，带回这个 `ETag` 再请求也必须仍然是 304
#[tokio::test]
async fn files_cache_hit_sends_the_same_headers() {
    use salvo::http::header::{
        CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED,
    };

    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let url = "http://127.0.0.1:5800/files/hello.txt";

    let missed = TestClient::get(url).send(Arc::clone(&router)).await;
    assert_eq!(missed.status_code, Some(StatusCode::OK));
    let hit = TestClient::get(url).send(Arc::clone(&router)).await;
    assert_eq!(hit.status_code, Some(StatusCode::OK));

    for name in [
        ETAG,
        CONTENT_DISPOSITION,
        CONTENT_TYPE,
        LAST_MODIFIED,
        CONTENT_LENGTH,
    ] {
        let missed = missed.headers().get(&name);
        assert!(missed.is_some(), "未命中的响应应当带上 {name}");
        assert_eq!(
            missed,
            hit.headers().get(&name),
            "命中与未命中的 {name} 必须一致"
        );
    }

    let etag = missed.headers().get(ETAG).unwrap().clone();
    let conditional = TestClient::get(url)
        .add_header("if-none-match", etag, true)
        .send(router)
        .await;
    assert_eq!(conditional.status_code, Some(StatusCode::NOT_MODIFIED));
}

#[tokio::test]
async fn files_endpoint_returns_404_for_missing_file() {
    let dir = TestDir::new();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/files/nope.txt")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn files_endpoint_serves_dot_files() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join(".hidden"), "secret").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/files/.hidden")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    assert_eq!(res.take_string().await.unwrap(), "secret");
}

#[tokio::test]
async fn files_endpoint_serves_hidden_dir_member() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join(".git")).unwrap();
    std::fs::write(dir.root().join(".git/config"), "secret-config").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/files/.git/config")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    assert_eq!(res.take_string().await.unwrap(), "secret-config");
}

#[tokio::test]
async fn files_endpoint_returns_404_for_directory() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();
    std::fs::write(dir.root().join("sub/inner.txt"), "xyz").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/files/sub")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[cfg(unix)]
#[tokio::test]
async fn files_endpoint_rejects_symlink() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("real.txt"), "real").unwrap();
    std::os::unix::fs::symlink(dir.root().join("real.txt"), dir.root().join("alias.txt")).unwrap();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/files/alias.txt")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn files_endpoint_rejects_path_traversal() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("secret.txt"), "top secret").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/files/%2e%2e%2f%2e%2e%2fetc%2fpasswd")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn files_endpoint_head_request_succeeds() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::head("http://127.0.0.1:5800/files/hello.txt")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
}

// ---- Real TCP download test ----

#[tokio::test]
async fn serves_over_real_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = api_router(dir.root().to_path_buf());
    let acceptor = TcpListener::new("127.0.0.1:0").bind().await;
    let addr = acceptor.local_addr().unwrap();
    let server = tokio::spawn(async move {
        Server::new(acceptor).serve(router).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /files/hello.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 200"), "response: {text}");
    assert!(text.contains("hello world"));

    server.abort();
}

// ---- sendfile tests ----

/// 9 MiB of non-zero, non-repeating bytes.
///
/// A placeholder leak would surface as zeros, and a mis-ordered or duplicated
/// range would surface as a byte mismatch, so an exact comparison proves the
/// response really came from `sendfile(2)`.
///
/// The size has to exceed the placeholder buffer's frame length (4 MiB) so the
/// body spans several frames: that is what exercises the stream's cross-frame
/// offset and remaining-length accounting. Raise it whenever that buffer grows.
fn sendfile_payload() -> Vec<u8> {
    (0..9 * 1024 * 1024_u32)
        .map(|index| (index % 251) as u8 + 1)
        .collect()
}

/// A few hundred non-zero bytes, for the same reason as [`sendfile_payload`].
fn small_payload() -> Vec<u8> {
    (0..700_u32).map(|index| (index % 251) as u8 + 1).collect()
}

/// Downloads `name` over a real connection and returns the head and body.
async fn download(addr: std::net::SocketAddr, name: &str, extra: &str) -> (String, Vec<u8>) {
    use tokio::io::AsyncWriteExt;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /files/{name} HTTP/1.1\r\nHost: localhost\r\n{extra}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    read_response(&mut stream).await
}

/// 起一个真正的 sendfile 服务：直接复用生产里的 `serve`，它自己跑 accept 循环、
/// 给每条连接装上 `SendfileStream` 并把槽位交给 handler。
async fn serve_with_sendfile(root: PathBuf) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let access_log = Arc::new(AccessLog::Tracing);
    let router = build_router(root.clone(), 8000, Arc::clone(&access_log));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        // 服务循环不返回；这里忽略它的 `io::Result`
        let _ = serve(listener, root, access_log, router).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    (addr, server)
}

/// Reads one response, using `Content-Length` to find the end of the body.
async fn read_response(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    use tokio::io::AsyncReadExt;

    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await.unwrap();
        assert_ne!(read, 0, "connection closed while reading the head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let len = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).await.unwrap();
    (head, body)
}

#[tokio::test]
async fn large_file_is_served_byte_for_byte_over_sendfile() {
    let dir = TestDir::new();
    let payload = sendfile_payload();
    std::fs::write(dir.root().join("big.bin"), &payload).unwrap();
    let (addr, server) = serve_with_sendfile(dir.root().to_path_buf()).await;

    let (head, body) = download(addr, "big.bin", "").await;
    server.abort();

    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("content-length: {}", payload.len())),
        "head: {head}"
    );
    assert_eq!(body, payload, "body must be the exact file contents");
}

#[tokio::test]
async fn small_file_is_served_byte_for_byte_over_sendfile() {
    let dir = TestDir::new();
    let payload = small_payload();
    std::fs::write(dir.root().join("small.bin"), &payload).unwrap();
    let (addr, server) = serve_with_sendfile(dir.root().to_path_buf()).await;

    let (head, body) = download(addr, "small.bin", "").await;
    server.abort();

    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
    // The body is a placeholder unless the transport really sent the file, so
    // an exact match also proves small files take the `sendfile` path.
    assert_eq!(body, payload, "small files must also be served by sendfile");
}

/// A run of small responses must not pay a per-response stall.
///
/// 一串小响应不能每个都卡一下。
///
/// The transport writes the head and the body as two separate writes, and when
/// the body is smaller than the MSS Nagle holds the second write back until the
/// peer's delayed ACK fires (about 40ms on Linux); the measured median for a
/// small response went from 0.2ms to 43ms. A fresh connection stays in Linux's
/// quick-ACK mode and hides the stall, so this reuses one keep-alive connection
/// to let the delayed ACK take effect.
///
/// 传输层把响应头与 body 分两次写出，当 body 小于 MSS 时 Nagle 会压住第二次写，
/// 直到对端 delayed ACK 超时（Linux 上约 40ms）；实测小响应中位数从 0.2ms 变成
/// 43ms。新连接仍处于 Linux 的 quick-ACK 模式，会掩盖这个停顿，所以这里复用同一条
/// keep-alive 连接，让 delayed ACK 生效。
#[tokio::test]
async fn small_response_body_is_not_held_by_nagle() {
    use tokio::io::AsyncWriteExt;

    let payload = small_payload();
    let dir = TestDir::new();
    std::fs::write(dir.root().join("small.bin"), &payload).unwrap();
    let (addr, server) = serve_with_sendfile(dir.root().to_path_buf()).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request =
        b"GET /files/small.bin HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";

    // 10 healthy responses take about 2ms; a stall per response takes about 430ms.
    let started = std::time::Instant::now();
    for _ in 0..10 {
        stream.write_all(request).await.unwrap();
        let (head, body) = read_response(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert_eq!(body, payload);
    }
    let elapsed = started.elapsed();

    server.abort();

    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "10 keep-alive responses took {elapsed:?}: Nagle is holding every small body until the delayed ACK fires"
    );
}

#[tokio::test]
async fn range_request_over_sendfile_returns_the_exact_slice() {
    let dir = TestDir::new();
    let payload = sendfile_payload();
    std::fs::write(dir.root().join("big.bin"), &payload).unwrap();
    let (addr, server) = serve_with_sendfile(dir.root().to_path_buf()).await;

    // 6 MiB wide, so the slice crosses a 4 MiB placeholder frame boundary and the
    // stream's offset arithmetic is exercised across frames rather than within one.
    let (start, end) = (1_000_000_usize, 7_000_000_usize);
    let (head, body) = download(
        addr,
        "big.bin",
        &format!("Range: bytes={start}-{}\r\n", end - 1),
    )
    .await;
    server.abort();

    assert!(head.starts_with("HTTP/1.1 206"), "head: {head}");
    assert_eq!(body, payload[start..end].to_vec());
}

#[tokio::test]
async fn sendfile_response_keeps_the_connection_reusable() {
    use tokio::io::AsyncWriteExt;

    let dir = TestDir::new();
    let payload = sendfile_payload();
    std::fs::write(dir.root().join("big.bin"), &payload).unwrap();
    let (addr, server) = serve_with_sendfile(dir.root().to_path_buf()).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /files/big.bin HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let (head, body) = read_response(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
    assert_eq!(body, payload);

    // The stream must be back to pass-through for the next response on the same
    // connection, or the JSON below would be swallowed as file content.
    stream
        .write_all(b"GET /api/list HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (head, body) = read_response(&mut stream).await;
    server.abort();

    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["path"], "/");
}

// ---- Frontend page tests ----

#[tokio::test]
async fn root_returns_html_page() {
    let dir = TestDir::new();
    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/").send(router).await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains("<title>文件浏览</title>"));
    assert!(body.contains("/static/style.css"));
    assert!(body.contains("/static/app.js"));
}

#[tokio::test]
async fn static_serves_css() {
    let dir = TestDir::new();
    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/static/style.css")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains("border-box"));
}

// ---- Zip download tests ----

#[tokio::test]
async fn api_zip_streams_folder() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();
    std::fs::write(dir.root().join("a.txt"), "hello").unwrap();
    std::fs::write(dir.root().join("sub/b.txt"), "world").unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/zip")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.starts_with("PK\x03\x04"));
    assert!(body.contains("a.txt"));
    assert!(body.contains("sub/b.txt"));
}

/// 统计 `needle` 在 `haystack` 中出现的次数。
fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// 分块流水线必须把每个文件的内容完整、连续地写进 zip。
///
/// 载荷大于单个读取分块（256 KiB），两段内容互不相同：内容被截断、分块串到别的
/// 条目上、或条目整个丢失，都会让下面的断言失败。空文件走的是「只有 `FileStart` 与
/// `FileEnd`、没有 `Chunk`」的路径，单独断言它的条目仍在。
#[tokio::test]
async fn api_zip_streams_each_file_intact() {
    const CHUNKED_SIZE: usize = 3 * 512 * 1024;

    let dir = TestDir::new();
    let a: Vec<u8> = (0..CHUNKED_SIZE as u32)
        .map(|index| (index % 251) as u8 + 1)
        .collect();
    let b: Vec<u8> = a.iter().map(|byte| byte.wrapping_add(100)).collect();
    std::fs::write(dir.root().join("a.bin"), &a).unwrap();
    std::fs::write(dir.root().join("b.bin"), &b).unwrap();
    std::fs::write(dir.root().join("empty.bin"), b"").unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/zip")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_bytes(None).await.unwrap();

    // Stored 压缩，文件内容原样落在 zip 流里
    assert_eq!(
        count_occurrences(&body, &a),
        1,
        "a.bin 的内容必须完整且只出现一次"
    );
    assert_eq!(
        count_occurrences(&body, &b),
        1,
        "b.bin 的内容必须完整且只出现一次"
    );
    assert!(
        count_occurrences(&body, b"empty.bin") >= 1,
        "空文件也应当保留条目"
    );
}

#[tokio::test]
async fn api_zip_returns_404_for_missing() {
    let dir = TestDir::new();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/api/zip/nope")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn api_zip_rejects_path_traversal() {
    let dir = TestDir::new();
    let router = api_router(dir.root().to_path_buf());
    let res = TestClient::get("http://127.0.0.1:5800/api/zip/%2e%2e")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn api_zip_includes_dot_files_and_dirs() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join(".hidden"), "secret").unwrap();
    std::fs::create_dir_all(dir.root().join(".git")).unwrap();
    std::fs::write(dir.root().join(".git/config"), "cfg").unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/zip")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains(".hidden"), "body: {body}");
    assert!(body.contains(".git/config"), "body: {body}");
}

#[tokio::test]
async fn api_zip_preserves_empty_directory() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("empty")).unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/zip")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains("empty/"), "body: {body}");
}

#[cfg(unix)]
#[tokio::test]
async fn api_zip_skips_unreadable_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("locked")).unwrap();
    std::fs::write(dir.root().join("locked/secret.txt"), "secret").unwrap();
    std::fs::set_permissions(
        dir.root().join("locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();

    let router = api_router(dir.root().to_path_buf());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/zip")
        .send(router)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let _ = res.take_string().await;

    // 恢复权限，避免 TestDir 清理失败
    std::fs::set_permissions(
        dir.root().join("locked"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}
