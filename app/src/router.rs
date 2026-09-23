use std::{
    fmt::{self, Write as _},
    net::IpAddr,
    path::PathBuf,
};

use lanfile_assets::static_routes;
use lanfile_list::list_routes;
use salvo::{
    http::{Version, header::CONTENT_LENGTH},
    prelude::*,
};

/// 访问日志的 target。`tracing` 默认取模块路径，这个文件就是 lib 的根，所以是 `lanfile`。
pub const ACCESS_LOG_TARGET: &str = module_path!();

/// 访问日志的内容，一条 `Display` 就能渲染成整行。
///
/// 原来是六个字段（`?ip, %method, ...`）：`tracing` 为每个字段都要走一遍 visitor
/// （`record_debug`/`record_str`/`record_u64` 各一次），再逐字段拼分隔符。手机上实测这条
/// 日志的用户态开销约 1.9 µs，而 Go 那边一次 `fmt.Sprintf` 只要 0.4 µs。合成一条消息后
/// 只剩一次 `write!`，而字段值的格式化仍然只发生在事件真的启用时——宏把消息放在 enabled
/// 判断里面，所以 `RUST_LOG=off` 时一个字节都不会拼。
pub struct AccessLine<'a> {
    ip: Option<IpAddr>,
    method: &'a str,
    path: &'a str,
    version: Version,
    status: u16,
    size: &'a str,
}

impl fmt::Display for AccessLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "access ip={:?} method={} path={} version={:?} status={} size={}",
            self.ip, self.method, self.path, self.version, self.status, self.size
        )
    }
}

/// 访问日志的出口。
pub enum AccessLog {
    /// `RUST_LOG` 把这条日志关掉了，一行都不写。
    Off,
    /// 交给 `tracing`：stdout 是终端时只有它会写 ANSI 颜色，格式也归它管。
    Tracing,
    /// 直接把整行写进日志缓冲，绕开 `tracing` 的分发与 fmt 层。
    ///
    /// 桌面 release 上实测：一个只有消息、没有字段的 `tracing` 事件每请求要 1.57 µs 用户态，
    /// 全是分发与 fmt 层的固定开销；直写只剩拼行本身。
    Direct(Box<dyn Fn(&AccessLine<'_>) + Send + Sync>),
}

impl AccessLog {
    /// 现在要不要记访问日志。
    ///
    /// `tracing` 的过滤器在 `init` 之后不会再变，所以问一次就够。唯一的例外是 `RUST_LOG`
    /// 里的 span 条件（`[span{field=value}]`），它依赖每条事件所在的 span，这里快照的结果
    /// 可能与逐事件判断不同；访问日志不在任何 span 里，用不到那种写法。
    #[must_use]
    pub fn enabled() -> bool {
        tracing::enabled!(target: ACCESS_LOG_TARGET, tracing::Level::INFO)
    }
}

/// 按 `tracing` 的格式拼出一整行（不含时间戳）：级别右对齐到 5、target、消息、换行。
///
/// 只给 [`AccessLog::Direct`] 用。格式一旦与 `tracing` 漂移，`direct_line_matches_tracing`
/// 就会失败——这是绕过 `tracing` 必须付的保险费。
pub fn render_line(line: &AccessLine<'_>, out: &mut String) -> fmt::Result {
    writeln!(out, "  INFO {ACCESS_LOG_TARGET}: {line}")
}

/// 访问日志的 hoop。
///
/// 用结构体而不是自由函数，是为了把出口挂在 handler 上；塞进 `Depot` 的话每请求都要多一次
/// 类型查找。
struct AccessLogHandler {
    access_log: AccessLog,
}

#[handler]
impl AccessLogHandler {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;

        let method = req.method().as_str();
        let path = req.uri().path();
        let ip = req.remote_addr().ip();
        let version = req.version();
        let status = res.status_code.map_or(200_u16, |c| c.as_u16());
        let size = res
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");

        let line = AccessLine {
            ip,
            method,
            path,
            version,
            status,
            size,
        };
        if let AccessLog::Direct(write) = &self.access_log {
            write(&line);
        } else {
            tracing::info!("{line}");
        }
    }
}

#[must_use]
pub fn build_router(root: PathBuf, port: u16, access_log: AccessLog) -> Router {
    // 日志关掉时连 hoop 都不挂：hoop 是 `#[async_trait]`，每请求要装箱一个 future 再
    // 动态分发一次，而它在 `Off` 下什么都不做。挂上与否的语义完全相同。
    let router = if matches!(access_log, AccessLog::Off) {
        Router::new()
    } else {
        Router::new().hoop(AccessLogHandler { access_log })
    };
    router
        .push(static_routes(root.clone()))
        .push(list_routes(root, port))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::{
        fmt, io,
        sync::{Arc, Mutex},
    };

    use salvo::http::Version;
    use tracing_subscriber::fmt::{MakeWriter, format::Writer, time::FormatTime};

    use super::{ACCESS_LOG_TARGET, AccessLine, render_line};

    /// 固定时间戳，好让直写与 `tracing` 的输出只差这个前缀。
    struct FixedStamp;

    impl FormatTime for FixedStamp {
        fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
            w.write_str("STAMP")
        }
    }

    /// 把 `tracing` 的输出收进内存，好在测试里和直写快路径逐字节比对。
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// 直写快路径拼出来的字节必须和 `tracing` 写出来的一模一样，否则绕过它就等于悄悄改了
    /// 日志格式。`tracing` 升级导致格式漂移时这里会失败。
    #[test]
    fn direct_line_matches_tracing() {
        let line = AccessLine {
            ip: Some("127.0.0.1".parse().unwrap()),
            method: "GET",
            path: "/files/one.bin",
            version: Version::HTTP_11,
            status: 206,
            size: "9",
        };

        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(Arc::clone(&captured)))
            .with_ansi(false)
            .with_timer(FixedStamp)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: ACCESS_LOG_TARGET, "{line}");
        });

        let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        let tail = text.strip_prefix("STAMP").unwrap();

        let mut direct = String::new();
        render_line(&line, &mut direct).unwrap();
        assert_eq!(tail, direct);
    }
}
