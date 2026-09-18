use std::path::PathBuf;

use filerserve_assets::static_routes;
use filerserve_list::list_routes;
use salvo::prelude::*;

#[must_use]
pub fn build_router(root: PathBuf, port: u16) -> Router {
    Router::new()
        .push(static_routes(root.clone()))
        .push(list_routes(root, port))
}
