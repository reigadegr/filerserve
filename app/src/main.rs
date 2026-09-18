use chrono::Local;
use salvo::{
    prelude::*,
    routing::{Filter, filters},
    serve_static::StaticDir,
};
use std::{fmt, io::IsTerminal, path::PathBuf};
use tracing_subscriber::{
    EnvFilter,
    fmt::{format::Writer, time::FormatTime},
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct LoggerFormatter;

impl FormatTime for LoggerFormatter {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

#[tokio::main]
async fn main() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let is_terminal = std::io::stdout().is_terminal();

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(LoggerFormatter)
        .with_ansi(is_terminal)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = args
        .first()
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(8000);
    let dir = args
        .get(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let root = std::fs::canonicalize(&dir).unwrap_or_else(|error| {
        tracing::error!("无法访问目录 {:?}: {error}", dir);
        std::process::exit(1);
    });

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("serving {} on http://{addr}", root.display());
    let router = Router::with_path("{**rest}")
        .filter(filters::get().or(filters::head()))
        .goal(StaticDir::new(root).auto_list(true));

    let acceptor = TcpListener::new(addr).bind().await;
    Server::new(acceptor).serve(router).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use salvo::http::header;
    use salvo::test::{ResponseExt, TestClient};
    use std::sync::atomic::{AtomicU32, Ordering};

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

    fn router(root: PathBuf) -> std::sync::Arc<Router> {
        std::sync::Arc::new(
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
        let router = std::sync::Arc::new(
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
}
