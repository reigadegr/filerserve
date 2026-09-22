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

use std::{
    collections::HashMap,
    fs::{File, Metadata},
    os::unix::fs::MetadataExt,
    sync::Mutex,
};

use lanfile_sendfile::duplicate_file;
use mime::Mime;

/// 条目上限，超过就整体清空：fd 数量必须有硬上限，宁可全部丢弃也不能耗光描述符。
const CAPACITY: usize = 512;

struct Entry {
    file: File,
    /// 这个 fd 自己的 `fstat` 结果，命中时直接交给 `NamedFile`，省掉每请求一次 `fstat`
    metadata: Metadata,
    /// 解析出来的 `Content-Type`（需要时已带上 `charset=`），命中时省掉嗅探的那次 `pread`
    content_type: Mime,
}

/// 请求路径 -> 已打开的文件。
#[derive(Default)]
pub struct FileCache {
    entries: Mutex<HashMap<Box<str>, Entry>>,
}

impl FileCache {
    /// 命中时返回独立的 fd 及它的元数据与 `Content-Type`，调用方会把 fd 交给 `NamedFile` 消费掉。
    ///
    /// 只有 `ino`、大小与修改时间都与本次 `lstat` 的结果一致才算命中。
    #[must_use]
    pub fn get(&self, path: &str, metadata: &Metadata) -> Option<(File, Metadata, Mime)> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(path)?;
        if entry.metadata.ino() != metadata.ino()
            || entry.metadata.len() != metadata.len()
            || (entry.metadata.mtime(), entry.metadata.mtime_nsec())
                != (metadata.mtime(), metadata.mtime_nsec())
        {
            return None;
        }
        let file = duplicate_file(&entry.file)?;
        let (metadata, content_type) = (entry.metadata.clone(), entry.content_type.clone());
        drop(entries);
        Some((file, metadata, content_type))
    }

    /// 未命中时把刚打开并已 `fstat` 的文件放进缓存：缓存自己留一份 fd，调用方那份继续用。
    ///
    /// `metadata` 必须是这个 fd 自己的 `fstat` 结果（而不是路径的 `lstat`）：命中时它会被
    /// 直接当作文件的元数据使用，两者必须是同一个 inode 的属性。
    pub fn insert(&self, path: &str, file: &File, metadata: Metadata, content_type: Mime) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if entries.len() >= CAPACITY {
            entries.clear();
        }
        let Some(cached) = duplicate_file(file) else {
            return;
        };
        entries.insert(
            path.into(),
            Entry {
                file: cached,
                metadata,
                content_type,
            },
        );
    }

    /// 路径已经不存在了，顺手把占着的 fd 放掉。
    pub fn remove(&self, path: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn text_plain() -> Mime {
        match "text/plain; charset=utf-8".parse() {
            Ok(mime) => mime,
            Err(_) => unreachable!("写死的类型应当能解析"),
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

    fn read_all(mut file: File) -> std::io::Result<String> {
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }

    #[test]
    fn hits_while_the_file_is_unchanged() -> std::io::Result<()> {
        let fixture = Fixture::new("hit")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;

        cache.insert("file.txt", &file, metadata, text_plain());
        let cached = cache.get("file.txt", &fixture.lstat()?);
        assert!(cached.is_some(), "元数据没变就应该命中");
        if let Some((file, metadata, content_type)) = cached {
            assert_eq!(read_all(file)?, "hello");
            assert_eq!(metadata.len(), 5, "命中时给出的元数据就是那个 fd 的");
            assert_eq!(content_type, text_plain(), "命中时类型也从缓存来");
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
        cache.insert("file.txt", &file, metadata, text_plain());

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
        cache.insert("file.txt", &file, metadata, text_plain());

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

    #[test]
    fn remove_drops_the_entry() -> std::io::Result<()> {
        let fixture = Fixture::new("remove")?;
        let cache = FileCache::default();
        let (file, metadata) = fixture.open()?;
        cache.insert("file.txt", &file, metadata, text_plain());
        cache.remove("file.txt");
        assert!(cache.get("file.txt", &fixture.lstat()?).is_none());
        Ok(())
    }
}
