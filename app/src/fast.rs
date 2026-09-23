//! `/files` 的 hyper 快路径。
//!
//! salvo 的 `HyperHandler` 每请求要做一整套：按 `Host` 重建 `Uri`、往 `Extensions` 里插
//! `ConnCtrl`、把路径 `to_owned`、构造 `PathState`、跑一遍路由匹配、重建 handler 链
//! （链上每个 handler 都是 `#[async_trait]`，各要装箱一个 future），最后再把整个 future
//! 装箱。按调用点归因，这一圈是每请求 15 次堆分配加三次异步跳转，而 `/files` 用不到路由
//! 与任何中间件。
//!
//! 所以这里自己跑 accept 循环：`/files/*` 直接构造 salvo 的 `Request`/`Response` 调用
//! [`ServeFiles::serve`]（不经过 `dyn Handler`，不装箱），其余路径原样交给 salvo 的
//! `HyperHandler`。HTTP/1 的配置直接用 salvo 的 [`HttpBuilder::new`]——`Server::new` 用的
//! 就是它，所以连接层行为与原来完全一致。

use std::{borrow::Cow, future::Future, io, path::PathBuf, pin::Pin, sync::Arc};

use lanfile_assets::ServeFiles;
use lanfile_sendfile::{SendfileSlot, SendfileStream, conn_key};
use salvo::{
    Depot, Request, Response, Router, Service,
    catcher::Catcher,
    conn::{ConnCtrl, HttpBuilder, SocketAddr, StraightStream},
    http::{Method, StatusCode, body::ResBody, uri::Scheme},
    hyper::{
        Request as HyperRequest, Response as HyperResponse, body::Incoming,
        service::Service as HyperService,
    },
    routing::decode_url_path,
};
use tokio::net::TcpListener;

use crate::{AccessLog, log_access};

/// salvo 的响应 future 类型，与 `HyperHandler` 的 `Future` 完全一致。
type BoxedFuture =
    Pin<Box<dyn Future<Output = Result<HyperResponse<ResBody>, salvo::hyper::Error>> + Send>>;

/// `/files/{**path}` 的取值：去掉前缀与开头的斜杠，再按 salvo 的规则解码。
///
/// 末尾带斜杠的请求在 salvo 那边匹配不上（实测 `/files/f.bin/` 是 404），这里用空串表示，
/// `ServeFiles` 同样会把它判成 404。返回 `None` 表示这条请求不归快路径管，交给 salvo。
fn files_sub_path(path: &str) -> Option<Cow<'_, str>> {
    let rest = path.strip_prefix("/files/")?.trim_start_matches('/');
    if rest.ends_with('/') {
        return Some(Cow::Borrowed(""));
    }
    Some(decode_url_path(rest))
}

/// `/files` 走快路径，其余路径交给 salvo。
///
/// 回退那一侧存成闭包：`Service::hyper_handler` 返回的 `HyperHandler` 在 salvo 里不可命名
/// （`service` 模块是私有的）。闭包每连接建一次，之后每请求只是一次间接调用。
struct FastService {
    files: Arc<ServeFiles>,
    access_log: Arc<AccessLog>,
    fallback: Box<dyn Fn(HyperRequest<Incoming>) -> BoxedFuture + Send + Sync>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl HyperService<HyperRequest<Incoming>> for FastService {
    type Response = HyperResponse<ResBody>;
    type Error = salvo::hyper::Error;
    type Future = BoxedFuture;

    fn call(&self, req: HyperRequest<Incoming>) -> Self::Future {
        // 先只做判定，判完才把请求交出去：快路径要独占它，回退路径要原样还给 salvo
        if files_sub_path(req.uri().path()).is_none() {
            return (self.fallback)(req);
        }
        let files = Arc::clone(&self.files);
        let access_log = Arc::clone(&self.access_log);
        let (local_addr, remote_addr) = (self.local_addr.clone(), self.remote_addr.clone());
        Box::pin(async move {
            let mut request = Request::from_hyper(req, Scheme::HTTP);
            *request.local_addr_mut() = local_addr;
            *request.remote_addr_mut() = remote_addr;
            let mut res = Response::new();
            // 一次把容量留够：`HeaderMap` 逐个 insert 会反复扩容，实测每请求 4 次分配
            res.headers_mut().reserve(8);
            let mut depot = Depot::new();
            // 借自 `request`，不再单独分配：`decode_url_path` 在没有 `%` 时就是借用
            let sub = files_sub_path(request.uri().path()).unwrap_or_default();
            if request.method() == Method::GET || request.method() == Method::HEAD {
                files.serve(&sub, &request, &mut res).await;
            } else {
                res.status_code(StatusCode::NOT_FOUND);
            }
            // 与 salvo 的 `Service` 完全一致地补错误页：状态码是 4xx/5xx 且没写出响应体时
            // 跑一遍 catcher（HEAD 不补体，RFC 9110 §9.3.2）
            if request.method() != Method::HEAD
                && (res.body.is_none() || res.body.is_error())
                && res
                    .status_code
                    .is_some_and(|code| code.is_client_error() || code.is_server_error())
            {
                Catcher::default()
                    .catch(&mut request, &mut depot, &mut res, ConnCtrl::new())
                    .await;
            }
            // 必须放在 catcher 之后：salvo 的 hoop 也是在整条链跑完后才记日志，错误页的
            // Content-Length 那时才写上去
            log_access(&access_log, &request, &res);
            Ok(res.into_hyper())
        })
    }
}

/// 跑 accept 循环，把每条连接交给 [`FastService`]。
///
/// 取代原来的 `Server::new(acceptor).serve(router)`。连接本身仍按 sendfile 的要求包装
/// （`TCP_NODELAY`、槽位 key），否则零拷贝体没有槽位可用。
pub async fn serve(
    listener: TcpListener,
    root: PathBuf,
    access_log: Arc<AccessLog>,
    router: Router,
) -> io::Result<()> {
    let builder = Arc::new(HttpBuilder::new());
    let service = Service::new(router);
    let files = Arc::new(ServeFiles::new(root));
    loop {
        let (conn, remote_addr) = listener.accept().await?;
        let local_addr: SocketAddr = conn.local_addr()?.into();
        let remote_addr: SocketAddr = remote_addr.into();
        // HTTP/1.1 把一个响应写成「响应头」+「body」两次写。body 小于 MSS 时 Nagle 会压住
        // 第二次写，直到对端的 delayed ACK 超时（Linux 约 40ms），小响应因此每个都平白多出
        // 40ms。Go 的 net 包默认就打开 TCP_NODELAY，这里对齐。
        conn.set_nodelay(true)?;
        let key = conn_key(&local_addr, &remote_addr);
        let stream = SendfileStream::new(conn, Arc::new(SendfileSlot::new()), key);
        let io = StraightStream::new(stream, None, ConnCtrl::new(), None);
        let handler = service.hyper_handler(
            local_addr.clone(),
            remote_addr.clone(),
            Scheme::HTTP,
            None,
            ConnCtrl::new(),
            None,
        );
        let fast = FastService {
            files: Arc::clone(&files),
            access_log: Arc::clone(&access_log),
            fallback: Box::new(move |req| handler.call(req)),
            local_addr,
            remote_addr,
        };
        let builder = Arc::clone(&builder);
        tokio::spawn(async move {
            if let Err(error) = builder
                .serve_connection(io, fast, None, ConnCtrl::new(), None)
                .await
            {
                tracing::debug!("连接出错: {error}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::borrow::Cow;

    use super::files_sub_path;

    /// 这些取值是拿旧二进制实测出来的：每个用例的注释是它当时的响应。
    #[test]
    fn sub_path_matches_the_router() {
        let cases: &[(&str, Option<&str>)] = &[
            ("/files/f.bin", Some("f.bin")),
            // 开头的空段被跳过
            ("/files//f.bin", Some("f.bin")),
            ("/files/./f.bin", Some("./f.bin")),
            ("/files/sub//g.txt", Some("sub//g.txt")),
            ("/files/sub/../f.bin", Some("sub/../f.bin")),
            // 百分号解码，但不把 `+` 当空格
            ("/files/a%20b.txt", Some("a b.txt")),
            ("/files/a+b.txt", Some("a+b.txt")),
            ("/files/%2e%2e/f.bin", Some("../f.bin")),
            // 末尾斜杠：salvo 那边匹配不上，这里用空串表达同一个 404
            ("/files/", Some("")),
            ("/files/f.bin/", Some("")),
            ("/files//", Some("")),
            // 不归快路径管
            ("/files", None),
            ("/", None),
            ("/api/list", None),
            ("/static/x.css", None),
        ];
        for (path, expected) in cases {
            assert_eq!(
                files_sub_path(path).as_deref(),
                *expected,
                "路径 {path} 的子路径取值不对"
            );
        }
    }

    #[test]
    fn sub_path_borrows_when_nothing_is_encoded() {
        assert!(matches!(
            files_sub_path("/files/f.bin"),
            Some(Cow::Borrowed(_))
        ));
    }
}
