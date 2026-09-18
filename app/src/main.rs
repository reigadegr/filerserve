use chrono::Local;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use salvo::{
    async_trait,
    fs::NamedFile,
    http::{StatusCode, StatusError},
    prelude::*,
    writing::Redirect,
};
use std::{
    fmt::{self, Write as _},
    io::IsTerminal,
    path::{Path, PathBuf},
    time::SystemTime,
};
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

/// 将原始请求路径（仍为百分号编码）解析到 `root` 下的文件系统路径。
///
/// 解码、规范化后若路径逃逸出 `root`（如 `../`），或无法按 UTF-8 解码，
/// 则返回 `None`。
fn resolve_path(root: &Path, raw_path: &str) -> Option<PathBuf> {
    let path = raw_path.split(['?', '#']).next().unwrap_or(raw_path);
    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }
    let mut resolved = root.to_path_buf();
    for segment in segments {
        resolved.push(segment);
    }
    Some(resolved)
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_time(modified: SystemTime) -> String {
    let datetime: chrono::DateTime<Local> = modified.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

async fn list_directory(dir: &Path, url_path: &str) -> std::io::Result<String> {
    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        entries.push(entry);
    }
    entries.sort_by_key(tokio::fs::DirEntry::file_name);

    let mut rows = String::new();
    if url_path != "/" {
        let trimmed = url_path.trim_end_matches('/');
        let parent = &trimmed[..trimmed.rfind('/').map_or(0, |idx| idx + 1)];
        let _ = writeln!(
            rows,
            "<tr><td><a href=\"{parent}\">../</a></td><td></td><td></td></tr>"
        );
    }
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let encoded = utf8_percent_encode(&name, NON_ALPHANUMERIC).to_string();
        let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
        let suffix = if is_dir { "/" } else { "" };
        let (size, modified) = entry
            .metadata()
            .await
            .ok()
            .map(|meta| {
                let size = if is_dir {
                    "-".to_string()
                } else {
                    human_size(meta.len())
                };
                let modified = meta.modified().ok().map(format_time).unwrap_or_default();
                (size, modified)
            })
            .unwrap_or_default();
        let _ = writeln!(
            rows,
            "<tr><td><a href=\"{url_path}{encoded}{suffix}\">{} {}</a></td><td>{}</td><td>{}</td></tr>",
            escape_html(&name),
            suffix,
            modified,
            size,
        );
    }

    let title = format!("Directory listing for {url_path}");
    Ok(format!(
        "<!DOCTYPE html>\n\
         <html lang=\"zh-CN\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         <style>\n\
         body {{ font-family: system-ui, -apple-system, sans-serif; margin: 2em auto; max-width: 60em; }}\n\
         h1 {{ font-size: 1.3em; }}\n\
         table {{ border-collapse: collapse; width: 100%; }}\n\
         th, td {{ text-align: left; padding: .35em .6em; border-bottom: 1px solid #eee; }}\n\
         th {{ border-bottom: 2px solid #ddd; }}\n\
         a {{ text-decoration: none; color: #0366d6; }}\n\
         a:hover {{ text-decoration: underline; }}\n\
         .size {{ color: #888; }}\n\
         </style>\n\
         </head>\n\
         <body>\n\
         <h1>{}</h1>\n\
         <hr>\n\
         <table>\n\
         <tr><th>名称</th><th>修改时间</th><th>大小</th></tr>\n\
         {}\
         </table>\n\
         <hr>\n\
         </body>\n\
         </html>\n",
        escape_html(&title),
        escape_html(&title),
        rows,
    ))
}

#[derive(Clone)]
struct ServeDir {
    root: PathBuf,
}

impl ServeDir {
    const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    async fn send_file(&self, path: &Path, req: &Request, res: &mut Response) {
        match NamedFile::builder(path).build().await {
            Ok(file) => file.send(req.headers(), res).await,
            Err(_) => res.render(StatusError::internal_server_error()),
        }
    }
}

#[async_trait]
impl Handler for ServeDir {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let raw_path = req.uri().path();
        let Some(path) = resolve_path(&self.root, raw_path) else {
            res.render(StatusError::not_found());
            return;
        };

        let Ok(meta) = tokio::fs::metadata(&path).await else {
            res.render(StatusError::not_found());
            return;
        };

        if meta.is_dir() {
            if !raw_path.ends_with('/') {
                let target = format!("{raw_path}/");
                if let Ok(redirect) =
                    Redirect::with_status_code(StatusCode::MOVED_PERMANENTLY, target)
                {
                    res.render(redirect);
                }
                return;
            }
            for index in ["index.html", "index.htm"] {
                let index_path = path.join(index);
                if let Ok(index_meta) = tokio::fs::metadata(&index_path).await
                    && index_meta.is_file()
                {
                    self.send_file(&index_path, req, res).await;
                    return;
                }
            }
            match list_directory(&path, raw_path).await {
                Ok(html) => res.render(Text::Html(html)),
                Err(_) => res.render(StatusError::internal_server_error()),
            }
        } else {
            self.send_file(&path, req, res).await;
        }
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
    let router = Router::with_path("{**rest}")
        .get(ServeDir::new(root.clone()))
        .head(ServeDir::new(root.clone()));

    tracing::info!("serving {} on http://{addr}", root.display());
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
                .get(ServeDir::new(root.clone()))
                .head(ServeDir::new(root)),
        )
    }

    #[test]
    fn resolve_path_normalizes() {
        let root = Path::new("/srv");
        assert_eq!(
            resolve_path(root, "/a/b.txt"),
            Some(PathBuf::from("/srv/a/b.txt"))
        );
        assert_eq!(
            resolve_path(root, "/a/./b.txt"),
            Some(PathBuf::from("/srv/a/b.txt"))
        );
        assert_eq!(
            resolve_path(root, "/a/../b.txt"),
            Some(PathBuf::from("/srv/b.txt"))
        );
        assert_eq!(
            resolve_path(root, "/a/%20b.txt"),
            Some(PathBuf::from("/srv/a/ b.txt"))
        );
        assert_eq!(resolve_path(root, "/../etc/passwd"), None);
        assert_eq!(resolve_path(root, "/.."), None);
    }

    #[tokio::test]
    async fn serves_index_html_at_root() {
        let dir = TestDir::new();
        std::fs::write(dir.root().join("index.html"), "<h1>hello</h1>").unwrap();
        let router = router(dir.root());
        let mut res = TestClient::get("http://127.0.0.1:5800/")
            .send(router.clone())
            .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        assert!(res.take_string().await.unwrap().contains("<h1>hello</h1>"));
    }

    #[tokio::test]
    async fn lists_directory_without_index() {
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
        assert!(body.contains("sub/"));
    }

    #[tokio::test]
    async fn redirects_dir_without_trailing_slash() {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.root().join("sub")).unwrap();
        let router = router(dir.root());
        let res = TestClient::get("http://127.0.0.1:5800/sub")
            .send(router.clone())
            .await;
        assert_eq!(res.status_code, Some(StatusCode::MOVED_PERMANENTLY));
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
    async fn rejects_path_traversal() {
        let dir = TestDir::new();
        std::fs::write(dir.root().join("secret.txt"), "top secret").unwrap();
        let router = router(dir.root());
        let res = TestClient::get("http://127.0.0.1:5800/%2e%2e%2fsecret.txt")
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
        std::fs::write(dir.root().join("index.html"), "<h1>hello</h1>").unwrap();
        let router = std::sync::Arc::new(
            Router::with_path("{**rest}")
                .get(ServeDir::new(dir.root()))
                .head(ServeDir::new(dir.root())),
        );
        let acceptor = TcpListener::new("127.0.0.1:0").bind().await;
        let addr = acceptor.local_addr().unwrap();
        let server = tokio::spawn(async move {
            Server::new(acceptor).serve(router).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 200"), "response: {text}");
        assert!(text.contains("<h1>hello</h1>"));

        server.abort();
    }
}
