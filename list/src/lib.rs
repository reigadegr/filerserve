use std::{
    mem::MaybeUninit,
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use async_zip::{Compression, ZipEntryBuilder, tokio::write::ZipFileWriter};
use futures_lite::io::AsyncWriteExt;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RawDir};
use salvo::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue};
use salvo::{prelude::*, routing::filters};
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

pub struct ListApi {
    root: std::path::PathBuf,
    pub port: u16,
    lan_ip: ArcSwap<LanIpCache>,
}

impl ListApi {
    #[must_use]
    pub fn new(root: std::path::PathBuf, port: u16) -> Self {
        Self {
            root,
            port,
            lan_ip: ArcSwap::new(Arc::new(LanIpCache {
                ip: None,
                fetched_at: None,
            })),
        }
    }

    fn get_lan_ip(&self) -> Option<String> {
        let cache = self.lan_ip.load();
        if cache
            .fetched_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return cache.ip.clone();
        }
        drop(cache);
        let new_ip = detect_lan_ip();
        self.lan_ip.store(Arc::new(LanIpCache {
            ip: new_ip.clone(),
            fetched_at: Some(Instant::now()),
        }));
        new_ip
    }
}

#[handler]
impl ListApi {
    #[allow(clippy::unused_async, clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let path = req.param::<String>("path").unwrap_or_default();
        let full = self.root.join(&path);

        let Ok(canonical_target) = full.canonicalize() else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        if !canonical_target.starts_with(&self.root) {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        // 1. openat 打开目录 fd
        //    OFlags::DIRECTORY 隐含 is_dir 检查，省 1 次 stat
        let Ok(dirfd) = fs::openat(
            fs::CWD,
            &canonical_target,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        // 2. RawDir 用栈缓冲遍历（零堆分配，vs std read_dir 内部 Vec）
        let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
        let mut raw_dir = RawDir::new(&dirfd, &mut buf);

        let mut list_entries: Vec<ListEntry> = Vec::with_capacity(64);
        while let Some(entry) = raw_dir.next() {
            let Ok(entry) = entry else {
                continue;
            };

            // 3. 用原始字节检查 dotfile（零分配跳过）
            let name_cstr = entry.file_name();
            let name_bytes = name_cstr.to_bytes();
            if name_bytes.first().is_some_and(|&b| b == b'.') {
                continue;
            }

            // 4. d_type 判断 is_dir（零 syscall，来自 dirent）
            let ft = entry.file_type();

            // 5. statat 相对 dirfd 获取 size + mtime
            //    SYMLINK_NOFOLLOW 不跟随符号链接（比 std metadata() 更安全）
            //    相对路径解析比绝对路径更快
            let Ok(stat) = fs::statat(&dirfd, name_cstr, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };

            // d_type 为 Unknown 时回退到 stat 的 st_mode
            let is_dir = if ft == FileType::Unknown {
                FileType::from_raw_mode(stat.st_mode).is_dir()
            } else {
                ft.is_dir()
            };

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

        let display_path = if path.is_empty() {
            "/".to_string()
        } else {
            format!("/{path}")
        };

        let response = ListResponse {
            path: display_path,
            lan_ip: self.get_lan_ip(),
            port: self.port,
            entries: list_entries,
        };

        res.render(Json(response));
    }
}

pub struct ZipApi {
    root: std::path::PathBuf,
}

impl ZipApi {
    #[must_use]
    pub const fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

#[handler]
impl ZipApi {
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let path = req.param::<String>("path").unwrap_or_default();
        let full = self.root.join(&path);

        let Ok(canonical) = full.canonicalize() else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };
        if !canonical.starts_with(&self.root) {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }
        if !canonical.is_dir() {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        let Ok(Ok(entries)) =
            tokio::task::spawn_blocking(move || zip::collect_folder(&canonical)).await
        else {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        };

        let folder_name = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_string();

        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        if let Ok(val) = HeaderValue::from_str(&zip::content_disposition(&folder_name)) {
            res.headers_mut().insert(CONTENT_DISPOSITION, val);
        }

        let tx = res.channel();
        tokio::spawn(async move {
            let mut writer = ZipFileWriter::with_tokio(tx);
            let mut buf = vec![0u8; 65536];
            for (abs, name) in &entries {
                let entry = ZipEntryBuilder::new(name.clone().into(), Compression::Stored);
                let Ok(mut ew) = writer.write_entry_stream(entry).await else {
                    return;
                };
                let Ok(mut f) = tokio::fs::File::open(abs).await else {
                    let _ = ew.close().await;
                    continue;
                };
                loop {
                    match f.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if ew.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                if ew.close().await.is_err() {
                    return;
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
