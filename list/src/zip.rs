use std::{
    fmt::Write as _,
    mem::MaybeUninit,
    path::{Path, PathBuf},
};

use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags, RawDir};

/// zip 归档中的一条记录：普通文件或目录（目录条目用于保留空目录结构）。
pub enum Entry {
    File { abs: PathBuf, name: String },
    Dir { name: String },
}

/// 递归收集目录树为 zip 条目列表。
/// `RawDir` 零分配遍历 + `d_type` 免 stat；与 /api/list 一致包含 dotfile、跳过符号链接。
/// 目录不可读（无权限等）时跳过该目录，不中断整个打包。
fn collect_entries(dir: &Path, prefix: &str, out: &mut Vec<Entry>) {
    let Ok(dirfd) = rfs::openat(
        rfs::CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return;
    };

    // 记录目录自身，空目录解压后也能保留，保证目录结构原样
    out.push(Entry::Dir {
        name: format!("{prefix}/"),
    });

    let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
    let mut raw_dir = RawDir::new(&dirfd, &mut buf);

    let mut entries: Vec<(FileType, String)> = Vec::new();
    while let Some(entry) = raw_dir.next() {
        let Ok(entry) = entry else {
            continue;
        };
        let name_bytes = entry.file_name().to_bytes();
        // 只跳过 . 与 ..，点开头的文件/目录一并打包
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        entries.push((entry.file_type(), name));
    }

    entries.sort_unstable_by(|a, b| a.1.cmp(&b.1));

    for (ft, name) in entries {
        let path = dir.join(&name);
        let zip_name = format!("{prefix}/{name}");

        let actual_ft = if ft == FileType::Unknown {
            rfs::statat(&dirfd, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_or(ft, |s| FileType::from_raw_mode(s.st_mode))
        } else {
            ft
        };

        if actual_ft.is_dir() {
            collect_entries(&path, &zip_name, out);
        } else if actual_ft.is_file() {
            out.push(Entry::File {
                abs: path,
                name: zip_name,
            });
        }
        // 符号链接等其它类型：跳过
    }
}

/// 收集文件夹根级列表，返回 (zip 内根前缀, 条目列表)。
pub fn collect_folder(dir: &Path) -> (String, Vec<Entry>) {
    let folder_name = dir
        .file_name()
        .map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned());
    let mut out = Vec::with_capacity(64);
    collect_entries(dir, &folder_name, &mut out);
    (folder_name, out)
}

/// 生成 RFC 5987 风格的 Content-Disposition 值：filename*=UTF-8''<pct>.zip
pub fn content_disposition(folder_name: &str) -> String {
    let mut out = String::from("attachment; filename*=UTF-8''");
    for &b in folder_name.as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out.push_str(".zip");
    out
}
