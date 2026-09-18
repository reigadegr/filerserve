use std::mem::MaybeUninit;

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RawDir};
use salvo::{prelude::*, routing::filters};
use serde::Serialize;

mod ip;

use ip::detect_lan_ip;

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
}

impl ListApi {
    #[must_use]
    pub const fn new(root: std::path::PathBuf, port: u16) -> Self {
        Self { root, port }
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

        list_entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            ("dir", "file") => std::cmp::Ordering::Less,
            ("file", "dir") => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });

        let display_path = if path.is_empty() {
            "/".to_string()
        } else {
            format!("/{path}")
        };

        let response = ListResponse {
            path: display_path,
            lan_ip: detect_lan_ip(),
            port: self.port,
            entries: list_entries,
        };

        res.render(Json(response));
    }
}

#[must_use]
pub fn list_routes(root: std::path::PathBuf, port: u16) -> Router {
    Router::with_path("/api/list/{**path}")
        .filter(filters::get())
        .goal(ListApi::new(root, port))
}
