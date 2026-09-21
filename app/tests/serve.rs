#![allow(clippy::unwrap_used)]

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use lanfile::build_router;
use lanfile_sendfile::SendfileListener;
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
    Arc::new(build_router(root, 8000))
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

/// 3 MiB of non-zero, non-repeating bytes.
///
/// A placeholder leak would surface as zeros, and a mis-ordered or duplicated
/// range would surface as a byte mismatch, so an exact comparison proves the
/// response really came from `sendfile(2)`.
fn sendfile_payload() -> Vec<u8> {
    (0..3 * 1024 * 1024_u32)
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

async fn serve_with_sendfile(root: PathBuf) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let router = api_router(root);
    let acceptor = SendfileListener::new(TcpListener::new("127.0.0.1:0"))
        .bind()
        .await;
    let addr = acceptor.local_addr().unwrap();
    let server = tokio::spawn(async move {
        Server::new(acceptor).serve(router).await;
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

#[tokio::test]
async fn range_request_over_sendfile_returns_the_exact_slice() {
    let dir = TestDir::new();
    let payload = sendfile_payload();
    std::fs::write(dir.root().join("big.bin"), &payload).unwrap();
    let (addr, server) = serve_with_sendfile(dir.root().to_path_buf()).await;

    let (start, end) = (500_000_usize, 2_600_000_usize);
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
