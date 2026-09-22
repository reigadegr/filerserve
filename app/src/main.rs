#[cfg(not(any(target_os = "linux", target_os = "android")))]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    cell::RefCell,
    fmt::{self, Write as _},
    io::IsTerminal,
    path::PathBuf,
};

use chrono::Local;
use lanfile::build_router;
use lanfile_sendfile::SendfileListener;
use salvo::prelude::{Listener, Server, TcpListener};
use tracing_subscriber::{
    EnvFilter,
    fmt::{format::Writer, time::FormatTime},
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

thread_local! {
    /// The last second formatted and the text for it.
    ///
    /// Every log line carries a timestamp and the access log writes one line per
    /// request, so `chrono`'s `strftime` ends up costing more than the rest of the
    /// line put together; what it returns only changes once a second. Reading the
    /// second off the coarse clock also avoids the timezone conversion that
    /// `Local::now` does on every call, without paying for a real clock read.
    static STAMP: RefCell<(i64, String)> = const { RefCell::new((i64::MIN, String::new())) };
}

/// The current second, used to tell whether the cached timestamp is stale.
///
/// `SystemTime::now()` is a real `clock_gettime` system call on a machine whose
/// clocksource is `hpet` (measured at 1223 ns here), while `CLOCK_REALTIME_COARSE`
/// reads the value the kernel already maintains for the current tick (3.3 ns).
/// The log timestamp only carries whole seconds, so the tick granularity of the
/// coarse clock is more than enough and its seconds match `SystemTime::now`'s.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_second() -> i64 {
    rustix::time::clock_gettime(rustix::time::ClockId::RealtimeCoarse).tv_sec
}

/// Platforms without `CLOCK_REALTIME_COARSE` fall back to a real clock read.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn current_second() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(i64::MIN, |since_epoch| {
            i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX)
        })
}

struct LoggerFormatter;

impl FormatTime for LoggerFormatter {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let second = current_second();
        STAMP.with(|stamp| {
            let mut stamp = stamp.borrow_mut();
            if stamp.0 != second {
                stamp.0 = second;
                stamp.1.clear();
                write!(stamp.1, "{}", Local::now().format("%Y-%m-%d %H:%M:%S"))?;
            }
            w.write_str(&stamp.1)
        })
    }
}

#[tokio::main]
async fn main() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let is_terminal = std::io::stdout().is_terminal();
    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stdout());

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(LoggerFormatter)
        .with_ansi(is_terminal)
        .with_writer(non_blocking)
        .init();

    // guard keeps the non-blocking writer's background thread alive;
    // bound it so the buffer is flushed on shutdown
    let _guard = guard;

    let (port, dir) = parse_args(std::env::args().skip(1));
    let root = std::fs::canonicalize(&dir).unwrap_or_else(|error| {
        tracing::error!("无法访问目录 {:?}: {error}", dir);
        std::process::exit(1);
    });

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("serving {} on http://{addr}", root.display());
    let router = build_router(root, port);

    let acceptor = SendfileListener::new(TcpListener::new(addr)).bind().await;
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
