use std::path::{Path, PathBuf};

use rust_embed::RustEmbed;
use rustix::fs::{self as rfs, Advice};
use salvo::{
    fs::NamedFile,
    http::Method,
    prelude::*,
    routing::{Filter, filters},
    serve_static::static_embed,
};

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct Asset;

struct ServeFiles {
    root: PathBuf,
}

impl ServeFiles {
    #[must_use]
    const fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// 解析请求路径对应的绝对文件路径，且必须位于 root 之内（防目录穿越）。
///
/// 用 `symlink_metadata` 判断类型，符号链接不会被当作文件服务，
/// 避免 canonicalize 跟随符号链接逃逸 root 或引入 TOCTOU 窗口。
/// 全程是同步阻塞的 fs 操作，应由调用方放进 `spawn_blocking`。
fn resolve_file(root: &Path, sub: &str) -> Option<PathBuf> {
    let joined = root.join(sub);
    let metadata = std::fs::symlink_metadata(&joined).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let canonical = std::fs::canonicalize(&joined).ok()?;
    canonical.starts_with(root).then_some(canonical)
}

#[handler]
impl ServeFiles {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let sub = req.param::<String>("path").unwrap_or_default();

        // 路径解析是阻塞的 fs 操作，整体放进阻塞线程池，避免拖慢异步 worker
        let root = self.root.clone();
        let Some(abs_path) = tokio::task::spawn_blocking(move || resolve_file(&root, &sub))
            .await
            .ok()
            .flatten()
        else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        let mut builder = NamedFile::builder(abs_path);
        if req.method() == Method::HEAD {
            builder = builder.preload_threshold(0);
        }
        let Ok(named_file) = builder.build().await else {
            res.render(StatusError::internal_server_error().brief("read file failed"));
            return;
        };
        // 内核顺序读提示：扩大预读窗口，大文件连续传输更快；仅设置标志、立即返回
        let _ = rfs::fadvise(named_file.file(), 0, None, Advice::Sequential);
        if req.method() == Method::HEAD {
            named_file.send_head(req.headers(), res).await;
        } else {
            named_file.send(req.headers(), res).await;
        }
    }
}

#[must_use]
pub fn static_routes(root: PathBuf) -> Router {
    Router::new()
        .push(
            Router::with_path("/files/{**path}")
                .filter(filters::get().or(filters::head()))
                .goal(ServeFiles::new(root)),
        )
        .push(
            Router::new()
                .filter(filters::get())
                .goal(static_embed::<Asset>().fallback("index.html")),
        )
        .push(
            Router::with_path("/static/{**path}")
                .filter(filters::get())
                .goal(static_embed::<Asset>()),
        )
}
