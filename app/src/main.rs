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

    let (port, dir) = parse_args(std::env::args().skip(1));
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

fn parse_args<I>(args: I) -> (u16, PathBuf)
where
    I: IntoIterator<Item = String>,
{
    let mut port: u16 = 8000;
    let mut dir = PathBuf::from(".");
    for arg in args {
        if let Ok(parsed) = arg.parse() {
            port = parsed;
        } else {
            dir = PathBuf::from(arg);
        }
    }
    (port, dir)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::parse_args;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn defaults_without_args() {
        let (port, dir) = parse_args(args(&[]));
        assert_eq!(port, 8000_u16);
        assert_eq!(dir, PathBuf::from("."));
    }

    #[test]
    fn single_path_arg_is_served_directory() {
        let (port, dir) = parse_args(args(&["/srv/www"]));
        assert_eq!(port, 8000_u16);
        assert_eq!(dir, PathBuf::from("/srv/www"));
    }

    #[test]
    fn single_port_arg_keeps_default_directory() {
        let (port, dir) = parse_args(args(&["8080"]));
        assert_eq!(port, 8080_u16);
        assert_eq!(dir, PathBuf::from("."));
    }

    #[test]
    fn port_arg_sets_port_with_explicit_directory() {
        let (port, dir) = parse_args(args(&["8080", "/srv/www"]));
        assert_eq!(port, 8080_u16);
        assert_eq!(dir, PathBuf::from("/srv/www"));
    }

    #[test]
    fn arg_order_does_not_matter() {
        let (port, dir) = parse_args(args(&["/srv/www", "8080"]));
        assert_eq!(port, 8080_u16);
        assert_eq!(dir, PathBuf::from("/srv/www"));
    }

    #[test]
    fn last_path_arg_wins() {
        let (port, dir) = parse_args(args(&["/srv/www", "/data"]));
        assert_eq!(port, 8000_u16);
        assert_eq!(dir, PathBuf::from("/data"));
    }
}
