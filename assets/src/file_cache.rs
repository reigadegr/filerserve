//! 已打开文件的缓存。
//!
//! 每个请求都要按路径 `open` 一次文件，而这条路径上还有 `canonicalize`（对每一层路径各一次
//! `readlink`）和 `fadvise`。把已经打开并验证过的 fd 按请求路径缓存起来，命中时就只剩一次
//! `dup`：手机上每请求能省下 7 次系统调用（见 target/bench-lock.md 第 19 节）。
//!
//! 缓存里存的是 **fd 而不是内容**，读到的永远是文件当前的内容；命中要求 `(ino, size, mtime)`
//! 与请求时 `lstat` 到的元数据完全一致，所以文件被改动、替换或删除都会立刻未命中并重新解析。
//! 因此缓存不会让响应变旧：`fstat`、字符集嗅探、`Content-Length` 每次仍然照做。

use std::{
    collections::HashMap,
    fs::{File, Metadata},
    os::unix::fs::MetadataExt,
    sync::Mutex,
};

use lanfile_sendfile::duplicate_file;

/// 条目上限，超过就整体清空：fd 数量必须有硬上限，宁可全部丢弃也不能耗光描述符。
const CAPACITY: usize = 512;

struct Entry {
    file: File,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
}

/// 请求路径 -> 已打开的文件。
#[derive(Default)]
pub struct FileCache {
    entries: Mutex<HashMap<Box<str>, Entry>>,
}

impl FileCache {
    /// 命中时返回一个独立的 fd，调用方会把它交给 `NamedFile` 消费掉。
    ///
    /// 只有 `ino`、大小与修改时间都与本次 `lstat` 的结果一致才算命中。
    #[must_use]
    pub fn get(&self, path: &str, metadata: &Metadata) -> Option<File> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(path)?;
        if entry.ino != metadata.ino()
            || entry.size != metadata.len()
            || entry.mtime != (metadata.mtime(), metadata.mtime_nsec())
        {
            return None;
        }
        let file = duplicate_file(&entry.file);
        drop(entries);
        file
    }

    /// 未命中时把刚打开的文件放进缓存：缓存自己留一份 fd，调用方那份继续用。
    pub fn insert(&self, path: &str, file: &File, metadata: &Metadata) {
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
                ino: metadata.ino(),
                size: metadata.len(),
                mtime: (metadata.mtime(), metadata.mtime_nsec()),
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

        fn metadata(&self) -> std::io::Result<Metadata> {
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
        let metadata = fixture.metadata()?;

        cache.insert("file.txt", &File::open(&fixture.path)?, &metadata);
        let cached = cache.get("file.txt", &metadata);
        assert!(cached.is_some(), "元数据没变就应该命中");
        if let Some(file) = cached {
            assert_eq!(read_all(file)?, "hello");
        }
        assert!(
            cache.get("other.txt", &metadata).is_none(),
            "路径不同不该命中"
        );
        Ok(())
    }

    #[test]
    fn misses_after_the_file_changes() -> std::io::Result<()> {
        let fixture = Fixture::new("change")?;
        let cache = FileCache::default();
        let metadata = fixture.metadata()?;
        cache.insert("file.txt", &File::open(&fixture.path)?, &metadata);

        std::fs::write(&fixture.path, b"hello, world")?;
        let metadata = fixture.metadata()?;
        assert!(
            cache.get("file.txt", &metadata).is_none(),
            "大小变了不该命中"
        );
        Ok(())
    }

    /// 文件被同大小、同时间戳的新文件替换掉（inode 变了），也必须未命中
    #[test]
    fn misses_after_the_file_is_replaced() -> std::io::Result<()> {
        let fixture = Fixture::new("replace")?;
        let cache = FileCache::default();
        let metadata = fixture.metadata()?;
        cache.insert("file.txt", &File::open(&fixture.path)?, &metadata);

        let replacement = fixture.dir.join("replacement.txt");
        std::fs::write(&replacement, b"world")?;
        rustix::fs::utimensat(
            rustix::fs::CWD,
            &replacement,
            &rustix::fs::Timestamps {
                last_access: rustix::fs::Timespec {
                    tv_sec: metadata.atime(),
                    tv_nsec: metadata.atime_nsec(),
                },
                last_modification: rustix::fs::Timespec {
                    tv_sec: metadata.mtime(),
                    tv_nsec: metadata.mtime_nsec(),
                },
            },
            rustix::fs::AtFlags::empty(),
        )?;
        std::fs::rename(&replacement, &fixture.path)?;

        let metadata = fixture.metadata()?;
        assert_eq!(metadata.len(), 5, "替换文件的大小必须和原文件一样");
        assert_ne!(metadata.ino(), 0);
        assert!(
            cache.get("file.txt", &metadata).is_none(),
            "inode 变了就不该命中"
        );
        Ok(())
    }

    #[test]
    fn remove_drops_the_entry() -> std::io::Result<()> {
        let fixture = Fixture::new("remove")?;
        let cache = FileCache::default();
        let metadata = fixture.metadata()?;
        cache.insert("file.txt", &File::open(&fixture.path)?, &metadata);
        cache.remove("file.txt");
        assert!(cache.get("file.txt", &metadata).is_none());
        Ok(())
    }
}
