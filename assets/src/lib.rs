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

pub struct ServeFiles {
    root: PathBuf,
}

impl ServeFiles {
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// 解析请求路径对应的绝对文件路径，且必须位于 root 之内（防目录穿越）。
///
/// 与 salvo `StaticDir` 一致：用 `symlink_metadata` 判断类型，符号链接不会被当作
/// 文件服务，从而避免 canonicalize 跟随符号链接绕过 dotfile 或引入 TOCTOU 窗口。
async fn resolve_file(root: &Path, sub: &str) -> Option<PathBuf> {
    let joined = root.join(sub);
    let metadata = tokio::fs::symlink_metadata(&joined).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    let canonical = tokio::fs::canonicalize(&joined).await.ok()?;
    canonical.starts_with(root).then_some(canonical)
}

#[handler]
impl ServeFiles {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let sub = req.param::<String>("path").unwrap_or_default();

        // 与 StaticDir 一致：在路径解析前按请求名跳过 dotfile，
        // 避免 canonicalize 跟随符号链接后改用目标名判定而被绕过。
        let is_dot_file = Path::new(&sub)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with('.'));
        if is_dot_file {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        let Some(abs_path) = resolve_file(&self.root, &sub).await else {
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
