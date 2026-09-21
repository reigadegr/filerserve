//! A Salvo listener that serves armed responses with `sendfile(2)`.

use std::{io, sync::Arc};

use salvo::{
    conn::{
        Accepted, Acceptor, ConnCtrl, Holding, Listener, SocketAddr, StraightStream, TcpListener,
        tcp::{TcpAcceptor, TcpCoupler},
    },
    fuse::{ArcFusePolicy, FuseAction, FuseInfo, TransProto},
    http::uri::Scheme,
};
use tokio::net::ToSocketAddrs;

use crate::{body::SendfileSlot, registry, stream::SendfileStream};

/// Wraps a [`TcpListener`] so accepted connections can serve `sendfile(2)`.
///
/// Everything else about the connection — Hyper, the router, the connection fuse
/// and Salvo's own coupler — is left exactly as it is; only the transport stream
/// is wrapped.
pub struct SendfileListener<T> {
    inner: TcpListener<T>,
}

impl<T> SendfileListener<T> {
    /// Wraps an already configured [`TcpListener`].
    #[must_use]
    pub const fn new(inner: TcpListener<T>) -> Self {
        Self { inner }
    }
}

impl<T> Listener for SendfileListener<T>
where
    T: ToSocketAddrs + Send + 'static,
{
    type Acceptor = SendfileAcceptor;

    async fn try_bind(self) -> salvo::Result<Self::Acceptor> {
        Ok(SendfileAcceptor {
            inner: self.inner.try_bind().await?,
            pending: None,
        })
    }
}

/// Accepts connections wrapped in a [`SendfileStream`].
///
/// Admission mirrors Salvo's own `TcpAcceptor`, including parking an accepted
/// socket across the asynchronous fuse decision so a cancelled `accept` does not
/// drop a client that already connected.
pub struct SendfileAcceptor {
    inner: TcpAcceptor,
    pending: Option<(tokio::net::TcpStream, std::net::SocketAddr)>,
}

impl SendfileAcceptor {
    /// The local address this acceptor is bound to.
    ///
    /// Useful after binding to port `0` to learn the port that was chosen.
    ///
    /// # Errors
    ///
    /// Returns the underlying socket error when the address cannot be read.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }
}

impl Acceptor for SendfileAcceptor {
    type Coupler = TcpCoupler<StraightStream<SendfileStream<tokio::net::TcpStream>>>;
    type Stream = StraightStream<SendfileStream<tokio::net::TcpStream>>;

    fn holdings(&self) -> &[Holding] {
        self.inner.holdings()
    }

    async fn accept(
        &mut self,
        fuse_policy: Option<ArcFusePolicy>,
    ) -> io::Result<Accepted<Self::Coupler, Self::Stream>> {
        loop {
            if self.pending.is_none() {
                let accepted = self.inner.inner().accept().await?;
                self.pending = Some(accepted);
            }
            let remote = match &self.pending {
                Some((_, addr)) => *addr,
                None => continue,
            };
            let remote_addr: SocketAddr = remote.into();
            let local_addr = match self.inner.holdings().first() {
                Some(holding) => holding.local_addr.clone(),
                None => return Err(io::Error::other("acceptor is bound to nothing")),
            };
            let conn_ctrl = ConnCtrl::new();

            let (fuse_config, observer) = match &fuse_policy {
                Some(policy) => {
                    let info = FuseInfo {
                        trans_proto: TransProto::Tcp,
                        remote_addr: remote_addr.clone(),
                        local_addr: local_addr.clone(),
                    };
                    match policy.decide(&info).await {
                        FuseAction::Accept(config) => {
                            (Some(config), policy.observe(&info, &conn_ctrl))
                        }
                        FuseAction::Reject => {
                            self.pending = None;
                            continue;
                        }
                    }
                }
                None => (None, None),
            };

            let Some((conn, _)) = self.pending.take() else {
                continue;
            };
            // HTTP/1.1 把一个响应写成「响应头」+「body」两次写。body 小于 MSS 时
            // Nagle 会压住第二次写，直到对端的 delayed ACK 超时（Linux 约 40ms），
            // 于是静态资源、JSON、小文件这类小响应每个都平白多出 40ms；实测小响应
            // 中位数从 0.2ms 变成 43ms。Go 的 net 包默认就打开 TCP_NODELAY，这里对齐。
            conn.set_nodelay(true)?;
            let key = registry::conn_key(&local_addr, &remote_addr);
            let stream = SendfileStream::new(conn, Arc::new(SendfileSlot::new()), key);
            return Ok(Accepted {
                coupler: TcpCoupler::new(),
                stream: StraightStream::new(stream, fuse_config, conn_ctrl.clone(), observer),
                fuse_config,
                conn_ctrl,
                local_addr,
                remote_addr,
                http_scheme: Scheme::HTTP,
            });
        }
    }
}
