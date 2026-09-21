use std::cmp;
use std::fmt::{self, Debug, Formatter};
use std::io::{self, Error as IoError, ErrorKind, Read, Result as IoResult, Seek};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_util::stream::Stream;

/// Internal state machine for [`ChunkedFile`].
pub(crate) enum ChunkedState<T> {
    /// Holding the file, ready to start the next read operation.
    File(Option<T>),
    /// Waiting for a blocking read operation to complete.
    Future(tokio::task::JoinHandle<IoResult<(T, Bytes)>>),
}

/// A streaming file reader that yields data in configurable chunks.
///
/// `ChunkedFile` implements [`Stream`], yielding
/// [`Bytes`] chunks as the file is read. This allows large files to be served
/// without loading the entire content into memory.
///
/// # How It Works
///
/// 1. Reading is performed in a blocking thread pool via `spawn_blocking`
/// 2. Each read operation yields a chunk of up to `buffer_size` bytes
/// 3. The stream completes when `total_size` bytes have been read
///
/// # Type Parameter
///
/// - `T`: The file type, which must implement [`Read`], [`Seek`], [`Unpin`], and [`Send`]
///
/// # Example
///
/// ```ignore
/// use salvo_core::fs::ChunkedFile;
/// use futures_util::StreamExt;
/// use std::fs::File;
///
/// let file = File::open("large_file.bin").unwrap();
/// let metadata = file.metadata().unwrap();
///
/// let mut stream = ChunkedFile::new(file, metadata.len(), 65536);
///
/// while let Some(chunk) = stream.next().await {
///     match chunk {
///         Ok(bytes) => println!("Read {} bytes", bytes.len()),
///         Err(e) => eprintln!("Error: {}", e),
///     }
/// }
/// ```
pub struct ChunkedFile<T> {
    pub(crate) total_size: u64,
    pub(crate) read_size: u64,
    pub(crate) buffer_size: u64,
    pub(crate) offset: u64,
    pub(crate) state: ChunkedState<T>,
}
impl<T> Debug for ChunkedFile<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkedFile")
            .field("total_size", &self.total_size)
            .field("read_size", &self.read_size)
            .field("buffer_size", &self.buffer_size)
            .field("offset", &self.offset)
            .finish()
    }
}

impl<T> Stream for ChunkedFile<T>
where
    T: Read + Seek + Unpin + Send + 'static,
{
    type Item = IoResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if self.total_size == self.read_size {
            return Poll::Ready(None);
        }

        match self.state {
            ChunkedState::File(ref mut file) => {
                let mut file = file.take().expect("`ChunkedFile` polled after completion");
                let max_bytes = cmp::min(
                    self.total_size.saturating_sub(self.read_size),
                    self.buffer_size,
                ) as usize;
                let offset = self.offset;
                let fut = tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; max_bytes];
                    file.seek(io::SeekFrom::Start(offset))?;
                    let bytes = file.read(&mut buf)?;
                    buf.truncate(bytes);
                    if bytes == 0 {
                        return Err(ErrorKind::UnexpectedEof.into());
                    }
                    Ok((file, Bytes::from(buf)))
                });

                self.state = ChunkedState::Future(fut);
                self.poll_next(cx)
            }
            ChunkedState::Future(ref mut fut) => {
                let (file, bytes) = ready!(Pin::new(fut).poll(cx))
                    .map_err(|_| IoError::other("`ChunkedFile` block error"))??;
                self.state = ChunkedState::File(Some(file));

                self.offset += bytes.len() as u64;
                self.read_size += bytes.len() as u64;

                Poll::Ready(Some(Ok(bytes)))
            }
        }
    }
}
