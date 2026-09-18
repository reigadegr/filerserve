use chrono::Local;
use salvo::{
    prelude::*,
    routing::{Filter, filters},
    serve_static::StaticDir,
};
use std::{fmt, io::IsTerminal, path::PathBuf};
use tracing_subscriber::{
    EnvFilter,
    fmt::{format::Writer, time::FormatTime},
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct LoggerFormatter;

impl FormatTime for LoggerFormatter {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

#[tokio::main]
async fn main() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let is_terminal = std::io::stdout().is_terminal();

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(LoggerFormatter)
        .with_ansi(is_terminal)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = args
        .first()
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(8000);
    let dir = args
        .get(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let root = std::fs::canonicalize(&dir).unwrap_or_else(|error| {
        tracing::error!("无法访问目录 {:?}: {error}", dir);
        std::process::exit(1);
    });

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("serving {} on http://{addr}", root.display());
    let router = Router::with_path("{**rest}")
        .filter(filters::get().or(filters::head()))
        .goal(StaticDir::new(root).auto_list(true));

    let acceptor = TcpListener::new(addr).bind().await;
    Server::new(acceptor).serve(router).await;
}
