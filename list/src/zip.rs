use std::{
    fmt::Write as _,
    io,
    mem::MaybeUninit,
    path::{Path, PathBuf},
};

use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags, RawDir};

/// 递归收集 (绝对路径, zip 内相对路径)。
/// `RawDir` 零分配遍历 + `d_type` 免 stat；跳过 dotfile，与 /api/list 行为一致。
pub fn collect_entries(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    let dirfd = rfs::openat(
        rfs::CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;

    let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
    let mut raw_dir = RawDir::new(&dirfd, &mut buf);

    let mut entries: Vec<(FileType, String)> = Vec::new();
    while let Some(entry) = raw_dir.next() {
        let Ok(entry) = entry else {
            continue;
        };
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes.first().is_some_and(|&b| b == b'.') {
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
            collect_entries(&path, &zip_name, out)?;
        } else if actual_ft.is_file() {
            out.push((path, zip_name));
        }
    }

    Ok(())
}

/// 收集文件夹根级列表。zip 内路径前缀 = 文件夹自身名。
pub fn collect_folder(dir: &Path) -> io::Result<Vec<(PathBuf, String)>> {
    let folder_name = dir
        .file_name()
        .map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned());
    let mut out = Vec::with_capacity(64);
    collect_entries(dir, &folder_name, &mut out)?;
    Ok(out)
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
