use std::path::PathBuf;

use filerserve_lib::{Asset, ListApi};
use salvo::{
    prelude::*,
    routing::{Filter, filters},
    serve_static::{StaticDir, static_embed},
};

#[must_use]
pub fn build_router(root: PathBuf, port: u16) -> Router {
    Router::new()
        .push(
            Router::new()
                .filter(filters::get())
                .goal(static_embed::<Asset>().fallback("index.html")),
        )
        .push(
            Router::with_path("/static/{**path}")
                .filter(filters::get())
                .goal(static_embed::<Asset>()),
        )
        .push(
            Router::with_path("/api/list")
                .filter(filters::get())
                .goal(ListApi::new(root.clone(), port)),
        )
        .push(
            Router::with_path("/api/list/{**path}")
                .filter(filters::get())
                .goal(ListApi::new(root.clone(), port)),
        )
        .push(
            Router::with_path("/files/{**path}")
                .filter(filters::get().or(filters::head()))
                .goal(StaticDir::new(root)),
        )
}
