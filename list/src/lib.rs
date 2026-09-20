use std::{
    mem::MaybeUninit,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use async_zip::{Compression, ZipEntryBuilder, tokio::write::ZipFileWriter};
use futures_lite::io::AsyncWriteExt;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RawDir};
use salvo::{
    http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue},
    prelude::*,
    routing::filters,
};
use serde::Serialize;
use tokio::io::AsyncReadExt;

mod ip;
mod zip;

use ip::detect_lan_ip;

struct LanIpCache {
    ip: Option<String>,
    fetched_at: Option<Instant>,
}

#[derive(Serialize)]
struct ListEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: &'static str,
    size: Option<u64>,
    modified: String,
}

#[derive(Serialize)]
struct ListResponse {
    path: String,
    lan_ip: Option<String>,
    port: u16,
    entries: Vec<ListEntry>,
}

struct ListApi {
    root: PathBuf,
    port: u16,
    lan_ip: ArcSwap<LanIpCache>,
}

impl ListApi {
    #[must_use]
    fn new(root: PathBuf, port: u16) -> Self {
        Self {
            root,
            port,
            lan_ip: ArcSwap::new(Arc::new(LanIpCache {
                ip: None,
                fetched_at: None,
            })),
        }
    }

    async fn get_lan_ip(&self) -> Option<String> {
        let cache = self.lan_ip.load();
        if cache
            .fetched_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return cache.ip.clone();
        }
        // 枚举网络接口是阻塞的系统调用，放到阻塞线程池，避免拖慢异步 worker
        let new_ip = tokio::task::spawn_blocking(detect_lan_ip)
            .await
            .ok()
            .flatten();
        self.lan_ip.store(Arc::new(LanIpCache {
            ip: new_ip.clone(),
            fetched_at: Some(Instant::now()),
        }));
        new_ip
    }
}

/// 解析请求路径对应的绝对目录，且必须位于 root 之内（防目录穿越）。
fn resolve_under(root: &std::path::Path, sub: &str) -> Option<PathBuf> {
    let canonical = root.join(sub).canonicalize().ok()?;
    canonical.starts_with(root).then_some(canonical)
}

/// 枚举目录并返回排序后的条目；路径非法或非目录返回 `None`。
/// 全程是同步阻塞的 fs 操作，应由调用方放进 `spawn_blocking`，避免拖慢异步 worker。
fn list_directory(root: &std::path::Path, path: &str) -> Option<Vec<ListEntry>> {
    let dir = resolve_under(root, path)?;

    // 1. openat 打开目录 fd
    //    OFlags::DIRECTORY 隐含 is_dir 检查，省 1 次 stat
    let Ok(dirfd) = fs::openat(
        fs::CWD,
        &dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return None;
    };

    // 2. RawDir 用栈缓冲遍历（零堆分配，vs std read_dir 内部 Vec）
    let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
    let mut raw_dir = RawDir::new(&dirfd, &mut buf);

    let mut list_entries: Vec<ListEntry> = Vec::with_capacity(64);
    while let Some(entry) = raw_dir.next() {
        let Ok(entry) = entry else {
            continue;
        };

        // 3. 名字按原始字节读取，后续 statat 与 String 分配复用
        let name_cstr = entry.file_name();
        let name_bytes = name_cstr.to_bytes();
        // RawDir 原样返回 . 与 ..，需显式跳过
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        // 4. d_type 判断类型（零 syscall，来自 dirent）
        let ft = entry.file_type();

        // 5. statat 相对 dirfd 获取 size + mtime
        //    SYMLINK_NOFOLLOW 不跟随符号链接（比 std metadata() 更安全）
        //    相对路径解析比绝对路径更快
        let Ok(stat) = fs::statat(&dirfd, name_cstr, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };

        // d_type 为 Unknown 时回退到 stat 的 st_mode
        let actual_ft = if ft == FileType::Unknown {
            FileType::from_raw_mode(stat.st_mode)
        } else {
            ft
        };

        // 符号链接不展示给前端：/files 下载同样拒绝，避免出现下载即 404 的条目
        if actual_ft.is_symlink() {
            continue;
        }

        let is_dir = actual_ft.is_dir();

        // 6. 名字只分配一次 String（vs 原先 to_string_lossy + to_string 两次分配）
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        let entry_type = if is_dir { "dir" } else { "file" };
        let size = if is_dir {
            None
        } else {
            #[allow(clippy::cast_sign_loss)]
            Some(stat.st_size as u64)
        };

        // 7. 直接读 st_mtime（跳过 SystemTime → Duration → as_secs 转换链）
        let modified = chrono::DateTime::from_timestamp(stat.st_mtime, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();

        list_entries.push(ListEntry {
            name,
            entry_type,
            size,
            modified,
        });
    }

    list_entries.sort_unstable_by(|a, b| {
        let a_dir = a.entry_type == "dir";
        let b_dir = b.entry_type == "dir";
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });

    Some(list_entries)
}

#[handler]
impl ListApi {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let path = req.param::<String>("path").unwrap_or_default();
        let root = self.root.clone();
        let display_path = if path.is_empty() {
            "/".to_string()
        } else {
            format!("/{path}")
        };

        // 目录枚举是阻塞的 fs 操作，整体放进阻塞线程池
        let Ok(listed) = tokio::task::spawn_blocking(move || list_directory(&root, &path)).await
        else {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        };
        let Some(entries) = listed else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        let response = ListResponse {
            path: display_path,
            lan_ip: self.get_lan_ip().await,
            port: self.port,
            entries,
        };

        res.render(Json(response));
    }
}

struct ZipApi {
    root: PathBuf,
}

impl ZipApi {
    #[must_use]
    const fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[handler]
impl ZipApi {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let path = req.param::<String>("path").unwrap_or_default();
        let root = self.root.clone();

        // 解析路径 + 类型判断是阻塞 fs 操作，放进阻塞线程池
        let resolved = tokio::task::spawn_blocking(move || {
            let canonical = resolve_under(&root, &path)?;
            canonical.is_dir().then_some(canonical)
        })
        .await;

        let canonical = match resolved {
            Ok(Some(canonical)) => canonical,
            Ok(None) => {
                res.status_code(StatusCode::NOT_FOUND);
                return;
            }
            Err(_) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                return;
            }
        };

        let folder_name = zip::folder_name(&canonical);
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        if let Ok(val) = HeaderValue::from_str(&zip::content_disposition(&folder_name)) {
            res.headers_mut().insert(CONTENT_DISPOSITION, val);
        }

        // 边遍历边流式打包，不先把整棵树攒进内存：
        // - 阻塞遍历在 spawn_blocking 里，通过有界 channel 逐条发 Entry（有界 = 内存封顶）
        // - 异步写 zip 在 tokio 里，逐条收 Entry 写入
        let (entry_tx, mut entry_rx) = tokio::sync::mpsc::channel::<zip::Entry>(64);
        tokio::task::spawn_blocking(move || {
            zip::walk(&canonical, &folder_name, &mut |entry| {
                entry_tx.blocking_send(entry).is_ok()
            });
        });

        let tx = res.channel();
        tokio::spawn(async move {
            let mut writer = ZipFileWriter::with_tokio(tx);
            let mut buf = vec![0u8; 262_144];
            while let Some(entry) = entry_rx.recv().await {
                match entry {
                    zip::Entry::Dir { name } => {
                        // 目录条目：名字以 / 结尾、置 S_IFDIR 权限位，解压后保留空目录结构
                        let dir = ZipEntryBuilder::new(name.into(), Compression::Stored)
                            .unix_permissions(0o40755);
                        if writer.write_entry_whole(dir, &[]).await.is_err() {
                            return;
                        }
                    }
                    zip::Entry::File { abs, name } => {
                        let entry = ZipEntryBuilder::new(name.into(), Compression::Stored);
                        let Ok(mut ew) = writer.write_entry_stream(entry).await else {
                            return;
                        };
                        let Ok(mut f) = tokio::fs::File::open(&abs).await else {
                            let _ = ew.close().await;
                            continue;
                        };
                        // 内核顺序读提示：扩大预读窗口，大文件连续传输更快；仅设置标志、立即返回
                        let _ = rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::Sequential);
                        if copy_entry(&mut f, &mut ew, &mut buf).await.is_err() {
                            return;
                        }
                        if ew.close().await.is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = writer.close().await;
        });
    }
}

#[must_use]
pub fn list_routes(root: std::path::PathBuf, port: u16) -> Router {
    Router::new()
        .push(
            Router::with_path("/api/list/{**path}")
                .filter(filters::get())
                .goal(ListApi::new(root.clone(), port)),
        )
        .push(
            Router::with_path("/api/zip/{**path}")
                .filter(filters::get())
                .goal(ZipApi::new(root)),
        )
}

/// 顺序拷贝文件内容到 entry 流；读错视为跳过该文件，写错向上传播中断整个 zip。
async fn copy_entry(
    src: &mut (impl tokio::io::AsyncRead + Unpin),
    dst: &mut (impl futures_lite::io::AsyncWrite + Unpin),
    buf: &mut [u8],
) -> std::io::Result<()> {
    loop {
        match src.read(buf).await {
            Ok(0) | Err(_) => return Ok(()),
            Ok(n) => dst.write_all(&buf[..n]).await?,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::{path::Path, time::Instant};

    use tokio::io::AsyncWriteExt;

    use super::copy_entry;

    const FILE_SIZE: usize = 64 * 1024 * 1024;

    async fn copy_throughput(buf_size: usize, path: &Path) -> f64 {
        let mut src = tokio::fs::File::open(path).await.unwrap();
        let mut dst = futures_lite::io::sink();
        let mut buf = vec![0u8; buf_size];
        let start = Instant::now();
        copy_entry(&mut src, &mut dst, &mut buf).await.unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        FILE_SIZE as f64 / (1024.0 * 1024.0) / elapsed
    }

    #[tokio::test]
    async fn zip_copy_throughput_by_buffer_size() {
        let path = std::env::temp_dir().join(format!("filerserve-perf-{}", std::process::id()));
        let mut f = tokio::fs::File::create(&path).await.unwrap();
        let chunk = vec![0xABu8; 1024 * 1024];
        let mut remaining = FILE_SIZE;
        while remaining > 0 {
            f.write_all(&chunk).await.unwrap();
            remaining -= chunk.len();
        }
        drop(f);

        let mut total_64k = 0.0;
        let mut total_256k = 0.0;

        let mut total_512k = 0.0;
        let mut total_1024k = 0.0;

        for _ in 0..10 {
            total_64k += copy_throughput(64 * 1024, &path).await;
            total_256k += copy_throughput(256 * 1024, &path).await;
            total_512k += copy_throughput(512 * 1024, &path).await;
            total_1024k += copy_throughput(1024 * 1024, &path).await;
        }
        let avg_64k = total_64k / 10.0;
        let avg_256k = total_256k / 10.0;

        let avg_512k = total_512k / 10.0;
        let avg_1024k = total_1024k / 10.0;

        println!("zip 拷贝吞吐 64KB 缓冲: {avg_64k:.1} MB/s");
        println!("zip 拷贝吞吐 256KB 缓冲: {avg_256k:.1} MB/s");

        println!("zip 拷贝吞吐 512KB 缓冲: {avg_512k:.1} MB/s");
        println!("zip 拷贝吞吐 1024KB 缓冲: {avg_1024k:.1} MB/s");

        std::fs::remove_file(&path).unwrap();

        assert!(
            avg_256k >= avg_64k * 0.8,
            "256KB 缓冲吞吐不应显著低于 64KB: {avg_64k:.1} vs {avg_256k:.1} MB/s",
        );
    }
}
