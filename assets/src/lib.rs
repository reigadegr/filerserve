use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::sync::Arc;

use lanfile_namedfile::NamedFile;
use lanfile_sendfile::{duplicate_file, upgrade_response};
use rust_embed::RustEmbed;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::fd::OwnedFd;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::fs::{self as rfs, Advice, Mode, OFlags, ResolveFlags};
use salvo::{
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
    /// root 的目录 fd：`openat2` 相对它解析路径，越界由内核直接拦下
    #[cfg(any(target_os = "linux", target_os = "android"))]
    root_fd: Option<Arc<OwnedFd>>,
}

impl ServeFiles {
    #[must_use]
    fn new(root: PathBuf) -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            root_fd: rfs::open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .ok()
            .map(Arc::new),
            root,
        }
    }

    /// 打开请求路径对应的文件，且必须位于 root 之内（防目录穿越）。
    ///
    /// 先用 `symlink_metadata` 判断类型，符号链接不会被当作文件服务；
    /// 再让内核用一次 `openat2(RESOLVE_BENEATH)` 同时完成路径解析、越界检查与打开，
    /// 省掉 `canonicalize` 对每一层路径各一次的 `readlink`。`openat2` 不可用时
    /// （旧内核、被 SELinux/seccomp 拦截等）回退到原来的 canonicalize 逻辑，
    /// 因此对外行为与改动前一致。
    fn open(&self, sub: &str) -> Option<(PathBuf, File)> {
        let joined = self.root.join(sub);
        let metadata = std::fs::symlink_metadata(&joined).ok()?;
        if !metadata.is_file() {
            return None;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(file) = self.root_fd.as_ref().and_then(|fd| open_beneath(fd, sub)) {
            return Some((joined, file));
        }
        open_via_canonicalize(&self.root, &joined).map(|file| (joined, file))
    }
}

/// 一次 `openat2` 完成路径解析、越界检查与打开。
///
/// `RESOLVE_BENEATH` 要求解析结果不得越出 `root_fd`，`O_NOFOLLOW` 保证末级不是符号链接。
/// 任何失败都返回 `None`，交给调用方回退到 canonicalize。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_beneath(root_fd: &OwnedFd, sub: &str) -> Option<File> {
    rfs::openat2(
        root_fd,
        sub,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
    )
    .ok()
    .map(File::from)
}

/// `openat2` 不可用时的回退路径：先 canonicalize 再打开，与改动前的行为一致。
fn open_via_canonicalize(root: &Path, joined: &Path) -> Option<File> {
    let canonical = std::fs::canonicalize(joined).ok()?;
    if !canonical.starts_with(root) {
        return None;
    }
    File::open(canonical).ok()
}

#[handler]
impl ServeFiles {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let sub = req.param::<String>("path").unwrap_or_default();

        // 路径解析直接在 worker 上做：只有 lstat + openat2，命中页缓存时是微秒级，
        // 而 spawn_blocking 的线程交接本身就要几十微秒，还得分摊 blocking pool 的全局锁。
        // 用阻塞线程池反而更慢：压测显示这一次 spawn_blocking 就占掉每请求约 7 次 futex 等待
        let Some((path, file)) = self.open(&sub) else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        // 关闭 NamedFile 的小文件预读：预读会把内容读进用户态，而 sendfile 直接从页缓存发，
        // 那次读纯属浪费；关掉后所有响应体都交给 sendfile，HEAD 本来也不需要预读
        let Ok(named_file) = NamedFile::builder(path)
            .preload_threshold(0)
            .build_from_file(file)
            .await
        else {
            res.render(StatusError::internal_server_error().brief("read file failed"));
            return;
        };
        // 内核顺序读提示：扩大预读窗口，大文件连续传输更快；仅设置标志、立即返回
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let _ = rfs::fadvise(named_file.file(), 0, None, Advice::Sequential);
        if req.method() == Method::HEAD {
            named_file.send_head(req.headers(), res).await;
            return;
        }

        // `send` 会消费原文件，先复制一份描述符供 sendfile 使用
        let sendfile_file = duplicate_file(named_file.file());
        named_file.send(req.headers(), res).await;

        // 命中条件时把响应体换成零拷贝体，否则保持 NamedFile 的普通响应体
        if let Some(file) = sendfile_file {
            upgrade_response(req, res, file);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时 root：`root/ok.txt`、`root/sub/deep.txt`，以及 root 之外的一个文件用于穿越测试
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> std::io::Result<Self> {
            let base =
                std::env::temp_dir().join(format!("lanfile-assets-{tag}-{}", std::process::id()));
            let root = base.join("root");
            std::fs::create_dir_all(root.join("sub"))?;
            std::fs::write(root.join("ok.txt"), b"hello")?;
            std::fs::write(root.join("sub/deep.txt"), b"deep")?;
            let outside = base.join("outside.txt");
            std::fs::write(&outside, b"secret")?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside, root.join("outside-link.txt"))?;
            Ok(Self { base, root })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// `openat2` 是快路径，结论必须和改动前的逻辑（lstat 判类型 + canonicalize 判越界）逐条一致
    #[test]
    fn open_keeps_previous_behaviour() -> std::io::Result<()> {
        let fixture = Fixture::new("parity")?;
        let files = ServeFiles::new(fixture.root.clone());
        for sub in [
            "ok.txt",
            "sub/deep.txt",
            "missing.txt",
            "sub",
            "outside-link.txt",
            "../outside.txt",
        ] {
            let joined = fixture.root.join(sub);
            let is_file = std::fs::symlink_metadata(&joined).is_ok_and(|meta| meta.is_file());
            let expected = is_file && open_via_canonicalize(&fixture.root, &joined).is_some();
            assert_eq!(
                files.open(sub).is_some(),
                expected,
                "{sub} 的结论必须与改动前一致"
            );
        }
        // 目录、符号链接、越界路径都必须拒绝
        assert!(files.open("sub").is_none(), "目录不是文件");
        assert!(files.open("outside-link.txt").is_none(), "符号链接不服务");
        assert!(files.open("../outside.txt").is_none(), "不得穿越出 root");
        // 回退路径本身必须可用：手机上 openat2 可能被 SELinux/seccomp 拦下
        assert!(
            open_via_canonicalize(&fixture.root, &fixture.root.join("ok.txt")).is_some(),
            "回退路径必须能打开普通文件"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn openat2_handles_ordinary_paths() -> std::io::Result<()> {
        let fixture = Fixture::new("fast")?;
        let files = ServeFiles::new(fixture.root.clone());
        let Some(fd) = files.root_fd.as_ref() else {
            panic!("root 目录 fd 应当打开成功");
        };
        assert!(
            open_beneath(fd, "ok.txt").is_some(),
            "快路径应当能打开普通文件"
        );
        assert!(
            open_beneath(fd, "sub/deep.txt").is_some(),
            "快路径应当能打开子目录里的文件"
        );
        assert!(
            open_beneath(fd, "../outside.txt").is_none(),
            "快路径必须拦下目录穿越"
        );
        assert!(
            open_beneath(fd, "outside-link.txt").is_none(),
            "O_NOFOLLOW 必须拦下符号链接"
        );
        Ok(())
    }
}
