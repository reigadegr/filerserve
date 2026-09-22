//! 已打开文件的缓存。
//!
//! 每个请求都要按路径 `open` 一次文件，而这条路径上还有 `canonicalize`（对每一层路径各一次
//! `readlink`）、`fstat`、`fadvise` 和字符集嗅探的一次 `pread`。把已经打开、已经 `fstat` 过
//! 并验证过的 fd 连同它的元数据与解析出来的 `Content-Type` 按请求路径缓存起来，命中时就只剩
//! 一次 `dup`：手机上每请求能省下 8 次系统调用（见 target/bench-lock.md 第 20 节）。
//!
//! 缓存里存的是 **fd 而不是内容**，读到的永远是文件当前的内容；命中要求 `(ino, size, mtime)`
//! 与请求时 `lstat` 到的元数据完全一致，所以文件被改动、替换或删除都会立刻未命中并重新解析。
//! 因此缓存不会让响应变旧：`Content-Length`、`Last-Modified`、`ETag` 都取自这份元数据，
//! 而它正是命中时那个 fd 自己的 `fstat` 结果。
//!
//! 并发上用 [`SHARDS`] 把一把全局锁拆成多把：路径按哈希固定落在其中一片，请求只在这一片上
//! 竞争，锁里只做查找与拷贝。条目与响应体共享同一个 fd（`Arc<File>`），命中与写入都不再
//! `dup`：sendfile 带显式 offset，共享文件描述符本来就是安全的。
//!
//! 淘汰按 LRU：每片记一个自增序号，命中时刷新该条的序号，满了就淘汰序号最小的那条。每片只有
//! [`CAPACITY_PER_SHARD`] 条，线性扫一遍比维护链表简单得多；也避免了"满了全清"把热文件一起
//! 丢掉——下载大量不同文件时，全清会让命中率归零，缓存反而比不缓存慢（见第 23 节）。

use std::{
    collections::HashMap,
    fs::{File, Metadata},
    hash::{Hash, Hasher},
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex},
};

use crate::CachedHeaders;

/// 分片数：把一把全局锁拆成 16 把
const SHARDS: usize = 16;
/// 每片的条目上限（总数 512 不变）：fd 数量必须有硬上限
const CAPACITY_PER_SHARD: usize = 32;

struct Entry {
    /// 与响应体共享的 fd：命中时只克隆 `Arc`，不再 `dup`
    file: Arc<File>,
    /// 这个 fd 自己的 `fstat` 结果，命中时直接交给 `NamedFile`，省掉每请求一次 `fstat`
    metadata: Metadata,
    headers: CachedHeaders,
    /// 最近一次被用到的序号，淘汰时取最小的那条
    used: u64,
}

/// 一片：条目表 + 该片自己的 LRU 序号（片内单调递增，不需要原子操作）
#[derive(Default)]
struct Shard {
    entries: HashMap<Box<str>, Entry>,
    clock: u64,
}

/// 请求路径 -> 已打开的文件。分片后每片一把锁。
#[derive(Default)]
pub struct FileCache {
    shards: [Mutex<Shard>; SHARDS],
}

/// 路径落在哪一片：同一路径永远落在同一片
fn shard_index(path: &str) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() as usize) % SHARDS
}

impl FileCache {
    fn shard(&self, path: &str) -> &Mutex<Shard> {
        &self.shards[shard_index(path)]
    }

    /// 命中时返回独立的 fd、它的元数据与已经编码好的响应头，调用方会把 fd 交给 `NamedFile` 消费掉。
    ///
    /// 只有 `ino`、大小与修改时间都与本次 `lstat` 的结果一致才算命中。
    #[must_use]
    pub fn get(
        &self,
        path: &str,
        metadata: &Metadata,
    ) -> Option<(Arc<File>, Metadata, CachedHeaders)> {
        let mut shard = self.shard(path).lock().ok()?;
        shard.clock += 1;
        let clock = shard.clock;
        let entry = shard.entries.get_mut(path)?;
        if entry.metadata.ino() != metadata.ino()
            || entry.metadata.len() != metadata.len()
            || (entry.metadata.mtime(), entry.metadata.mtime_nsec())
                != (metadata.mtime(), metadata.mtime_nsec())
        {
            return None;
        }
        entry.used = clock;
        // 锁里只做拷贝：克隆 Arc、元数据与已经编码好的响应头，没有任何系统调用
        let file = Arc::clone(&entry.file);
        let (metadata, headers) = (entry.metadata.clone(), entry.headers.clone());
        drop(shard);
        Some((file, metadata, headers))
    }

    /// 未命中时把刚打开并已 `fstat` 的文件放进缓存：缓存自己留一份 fd，调用方那份继续用。
    ///
    /// `metadata` 必须是这个 fd 自己的 `fstat` 结果（而不是路径的 `lstat`）：命中时它会被
    /// 直接当作文件的元数据使用，两者必须是同一个 inode 的属性。`headers` 里的 `ETag` 与
    /// `Content-Disposition` 必须是从同一份元数据算出来的，否则命中时会给出错的响应头。
    pub fn insert(&self, path: &str, file: Arc<File>, metadata: Metadata, headers: CachedHeaders) {
        let Ok(mut shard) = self.shard(path).lock() else {
            return;
        };
        shard.clock += 1;
        let clock = shard.clock;
        // 满了就淘汰最久没被用到的那条，而不是把整片清空
        if shard.entries.len() >= CAPACITY_PER_SHARD && !shard.entries.contains_key(path) {
            let oldest = shard
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                shard.entries.remove(&oldest);
            }
        }
        shard.entries.insert(
            path.into(),
            Entry {
                file,
                metadata,
                headers,
                used: clock,
            },
        );
    }

    /// 路径已经不存在了，顺手把占着的 fd 放掉。
    pub fn remove(&self, path: &str) {
        if let Ok(mut shard) = self.shard(path).lock() {
            shard.entries.remove(path);
        }
    }

    /// 总条目数，只有测试用得到
    #[cfg(test)]
    fn total_len(&self) -> usize {
        self.shards
            .iter()
            .filter_map(|shard| shard.lock().ok())
            .map(|shard| shard.entries.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mime::Mime;
    use salvo::http::{HeaderValue, headers::ETag};
    use std::io::Read as _;

    fn text_plain() -> Mime {
        match "text/plain; charset=utf-8".parse() {
            Ok(mime) => mime,
            Err(_) => unreachable!("写死的类型应当能解析"),
        }
    }

    /// 测试用：只带类型，ETag 与 Content-Disposition 留空
    fn headers() -> CachedHeaders {
        CachedHeaders {
            content_type: text_plain(),
            etag: None,
            disposition: None,
        }
    }

    /// 测试用：写死的 ETag（解析不了就说明测试自己写错了）
    fn etag(value: &str) -> ETag {
        match value.parse() {
            Ok(etag) => etag,
            Err(_) => unreachable!("写死的 ETag 应当能解析"),
        }
    }

    /// 临时目录里的一个文件，Drop 时整个目录一起删掉
    struct Fixture {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> std::io::Result<Self> {
            let dir = std::env::temp_dir()
                .join(format!("lanfile-file-cache-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("file.txt");
            std::fs::write(&path, b"hello")?;
            Ok(Self { dir, path })
        }

        /// 打开文件并返回它的 fd 与 `fstat` 结果，模拟未命中时写入缓存的那份
        fn open(&self) -> std::io::Result<(File, Metadata)> {
            let file = File::open(&self.path)?;
            let metadata = file.metadata()?;
            Ok((file, metadata))
        }

        fn lstat(&self) -> std::io::Result<Metadata> {
            std::fs::symlink_metadata(&self.path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn read_all(file: &File) -> std::io::Result<String> {
        let mut text = String::new();
        let mut reader = file;
        reader.read_to_string(&mut text)?;
        Ok(text)
    }

    #[test]
    fn hits_while_the_file_is_unchanged() -> std::io::Result<()> {
        let fixture = Fixture::new("hit")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;

        cache.insert("file.txt", Arc::new(file), metadata, headers());
        let cached = cache.get("file.txt", &fixture.lstat()?);
        assert!(cached.is_some(), "元数据没变就应该命中");
        if let Some((file, metadata, headers)) = cached {
            assert_eq!(read_all(&file)?, "hello");
            assert_eq!(metadata.len(), 5, "命中时给出的元数据就是那个 fd 的");
            assert_eq!(headers.content_type, text_plain(), "命中时类型也从缓存来");
        }
        assert!(
            cache.get("other.txt", &fixture.lstat()?).is_none(),
            "路径不同不该命中"
        );
        Ok(())
    }

    #[test]
    fn misses_after_the_file_changes() -> std::io::Result<()> {
        let fixture = Fixture::new("change")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        cache.insert("file.txt", Arc::new(file), metadata, headers());

        std::fs::write(&fixture.path, b"hello, world")?;
        assert!(
            cache.get("file.txt", &fixture.lstat()?).is_none(),
            "大小变了不该命中"
        );
        Ok(())
    }

    /// 文件被同大小、同时间戳的新文件替换掉（inode 变了），也必须未命中
    #[test]
    fn misses_after_the_file_is_replaced() -> std::io::Result<()> {
        let fixture = Fixture::new("replace")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        let original = fixture.lstat()?;
        cache.insert("file.txt", Arc::new(file), metadata, headers());

        let replacement = fixture.dir.join("replacement.txt");
        std::fs::write(&replacement, b"world")?;
        rustix::fs::utimensat(
            rustix::fs::CWD,
            &replacement,
            &rustix::fs::Timestamps {
                last_access: rustix::fs::Timespec {
                    tv_sec: original.atime(),
                    tv_nsec: original.atime_nsec(),
                },
                last_modification: rustix::fs::Timespec {
                    tv_sec: original.mtime(),
                    tv_nsec: original.mtime_nsec(),
                },
            },
            rustix::fs::AtFlags::empty(),
        )?;
        std::fs::rename(&replacement, &fixture.path)?;

        let replaced = fixture.lstat()?;
        assert_eq!(replaced.len(), 5, "替换文件的大小必须和原文件一样");
        assert!(
            cache.get("file.txt", &replaced).is_none(),
            "inode 变了就不该命中"
        );
        Ok(())
    }

    /// 已经编码好的 `ETag` 与 `Content-Disposition` 跟着条目一起存取，命中时原样拿回来
    #[test]
    fn keeps_the_encoded_headers() -> std::io::Result<()> {
        let fixture = Fixture::new("headers")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        let etag = etag("\"abc-1\"");
        let disposition = HeaderValue::from_static("inline");
        cache.insert(
            "file.txt",
            Arc::new(file),
            metadata,
            CachedHeaders {
                content_type: text_plain(),
                etag: Some(etag.clone()),
                disposition: Some(disposition.clone()),
            },
        );

        let Some((_, _, cached)) = cache.get("file.txt", &fixture.lstat()?) else {
            panic!("元数据没变就应该命中");
        };
        assert_eq!(cached.etag, Some(etag), "命中时应当给出缓存里的 ETag");
        assert_eq!(
            cached.disposition,
            Some(disposition),
            "命中时应当给出缓存里的 Content-Disposition"
        );
        Ok(())
    }

    #[test]
    fn remove_drops_the_entry() -> std::io::Result<()> {
        let fixture = Fixture::new("remove")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        cache.insert("file.txt", Arc::new(file), metadata, headers());
        cache.remove("file.txt");
        assert!(cache.get("file.txt", &fixture.lstat()?).is_none());
        Ok(())
    }

    /// 路径数量远超容量时，条目总数必须有硬上限，且最后插入的那条仍然在
    #[test]
    fn stays_bounded_under_many_paths() -> std::io::Result<()> {
        let fixture = Fixture::new("bound")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        let file = Arc::new(file);
        for i in 0..2000 {
            cache.insert(
                &format!("path-{i}"),
                Arc::clone(&file),
                metadata.clone(),
                headers(),
            );
        }
        assert!(
            cache.total_len() <= SHARDS * CAPACITY_PER_SHARD,
            "条目总数不能超过上限"
        );
        assert!(
            cache.get("path-1999", &fixture.lstat()?).is_some(),
            "最后插入的那条必须还在"
        );
        Ok(())
    }

    /// 淘汰的是最久没被用到的那条，而不是把整片清空
    #[test]
    fn evicts_the_least_recently_used() -> std::io::Result<()> {
        let fixture = Fixture::new("lru")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        let file = Arc::new(file);
        // 凑够同一片里的 CAPACITY_PER_SHARD + 1 条路径，把这一片填满
        let shard = shard_index("same-shard-0");
        let mut paths = Vec::new();
        let mut i = 0;
        while paths.len() <= CAPACITY_PER_SHARD {
            let path = format!("same-shard-{i}");
            if shard_index(&path) == shard {
                paths.push(path);
            }
            i += 1;
        }
        let target = paths[0].clone();
        let Some(extra) = paths.pop() else {
            unreachable!("至少有一条用于触发淘汰");
        };
        for path in &paths {
            cache.insert(path, Arc::clone(&file), metadata.clone(), headers());
        }
        assert!(
            cache.get(&target, &fixture.lstat()?).is_some(),
            "刚插入的应当命中"
        );

        cache.insert(&extra, Arc::clone(&file), metadata, headers());
        assert!(
            cache.get(&target, &fixture.lstat()?).is_some(),
            "刚用过的不能被淘汰"
        );
        assert!(
            cache.get(&paths[1], &fixture.lstat()?).is_none(),
            "最久没被用到的应当被淘汰"
        );
        Ok(())
    }
}
