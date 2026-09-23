use std::{fmt, net::IpAddr, path::PathBuf};

use lanfile_assets::static_routes;
use lanfile_list::list_routes;
use salvo::{
    http::{Version, header::CONTENT_LENGTH},
    prelude::*,
};

/// 访问日志的内容，作为**一条** `Display` 消息交给 `tracing`。
///
/// 原来是六个字段（`?ip, %method, ...`）：`tracing` 为每个字段都要走一遍 visitor
/// （`record_debug`/`record_str`/`record_u64` 各一次），再逐字段拼分隔符。手机上实测这条
/// 日志的用户态开销约 1.9 µs，而 Go 那边一次 `fmt.Sprintf` 只要 0.4 µs。合成一条消息后
/// 只剩一次 `write!`，而字段值的格式化仍然只发生在事件真的启用时——宏把消息放在 enabled
/// 判断里面，所以 `RUST_LOG=off` 时一个字节都不会拼。
struct AccessLine<'a> {
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

#[handler]
async fn access_log(req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
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
    tracing::info!("{line}");
}

#[must_use]
pub fn build_router(root: PathBuf, port: u16) -> Router {
    Router::new()
        .hoop(access_log)
        .push(static_routes(root.clone()))
        .push(list_routes(root, port))
}
