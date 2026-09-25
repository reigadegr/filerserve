//! `lanfile get` 的递归拉取客户端：把远端 lanfile 掌管的一棵目录树原样镜像到本地，
//! 不打压缩包、不占服务端额外空间。
//!
//! 只走服务端已有的两个 GET 端点，服务端一行不改：
//! - `/api/list/<dir>` 拿到一层目录的条目（name/type/size）；
//! - `/files/<sub>/<name>` 逐个文件落盘。
//!
//! v1 顺序拉取：一个文件一个文件、每请求一条 TCP 连接（`Connection: close`，
//! 读到 EOF 即整段正文，连 `Content-Length` 都不用解析）。结构上每个文件的抓取收口在
//! [`fetch_file`]、目录枚举收口在 [`pull_dir`]，未来要做有限并发时把它们解耦、对文件
//! 任务套一层 `buffer_unordered` 即可，不必重写本模块。

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, tcp::OwnedReadHalf};

/// 简化错误类型：一个能跨线程的 boxed error，`?` 直接收 `io::Error`/`serde_json::Error`。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 子命令入口：`lanfile get <base_url> <remote_dir> [local_dir]`。
///
/// 把 `<base_url>` 下掌管的 `<remote_dir>` 整棵树拉到 `<local_dir>` 之下——先在
/// `<local_dir>` 里以远端目录名建一层子目录，再把该目录的内容塞进去，对齐
/// `scp -r host:dir <local_dir>` 落成 `<local_dir>/dir/` 的语义；拉 root（无目录名可套）
/// 时直接进 `<local_dir>`。不给 `<local_dir>` 则缺省当前目录（拉根缺省 `lanfile-root`）。
pub async fn run(args: &[String]) -> Result<(), BoxError> {
    let (base, remote, local) = parse_pull_args(args)?;
    // `parse_pull_args` 已经去掉 `base` 末尾的 `/`，这里只需剥掉 scheme。
    let host = base
        .strip_prefix("http://")
        .ok_or("base_url 必须以 http:// 开头（不支持 https）")?;
    let target = local_target(&local, &remote);
    tokio::fs::create_dir_all(&target).await?;
    let stats = pull_dir(host, &remote, &target).await?;
    let remote_disp = format!("/{remote}");
    eprintln!(
        "lanfile get: {base}{remote_disp} -> {}（{} 文件，{} 字节，{} 目录）",
        target.display(),
        stats.files,
        stats.bytes,
        stats.dirs
    );
    Ok(())
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs: u64,
    bytes: u64,
}

fn parse_pull_args(args: &[String]) -> Result<(String, String, PathBuf), BoxError> {
    if args.is_empty() {
        return Err("用法: lanfile get <base_url> <remote_dir> [local_dir]".into());
    }
    let base = args[0].trim_end_matches('/').to_string();
    let remote = args
        .get(1)
        .map(|s| s.trim_matches('/').to_string())
        .unwrap_or_default();
    let local = match args.get(2) {
        Some(s) => PathBuf::from(s),
        // 不给 local：命名远端缺省当前目录（run 里再套 basename 一层，落成 ./<basename>）；
        // 拉根缺省 lanfile-root，避免把整棵 share 散落进当前目录。
        None => PathBuf::from(if remote.is_empty() {
            "lanfile-root"
        } else {
            "."
        }),
    };
    Ok((base, remote, local))
}

/// 实际落盘根目录：命名远端目录时在 `local` 下套一层以远端目录名命名的子目录
/// （对齐 `scp -r host:dir local` 落成 `local/dir/` 的语义）；拉 root 时没有名字可套，
/// 直接用 `local`。
fn local_target(local: &Path, remote: &str) -> PathBuf {
    match basename(remote) {
        Some(name) => local.join(name),
        None => local.to_path_buf(),
    }
}

/// 远端路径的末段目录名；root（去首尾斜杠后为空）返回 `None`。
fn basename(remote: &str) -> Option<&str> {
    let trimmed = remote.trim_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        trimmed.rsplit('/').next()
    }
}

/// 递归拉取 `remote` 目录到 `local`。单文件失败只记一条警告并继续；目录枚举失败才上抛。
async fn pull_dir(host: &str, remote: &str, local: &Path) -> Result<Stats, BoxError> {
    let mut stats = Stats::default();
    let list_path = if remote.is_empty() {
        "/api/list".to_string()
    } else {
        format!("/api/list/{}", encode_path(remote))
    };
    let value = get_json(host, &list_path).await?;
    let entries = value["entries"]
        .as_array()
        .ok_or("list 响应缺少 entries 数组")?;
    for entry in entries {
        let name = entry["name"].as_str().ok_or("条目缺少 name")?;
        let entry_type = entry["type"].as_str().unwrap_or("file");
        let remote_child = if remote.is_empty() {
            name.to_string()
        } else {
            format!("{remote}/{name}")
        };
        let local_child = local.join(name);
        if entry_type == "dir" {
            tokio::fs::create_dir_all(&local_child).await?;
            stats.dirs += 1;
            // async 递归必须装箱，否则 future 尺寸无限
            let sub = Box::pin(pull_dir(host, &remote_child, &local_child)).await?;
            stats.files += sub.files;
            stats.dirs += sub.dirs;
            stats.bytes += sub.bytes;
        } else {
            let remote_size = entry["size"].as_u64();
            if !skip_existing(&local_child, remote_size).await {
                match fetch_file(host, &remote_child, &local_child).await {
                    Ok(n) => stats.bytes += n,
                    Err(error) => eprintln!("  跳过 {remote_child}：{error}"),
                }
            }
            stats.files += 1;
        }
    }
    Ok(stats)
}

/// 拉一个文件到 `local`：每请求一条连接，`Connection: close`，正文读到 EOF 落盘。
async fn fetch_file(host: &str, remote: &str, local: &Path) -> Result<u64, BoxError> {
    let path = format!("/files/{}", encode_path(remote));
    let mut reader = http_get(host, &path).await?;
    let mut file = tokio::fs::File::create(local).await?;
    let copied = tokio::io::copy(&mut reader, &mut file).await?;
    Ok(copied)
}

/// `GET <path>` 取 JSON 正文（目录列表）。
async fn get_json(host: &str, path: &str) -> Result<Value, BoxError> {
    let mut reader = http_get(host, path).await?;
    let mut body = Vec::new();
    reader.read_to_end(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// 建连、写 `GET` 请求、读状态行并跳过响应头；返回可继续读正文的 reader，非 200 报错。
/// `fetch_file`、`get_json` 共有的请求前置收口于此，避免两处重复。
async fn http_get(host: &str, path: &str) -> Result<BufReader<OwnedReadHalf>, BoxError> {
    let stream = TcpStream::connect(host).await?;
    let _ = stream.set_nodelay(true);
    let (read, mut write) = stream.into_split();
    write
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
    let mut reader = BufReader::new(read);
    let status = read_status(&mut reader).await?;
    if status != 200 {
        return Err(format!("HTTP {status} 请求 {path}").into());
    }
    Ok(reader)
}

/// 读状态行 + 跳过响应头，返回状态码。正文留给调用方接着读。
async fn read_status(reader: &mut BufReader<OwnedReadHalf>) -> Result<u16, BoxError> {
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or("状态行格式异常")?
        .parse::<u16>()?;
    // 复用同一个 String 读响应头，免得每行各分配一次。
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 || line.trim().is_empty() {
            break;
        }
    }
    Ok(status)
}

/// 本地已存在且尺寸与远端一致就跳过（尺寸级幂等，避免重复落盘）。
async fn skip_existing(path: &Path, remote_size: Option<u64>) -> bool {
    let Some(remote) = remote_size else {
        return false;
    };
    matches!(tokio::fs::metadata(path).await, Ok(m) if m.len() == remote)
}

/// 把远端路径做百分号编码：保留 `A-Za-z0-9-_.~/` 与分隔符 `/`，其余按 UTF-8 字节转义。
/// 用于 `/files/<sub>/<name>` 与 `/api/list/<sub>` 这两类路径。
fn encode_path(path: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parse_pull_args_defaults_local_to_cwd() {
        let (base, remote, local) = parse_pull_args(&["http://h:1".into(), "sub".into()]).unwrap();
        assert_eq!(base, "http://h:1");
        assert_eq!(remote, "sub");
        // 不给 local：缺省当前目录；run 会再套 basename 一层，最终落成 ./sub/
        assert_eq!(local, PathBuf::from("."));
    }

    #[test]
    fn local_target_wraps_named_remote_in_basename_layer() {
        assert_eq!(
            local_target(Path::new("./dst"), "sub"),
            PathBuf::from("./dst/sub")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "a/b"),
            PathBuf::from("./dst/b")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "/sub/"),
            PathBuf::from("./dst/sub")
        );
    }

    #[test]
    fn local_target_root_has_no_wrap() {
        assert_eq!(local_target(Path::new("./dst"), ""), PathBuf::from("./dst"));
        assert_eq!(
            local_target(Path::new("./dst"), "/"),
            PathBuf::from("./dst")
        );
    }

    #[test]
    fn parse_pull_args_strips_slashes() {
        let (_, remote, _) = parse_pull_args(&["http://h:1/".into(), "/sub/deep/".into()]).unwrap();
        assert_eq!(remote, "sub/deep");
    }

    #[test]
    fn parse_pull_args_root_defaults_local() {
        let (_, remote, local) = parse_pull_args(&["http://h:1".into()]).unwrap();
        assert_eq!(remote, "");
        assert_eq!(local, PathBuf::from("lanfile-root"));
    }

    #[test]
    fn parse_pull_args_explicit_local() {
        let (_, _, local) =
            parse_pull_args(&["http://h:1".into(), "sub".into(), "./dst".into()]).unwrap();
        assert_eq!(local, PathBuf::from("./dst"));
    }

    #[test]
    fn parse_pull_args_empty_errors() {
        assert!(parse_pull_args(&[]).is_err());
    }

    #[test]
    fn encode_path_keeps_unreserved_and_slash() {
        assert_eq!(encode_path("sub/a_b-1.txt"), "sub/a_b-1.txt");
    }

    #[test]
    fn encode_path_percent_encodes_space_and_unicode() {
        assert_eq!(encode_path("a b.txt"), "a%20b.txt");
        assert_eq!(encode_path("中"), "%E4%B8%AD");
    }
}
