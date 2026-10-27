//! The local channel for blocking code: the same channel as [`super`], peer-checked the same way, driven from
//! threads that block instead of from async tasks. For a caller with no async runtime of its own, such as a proxy that
//! splices bytes with one thread per direction.
//!
//! [`Stream::connect_within`] is [`Channel::connect_within`], waited for: one deadline for the whole connect, counted
//! from the call, the OS's own wait for a busy pipe included, and the server refused (`PermissionDenied`) unless it is
//! this user. What this module adds is what blocking callers expect of a socket, as std's `TcpStream` gives it:
//! deadlines on reads and writes ([`Stream::set_read_timeout`], [`Stream::set_write_timeout`]), more than one handle
//! on a connection ([`Stream::try_clone`]), and ending it from any thread ([`Stream::shutdown`]).
//!
//! The streams are the async ones, driven by a runtime of this module's own: one worker thread, made on first use,
//! that runs the IO and the timers of every blocking stream in the process while each caller's thread waits for its
//! own operation. So nothing here is per OS but how a connection is abandoned ([`Stream::shutdown`]). Every call
//! blocks its thread: calling one from async code (a thread running a Tokio runtime) panics, as Tokio's `block_on`
//! does.

use super::{Channel as _, Channels, Endpoint, Stream as Channel_, CONNECT_WAIT};
use std::future::Future;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio::sync::watch;

/// The runtime every blocking stream is driven on: one worker thread for the IO and the timers; each operation is
/// waited for on its caller's thread.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("sidevoice-ipc")
            .enable_all()
            .build()
            .expect("the blocking channel's runtime starts")
    })
}

/// Runs `future` to completion on [`runtime`], blocking this thread until then.
fn block_on<F: Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

/// One connection, its peer checked to be this user. Reads and writes block, bounded by [`Stream::set_read_timeout`]
/// and [`Stream::set_write_timeout`]: one past its deadline fails as [`io::ErrorKind::TimedOut`] having taken or sent
/// nothing (the async stream's reads and writes are cancel-safe), and the connection stays usable. `Read` and `Write`
/// are implemented for `&Stream` too, so one handle can be read on one thread and written on another.
///
/// Dropping the last handle closes the connection the way the async stream closes: on Windows, bytes written and not
/// yet read by the peer are first flushed by `interprocess` in the background, so a peer that never reads keeps the
/// pipe open. [`Stream::shutdown`] closes it at once.
pub struct Stream(Arc<Shared>);

/// What every handle on a connection shares.
struct Shared {
    /// The connection, until [`Stream::shutdown`] takes it.
    channel: RwLock<Option<Channel_>>,
    /// Set once by [`Stream::shutdown`]: whatever is in progress on the connection stops.
    ended: watch::Sender<bool>,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stream")
    }
}

impl Stream {
    /// Connects to `endpoint`, refusing a server that is not this user ([`io::ErrorKind::PermissionDenied`]), giving
    /// up after [`CONNECT_WAIT`] ([`Stream::connect_within`]).
    pub fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
        Stream::connect_within(endpoint, CONNECT_WAIT)
    }

    /// [`Channel::connect_within`](super::Channel::connect_within), blocking: one deadline, `wait` from this call,
    /// for the whole connect, the OS's wait for a busy pipe included ([`io::ErrorKind::TimedOut`] past it), then the
    /// peer check ([`io::ErrorKind::PermissionDenied`] for a server that is not this user). Nobody listening is
    /// [`io::ErrorKind::NotFound`] or [`io::ErrorKind::ConnectionRefused`].
    pub fn connect_within(endpoint: &Endpoint, wait: Duration) -> io::Result<Stream> {
        block_on(Channels::connect_within(endpoint, wait)).map(Stream::new)
    }

    fn new(channel: Channel_) -> Stream {
        Stream(Arc::new(Shared {
            channel: RwLock::new(Some(channel)),
            ended: watch::Sender::new(false),
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        }))
    }

    /// The deadline of each read from now on, on every handle of this connection; `None` waits as long as it takes. A
    /// zero timeout is refused ([`io::ErrorKind::InvalidInput`]), as std's sockets refuse it.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self
            .0
            .read_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = nonzero(timeout)?;
        Ok(())
    }

    /// The deadline of each write from now on, on every handle of this connection; `None` waits as long as it takes.
    /// A zero timeout is refused ([`io::ErrorKind::InvalidInput`]).
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self
            .0
            .write_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = nonzero(timeout)?;
        Ok(())
    }

    /// Another handle on the same connection, sharing its deadlines: to read on one thread and write on another, or to
    /// end it from outside.
    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(Stream(Arc::clone(&self.0)))
    }

    /// Ends the connection for every handle on it, from any thread: a read or write in progress on another thread
    /// stops (a read returns 0, a write fails as [`io::ErrorKind::BrokenPipe`]), later reads return 0 and writes fail
    /// the same way, and the connection is closed at once, discarding what the peer has not read yet (on Windows, also
    /// what is still being written to a peer that does not read), so the peer sees it closed.
    pub fn shutdown(&self) -> io::Result<()> {
        self.0.ended.send_replace(true);
        // Every read or write in progress has seen `ended` and let go of the connection.
        let channel = self
            .0
            .channel
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(channel) = channel {
            super::imp::abandon(channel);
        }
        Ok(())
    }

    /// Does `op` on the connection within its deadline, unless the connection is or gets ended first. Bytes read or
    /// written, 0 for a flush.
    fn run(&self, op: Op<'_>) -> io::Result<usize> {
        let (timeout, on_end): (_, fn() -> io::Result<usize>) = match op {
            Op::Read(_) => (&self.0.read_timeout, ended_read),
            Op::Write(_) | Op::Flush => (&self.0.write_timeout, ended_write),
        };
        let channel = self
            .0
            .channel
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(mut channel) = channel.as_ref() else {
            return on_end();
        };
        let timeout = *timeout.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ended = self.0.ended.subscribe();
        block_on(async {
            let io = async {
                match op {
                    Op::Read(buf) => channel.read(buf).await,
                    Op::Write(buf) => channel.write(buf).await,
                    Op::Flush => channel.flush().await.map(|()| 0),
                }
            };
            tokio::select! {
                biased;
                _ = ended.wait_for(|ended| *ended) => on_end(),
                done = within(timeout, io) => done,
            }
        })
    }
}

/// A read, a write or a flush ([`Stream::run`]).
enum Op<'b> {
    Read(&'b mut [u8]),
    Write(&'b [u8]),
    Flush,
}

/// `timeout`, unless it is zero.
fn nonzero(timeout: Option<Duration>) -> io::Result<Option<Duration>> {
    match timeout {
        Some(timeout) if timeout.is_zero() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a zero timeout",
        )),
        timeout => Ok(timeout),
    }
}

/// `io`, failing as [`io::ErrorKind::TimedOut`] past `timeout`.
async fn within<T>(
    timeout: Option<Duration>,
    io: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, io)
            .await
            .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into())),
        None => io.await,
    }
}

/// A read on an ended connection: the end of the stream.
fn ended_read() -> io::Result<usize> {
    Ok(0)
}

/// A write on an ended connection.
fn ended_write() -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the connection was ended",
    ))
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.run(Op::Read(buf))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.run(Op::Write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.run(Op::Flush).map(|_| ())
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

/// A listening endpoint ([`Channel::listen`](super::Channel::listen)), for blocking code: the same listener, private
/// to this user, handing out only streams whose peer is this user.
pub struct Listener(super::Listener);

impl Listener {
    /// Starts listening at `endpoint`, as [`Channel::listen`](super::Channel::listen) does.
    pub fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        let _runtime = runtime().enter();
        Channels::listen(endpoint).map(Listener)
    }

    /// The next connection whose peer is this user, waiting as long as it takes ([`super::Listener::accept`]).
    pub fn accept(&self) -> io::Result<Stream> {
        block_on(self.0.accept()).map(Stream::new)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::scratch;
    use super::super::Address as _;
    use super::*;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    /// How long a test waits for something that should happen: past it, the test fails instead of hanging.
    const LIMIT: Duration = Duration::from_secs(20);

    /// What `thread` returned; a failure naming `what` when it is still running after [`LIMIT`].
    fn joined<T>(thread: JoinHandle<T>, what: &str) -> T {
        let started = Instant::now();
        while !thread.is_finished() {
            assert!(
                started.elapsed() < LIMIT,
                "{what} did not stop within {LIMIT:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread.join().unwrap()
    }

    /// A connection to a server that accepts and then neither reads nor writes until `release` is dropped, then hands
    /// its end of the connection back through its thread.
    struct Silent {
        _dir: tempfile::TempDir,
        client: Stream,
        release: mpsc::Sender<()>,
        server: JoinHandle<Stream>,
    }

    fn silent() -> Silent {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "silent");
        let listener = Listener::listen(&endpoint).unwrap();
        let (release, held) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            let _ = held.recv();
            stream
        });
        let client = Stream::connect(&endpoint).unwrap();
        Silent {
            _dir: dir,
            client,
            release,
            server,
        }
    }

    /// Writes to `stream` until a write fails, which returns that failure: writes to a peer that does not read fill
    /// the buffers and then block.
    fn write_until_it_fails(mut stream: Stream) -> io::Error {
        let chunk = vec![0u8; 64 * 1024];
        loop {
            if let Err(error) = stream.write(&chunk) {
                return error;
            }
        }
    }

    #[test]
    fn this_users_own_listener_answers_and_bytes_flow_both_ways_until_it_is_ended() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "echo");
        let listener = Listener::listen(&endpoint).unwrap();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).unwrap();
            stream.write_all(&line).unwrap();
            // Until the client ends the connection.
            stream.read(&mut line).unwrap()
        });
        let mut stream = Stream::connect_within(&endpoint, Duration::from_secs(5)).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        stream.write_all(b"hello").unwrap();
        stream.flush().unwrap();
        let mut echoed = [0u8; 5];
        stream.try_clone().unwrap().read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"hello");
        stream.shutdown().unwrap();
        assert_eq!(
            joined(server, "the echo server, once the connection ended"),
            0
        );
        assert_eq!(stream.read(&mut echoed).unwrap(), 0, "ended");
        assert_eq!(
            stream.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            stream.flush().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        stream.shutdown().unwrap();
    }

    #[test]
    fn nobody_listening_is_not_found_or_refused_not_a_hang() {
        let dir = scratch();
        let error = Stream::connect(&Endpoint::new(dir.path(), "nobody")).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
            "{error:?}"
        );
    }

    /// A pipe's one instance is taken by the first client while the listener never accepts (Windows: the next ones
    /// find it busy and wait for an instance to the connect's deadline); a socket's backlog keeps them (Unix). Either
    /// way each connect is over within its wait.
    #[test]
    fn a_listener_that_never_accepts_is_given_up_on_within_the_wait() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "busy");
        let _listener = Listener::listen(&endpoint).unwrap();
        let wait = Duration::from_millis(300);
        let mut held = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            match Stream::connect_within(&endpoint, wait) {
                Ok(stream) => held.push(stream),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}"),
            }
            assert!(
                started.elapsed() < wait + Duration::from_secs(3),
                "bounded: {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn a_read_past_its_deadline_fails_and_the_connection_stays_usable() {
        let silent = silent();
        let stream = silent.client;
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let started = Instant::now();
        let reader = stream.try_clone().unwrap();
        let reading = thread::spawn(move || (&reader).read(&mut [0u8; 8]).unwrap_err());
        assert_eq!(
            joined(reading, "a read past its deadline").kind(),
            io::ErrorKind::TimedOut
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded: {:?}",
            started.elapsed()
        );
        // A clone shares the deadline.
        let mut clone = stream.try_clone().unwrap();
        let error = joined(
            thread::spawn(move || clone.read(&mut [0u8; 8]).unwrap_err()),
            "the clone's read",
        );
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            stream
                .set_read_timeout(Some(Duration::ZERO))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        // Usable: what is written now reaches the server, and what it writes back is read.
        (&stream).write_all(b"ping").unwrap();
        drop(silent.release);
        let mut server = joined(silent.server, "the silent server");
        server.set_read_timeout(Some(LIMIT)).unwrap();
        let mut line = [0u8; 4];
        server.read_exact(&mut line).unwrap();
        assert_eq!(&line, b"ping");
        server.write_all(b"pong").unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        (&stream).read_exact(&mut line).unwrap();
        assert_eq!(&line, b"pong");
    }

    #[test]
    fn a_write_to_a_peer_that_does_not_read_fails_at_its_deadline() {
        let silent = silent();
        silent
            .client
            .set_write_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let writer = silent.client.try_clone().unwrap();
        let error = joined(
            thread::spawn(move || write_until_it_fails(writer)),
            "writes past their deadline",
        );
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        drop(silent.release);
        joined(silent.server, "the silent server");
    }

    #[test]
    fn ending_a_connection_stops_a_blocked_read_and_write_on_other_threads() {
        let silent = silent();
        let mut reader = silent.client.try_clone().unwrap();
        let reading = thread::spawn(move || reader.read(&mut [0u8; 8]));
        let writer = silent.client.try_clone().unwrap();
        let writing = thread::spawn(move || write_until_it_fails(writer));
        thread::sleep(Duration::from_millis(300));
        silent.client.shutdown().unwrap();
        assert_eq!(joined(reading, "the blocked read").unwrap(), 0, "ended");
        assert_eq!(
            joined(writing, "the blocked write").kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(silent.release);
        joined(silent.server, "the silent server");
    }

    /// The revocation case: a connection ended while a write to a peer that never reads is still pending is closed at
    /// once, not held open until that peer reads (Windows: `interprocess`' flush on close, and a pending write).
    /// The peer sees it closed: its writes fail, instead of filling a buffer nobody reads.
    #[test]
    fn ending_a_connection_to_a_stalled_peer_closes_it_at_once() {
        let silent = silent();
        let writer = silent.client.try_clone().unwrap();
        let writing = thread::spawn(move || write_until_it_fails(writer));
        thread::sleep(Duration::from_millis(300));
        silent.client.shutdown().unwrap();
        joined(writing, "the blocked write");
        drop(silent.release);
        let server = joined(silent.server, "the silent server");
        server
            .set_write_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let started = Instant::now();
        let chunk = vec![0u8; 64 * 1024];
        loop {
            match (&server).write(&chunk) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
            assert!(
                started.elapsed() < LIMIT,
                "the peer still sees the connection open after {LIMIT:?}"
            );
        }
    }
}
