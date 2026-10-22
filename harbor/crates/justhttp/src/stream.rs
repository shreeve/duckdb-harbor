//! The socket layer: TCP + unix-socket listeners, and the half-close
//! stream that lets one connection be read and written from two threads.

pub use listen::{ListenAddr, Listener};
pub(crate) use listen::Connection;
pub(crate) use refined::{RefinedTcpStream, Socket};

mod listen {
    //! Abstractions of Tcp and Unix socket types

    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    #[cfg(unix)]
    use std::os::unix::net as unix_net;

    /// Unified listener. Either a [`TcpListener`] or [`std::os::unix::net::UnixListener`]
    pub enum Listener {
        Tcp(TcpListener),
        #[cfg(unix)]
        Unix(unix_net::UnixListener),
    }
    impl Listener {
        pub(crate) fn local_addr(&self) -> std::io::Result<ListenAddr> {
            match self {
                Self::Tcp(l) => l.local_addr().map(ListenAddr::from),
                #[cfg(unix)]
                Self::Unix(l) => l.local_addr().map(ListenAddr::from),
            }
        }

        pub(crate) fn accept(&self) -> std::io::Result<(Connection, Option<SocketAddr>)> {
            match self {
                Self::Tcp(l) => l
                    .accept()
                    .map(|(conn, addr)| (Connection::from(conn), Some(addr))),
                #[cfg(unix)]
                Self::Unix(l) => l.accept().map(|(conn, _)| (Connection::from(conn), None)),
            }
        }
    }
    impl From<TcpListener> for Listener {
        fn from(s: TcpListener) -> Self {
            Self::Tcp(s)
        }
    }
    #[cfg(unix)]
    impl From<unix_net::UnixListener> for Listener {
        fn from(s: unix_net::UnixListener) -> Self {
            Self::Unix(s)
        }
    }

    /// Unified connection. Either a [`TcpStream`] or [`std::os::unix::net::UnixStream`].
    #[derive(Debug)]
    pub(crate) enum Connection {
        Tcp(TcpStream),
        #[cfg(unix)]
        Unix(unix_net::UnixStream),
    }
    // On a shared reference, as std does for both stream types: the read and
    // write halves of one connection share one socket.
    impl std::io::Read for &Connection {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match *self {
                Connection::Tcp(s) => (&*s).read(buf),
                #[cfg(unix)]
                Connection::Unix(s) => (&*s).read(buf),
            }
        }
    }
    impl std::io::Write for &Connection {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match *self {
                Connection::Tcp(s) => (&*s).write(buf),
                #[cfg(unix)]
                Connection::Unix(s) => (&*s).write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match *self {
                Connection::Tcp(s) => (&*s).flush(),
                #[cfg(unix)]
                Connection::Unix(s) => (&*s).flush(),
            }
        }
    }
    impl Connection {
        /// Gets the peer's address. Some for TCP, None for Unix sockets.
        pub(crate) fn peer_addr(&self) -> std::io::Result<Option<SocketAddr>> {
            match self {
                Self::Tcp(s) => s.peer_addr().map(Some),
                #[cfg(unix)]
                Self::Unix(_) => Ok(None),
            }
        }

        pub(crate) fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.shutdown(how),
                #[cfg(unix)]
                Self::Unix(s) => s.shutdown(how),
            }
        }

        /// Bound how long a single write to this socket may block. Without a
        /// timeout, a client that stops reading its response leaves the server
        /// thread parked in a `write` forever. A client that keeps draining
        /// resets the timer on every write; only a fully stalled peer trips it,
        /// after which the write errors and the connection is dropped. (One of
        /// the hardening behaviors this crate carries; see README.md.)
        pub(crate) fn set_write_timeout(
            &self,
            dur: Option<std::time::Duration>,
        ) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.set_write_timeout(dur),
                #[cfg(unix)]
                Self::Unix(s) => s.set_write_timeout(dur),
            }
        }

        /// Bound how long a single read from this socket may block. The
        /// header reader treats a timeout before the request has started as an
        /// idle keep-alive wait and keeps waiting; once a request is under way
        /// a timeout is fatal, which is what bounds a slowloris. It also bounds
        /// each read of a body, and of the unread-body drain, against a peer
        /// that has stopped sending.
        pub(crate) fn set_read_timeout(
            &self,
            dur: Option<std::time::Duration>,
        ) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.set_read_timeout(dur),
                #[cfg(unix)]
                Self::Unix(s) => s.set_read_timeout(dur),
            }
        }

        /// Disable Nagle's algorithm on TCP connections. The server writes each
        /// response as one buffered flush, so there is no small-packet spray to
        /// coalesce — but a response that does take more than one write (headers
        /// plus a large chunked body, or the chunked terminator after a full
        /// chunk) must not sit in the kernel waiting on the peer's delayed-ACK
        /// timer. Unix sockets have no Nagle; the arm is a no-op so both
        /// transports behave identically.
        pub(crate) fn set_nodelay(&self, nodelay: bool) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.set_nodelay(nodelay),
                #[cfg(unix)]
                Self::Unix(_) => Ok(()),
            }
        }
    }
    impl From<TcpStream> for Connection {
        fn from(s: TcpStream) -> Self {
            Self::Tcp(s)
        }
    }
    #[cfg(unix)]
    impl From<unix_net::UnixStream> for Connection {
        fn from(s: unix_net::UnixStream) -> Self {
            Self::Unix(s)
        }
    }

    /// Unified listen socket address. Either a [`SocketAddr`] or [`std::os::unix::net::SocketAddr`].
    #[derive(Debug, Clone)]
    pub enum ListenAddr {
        Ip(SocketAddr),
        #[cfg(unix)]
        Unix(unix_net::SocketAddr),
    }
    impl From<SocketAddr> for ListenAddr {
        fn from(s: SocketAddr) -> Self {
            Self::Ip(s)
        }
    }
    #[cfg(unix)]
    impl From<unix_net::SocketAddr> for ListenAddr {
        fn from(s: unix_net::SocketAddr) -> Self {
            Self::Unix(s)
        }
    }
    impl std::fmt::Display for ListenAddr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Ip(s) => s.fmt(f),
                #[cfg(unix)]
                Self::Unix(s) => std::fmt::Debug::fmt(s, f),
            }
        }
    }
}

mod refined {
    use std::io::Result as IoResult;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{Shutdown, SocketAddr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::listen::Connection;

    /// The most a look for the peer's departure reads ahead of the
    /// connection's readers (`Socket::look_for_departure`).
    const AHEAD_MAX: usize = 64 * 1024;

    /// One accepted socket, shared by the halves that read and write it and
    /// by the requests it carries: one descriptor per connection.
    pub(crate) struct Socket {
        conn: Connection,
        /// The server cannot know where the next request on this
        /// connection begins.
        ended: AtomicBool,
        /// The connection's reader saw the client close or reset it.
        gone: AtomicBool,
        /// What a look for the peer's departure read, which the
        /// connection's readers read before the socket.
        ahead: Mutex<Vec<u8>>,
        /// `ahead` holds something.
        has_ahead: AtomicBool,
    }

    impl Socket {
        /// Ends the connection after a request whose body was abandoned
        /// part-way, called once that request is answered, as it drops; or
        /// after a response that failed to write, cut where the client
        /// cannot tell it ends.
        ///
        /// The stream sits at an offset neither side agrees on, and the bytes
        /// still to come — already in the connection's buffer, or still
        /// arriving — would otherwise be parsed as the next request, a
        /// request the client never sent. `ClientConnection` parses nothing
        /// after this. The write side is shut down here, so the client sees
        /// the end of the connection right after its response and closes
        /// its own side, which is what lets the connection thread stop
        /// reading.
        pub(crate) fn end(&self) {
            self.ended.store(true, Ordering::Release);
            self.conn.shutdown(Shutdown::Write).ok();
        }

        pub(crate) fn ended(&self) -> bool {
            self.ended.load(Ordering::Acquire)
        }

        /// Records that a read on this socket saw EOF or a reset.
        pub(crate) fn depart(&self) {
            self.gone.store(true, Ordering::Release);
        }

        pub(crate) fn gone(&self) -> bool {
            self.gone.load(Ordering::Acquire)
        }

        /// Looks whether the peer has closed or reset the connection, while
        /// nothing reads it: what it sent is read ahead, up to
        /// [`AHEAD_MAX`], and kept for the connection's readers, and an end
        /// or a reset behind it is the peer's departure. Each read waits a
        /// millisecond at most; then the socket's read timeout is
        /// `READ_TIMEOUT` again.
        pub(crate) fn look_for_departure(&self) {
            if self.gone() {
                return;
            }
            let mut ahead = self.ahead.lock().unwrap();
            if ahead.len() >= AHEAD_MAX {
                return;
            }
            // macOS refuses the option on a socket shut down both ways, as
            // a unix socket is once its peer has gone, and its reads do not
            // wait.
            let _ = self.conn.set_read_timeout(Some(Duration::from_millis(1)));
            let mut scratch = [0u8; 4096];
            while ahead.len() < AHEAD_MAX {
                match (&self.conn).read(&mut scratch) {
                    Ok(0) => {
                        self.depart();
                        break;
                    }
                    Ok(n) => ahead.extend_from_slice(&scratch[..n]),
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                    Err(_) => {
                        self.depart();
                        break;
                    }
                }
            }
            self.has_ahead.store(!ahead.is_empty(), Ordering::Release);
            let _ = self.conn.set_read_timeout(Some(crate::READ_TIMEOUT));
        }

        /// Read what was read ahead, if anything was.
        fn read_ahead(&self, buf: &mut [u8]) -> Option<usize> {
            if !self.has_ahead.load(Ordering::Acquire) {
                return None;
            }
            let mut ahead = self.ahead.lock().unwrap();
            let n = buf.len().min(ahead.len());
            buf[..n].copy_from_slice(&ahead[..n]);
            ahead.drain(..n);
            self.has_ahead.store(!ahead.is_empty(), Ordering::Release);
            (n > 0).then_some(n)
        }
    }

    /// One half of a connection: dropping it shuts its direction down.
    pub struct RefinedTcpStream {
        socket: Arc<Socket>,
        close_read: bool,
        close_write: bool,
    }

    impl RefinedTcpStream {
        pub(crate) fn new<S>(stream: S) -> (RefinedTcpStream, RefinedTcpStream)
        where
            S: Into<Connection>,
        {
            let socket = Arc::new(Socket {
                conn: stream.into(),
                ended: AtomicBool::new(false),
                gone: AtomicBool::new(false),
                ahead: Mutex::new(Vec::new()),
                has_ahead: AtomicBool::new(false),
            });

            let read = RefinedTcpStream {
                socket: socket.clone(),
                close_read: true,
                close_write: false,
            };

            let write = RefinedTcpStream {
                socket,
                close_read: false,
                close_write: true,
            };

            (read, write)
        }

        pub(crate) fn peer_addr(&mut self) -> IoResult<Option<SocketAddr>> {
            self.socket.conn.peer_addr()
        }

        /// The socket under this half, for code that does not own the stream.
        pub(crate) fn socket(&self) -> Arc<Socket> {
            self.socket.clone()
        }
    }

    impl Drop for RefinedTcpStream {
        fn drop(&mut self) {
            if self.close_read {
                self.socket.conn.shutdown(Shutdown::Read).ok();
            }

            if self.close_write {
                self.socket.conn.shutdown(Shutdown::Write).ok();
            }
        }
    }

    impl Read for RefinedTcpStream {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            match self.socket.read_ahead(buf) {
                Some(n) => Ok(n),
                None => (&self.socket.conn).read(buf),
            }
        }
    }

    impl Write for RefinedTcpStream {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            (&self.socket.conn).write(buf)
        }

        fn flush(&mut self) -> IoResult<()> {
            (&self.socket.conn).flush()
        }
    }
}
