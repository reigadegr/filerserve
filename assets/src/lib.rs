use std::path::PathBuf;

use rust_embed::RustEmbed;
use salvo::{
    prelude::*,
    routing::{Filter, filters},
    serve_static::{StaticDir, static_embed},
};

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct Asset;

#[must_use]
pub fn static_routes(root: PathBuf) -> Router {
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
            Router::with_path("/files/{**path}")
                .filter(filters::get().or(filters::head()))
                .goal(StaticDir::new(root)),
        )
}
