use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lanfile_namedfile::NamedFile;
use lanfile_sendfile::upgrade_response;
use mime::Mime;
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

#[cfg(any(target_os = "linux", target_os = "android"))]
mod file_cache;

#[cfg(any(target_os = "linux", target_os = "android"))]
use file_cache::FileCache;

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct Asset;

struct ServeFiles {
    root: PathBuf,
    /// root 的目录 fd：`openat2` 相对它解析路径，越界由内核直接拦下
    #[cfg(any(target_os = "linux", target_os = "android"))]
    root_fd: Option<Arc<OwnedFd>>,
    /// 是否允许调用 `openat2`：装了 seccomp filter 的环境里它不在白名单，调用即被 SIGSYS 杀死
    #[cfg(any(target_os = "linux", target_os = "android"))]
    openat2_allowed: bool,
    /// 已打开文件的缓存：命中时省掉 openat、4 次 readlink 与 fadvise
    #[cfg(any(target_os = "linux", target_os = "android"))]
    cache: FileCache,
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
            #[cfg(any(target_os = "linux", target_os = "android"))]
            openat2_allowed: !seccomp_filter_installed(),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            cache: FileCache::default(),
            root,
        }
    }

    /// 打开请求路径对应的文件，且必须位于 root 之内（防目录穿越）。
    ///
    /// 先用 `symlink_metadata` 判断类型，符号链接不会被当作文件服务；这一步同时用来校验
    /// 缓存是否还有效。命中时直接给出缓存里的 fd、它的元数据与解析好的 `Content-Type`
    /// （第四个元素为 `Some`）。未命中才真正去解析路径：让内核用一次
    /// `openat2(RESOLVE_BENEATH)` 同时完成路径解析、越界检查与打开，省掉 `canonicalize`
    /// 对每一层路径各一次的 `readlink`。装了 seccomp filter 的环境（Android）根本不调用
    /// `openat2`（调用会被 SIGSYS 杀掉进程，见 [`seccomp_filter_installed`]），旧内核上它
    /// 会返回错误，两种情况都回退到 canonicalize，因此对外行为与改动前一致。
    fn open(&self, sub: &str) -> Option<(PathBuf, Arc<File>, Metadata, Option<Mime>)> {
        let joined = self.root.join(sub);
        let Ok(metadata) = std::fs::symlink_metadata(&joined) else {
            // 路径已经不存在了，顺手把缓存里占着的 fd 放掉
            #[cfg(any(target_os = "linux", target_os = "android"))]
            self.cache.remove(sub);
            return None;
        };
        if !metadata.is_file() {
            return None;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some((file, metadata, content_type)) = self.cache.get(sub, &metadata) {
            return Some((joined, file, metadata, Some(content_type)));
        }
        let file = self.open_uncached(sub, &joined)?;
        // 取这个 fd 自己的元数据：它会随缓存一起给出去，命中时就不必再 fstat 一次。
        // 缓存里必须记 fd 的属性而不是路径的 lstat，否则文件被换掉时会串味。
        let metadata = file.metadata().ok()?;
        // 内核顺序读提示：扩大预读窗口，大文件连续传输更快；仅设置标志、立即返回。
        // 提示作用在 fd 上，缓存命中的那个 fd 早就设过，所以只在未命中时调一次。
        // 一页以内的文件整个读完也只有一页，预读窗口开多大结果都一样，这次系统调用可以省掉。
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if metadata.len() > 4096 {
            let _ = rfs::fadvise(&file, 0, None, Advice::Sequential);
        }
        Some((joined, Arc::new(file), metadata, None))
    }

    /// 缓存未命中时真正去解析并打开文件（类型检查已由 [`Self::open`] 完成）。
    fn open_uncached(&self, sub: &str, joined: &Path) -> Option<File> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self.openat2_allowed
            && let Some(file) = self.root_fd.as_ref().and_then(|fd| open_beneath(fd, sub))
        {
            return Some(file);
        }
        open_via_canonicalize(&self.root, joined)
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

/// 判断当前进程是否装了 seccomp filter（`SECCOMP_MODE_FILTER`）。
///
/// Android 的 `untrusted_app` 域由 zygote 装一个系统调用白名单 filter，不在白名单里的调用
/// 会被 `SECCOMP_RET_TRAP` 处理：内核直接发 SIGSYS 杀掉进程，而不是返回错误码——手机上实测
/// `openat2` 就是这样（`si_code=1` 即 `SYS_SECCOMP`，进程立即终止），"失败就回退"来不及生效。
/// 读不到状态时按"装了"处理：猜错的代价是进程被杀。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn seccomp_filter_installed() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return true;
    };
    has_seccomp_filter(&status)
}

/// `/proc/self/status` 里 `Seccomp:` 为 2 即 `SECCOMP_MODE_FILTER`
#[cfg(any(target_os = "linux", target_os = "android"))]
fn has_seccomp_filter(status: &str) -> bool {
    status.lines().any(|line| {
        line.strip_prefix("Seccomp:")
            .is_some_and(|value| value.trim() == "2")
    })
}

#[handler]
impl ServeFiles {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        // 直接从路由参数里借一个 &str：`param::<String>` 会为每个请求分配一个 String，
        // 再走一遍 serde 反序列化；通配参数就在这里，借出来就够了
        let sub = req.params().get("path").map_or("", String::as_str);

        // 路径解析直接在 worker 上做：只有 lstat + openat2，命中页缓存时是微秒级，
        // 而 spawn_blocking 的线程交接本身就要几十微秒，还得分摊 blocking pool 的全局锁。
        // 用阻塞线程池反而更慢：压测显示这一次 spawn_blocking 就占掉每请求约 7 次 futex 等待
        let Some((path, file, metadata, cached_type)) = self.open(sub) else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        // 关闭 NamedFile 的小文件预读：预读会把内容读进用户态，而 sendfile 直接从页缓存发，
        // 那次读纯属浪费；关掉后所有响应体都交给 sendfile，HEAD 本来也不需要预读
        let mut builder = NamedFile::builder(path).preload_threshold(0);
        let missed = cached_type.is_none();
        // 缓存命中时把上次解析好的类型直接交给它：需要 charset 的类型因此不必再读一次样本
        if let Some(content_type) = cached_type {
            builder = builder.content_type(content_type);
        }
        // 元数据跟着缓存一起给出来（未命中时是刚 fstat 的），所以这里不必再 fstat 一次
        let Ok(named_file) = builder
            .build_from_file_with_metadata(Arc::clone(&file), metadata.clone())
            .await
        else {
            res.render(StatusError::internal_server_error().brief("read file failed"));
            return;
        };
        // 未命中：把 fd、它的元数据和刚解析出来的类型一起存进缓存
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if missed {
            self.cache.insert(
                sub,
                Arc::clone(&file),
                metadata,
                named_file.content_type().clone(),
            );
        }
        if req.method() == Method::HEAD {
            named_file.send_head(req.headers(), res).await;
            return;
        }

        named_file.send(req.headers(), res).await;

        // 命中条件时把响应体换成零拷贝体，否则保持 NamedFile 的普通响应体。
        // 这里不再 dup：响应体直接共享缓存里那个 fd（sendfile 带显式 offset，共享描述符是安全的）
        upgrade_response(req, res, file);
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

    /// 手机上实测的 /proc/self/status 片段：`Seccomp:` 为 2 表示装了 filter，必须放弃 openat2
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn detects_seccomp_filter() {
        assert!(has_seccomp_filter(
            "Name:\tlanfile\nSeccomp:\t2\nSeccomp_filters:\t1\n"
        ));
        assert!(!has_seccomp_filter("Name:\tlanfile\nSeccomp:\t0\n"));
        // 只有 `Seccomp:` 字段本身算数，`Seccomp_filters:` 不算
        assert!(!has_seccomp_filter("Seccomp_filters:\t1\n"));
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

    /// 缓存命中时 `open` 要把 fd、元数据与类型一起给出来，构建时不再 fstat、也不再读嗅探样本
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn cache_hit_reuses_the_open_file() -> std::io::Result<()> {
        let fixture = Fixture::new("cache")?;
        let files = ServeFiles::new(fixture.root.clone());
        tokio::runtime::Builder::new_current_thread()
            .build()?
            .block_on(async {
                let Some((_, file, metadata, cached)) = files.open("ok.txt") else {
                    panic!("第一次应当打开成功");
                };
                assert!(cached.is_none(), "第一次不该命中");
                let named = NamedFile::builder(fixture.root.join("ok.txt"))
                    .preload_threshold(0)
                    .build_from_file_with_metadata(Arc::clone(&file), metadata.clone())
                    .await
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                files
                    .cache
                    .insert("ok.txt", file, metadata, named.content_type().clone());
                let Some((_, _, _, cached)) = files.open("ok.txt") else {
                    panic!("第二次应当打开成功");
                };
                assert!(cached.is_some(), "第二次应当命中并带上缓存的类型");
                Ok::<(), std::io::Error>(())
            })
    }
}
