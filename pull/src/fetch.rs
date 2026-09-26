//! 拉取操作：单文件下载 [`fetch_file`] 与目录列举 [`list_entries`]，都建在
//! [`crate::http`] 的 keep-alive 传输之上。正文按响应声明的 `Content-Length` 精确读满即止，
//! 读满的连接归还池子复用；读不满即截断，连接丢弃。

use crate::error::Error;
use crate::http::{Pool, READ_TIMEOUT, copy_body, http_get};
use serde::Deserialize;
use std::path::Path;
use tokio::io::AsyncReadExt;

/// 响应没带 `Content-Length` 时的报错：正文边界无从得知，当场说清，不猜长度。
///
/// 这里不能退化成读到 EOF——请求不带 `Connection: close`，对端不会关连接，`read_to_end`
/// 只会空等到 `READ_TIMEOUT` 再报超时，还不如当场把话说清。`/api/list`（salvo 的 `Json`）
/// 与 `/pull`（`NamedFile`）都带长度，所以正常走不到这一支。
const NO_CONTENT_LENGTH: &str = "响应没有 Content-Length，无法确定正文边界";

/// `/api/list` 返回的一条条目。
///
/// `type` 缺字段按文件处理（与原先 `unwrap_or("file")` 一致）；`size` 仅文件有，目录为
/// `None`，用于"本地已存在且尺寸一致就跳过"。
#[derive(Deserialize)]
pub struct RemoteEntry {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub size: Option<u64>,
}

impl RemoteEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// `/api/list` 的响应体：只取 `entries`，其余字段（`path`/`lan_ip`/`port`）客户端用不到。
#[derive(Deserialize)]
struct ListResponse {
    entries: Vec<RemoteEntry>,
}

/// 取一层目录的条目：`GET /api/list[/<remote>]`。正文按 `Content-Length` 增量读满，连接干净
/// 归还池子复用；读不满即截断，连接丢弃。
pub async fn list_entries(
    pool: &mut Pool,
    host: &str,
    remote: &str,
) -> Result<Vec<RemoteEntry>, Error> {
    let path = if remote.is_empty() {
        "/api/list".to_string()
    } else {
        format!("/api/list/{}", encode_path(remote))
    };
    let (reader, declared) = http_get(pool, host, &path).await?;
    let len = declared.ok_or(Error::Malformed(NO_CONTENT_LENGTH))?;
    // 列表正文就几十 KB 出头，这里卡的是整段读完的总时长（不是空闲）。用 `take` 把读取截在
    // 声明的长度上，而不是先按这个长度开一块：对端报的数在读懂之前都不算数。
    let mut limited = reader.take(len);
    let mut body = Vec::new();
    tokio::time::timeout(READ_TIMEOUT, limited.read_to_end(&mut body))
        .await
        .map_err(|_| Error::Timeout {
            phase: "读取目录列表",
        })??;
    if body.len() as u64 != len {
        return Err(Error::Truncated {
            remote: remote.to_string(),
            want: len,
            got: body.len() as u64,
        });
    }
    pool.release(limited.into_inner());
    Ok(serde_json::from_slice::<ListResponse>(&body)?.entries)
}

/// 拉一个文件到 `local`：正文按响应声明的 `Content-Length` 精确读满即停。
///
/// 读满后连接干净，归还池子给下一个文件复用；服务端提前 EOF（读到的字节数不足声明的长度）
/// 或半路超时/IO 出错，都把没写完的文件删掉再报错——宁可什么都没有，也不留一个看着完整
/// 其实残缺的文件。
pub async fn fetch_file(
    pool: &mut Pool,
    host: &str,
    remote: &str,
    local: &Path,
) -> Result<u64, Error> {
    let path = format!("/pull/{}", encode_path(remote));
    let (mut reader, declared) = http_get(pool, host, &path).await?;
    let want = declared.ok_or(Error::Malformed(NO_CONTENT_LENGTH))?;
    let mut file = tokio::fs::File::create(local).await?;
    let copied = match copy_body(&mut reader, &mut file, want).await {
        Ok(copied) => copied,
        Err(error) => {
            discard(local).await;
            return Err(error);
        }
    };
    // 只有读满声明的长度、连接干净才归还：读多一个字节会把下一条响应的开头吃进缓冲，
    // 读不满即传输不完整，两种情况都不能复用。
    if copied != want {
        discard(local).await;
        return Err(Error::Truncated {
            remote: remote.to_string(),
            want,
            got: copied,
        });
    }
    pool.release(reader);
    Ok(copied)
}

/// 删掉没写完整的本地文件。删不掉也不覆盖真正的错误，只在 stderr 上留一句。
async fn discard(local: &Path) {
    if let Err(error) = tokio::fs::remove_file(local).await {
        eprintln!("  清理 {} 失败：{error}", local.display());
    }
}

/// 把远端路径做百分号编码：保留 `A-Za-z0-9-_.~/` 与分隔符 `/`，其余按 UTF-8 字节转义。
/// 用于 `/files/<sub>/<name>` 与 `/api/list/<sub>` 这两类路径。
///
/// 转义直接查表手写两个 hex 字符，不走 `fmt::Write`：文件名带中文或空格时每个字节
/// 都会走一次格式化分发，这条路径在每个文件下载时都会经过。
fn encode_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[(byte >> 4) as usize]));
                out.push(char::from(HEX[(byte & 0x0F) as usize]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn encode_path_keeps_unreserved_and_slash() {
        assert_eq!(encode_path("sub/a_b-1.txt"), "sub/a_b-1.txt");
    }

    #[test]
    fn encode_path_percent_encodes_space_and_unicode() {
        assert_eq!(encode_path("a b.txt"), "a%20b.txt");
        assert_eq!(encode_path("中"), "%E4%B8%AD");
    }

    /// 服务端接上却一句话不说时，读取超时要把客户端放出来，并且不留半截文件。
    #[tokio::test]
    async fn fetch_file_times_out_on_a_silent_server() {
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 收下请求就不吭声：既不回响应，也不断开
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 256];
            let _ = conn.read(&mut request).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let target =
            std::env::temp_dir().join(format!("lanfile-pull-timeout-{}", std::process::id()));
        let mut pool = Pool::default();
        let error = fetch_file(&mut pool, &addr.to_string(), "x.bin", &target)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }), "{error}");
        assert!(!target.exists(), "超时后不该留半截文件");
        server.abort();
    }

    /// 两个文件走同一条 keep-alive 连接：服务端只 accept 一次，第二条请求复用第一条归还的连接。
    /// 正文按 `Content-Length` 精确读满即停，读多的一个字节会把下一条响应的开头吃掉——这条
    /// 测试盯住「读满即止」与「归还复用」两件事同时成立。
    #[tokio::test]
    async fn fetch_file_reuses_one_connection_across_files() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpStream;

        // 把请求头读到空行即止（GET 无正文）；BufReader 把整段请求吃进缓冲，读完恰好干净
        async fn read_request(reader: &mut BufReader<TcpStream>) {
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                if line.trim().is_empty() {
                    break;
                }
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(conn);
            read_request(&mut reader).await;
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\naaa")
                .await
                .unwrap();
            read_request(&mut reader).await;
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nbbb")
                .await
                .unwrap();
        });

        let dir = std::env::temp_dir().join(format!("lanfile-pull-reuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        let mut pool = Pool::default();
        fetch_file(&mut pool, &addr.to_string(), "a.bin", &f1)
            .await
            .unwrap();
        fetch_file(&mut pool, &addr.to_string(), "b.bin", &f2)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&f1).unwrap(), b"aaa");
        assert_eq!(std::fs::read(&f2).unwrap(), b"bbb");
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
