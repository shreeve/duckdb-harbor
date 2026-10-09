//! Just HTTP: a small synchronous HTTP/1.1 server over TCP or unix sockets.
//!
//! A `Server` starts listening as soon as it is built. Worker threads take
//! requests with `recv_timeout`, and `unblock` wakes one of them for a
//! graceful shutdown. Every `Request` is answered with `respond`; one dropped
//! unanswered gets a 500.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! let server = justhttp::Server::http("127.0.0.1:0").unwrap();
//! while let Ok(Some(request)) = server.recv_timeout(Duration::from_millis(200)) {
//!     let _ = request.respond(justhttp::Response::from_string("hello world"));
//! }
//! ```
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

use std::error::Error;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::Duration;

use conn::ClientConnection;
use pool::MessagesQueue;
use stream::Connection;

pub use http::{Header, Method, StatusCode};
pub use request::{Peer, Request};
pub use response::Response;
pub use stream::{ListenAddr, Listener};

mod conn;
mod http;
mod pool;
mod request;
mod response;
mod stream;

/// The main class of this library.
///
/// Destroying this object closes the listening sockets. Requests already
/// returned by `recv_timeout()` stay open, and their responses still reach
/// the client.
pub struct Server {
    // should be false as long as the server exists
    // when set to true, all the subtasks will close within a few hundreds ms
    close: Arc<AtomicBool>,

    // queue for messages received by child threads
    messages: Arc<MessagesQueue<Message>>,

    // result of TcpListener::local_addr()
    // Every bound address, primary first (server_addr() reports the first;
    // Drop wakes and, for unix paths, unlinks each one).
    listening_addrs: Vec<ListenAddr>,

    // live client connections, counted at accept and at connection end.
    // A fact, not a policy: the host reads this to decide lifetime (a
    // refcounted server exits when nobody has been connected for a
    // while), and policy stays out of the HTTP layer.
    connections: Arc<AtomicUsize>,
}

enum Message {
    Error(IoError),
    NewRequest(Request),
}

/// Whether an `accept()` failure is one the listener recovers from on its own.
///
/// Everything here leaves the listening socket perfectly usable: the peer
/// vanished before it could be accepted, a signal interrupted the call, or the
/// process is momentarily out of file descriptors or socket buffers. Only a
/// failure that means the listener is *gone* — a closed or invalid descriptor —
/// should end the accept loop.
///
/// Descriptor and buffer exhaustion have no stable `ErrorKind`, so they are
/// matched by errno.
fn transient_accept_error(e: &IoError) -> bool {
    use std::io::ErrorKind::{
        ConnectionAborted, ConnectionRefused, ConnectionReset, Interrupted, OutOfMemory, TimedOut,
        WouldBlock,
    };
    if matches!(
        e.kind(),
        ConnectionAborted
            | ConnectionRefused
            | ConnectionReset
            | Interrupted
            | OutOfMemory
            | TimedOut
            | WouldBlock
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        const EINTR: i32 = 4;
        const ENOMEM: i32 = 12;
        const ENFILE: i32 = 23;
        const EMFILE: i32 = 24;
        #[cfg(target_os = "linux")]
        const ENOBUFS: i32 = 105;
        #[cfg(not(target_os = "linux"))]
        const ENOBUFS: i32 = 55;
        matches!(
            e.raw_os_error(),
            Some(EINTR) | Some(ENOMEM) | Some(ENFILE) | Some(EMFILE) | Some(ENOBUFS)
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

impl From<IoError> for Message {
    fn from(e: IoError) -> Message {
        Message::Error(e)
    }
}

impl From<Request> for Message {
    fn from(rq: Request) -> Message {
        Message::NewRequest(rq)
    }
}

// compile-time proof that Server can be shared across threads
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<Server>;
};

impl Server {
    /// Live client connections right now — everything accepted whose
    /// request loop has not ended. Idle keep-alive connections count;
    /// that is the point: an attached client, even a quiet one, is a
    /// claim on the server's lifetime.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Relaxed)
    }
}

impl Server {
    /// A server on a TCP address.
    #[inline]
    pub fn http<A>(addr: A) -> Result<Server, Box<dyn Error + Send + Sync + 'static>>
    where
        A: ToSocketAddrs,
    {
        let listener = std::net::TcpListener::bind(addr)?;
        Self::start(stream::Listener::Tcp(listener))
    }

    #[cfg(unix)]
    #[inline]
    /// A server on a UNIX socket at a specific path.
    pub fn http_unix(
        path: &std::path::Path,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        Self::start(stream::Listener::Unix(listener))
    }

    /// Spawns the accept thread over a bound listener.
    fn start(listener: stream::Listener) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        Self::serve(vec![listener])
    }

    /// One server over any number of pre-bound doors: one request queue, one
    /// close trigger, one connection count, one pool of connection threads —
    /// and one accept thread per listener feeding them. The caller owns the binding policy (which
    /// addresses, which sockets); this owns everything after the bind. The
    /// first listener is the primary: `server_addr()` reports it.
    pub fn serve(
        listeners: Vec<stream::Listener>,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        let close_trigger = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let messages = MessagesQueue::with_capacity(8);

        let mut listening_addrs = Vec::with_capacity(listeners.len());
        for listener in &listeners {
            listening_addrs.push(listener.local_addr()?);
        }
        let tasks = Arc::new(pool::TaskPool::new());
        for listener in listeners {
            spawn_accept(
                listener,
                close_trigger.clone(),
                messages.clone(),
                connections.clone(),
                tasks.clone(),
            );
        }

        Ok(Server { messages, close: close_trigger, listening_addrs, connections })
    }
}

/// The accept loop for one listener: accepted connections are dispatched to
/// the task pool every listener shares, and every request they produce lands
/// in the shared queue.
fn spawn_accept(
    server: stream::Listener,
    inside_close_trigger: Arc<AtomicBool>,
    inside_messages: Arc<MessagesQueue<Message>>,
    inside_connections: Arc<AtomicUsize>,
    tasks_pool: Arc<pool::TaskPool>,
) {
    thread::spawn(move || {
            // The ceiling on how long a single
            // response write may block before the connection is dropped. A dead
            // reader is finite, not precise — 10s reads as "this peer is gone,"
            // and the edge proxy owns precise per-request timeouts. Because
            // write_all resets this per syscall on any forward progress, the
            // real reclaim is somewhat longer, which is fine for a backstop.
            const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

            // The ceiling on how long a single read may block. A request that
            // has not started yet (an idle keep-alive connection) waits through
            // these ticks forever; once a request is under way this is what
            // stops a client from holding a serving thread — or an unbounded
            // header buffer — open at one byte per minute. See conn.rs.
            const READ_TIMEOUT: Duration = Duration::from_secs(5);

            // How long to wait after a transient accept failure before trying
            // again, and the ceiling that backoff climbs to.
            const ACCEPT_BACKOFF_MAX: Duration = Duration::from_millis(200);
            // How long a transient failure may persist before it stops being
            // treated as transient. Retrying forever survives an fd storm, but
            // it also means a listener that is broken in a way this
            // classifier does not recognize spins here silently for the life
            // of the process. A full minute without accepting a single
            // connection is not a storm; report it and stop, the way an
            // unrecognized error already does, so the host can say so.
            const ACCEPT_GIVE_UP_AFTER: Duration = Duration::from_secs(60);
            let mut accept_failures: u32 = 0;
            let mut failing_since: Option<std::time::Instant> = None;

            while !inside_close_trigger.load(Relaxed) {
                let new_client = match server.accept() {
                    Ok((sock, _)) => {
                        use crate::stream::RefinedTcpStream;
                        // Bound how long a single response write may block, so
                        // a client that stops reading cannot park a server
                        // thread inside `write` forever. Read side is left
                        // untouched — keep-alive connections must wait
                        // indefinitely between requests. Only a fully stalled
                        // peer trips it; a draining client resets the timer
                        // every write. Best-effort — a socket that rejects the
                        // option just keeps upstream's original behavior.
                        let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
                        let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
                        // One response = one flush, so Nagle has nothing to
                        // coalesce here — but it would hold the last small
                        // segment of a multi-write response (e.g. a chunked
                        // terminator) against the peer's delayed-ACK timer.
                        let _ = sock.set_nodelay(true);
                        let (read_closable, write_closable) = RefinedTcpStream::new(sock);

                        Ok(ClientConnection::new(write_closable, read_closable))
                    }
                    Err(e) => Err(e),
                };

                match new_client {
                    Ok(client) => {
                        accept_failures = 0;
                        failing_since = None;
                        let messages = inside_messages.clone();
                        let mut client = Some(client);
                        // Counted from accept to the end of the connection's
                        // request loop. A drop guard, not a bare dec, so a
                        // panic while handling the connection cannot leak the
                        // count and pin a refcounted host open forever.
                        struct Connected(Arc<AtomicUsize>);
                        impl Drop for Connected {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Relaxed);
                            }
                        }
                        inside_connections.fetch_add(1, Relaxed);
                        let mut guard = Some(Connected(inside_connections.clone()));
                        tasks_pool.spawn(Box::new(move || {
                            let _connected = guard.take();
                            if let Some(client) = client.take() {
                                for rq in client {
                                    messages.push(rq.into());
                                }
                            }
                        }));
                    }

                    Err(e) if transient_accept_error(&e) => {
                        // Leaving this loop drops the listener and closes the
                        // listening socket for the life of the process: the
                        // berth then sits there, alive and holding the database,
                        // accepting nothing and logging nothing. A single
                        // ECONNABORTED (the peer reset between the connection
                        // landing and accept() taking it) or one brush with the
                        // fd limit is enough to trigger it, and neither says
                        // anything is wrong with the listener. So retry, with a
                        // backoff so an fd-exhaustion storm does not spin a core
                        // while descriptors free up.
                        accept_failures = accept_failures.saturating_add(1);
                        let since = *failing_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= ACCEPT_GIVE_UP_AFTER {
                            inside_messages.push(e.into());
                            break;
                        }
                        thread::sleep(
                            ACCEPT_BACKOFF_MAX
                                .min(Duration::from_millis(u64::from(accept_failures))),
                        );
                        continue;
                    }

                    Err(e) => {
                        // Not transient — the listener itself is gone (EBADF,
                        // EINVAL). Surface it through recv() and stop accepting.
                        inside_messages.push(e.into());
                        break;
                    }
                }
            }
    });
}

impl Server {
    /// Returns the primary address the server is listening to (the first
    /// bound listener; a dual server reports its TCP address here).
    #[inline]
    pub fn server_addr(&self) -> ListenAddr {
        self.listening_addrs[0].clone()
    }

    /// The next request, waiting no longer than `timeout`: `None` on timeout
    /// or when `unblock()` woke this caller. An `Err` is the listener
    /// failing, which ends it.
    pub fn recv_timeout(&self, timeout: Duration) -> IoResult<Option<Request>> {
        match self.messages.pop_timeout(timeout) {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }

    /// Wakes one thread waiting in `recv_timeout()`, which then returns
    /// `None`: a graceful shutdown wakes each worker this way.
    pub fn unblock(&self) {
        self.messages.unblock();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close.store(true, Relaxed);
        // Connect briefly to each listener to unblock its accept thread,
        // then sweep every unix socket path off disk.
        for addr in &self.listening_addrs {
            let maybe_stream = match addr {
                ListenAddr::Ip(addr) => TcpStream::connect(addr).map(Connection::from),
                #[cfg(unix)]
                ListenAddr::Unix(addr) => {
                    // TODO: use connect_addr when its stabilized.
                    let path = addr.as_pathname().unwrap();
                    std::os::unix::net::UnixStream::connect(path).map(Connection::from)
                }
            };
            if let Ok(stream) = maybe_stream {
                let _ = stream.shutdown(Shutdown::Both);
            }

            #[cfg(unix)]
            if let ListenAddr::Unix(addr) = addr {
                if let Some(path) = addr.as_pathname() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}
