use std::path::PathBuf;

use filerserve_assets::static_routes;
use filerserve_list::list_routes;
use salvo::http::header::CONTENT_LENGTH;
use salvo::prelude::*;

#[handler]
async fn access_log(req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let ip = match req.remote_addr().ip() {
        Some(ip) => ip.to_string(),
        None => "-".to_string(),
    };
    let version = req.version();

    ctrl.call_next(req, depot, res).await;

    let status = res.status_code.map_or(200_u16, |c| c.as_u16());
    let size = res
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    tracing::info!("{ip} - \"{method} {uri} {version:?}\" {status} {size}");
}

#[must_use]
pub fn build_router(root: PathBuf, port: u16) -> Router {
    Router::new()
        .hoop(access_log)
        .push(static_routes(root.clone()))
        .push(list_routes(root, port))
}
