use std::{
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

/// 递归收集 (绝对路径, zip 内相对路径)。
/// `symlink_metadata` 不跟随符号链接；跳过 dotfile，与 /api/list 行为一致。
pub fn collect_entries(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        let zip_name = format!("{prefix}/{name}");
        if ft.is_dir() {
            collect_entries(&path, &zip_name, out)?;
        } else if ft.is_file() {
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
    let mut out = Vec::new();
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
