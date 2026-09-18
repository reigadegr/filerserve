use salvo::prelude::*;
use salvo::routing::{Filter, filters};
use salvo::serve_static::StaticDir;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
struct ListEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: &'static str,
    size: Option<u64>,
    modified: String,
}

#[derive(Serialize)]
struct ListResponse {
    path: String,
    lan_ip: Option<String>,
    port: u16,
    entries: Vec<ListEntry>,
}

pub struct ListApi {
    root: PathBuf,
    pub port: u16,
}

impl ListApi {
    #[must_use]
    pub const fn new(root: PathBuf, port: u16) -> Self {
        Self { root, port }
    }

    #[must_use]
    pub const fn root(&self) -> &PathBuf {
        &self.root
    }
}

#[handler]
impl ListApi {
    #[allow(clippy::unused_async, clippy::needless_pass_by_ref_mut)]
    async fn handle(&self, req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let path = req.param::<String>("path").unwrap_or_default();
        let full = self.root.join(&path);

        let Ok(canonical_root) = self.root.canonicalize() else {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        };

        let Ok(canonical_target) = full.canonicalize() else {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        };

        if !canonical_target.starts_with(&canonical_root) {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        if !canonical_target.is_dir() {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }

        let Ok(entries) = std::fs::read_dir(&canonical_target) else {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        };

        let mut list_entries: Vec<ListEntry> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let (entry_type, size) = if metadata.is_dir() {
                ("dir", None)
            } else {
                ("file", Some(metadata.len()))
            };
            #[allow(clippy::cast_possible_wrap)]
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| {
                    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
                })
                .unwrap_or_default();

            list_entries.push(ListEntry {
                name,
                entry_type,
                size,
                modified,
            });
        }

        list_entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            ("dir", "file") => std::cmp::Ordering::Less,
            ("file", "dir") => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });

        let display_path = if path.is_empty() {
            "/".to_string()
        } else {
            format!("/{path}")
        };

        let response = ListResponse {
            path: display_path,
            lan_ip: None,
            port: self.port,
            entries: list_entries,
        };

        res.render(Json(response));
    }
}

#[must_use]
pub fn build_router(root: PathBuf, port: u16) -> Router {
    Router::new()
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
            Router::with_path("{**rest}")
                .filter(filters::get().or(filters::head()))
                .goal(StaticDir::new(root).auto_list(true)),
        )
}
