//! `lanfile get` 的拉取客户端：把远端 lanfile 掌管的一棵目录树原样镜像到本地，或拉单个文件，
//! 不打压缩包、不占服务端额外空间。
//!
//! 来源两种：
//! - 裸 host：`http://h [remote] [local]`——`remote` 缺省拉根；给了名字先试目录，
//!   `/api/list` 返回 200 当目录拉，404 当单个文件拉；
//! - 直链：URL 的路径或 fragment 直接指明远端——`http://h/files/<sub>`、`http://h/pull/<sub>`
//!   当文件，`http://h/api/zip/<sub>`、`http://h/api/list/<sub>`、`http://h/#<sub>` 当目录，
//!   其余非空路径（`http://h/<sub>`，如 `/.pi`）就是远端本身、文件还是目录交给 `/api/list` 探测。
//!
//! 只走服务端两个 GET 端点：
//! - `/api/list/<dir>` 拿到一层目录的条目（name/type/size）；
//! - `/pull/<sub>` 逐个文件落盘。`/pull` 是拉取专用的端点：不碰 `/files` 那套 fd 缓存，
//!   也不编码拉取端用不到的 `ETag`、`Last-Modified` 与 `Content-Disposition`（见 `lanfile_assets`）。
//!
//! v1 顺序拉取：一个文件一个文件、每请求一条 TCP 连接（`Connection: close`，
//! 读到 EOF 即整段正文，连 `Content-Length` 都不用解析）。结构上每个文件的抓取收口在
//! [`fetch_file`]、目录枚举收口在 [`list_entries`]，未来要做有限并发时把它们解耦、对文件
//! 任务套一层 `buffer_unordered` 即可，不必重写本模块。

use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// 简化错误类型：一个能跨线程的 boxed error，`run` 的出口用它收口内部各处错误。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// `lanfile get` 的内部错误。
///
/// 原先一律 `Box<dyn Error>` 加一句 `HTTP {status} 请求 {path}`，把"给的是文件、所以
/// `/api/list` 返回 404"说成了普通的 HTTP 失败。这里按"哪一步、为什么"拆开：
/// 连不上、服务端非 200、远端不存在、响应结构不对、底层 IO、JSON 解析各占一条，
/// 报错时能说清到底卡在哪。
#[derive(Debug)]
pub enum Error {
    /// 连不上远端。
    Connect {
        host: String,
        source: std::io::Error,
    },
    /// 远端返回了非 200 状态码。
    Http { status: u16, path: String },
    /// 远端这个路径既不是目录也不是文件（`/api/list` 与 `/pull` 都 404）。
    NotFound { remote: String },
    /// 命令行参数或响应结构不符合预期。
    Malformed(&'static str),
    /// 底层 IO（读写、建连之后的网络错误等）。
    Io(std::io::Error),
    /// JSON 解析失败。
    Json(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect { host, source } => {
                write!(f, "连接 `{host}` 失败：{source}")
            }
            Self::Http { status, path } => write!(f, "HTTP {status} 请求 {path}"),
            Self::NotFound { remote } => {
                write!(f, "远端 `{remote}` 不存在（既不是目录也不是文件）")
            }
            Self::Malformed(what) => write!(f, "{what}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect { source, .. } | Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// 用法串：直链与裸 host 两种源共用同一个尾部（可选 `local_dir` 与 `--flat`）。
const USAGE: &str = "用法: lanfile get <base_url|直链> [<remote_dir>] [local_dir] [--flat]";

/// 子命令入口：`lanfile get <base_url|直链> [<remote_dir>] [local_dir] [--flat]`。
///
/// 两种来源：
/// - 裸 host（`http://h [remote] [local] [--flat]`）：`remote` 缺省即拉根；给了名字则先试
///   目录，`/api/list` 返回 200 当目录拉，404 当单个文件拉（对 `lanfile get http://h a.tgz`
///   不再因 `/api/list` 404 直接失败，而是改走 `/pull` 把文件拉下来）。
/// - 直链（URL 的路径/fragment 已指明远端）：`http://h/files/<sub>`、`http://h/pull/<sub>` 当
///   文件，`http://h/api/zip/<sub>`、`http://h/api/list/<sub>`、`http://h/#<sub>` 当目录，其余
///   非空路径（`http://h/<sub>`，如 `/.pi`）就是远端本身、kind 交给 `/api/list` 探测；不给
///   `local` 则落进当前目录（文件取末段为名）。
///
/// 落盘语义对齐 `scp -r`：默认拉目录时在 `local` 下套一层以远端目录名命名的子目录
/// （`local/dir/`）；`--flat`/`-f` 不套层，目录内容直接落 `local`（恢复 8f8a234 前的默认）。
/// 拉单个文件时直接落 `local/<basename>`，不套层。不给 `local` 时，命名远端/文件缺省
/// 当前目录，拉根缺省 `lanfile-root`（避免把整棵 share 散落进当前目录）。
pub async fn run(args: &[String]) -> Result<(), BoxError> {
    let p = parse_args(args)?;
    match p.kind {
        // 直链已指明 kind：文件直接拉、目录当目录拉。
        Kind::File => run_file(&p).await,
        Kind::Dir => run_dir(&p, false).await,
        // 裸 host：先试目录，`/api/list` 404 再当文件。
        Kind::Auto => run_dir(&p, true).await,
    }
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs: u64,
    bytes: u64,
}

/// 解析后的参数。
struct Parsed {
    /// `http://<host>`，打印进度用。
    base: String,
    /// 已剥 scheme 的 `host[:port]`，建连用。
    host: String,
    /// 远端相对路径（已剥首尾斜杠）；拉根为空串。
    remote: String,
    /// 已知 kind，还是得探测。
    kind: Kind,
    /// 本地落盘目录。
    local: PathBuf,
    /// 不套 basename 一层：目录内容直接落进 `local`，根总是如此（无名字可套）。
    flat: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// 裸 host + 名字：先试目录，404 再当文件。
    Auto,
    /// `/api/zip/`、`/api/list/`、`/#<sub>` 直链：当目录。
    Dir,
    /// `/files/`、`/pull/` 直链：直接当文件。
    File,
}

/// 把 404 转成"远端不存在"；其他错误原样返回。
///
/// `/api/list` 与 `/pull` 都用 404 表示"这条远端路径不存在"，目录探测与单文件拉取两处
/// 都需要做同一个转换，所以收在这里。
fn to_not_found(error: Error, remote: &str) -> Error {
    match error {
        Error::Http { status: 404, .. } => Error::NotFound {
            remote: remote.to_string(),
        },
        other => other,
    }
}

/// 当文件拉 `/pull/<remote>`；404 统一转成「远端不存在」（裸 host 探测到这一步即目录与文件都不是）。
async fn run_file(p: &Parsed) -> Result<(), BoxError> {
    pull_file_run(p)
        .await
        .map_err(|error| to_not_found(error, &p.remote).into())
}

/// 当目录拉 `/api/list/<remote>`；404 时按 `fallback_file` 决定下一步：
/// - `true`（裸 host）：改走 `/pull` 试单个文件；
/// - `false`（目录直链）：URL 已经说清楚是目录，直接报"远端不存在"。
async fn run_dir(p: &Parsed, fallback_file: bool) -> Result<(), BoxError> {
    match list_entries(&p.host, &p.remote).await {
        Ok(entries) => pull_dir_run(p, entries).await.map_err(Into::into),
        Err(Error::Http { status: 404, .. }) if fallback_file => run_file(p).await,
        Err(error) => Err(to_not_found(error, &p.remote).into()),
    }
}

/// 解析命令行：`<base_url|直链> [remote] [local] [--flat]`。
fn parse_args(args: &[String]) -> Result<Parsed, Error> {
    if args.is_empty() {
        return Err(Error::Malformed(USAGE));
    }
    let src = parse_source(&args[0])?;
    let (pos, flat) = parse_trailing(&args[1..]);
    let (remote, kind, local) = if let Some(direct) = src.direct {
        // 直链：remote 已在 URL 里指明，后面只剩可选 local 与 flag。
        let local = match pos {
            [] => None,
            [s] => Some(PathBuf::from(s)),
            _ => return Err(Error::Malformed(USAGE)),
        };
        (direct.remote, direct.kind, local)
    } else {
        // 裸 host：[remote] [local]；remote 缺省即拉根。
        if pos.len() > 2 {
            return Err(Error::Malformed(USAGE));
        }
        let remote = pos
            .first()
            .map(|s| s.trim_matches('/').to_string())
            .unwrap_or_default();
        let local = pos.get(1).map(PathBuf::from);
        (remote, Kind::Auto, local)
    };
    // 不给 local：命名远端/文件缺省当前目录，拉根缺省 lanfile-root。
    let local = local.unwrap_or_else(|| {
        if remote.is_empty() {
            PathBuf::from("lanfile-root")
        } else {
            PathBuf::from(".")
        }
    });
    Ok(Parsed {
        base: src.base,
        host: src.host,
        remote,
        kind,
        local,
        flat,
    })
}

/// 末尾若是 `--flat`/`-f` 则取下，返回剩余位置参数与 flat 标志。
/// flag 只认末尾一个：按用户的写法，它跟在 `local_dir` 之后。
fn parse_trailing(args: &[String]) -> (&[String], bool) {
    match args.last() {
        Some(last) if last.as_str() == "--flat" || last.as_str() == "-f" => {
            (&args[..args.len() - 1], true)
        }
        _ => (args, false),
    }
}

/// 直链解析结果：URL 已指明远端路径与它是文件还是目录。
struct Direct {
    remote: String,
    kind: Kind,
}

/// URL 解析结果：`base`/`host` 永远有；`direct` 为 `None` 即裸 host（remote 留给位置参数）。
struct Source {
    base: String,
    host: String,
    direct: Option<Direct>,
}

/// 解析 URL：剥 scheme，分 `host`、路径与 fragment，再交给 [`direct_of`] 认直链。
fn parse_source(url: &str) -> Result<Source, Error> {
    let rest = url.strip_prefix("http://").ok_or(Error::Malformed(
        "base_url 必须以 http:// 开头（不支持 https）",
    ))?;
    // host 到第一个 `/`、`?` 或 `#` 为止——`http://h#frag` 这种没有 `/` 的写法也要切对。
    let host_end = rest
        .bytes()
        .position(|b| matches!(b, b'/' | b'?' | b'#'))
        .unwrap_or(rest.len());
    let host = rest[..host_end].to_string();
    let tail = &rest[host_end..];
    let after = tail.strip_prefix('/').unwrap_or(tail);
    // `#` 之后是 fragment；路径到 `#` 前的第一个 `?` 为止——`?` 在 `#` 之后时归 fragment。
    let (head, fragment) = after.split_once('#').unwrap_or((after, ""));
    let path = head.split_once('?').map_or(head, |(path, _)| path);
    Ok(Source {
        base: format!("http://{host}"),
        host,
        direct: direct_of(path, fragment),
    })
}

/// 认直链，给出 URL 里已经指明的那条远端路径：
/// - `/files/<sub>`、`/pull/<sub>` 当文件，`/api/zip/<sub>`、`/api/list/<sub>` 当目录；
/// - `/#<sub>` 当目录；
/// - 其余非空路径本身就是远端，kind 待探测——`http://h/.pi` 等价于 `lanfile get http://h .pi`；
/// - 只有空路径（`http://h`、`http://h/`）返回 `None`，remote 留给位置参数。
fn direct_of(path: &str, fragment: &str) -> Option<Direct> {
    if let Some(sub) = path
        .strip_prefix("files/")
        .or_else(|| path.strip_prefix("pull/"))
        .filter(|sub| !sub.is_empty())
    {
        return Some(Direct {
            remote: percent_decode(sub),
            kind: Kind::File,
        });
    }
    if let Some(sub) = path
        .strip_prefix("api/zip/")
        .or_else(|| path.strip_prefix("api/list/"))
        .filter(|sub| !sub.is_empty())
    {
        return Some(Direct {
            remote: percent_decode(sub),
            kind: Kind::Dir,
        });
    }
    // 站内直链 /#<sub>：path 为空（`/`、`/#<sub>`，或没写 `/` 的 `#<sub>`）。`#` 在 `?` 之前时
    // 用户多写的查询串也算 fragment（`/#sub?x=1`），一并切掉。
    if path.is_empty() {
        let sub = fragment.trim_start_matches('/');
        let sub = sub.split_once('?').map_or(sub, |(sub, _)| sub);
        if !sub.is_empty() {
            return Some(Direct {
                remote: percent_decode(sub),
                kind: Kind::Dir,
            });
        }
        return None;
    }
    // 其余非空路径本身就是远端，是文件还是目录留给 `/api/list` 探测。早先这里返回 `None`
    // 退回裸 host，而裸 host 的 remote 缺省为空＝拉根，于是 `http://h/.pi` 这样最自然的
    // 写法会默默把整棵 share 拖进 `lanfile-root`。
    let sub = path.trim_matches('/');
    if sub.is_empty() {
        return None;
    }
    Some(Direct {
        remote: percent_decode(sub),
        kind: Kind::Auto,
    })
}

/// 百分号解码：把 `%XX` 还原成原字节，用于直链里 URL 编码过的子路径（解码后再交给
/// [`encode_path`] 重新编码发请求，本地文件名取解码后的末段）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 拉目录到 `local`（默认在 `local` 下套一层远端目录名，对齐 `scp -r`；`--flat` 不套层）。
async fn pull_dir_run(p: &Parsed, entries: Vec<RemoteEntry>) -> Result<(), Error> {
    let remote = &p.remote;
    let target = local_target(&p.local, remote, p.flat);
    tokio::fs::create_dir_all(&target).await?;
    let stats = pull_entries(&p.host, remote, &target, entries).await?;
    eprintln!(
        "lanfile get: {}/{remote} -> {}（{} 文件，{} 字节，{} 目录）",
        p.base,
        target.display(),
        stats.files,
        stats.bytes,
        stats.dirs
    );
    Ok(())
}

/// 拉单个文件到 `local/<basename>`：不套层，落盘根目录按需建。
async fn pull_file_run(p: &Parsed) -> Result<(), Error> {
    let remote = &p.remote;
    let name = basename(remote).unwrap_or("download");
    let target = p.local.join(name);
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = fetch_file(&p.host, remote, &target).await?;
    eprintln!(
        "lanfile get: {}/{remote} -> {}（{bytes} 字节）",
        p.base,
        target.display()
    );
    Ok(())
}

/// 实际落盘根目录：默认在 `local` 下套一层以远端目录名命名的子目录（对齐
/// `scp -r host:dir local` 落成 `local/dir/` 的语义）；`flat` 为真或拉 root（无名字可套）
/// 时直接用 `local`。
fn local_target(local: &Path, remote: &str, flat: bool) -> PathBuf {
    if !flat && let Some(name) = basename(remote) {
        return local.join(name);
    }
    local.to_path_buf()
}

/// 远端路径的末段目录名；root（去首尾斜杠后为空）返回 `None`。
///
/// 用 `memrchr` 从尾部找最后一个 `/`，省掉 `trim_matches` + `rsplit` 两层迭代器
/// （基准见 `bench_basename`）。
fn basename(remote: &str) -> Option<&str> {
    // 先跳过尾部的 `/`，等价于 `trim_matches('/')` 的右侧
    let end = remote.as_bytes().iter().rposition(|b| *b != b'/')? + 1;
    let head = &remote[..end];
    Some(match memchr::memrchr(b'/', head.as_bytes()) {
        Some(at) => &head[at + 1..],
        None => head,
    })
}

/// `/api/list` 返回的一条条目。
///
/// `type` 缺字段按文件处理（与原先 `unwrap_or("file")` 一致）；`size` 仅文件有，目录为
/// `None`，用于"本地已存在且尺寸一致就跳过"。
#[derive(Deserialize)]
struct RemoteEntry {
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    size: Option<u64>,
}

impl RemoteEntry {
    fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// `/api/list` 的响应体：只取 `entries`，其余字段（`path`/`lan_ip`/`port`）客户端用不到。
#[derive(Deserialize)]
struct ListResponse {
    entries: Vec<RemoteEntry>,
}

/// 递归拉取 `remote` 目录到 `local`：先取这层条目，再逐条落盘。
async fn pull_dir(host: &str, remote: &str, local: &Path) -> Result<Stats, Error> {
    let entries = list_entries(host, remote).await?;
    pull_entries(host, remote, local, entries).await
}

/// 取一层目录的条目：`GET /api/list[/<remote>]`，正文一次性读全再反序列化。
async fn list_entries(host: &str, remote: &str) -> Result<Vec<RemoteEntry>, Error> {
    let path = if remote.is_empty() {
        "/api/list".to_string()
    } else {
        format!("/api/list/{}", encode_path(remote))
    };
    let mut reader = http_get(host, &path).await?;
    let mut body = Vec::new();
    reader.read_to_end(&mut body).await?;
    Ok(serde_json::from_slice::<ListResponse>(&body)?.entries)
}

/// 把一层条目落到 `local`：目录递归，文件逐个抓。单文件失败只记一条警告并继续。
///
/// 与 [`list_entries`] 拆开是为了让顶层那一次列表请求的失败（404）能被 [`run_dir`] 捕获、
/// 转成"远端不存在"，而不是在这里被当成"递归里某层目录没了"。
async fn pull_entries(
    host: &str,
    remote: &str,
    local: &Path,
    entries: Vec<RemoteEntry>,
) -> Result<Stats, Error> {
    let mut stats = Stats::default();
    for entry in entries {
        let remote_child = if remote.is_empty() {
            entry.name.clone()
        } else {
            format!("{remote}/{}", entry.name)
        };
        let local_child = local.join(&entry.name);
        if entry.is_dir() {
            tokio::fs::create_dir_all(&local_child).await?;
            stats.dirs += 1;
            // async 递归必须装箱，否则 future 尺寸无限
            let sub = Box::pin(pull_dir(host, &remote_child, &local_child)).await?;
            stats.files += sub.files;
            stats.dirs += sub.dirs;
            stats.bytes += sub.bytes;
        } else {
            let remote_size = entry.size;
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
async fn fetch_file(host: &str, remote: &str, local: &Path) -> Result<u64, Error> {
    let path = format!("/pull/{}", encode_path(remote));
    let mut reader = http_get(host, &path).await?;
    let mut file = tokio::fs::File::create(local).await?;
    let copied = tokio::io::copy(&mut reader, &mut file).await?;
    Ok(copied)
}

/// 建连、写 `GET` 请求、读状态行并跳过响应头；返回可继续读正文的 reader，非 200 报错。
/// `fetch_file`、`list_entries` 共有的请求前置收口于此，避免两处重复。
///
/// 建连后整条 `TcpStream` 直接交给 reader，**不做 `into_split`**：那样写半边会在请求发完
/// 后出作用域，`OwnedWriteHalf::drop` 顺手 `shutdown(Write)`，而这个提前的 half-close 会让
/// 服务端（salvo/hyper 的 `http1` 默认 `half_close = false`）在读到 EOF 时判定连接中断、
/// 丢掉还在飞的响应——几十 MB 的文件就只落下几 MB。标准客户端（curl、浏览器）也不 half-close。
async fn http_get(host: &str, path: &str) -> Result<BufReader<TcpStream>, Error> {
    let mut stream = TcpStream::connect(host)
        .await
        .map_err(|source| Error::Connect {
            host: host.to_string(),
            source,
        })?;
    let _ = stream.set_nodelay(true);
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
    let mut reader = BufReader::new(stream);
    let status = read_status(&mut reader).await?;
    if status != 200 {
        return Err(Error::Http {
            status,
            path: path.to_string(),
        });
    }
    Ok(reader)
}

/// 读状态行 + 跳过响应头，返回状态码。正文留给调用方接着读。
async fn read_status(reader: &mut BufReader<TcpStream>) -> Result<u16, Error> {
    // 状态行最长也就几十字节，一次给够，免得 `read_line` 中途扩容
    let mut status_line = String::with_capacity(64);
    reader.read_line(&mut status_line).await?;
    let status = status_code(&status_line).ok_or(Error::Malformed("状态行格式异常"))?;
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

/// 从状态行 `HTTP/1.1 200 OK` 里取出状态码。
///
/// 先用 `memchr` 定位版本号后那个空格，再在剩下的一小段里取词：比
/// `split_whitespace().nth(1)` 少一整层 `Pattern` 与迭代器分发（基准见 `bench_status_code`）。
/// 仍按任意 ASCII 空白切分，与原来的宽容度一致。
fn status_code(line: &str) -> Option<u16> {
    let bytes = line.as_bytes();
    let rest = &bytes[memchr::memchr(b' ', bytes)? + 1..];
    let start = rest.iter().position(|b| !b.is_ascii_whitespace())?;
    let token = &rest[start..];
    let end = token
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(token.len());
    std::str::from_utf8(&token[..end]).ok()?.parse().ok()
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
///
/// 转义直接查表手写两个 hex 字符，不走 `fmt::Write`：文件名带中文或空格时每个字节
/// 都会走一次格式化分发，这条路径在每个文件下载时都会经过。
fn encode_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[(byte >> 4) as usize]));
                out.push(char::from(HEX[(byte & 0x0F) as usize]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// 跑 `iters` 次取平均纳秒，先热身 `iters/10` 次；两个基准共用。
    fn time<R>(iters: u32, f: impl Fn() -> R) -> f64 {
        use std::hint::black_box;
        use std::time::Instant;

        for _ in 0..iters / 10 {
            black_box(f());
        }
        let start = Instant::now();
        for _ in 0..iters {
            black_box(f());
        }
        start.elapsed().as_secs_f64() * 1e9 / f64::from(iters)
    }

    #[test]
    fn parse_args_bare_host_defaults_local_to_cwd() {
        let p = parse_args(&["http://h:1".into(), "sub".into()]).unwrap();
        assert_eq!(p.base, "http://h:1");
        assert_eq!(p.host, "h:1");
        assert_eq!(p.remote, "sub");
        assert_eq!(p.kind, Kind::Auto);
        // 不给 local：缺省当前目录；run 会再套 basename 一层，最终落成 ./sub/
        assert_eq!(p.local, PathBuf::from("."));
    }

    #[test]
    fn parse_args_bare_host_strips_slashes() {
        let p = parse_args(&["http://h:1/".into(), "/sub/deep/".into()]).unwrap();
        assert_eq!(p.remote, "sub/deep");
        assert_eq!(p.kind, Kind::Auto);
    }

    #[test]
    fn parse_args_bare_host_root_defaults_local() {
        let p = parse_args(&["http://h:1".into()]).unwrap();
        assert_eq!(p.remote, "");
        assert_eq!(p.local, PathBuf::from("lanfile-root"));
    }

    #[test]
    fn parse_args_bare_host_explicit_local() {
        let p = parse_args(&["http://h:1".into(), "sub".into(), "./dst".into()]).unwrap();
        assert_eq!(p.local, PathBuf::from("./dst"));
    }

    #[test]
    fn parse_args_empty_errors() {
        assert!(parse_args(&[]).is_err());
    }

    #[test]
    fn parse_args_file_direct_link_defaults_local_to_cwd() {
        let p = parse_args(&["http://h:1/files/boards.md".into()]).unwrap();
        assert_eq!(p.base, "http://h:1");
        assert_eq!(p.host, "h:1");
        assert_eq!(p.remote, "boards.md");
        assert_eq!(p.kind, Kind::File);
        assert_eq!(p.local, PathBuf::from("."));
    }

    #[test]
    fn parse_args_pull_direct_link_is_file() {
        let p = parse_args(&["http://h:1/pull/a/b.txt".into()]).unwrap();
        assert_eq!(p.remote, "a/b.txt");
        assert_eq!(p.kind, Kind::File);
    }

    #[test]
    fn parse_args_file_direct_link_explicit_local() {
        let p = parse_args(&["http://h:1/files/x".into(), "./dst".into()]).unwrap();
        assert_eq!(p.local, PathBuf::from("./dst"));
    }

    #[test]
    fn parse_args_direct_link_strips_query_and_fragment() {
        let p = parse_args(&["http://h:1/files/x.txt?v=1#frag".into()]).unwrap();
        assert_eq!(p.remote, "x.txt");
    }

    #[test]
    fn parse_args_fragment_after_query_direct_link_is_dir() {
        // `?` 在 `#` 之前：`?` 之后是查询串，fragment 仍要认出来。
        let p = parse_args(&["http://h:1?x=1#filerserve".into()]).unwrap();
        assert_eq!(p.host, "h:1");
        assert_eq!(p.remote, "filerserve");
        assert_eq!(p.kind, Kind::Dir);
    }

    #[test]
    fn parse_args_fragment_direct_link_strips_query() {
        // `#` 在 `?` 之前：查询串随 fragment 一起被切掉。
        let p = parse_args(&["http://h:1/#filerserve?x=1".into()]).unwrap();
        assert_eq!(p.remote, "filerserve");
        assert_eq!(p.kind, Kind::Dir);
    }

    #[test]
    fn parse_args_path_only_url_is_a_remote() {
        // 路径本身就是远端（kind 待探测）：http://h/.pi 等价于 lanfile get http://h .pi。
        let p = parse_args(&["http://h:1/.pi".into()]).unwrap();
        assert_eq!(p.host, "h:1");
        assert_eq!(p.remote, ".pi");
        assert_eq!(p.kind, Kind::Auto);
        assert_eq!(p.local, PathBuf::from("."));
        // 多级路径整条都是远端；它后面的位置参数是 local。
        let p = parse_args(&["http://h:1/a/b".into(), "./dst".into()]).unwrap();
        assert_eq!(p.remote, "a/b");
        assert_eq!(p.kind, Kind::Auto);
        assert_eq!(p.local, PathBuf::from("./dst"));
        // 带 fragment 时路径优先，fragment 不再重复当远端。
        let p = parse_args(&["http://h:1/.pi#x".into()]).unwrap();
        assert_eq!(p.remote, ".pi");
        // 只有空路径才算裸 host，remote 仍从位置参数来。
        let p = parse_args(&["http://h:1/".into(), "sub".into()]).unwrap();
        assert_eq!(p.remote, "sub");
        assert_eq!(p.kind, Kind::Auto);
    }

    #[test]
    fn parse_args_flat_flag_after_local() {
        let p = parse_args(&[
            "http://h:1".into(),
            "sub".into(),
            "./dst".into(),
            "--flat".into(),
        ])
        .unwrap();
        assert_eq!(p.remote, "sub");
        assert_eq!(p.local, PathBuf::from("./dst"));
        assert!(p.flat);
    }

    #[test]
    fn parse_args_flat_short_flag_no_local() {
        // -f 且不给 local：命名远端缺省当前目录。
        let p = parse_args(&["http://h:1".into(), "sub".into(), "-f".into()]).unwrap();
        assert_eq!(p.remote, "sub");
        assert_eq!(p.local, PathBuf::from("."));
        assert!(p.flat);
    }

    #[test]
    fn parse_args_flat_root() {
        // 拉根 + --flat：flat 对根是 no-op。
        let p = parse_args(&["http://h:1".into(), "--flat".into()]).unwrap();
        assert_eq!(p.remote, "");
        assert_eq!(p.local, PathBuf::from("lanfile-root"));
        assert!(p.flat);
    }

    #[test]
    fn parse_args_file_direct_link_flat() {
        let p =
            parse_args(&["http://h:1/files/x".into(), "./dst".into(), "--flat".into()]).unwrap();
        assert_eq!(p.remote, "x");
        assert_eq!(p.kind, Kind::File);
        assert_eq!(p.local, PathBuf::from("./dst"));
        assert!(p.flat);
    }

    #[test]
    fn parse_args_no_flat_by_default() {
        let p = parse_args(&["http://h:1".into(), "sub".into(), "./dst".into()]).unwrap();
        assert!(!p.flat);
    }

    #[test]
    fn parse_args_zip_direct_link_is_dir() {
        let p = parse_args(&["http://h:1/api/zip/filerserve".into()]).unwrap();
        assert_eq!(p.host, "h:1");
        assert_eq!(p.remote, "filerserve");
        assert_eq!(p.kind, Kind::Dir);
        assert_eq!(p.local, PathBuf::from("."));
    }

    #[test]
    fn parse_args_list_direct_link_is_dir() {
        let p = parse_args(&["http://h:1/api/list/a/b".into()]).unwrap();
        assert_eq!(p.remote, "a/b");
        assert_eq!(p.kind, Kind::Dir);
    }

    #[test]
    fn parse_args_fragment_direct_link_is_dir() {
        let p = parse_args(&["http://h:1/#filerserve".into()]).unwrap();
        assert_eq!(p.remote, "filerserve");
        assert_eq!(p.kind, Kind::Dir);
        // 不写 `/`、不带路径的 `#<sub>` 同样认
        let p = parse_args(&["http://h:1#filerserve".into()]).unwrap();
        assert_eq!(p.remote, "filerserve");
        assert_eq!(p.kind, Kind::Dir);
    }

    #[test]
    fn parse_args_dir_direct_link_explicit_local_and_flat() {
        let p = parse_args(&[
            "http://h:1/api/zip/filerserve".into(),
            "./dst".into(),
            "--flat".into(),
        ])
        .unwrap();
        assert_eq!(p.local, PathBuf::from("./dst"));
        assert!(p.flat);
    }

    #[test]
    fn parse_args_route_prefix_without_name_is_not_a_direct_link() {
        // `/api/zip/` 后面没名字：不当目录直链，整条路径退化成普通远端（kind 仍待探测）。
        let p = parse_args(&["http://h:1/api/zip/".into(), "./dst".into()]).unwrap();
        assert_eq!(p.remote, "api/zip");
        assert_eq!(p.kind, Kind::Auto);
        assert_eq!(p.local, PathBuf::from("./dst"));
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("boards.md"), "boards.md");
        assert_eq!(percent_decode("a%20b.txt"), "a b.txt");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        // 非法 %XX 原样保留
        assert_eq!(percent_decode("a%2z.txt"), "a%2z.txt");
    }

    #[test]
    fn local_target_wraps_named_remote_in_basename_layer() {
        assert_eq!(
            local_target(Path::new("./dst"), "sub", false),
            PathBuf::from("./dst/sub")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "a/b", false),
            PathBuf::from("./dst/b")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "/sub/", false),
            PathBuf::from("./dst/sub")
        );
    }

    #[test]
    fn local_target_root_has_no_wrap() {
        assert_eq!(
            local_target(Path::new("./dst"), "", false),
            PathBuf::from("./dst")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "/", false),
            PathBuf::from("./dst")
        );
    }

    #[test]
    fn local_target_flat_drops_basename_layer() {
        // --flat：不套 basename 一层，直接落 local；根无名字可套，flat 是 no-op。
        assert_eq!(
            local_target(Path::new("./dst"), "sub", true),
            PathBuf::from("./dst")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "a/b", true),
            PathBuf::from("./dst")
        );
        assert_eq!(
            local_target(Path::new("./dst"), "", true),
            PathBuf::from("./dst")
        );
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

    /// 基准：`memchr` 取状态码 vs `split_whitespace().nth(1)`，逐文件都会走一遍。
    ///
    /// `cargo test` 默认跑在 `opt-level = 0`：std 是预编译的优化产物而 `memchr` 不是，
    /// 那种 profile 下这一项偏向原实现；要看真实差距得加 `--release`。
    #[test]
    #[ignore = "微基准，需 cargo test --release -- --ignored 显式运行"]
    fn bench_status_code() {
        for line in ["HTTP/1.1 200 OK\r\n", "HTTP/1.1 404 Not Found\r\n"] {
            let old = || line.split_whitespace().nth(1)?.parse::<u16>().ok();
            assert_eq!(old(), status_code(line), "{line} 取值不一致");

            let old_ns = time(500_000, old);
            let memchr_ns = time(500_000, || status_code(line));
            println!(
                "基准 status_code（{}B）: split_whitespace {old_ns:.1} ns vs memchr {memchr_ns:.1} ns",
                line.len()
            );
            // 只卡数量级：未优化的测试 profile 抖动大，这里不追求证明「更快」
            assert!(
                memchr_ns < old_ns * 10.0,
                "memchr 版比原实现慢了一个数量级: {memchr_ns:.1} vs {old_ns:.1} ns"
            );
        }
    }

    /// 基准：`memrchr` 找末段 vs `trim_matches` + `rsplit`
    #[test]
    #[ignore = "微基准，需 cargo test --release -- --ignored 显式运行"]
    fn bench_basename() {
        for remote in ["sub", "a/b", "sub/deeper/more/leaf", "sub/deeper/", "///"] {
            // 原实现：去首尾斜杠后为空即 `None`（`///` 走的就是这一支）
            let old = || {
                let trimmed = remote.trim_matches('/');
                if trimmed.is_empty() {
                    None
                } else {
                    trimmed.rsplit('/').next()
                }
            };
            assert_eq!(old(), basename(remote), "{remote} 取值不一致");

            let old_ns = time(500_000, old);
            let memchr_ns = time(500_000, || basename(remote));
            println!(
                "基准 basename（{}B）: rsplit {old_ns:.1} ns vs memrchr {memchr_ns:.1} ns",
                remote.len()
            );
            // 只卡数量级：未优化的测试 profile 抖动大，这里不追求证明「更快」
            assert!(
                memchr_ns < old_ns * 10.0,
                "memchr 版比原实现慢了一个数量级: {memchr_ns:.1} vs {old_ns:.1} ns"
            );
        }
    }
}
