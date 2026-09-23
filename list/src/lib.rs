use std::{
    io::Read as _,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::mem::MaybeUninit;

use arc_swap::ArcSwap;
use async_zip::{Compression, ZipEntryBuilder, tokio::write::ZipFileWriter};
use futures_lite::io::AsyncWriteExt;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RawDir};
use salvo::{
    http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue},
    prelude::*,
    routing::filters,
};
use serde::Serialize;

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

/// 目录条目排序：目录在前，同类按名称升序。
fn sort_list_entries(entries: &mut [ListEntry]) {
    entries.sort_unstable_by(|a, b| {
        let a_dir = a.entry_type == "dir";
        let b_dir = b.entry_type == "dir";
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
}

#[cfg(any(target_os = "linux", target_os = "android"))]
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

    sort_list_entries(&mut list_entries);

    Some(list_entries)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
/// 非 Linux/Android（Windows、macOS 等）下的目录枚举：`rustix::fs` 的 Linux 专用接口不可用，改用 `std::fs`，行为与 Linux 版本一致。
fn list_directory(root: &std::path::Path, path: &str) -> Option<Vec<ListEntry>> {
    let dir = resolve_under(root, path)?;

    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return None;
    };

    let mut list_entries: Vec<ListEntry> = Vec::with_capacity(64);
    for entry in read_dir {
        let Ok(entry) = entry else {
            continue;
        };
        let entry_path = entry.path();
        // symlink_metadata 不跟随符号链接，与 Unix 版本 SYMLINK_NOFOLLOW 语义一致
        let Ok(metadata) = std::fs::symlink_metadata(&entry_path) else {
            continue;
        };
        let ft = metadata.file_type();
        // 符号链接不展示给前端：/files 下载同样拒绝，避免出现下载即 404 的条目
        if ft.is_symlink() {
            continue;
        }
        let is_dir = ft.is_dir();
        let name = entry.file_name().to_string_lossy().into_owned();
        let size = if is_dir { None } else { Some(metadata.len()) };
        let modified = metadata
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();

        list_entries.push(ListEntry {
            name,
            entry_type: if is_dir { "dir" } else { "file" },
            size,
            modified,
        });
    }

    sort_list_entries(&mut list_entries);

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

/// zip 打包流水线里，阻塞遍历线程发往异步写线程的一条消息。
///
/// 文件内容由遍历线程自己 `read` 后分块发出，异步侧不再碰 `tokio::fs`：
/// 每次读盘只花一次 `read`，不必再付一次 `spawn_blocking` 派发，
/// 也不必让 tokio 先把数据读进自己的缓冲、再整块拷到调用方的缓冲。
///
/// 协议：`FileStart` 之后只会跟同一文件的若干 `Chunk`，直到 `FileEnd`；装得下一块的文件
/// 直接发一条自足的 `Whole`，不再走 `FileStart`/`Chunk`/`FileEnd` 三段。
/// `Chunk` 与 `Whole` 的 `data` 始终保持 `ZIP_CHUNK` 满长（便于消费侧原样归还后复用），
/// 有效字节数分别是 `Chunk.len` 与 `Whole.len`。
enum Item {
    Dir {
        name: String,
    },
    /// 小于一块的文件：一次读全，异步侧走 `write_entry_whole`
    Whole {
        name: String,
        data: Vec<u8>,
        len: usize,
    },
    FileStart {
        name: String,
    },
    Chunk {
        data: Vec<u8>,
        len: usize,
    },
    FileEnd,
}

/// 每次读盘发送的字节数，也是流水线的拷贝粒度。
const ZIP_CHUNK: usize = 262_144;

/// 有界队列深度；内存上界约为 `(ZIP_QUEUE + 2) * ZIP_CHUNK`（约 2.5 MiB）——
/// 消息队列最多压 `ZIP_QUEUE` 块，再加上生产、消费两侧各自手上的一块。
const ZIP_QUEUE: usize = 8;

/// 交给 channel 之前的攒批大小。
///
/// `async_zip` 写一个条目会按字段分成很多次小写（本地头、数据、数据描述符、中央目录…），
/// 每次小写经 channel 都会变成一个 chunked 分帧加一次 `sendto`——实测一个 604 字节的
/// 归档打出了 134 次 `sendto`，内核态因此占了 74%。先在内存里攒够再交出去。
const ZIP_FLUSH: usize = 64 * 1024;

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
        // - 阻塞遍历线程自己读文件内容，通过有界 channel 逐块发 Item（有界 = 内存封顶）
        // - 异步写 zip 在 tokio 里，逐条收 Item 写入
        // - 空缓冲经 free channel 回传复用，见 send_file_chunks
        let (item_tx, mut item_rx) = tokio::sync::mpsc::channel::<Item>(ZIP_QUEUE);
        let (free_tx, mut free_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(ZIP_QUEUE);
        tokio::task::spawn_blocking(move || {
            zip::walk(&canonical, &folder_name, &mut |entry| match entry {
                zip::Entry::Dir { name } => item_tx.blocking_send(Item::Dir { name }).is_ok(),
                zip::Entry::File { abs, name } => send_one_file(&item_tx, &mut free_rx, &abs, name),
            });
        });

        let tx = res.channel();
        tokio::spawn(async move {
            let mut writer =
                ZipFileWriter::with_tokio(tokio::io::BufWriter::with_capacity(ZIP_FLUSH, tx));
            while let Some(item) = item_rx.recv().await {
                match item {
                    Item::Dir { name } => {
                        // 目录条目：名字以 / 结尾、置 S_IFDIR 权限位，解压后保留空目录结构
                        let dir = ZipEntryBuilder::new(name.into(), Compression::Stored)
                            .unix_permissions(0o40755);
                        if writer.write_entry_whole(dir, &[]).await.is_err() {
                            return;
                        }
                    }
                    Item::Whole { name, data, len } => {
                        let entry = ZipEntryBuilder::new(name.into(), Compression::Stored);
                        if writer.write_entry_whole(entry, &data[..len]).await.is_err() {
                            return;
                        }
                        // 池满（消费快于生产）就丢弃，只是少一次复用，不影响正确性
                        let _ = free_tx.try_send(data);
                    }
                    Item::FileStart { name } => {
                        let entry = ZipEntryBuilder::new(name.into(), Compression::Stored);
                        let Ok(mut ew) = writer.write_entry_stream(entry).await else {
                            return;
                        };
                        // 遍历线程保证 FileStart 与 FileEnd 之间只会出现 Chunk；
                        // 收到 FileEnd 或 channel 关闭（遍历线程已退出）都收尾。
                        // write_all 返回时数据已被拷进 body，缓冲可以安全归还复用。
                        while let Some(Item::Chunk { data, len }) = item_rx.recv().await {
                            if ew.write_all(&data[..len]).await.is_err() {
                                return;
                            }
                            // 池满（消费快于生产）就丢弃，只是少一次复用，不影响正确性
                            let _ = free_tx.try_send(data);
                        }
                        if ew.close().await.is_err() {
                            return;
                        }
                    }
                    // 按协议不会单独出现：Chunk / FileEnd 已在上面就地消费
                    Item::Chunk { .. } | Item::FileEnd => {}
                }
            }
            // `close` 把中央目录写完并把内部 writer 还回来，还得再 flush 一次，
            // 否则攒在 `BufWriter` 里的尾巴会随任务结束一起丢掉
            if let Ok(mut buffered) = writer.close().await {
                let _ = futures_lite::AsyncWriteExt::flush(&mut buffered).await;
            }
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

/// 阻塞线程内顺序读文件，按 `chunk_size` 分块发往异步侧；返回 `false` 表示 channel 已关闭、应停止遍历。
///
/// 缓冲优先取 `free_rx` 里消费侧归还的空缓冲，取不到才新建。`vec![0u8; n]` 走
/// `alloc_zeroed`，实测 256 KiB 一次约 2.1 µs、其中 98% 是清零；归还的缓冲保持满长，
/// 所以复用既省掉分配也省掉清零，且读入前不需要 `resize`（那等于把清零做回来）。
/// 读错与读到 EOF 同样收尾，与原先 `copy_entry` 的语义一致。
/// 发一个文件：装得下一块就走整条目写入，否则流式分块。返回是否应继续遍历。
///
/// 整条目写入把大小与 CRC 直接写进本地头，省掉流式那条数据描述符和收尾往返。
fn send_one_file(
    item_tx: &tokio::sync::mpsc::Sender<Item>,
    free_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    abs: &std::path::Path,
    name: String,
) -> bool {
    let Ok(mut f) = std::fs::File::open(abs) else {
        // 打不开的文件仍留一个空条目，与原先 open 失败后立即 close 的行为一致
        return item_tx.blocking_send(Item::FileStart { name }).is_ok()
            && item_tx.blocking_send(Item::FileEnd).is_ok();
    };
    // 内核顺序读提示：扩大预读窗口，大文件连续传输更快；仅设置标志、立即返回
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::Sequential);
    if f.metadata().map_or(usize::MAX, |m| m.len() as usize) < ZIP_CHUNK {
        let (buf, len) = read_first_chunk(&mut f, free_rx);
        if len < ZIP_CHUNK {
            return item_tx
                .blocking_send(Item::Whole {
                    name,
                    data: buf,
                    len,
                })
                .is_ok();
        }
        // 读满说明文件在 fstat 之后长大了：已读那块先发出去再接着流式读完，
        // 直接退回流式会把这一段丢掉
        if item_tx.blocking_send(Item::FileStart { name }).is_err() {
            return false;
        }
        if item_tx
            .blocking_send(Item::Chunk { data: buf, len })
            .is_err()
        {
            return false;
        }
    } else if item_tx.blocking_send(Item::FileStart { name }).is_err() {
        return false;
    }
    send_file_chunks(item_tx, free_rx, &mut f, ZIP_CHUNK)
        && item_tx.blocking_send(Item::FileEnd).is_ok()
}

/// 读第一块：从空闲池取一块满长缓冲，读到读满或读到 EOF 为止。
///
/// 返回的有效长度等于 `ZIP_CHUNK` 时说明文件一块装不下（或它在 `fstat` 之后长大了），
/// 调用方应继续走流式路径。
fn read_first_chunk(
    file: &mut std::fs::File,
    free_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> (Vec<u8>, usize) {
    let mut buf = free_rx.try_recv().unwrap_or_else(|_| vec![0u8; ZIP_CHUNK]);
    let mut len = 0;
    while len < ZIP_CHUNK {
        match file.read(&mut buf[len..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n,
        }
    }
    (buf, len)
}

fn send_file_chunks(
    tx: &tokio::sync::mpsc::Sender<Item>,
    free_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    file: &mut std::fs::File,
    chunk_size: usize,
) -> bool {
    loop {
        let mut buf = free_rx.try_recv().unwrap_or_else(|_| vec![0u8; chunk_size]);
        // 归还的缓冲始终满长，这里只读不截断，才能原样复用
        debug_assert_eq!(buf.len(), chunk_size);
        match file.read(&mut buf) {
            Ok(0) | Err(_) => return true,
            Ok(n) => {
                if tx.blocking_send(Item::Chunk { data: buf, len: n }).is_err() {
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::{fs::File, io::Write as _, path::Path, time::Instant};

    use tokio::sync::mpsc;

    use super::{Item, ZIP_QUEUE, send_file_chunks};

    const FILE_SIZE: usize = 64 * 1024 * 1024;

    /// 复刻流水线的读取侧：阻塞线程分块读文件 → 有界 channel → 异步侧消费并归还缓冲。
    /// 生产路径固定用 `ZIP_CHUNK`，这里开放 `chunk_size` 只为观察分块大小对吞吐的影响。
    async fn copy_throughput(chunk_size: usize, path: &Path) -> f64 {
        let (tx, mut rx) = mpsc::channel::<Item>(ZIP_QUEUE);
        let (free_tx, mut free_rx) = mpsc::channel::<Vec<u8>>(ZIP_QUEUE);
        let path = path.to_path_buf();
        let producer = tokio::task::spawn_blocking(move || {
            let mut f = File::open(&path).unwrap();
            let _ = send_file_chunks(&tx, &mut free_rx, &mut f, chunk_size);
        });

        let start = Instant::now();
        while let Some(Item::Chunk { data, .. }) = rx.recv().await {
            let _ = free_tx.try_send(data);
        }
        let elapsed = start.elapsed().as_secs_f64();
        producer.await.unwrap();

        FILE_SIZE as f64 / (1024.0 * 1024.0) / elapsed
    }

    #[tokio::test]
    async fn zip_copy_throughput_by_buffer_size() {
        let path = std::env::temp_dir().join(format!("lanfile-perf-{}", std::process::id()));
        let mut f = File::create(&path).unwrap();
        let chunk = vec![0xABu8; 1024 * 1024];
        let mut remaining = FILE_SIZE;
        while remaining > 0 {
            f.write_all(&chunk).unwrap();
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
