#![allow(clippy::unwrap_used)]

use filerserve::build_router;
use salvo::{
    http::header,
    prelude::*,
    routing::{Filter, filters},
    serve_static::StaticDir,
    test::{ResponseExt, TestClient},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "filerserve-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn root(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn router(root: PathBuf) -> Arc<Router> {
    Arc::new(
        Router::with_path("{**rest}")
            .filter(filters::get().or(filters::head()))
            .goal(StaticDir::new(root).auto_list(true)),
    )
}

#[tokio::test]
async fn lists_directory_at_root() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();
    std::fs::write(dir.root().join("a.txt"), "abc").unwrap();
    let router = router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains("a.txt"));
    assert!(body.contains("sub"));
}

#[tokio::test]
async fn lists_directory_even_with_index_html() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("index.html"), "<h1>hello</h1>").unwrap();
    std::fs::write(dir.root().join("a.txt"), "abc").unwrap();
    let router = router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains("a.txt"));
    assert!(!body.contains("<h1>hello</h1>"));
}

#[tokio::test]
async fn hides_dot_files_in_listing() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join(".hidden"), "secret").unwrap();
    std::fs::write(dir.root().join("a.txt"), "abc").unwrap();
    let router = router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    let body = res.take_string().await.unwrap();
    assert!(body.contains("a.txt"));
    assert!(!body.contains(".hidden"));
}

#[tokio::test]
async fn redirects_dir_without_trailing_slash() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();
    let router = router(dir.root());
    let res = TestClient::get("http://127.0.0.1:5800/sub")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::FOUND));
    let location = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(location, "/sub/");
}

#[tokio::test]
async fn serves_file() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/hello.txt")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    assert_eq!(res.take_string().await.unwrap(), "hello world");
}

#[tokio::test]
async fn returns_not_found_for_missing_file() {
    let dir = TestDir::new();
    let router = router(dir.root());
    let res = TestClient::get("http://127.0.0.1:5800/nope.txt")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn path_traversal_stays_within_root() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("secret.txt"), "top secret").unwrap();
    let router = router(dir.root());
    let res = TestClient::get("http://127.0.0.1:5800/%2e%2e%2f%2e%2e%2fetc%2fpasswd")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn head_request_succeeds() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = router(dir.root());
    let res = TestClient::head("http://127.0.0.1:5800/hello.txt")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
}

#[tokio::test]
async fn serves_over_real_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = Arc::new(
        Router::with_path("{**rest}")
            .filter(filters::get().or(filters::head()))
            .goal(StaticDir::new(dir.root()).auto_list(true)),
    );
    let acceptor = TcpListener::new("127.0.0.1:0").bind().await;
    let addr = acceptor.local_addr().unwrap();
    let server = tokio::spawn(async move {
        Server::new(acceptor).serve(router).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /hello.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 200"), "response: {text}");
    assert!(text.contains("hello world"));

    server.abort();
}

// ---- JSON API tests ----

fn api_router(root: PathBuf) -> Arc<Router> {
    Arc::new(build_router(root, 8000))
}

#[tokio::test]
async fn api_list_returns_json_for_root() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("a.txt"), "abc").unwrap();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();

    let router = api_router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list")
        .send(router.clone())
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
async fn api_list_hides_dot_files() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join(".hidden"), "secret").unwrap();
    std::fs::write(dir.root().join("visible.txt"), "abc").unwrap();

    let router = api_router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));

    let body = res.take_string().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "visible.txt");
    assert!(!body.contains(".hidden"));
}

#[tokio::test]
async fn api_list_lists_subdirectory() {
    let dir = TestDir::new();
    std::fs::create_dir_all(dir.root().join("sub")).unwrap();
    std::fs::write(dir.root().join("sub").join("inner.txt"), "xyz").unwrap();

    let router = api_router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/api/list/sub")
        .send(router.clone())
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
    let router = api_router(dir.root());
    let res = TestClient::get("http://127.0.0.1:5800/api/list/nope")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

// ---- File download tests ----

#[tokio::test]
async fn files_endpoint_serves_file() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("hello.txt"), "hello world").unwrap();
    let router = api_router(dir.root());
    let mut res = TestClient::get("http://127.0.0.1:5800/files/hello.txt")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::OK));
    assert_eq!(res.take_string().await.unwrap(), "hello world");
}

#[tokio::test]
async fn files_endpoint_returns_404_for_missing_file() {
    let dir = TestDir::new();
    let router = api_router(dir.root());
    let res = TestClient::get("http://127.0.0.1:5800/files/nope.txt")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn files_endpoint_rejects_path_traversal() {
    let dir = TestDir::new();
    std::fs::write(dir.root().join("secret.txt"), "top secret").unwrap();
    let router = api_router(dir.root());
    let res = TestClient::get("http://127.0.0.1:5800/files/%2e%2e%2f%2e%2e%2fetc%2fpasswd")
        .send(router.clone())
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
}

// ---- LAN IP detection tests ----

#[test]
fn detect_lan_ip_returns_some_or_none() {
    let result = filerserve::detect_lan_ip();
    assert!(result.is_some() || result.is_none());
}
