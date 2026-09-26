//! `lanfile get` 的错误类型：按"哪一步、为什么"拆开，报错时能说清到底卡在哪。

use std::fmt;

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
    /// 连接或读取超时：远端在约定时间内一句话都没回。
    Timeout {
        /// 卡在哪一步（`连接`、`读取响应`、`读取目录列表`、`读取正文`）。
        phase: &'static str,
    },
    /// 正文被提前截断：收到的字节数与应得的不一致。
    Truncated { remote: String, want: u64, got: u64 },
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
            Self::Timeout { phase } => {
                write!(f, "{phase}超时：远端在约定时间内没有应答")
            }
            Self::Truncated { remote, want, got } => write!(
                f,
                "远端 `{remote}` 传输不完整：应得 {want} 字节，只收到 {got} 字节"
            ),
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
