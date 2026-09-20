use std::path::PathBuf;

use lanfile_assets::static_routes;
use lanfile_list::list_routes;
use salvo::{http::header::CONTENT_LENGTH, prelude::*};

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

    tracing::info!(?ip, %method, %path, ?version, status, %size, "access");
}

#[must_use]
pub fn build_router(root: PathBuf, port: u16) -> Router {
    Router::new()
        .hoop(access_log)
        .push(static_routes(root.clone()))
        .push(list_routes(root, port))
}
