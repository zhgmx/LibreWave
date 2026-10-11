//! Linux transport for the portable `LibreWave` IPC contract.

use librewave_core::Command;
use librewave_ipc::{
    CodecError, IpcError, IpcErrorKind, MAX_FRAME_SIZE, PROTOCOL_VERSION, RequestEnvelope,
    Response, ResponseEnvelope, decode_request, decode_response, encode_request, encode_response,
    process_request,
};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors returned by the Linux local client before or during a request.
#[derive(Debug)]
pub enum ClientError {
    Connection { path: PathBuf, message: String },
    PermissionDenied,
    VersionMismatch { expected: u16, actual: u16 },
    Protocol(String),
    Remote(IpcError),
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection { path, message } => {
                write!(formatter, "could not connect to {}: {message}", path.display())
            }
            Self::PermissionDenied => formatter.write_str("the daemon denied local-user access"),
            Self::VersionMismatch { expected, actual } => {
                write!(formatter, "protocol version mismatch: client {expected}, daemon {actual}")
            }
            Self::Protocol(message) => write!(formatter, "invalid daemon response: {message}"),
            Self::Remote(error) => formatter.write_str(&error.message),
        }
    }
}

impl std::error::Error for ClientError {}

/// A synchronous Linux client for the local daemon.
#[derive(Clone, Debug)]
pub struct Client {
    socket_path: PathBuf,
    next_request_id: u64,
}

impl Client {
    /// Creates a client for a socket path.
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self { socket_path: socket_path.into(), next_request_id: 1 }
    }

    /// Returns the socket path used by this client.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Sends one command and validates the response envelope.
    ///
    /// # Errors
    ///
    /// Returns a connection, permission, version, protocol, or daemon error when the request
    /// cannot be completed.
    pub fn request(&mut self, command: Command) -> Result<Response, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let mut stream = UnixStream::connect(&self.socket_path).map_err(|error| {
            let message = error.to_string();
            if error.kind() == io::ErrorKind::PermissionDenied {
                ClientError::PermissionDenied
            } else {
                ClientError::Connection { path: self.socket_path.clone(), message }
            }
        })?;
        stream.set_nonblocking(true).map_err(|error| ClientError::Connection {
            path: self.socket_path.clone(),
            message: error.to_string(),
        })?;
        let request = RequestEnvelope { version: PROTOCOL_VERSION, request_id, command };
        let frame = encode_request(&request)
            .map_err(|error| ClientError::Protocol(format!("could not encode request: {error}")))?;
        write_frame(&mut stream, &frame, CONNECTION_TIMEOUT, &mut || {}).map_err(|error| {
            ClientError::Connection { path: self.socket_path.clone(), message: error.to_string() }
        })?;
        let payload = read_frame(&mut stream, CONNECTION_TIMEOUT, &mut || {}).map_err(|error| {
            ClientError::Connection { path: self.socket_path.clone(), message: error.to_string() }
        })?;
        let response =
            decode_response(&payload).map_err(|error| ClientError::Protocol(error.to_string()))?;
        if response.version != PROTOCOL_VERSION {
            return Err(ClientError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                actual: response.version,
            });
        }
        if response.request_id != request_id {
            return Err(ClientError::Protocol(format!(
                "response ID {} does not match request {request_id}",
                response.request_id
            )));
        }
        match response.result {
            Ok(response) => Ok(response),
            Err(error) if error.kind == IpcErrorKind::PermissionDenied => {
                Err(ClientError::PermissionDenied)
            }
            Err(error) => Err(ClientError::Remote(error)),
        }
    }
}

/// Returns the default per-user daemon socket path.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("librewave.sock");
    }
    PathBuf::from(format!("/run/user/{}/librewave.sock", current_uid()))
}

/// Returns the per-user socket path or the explicit development override.
#[must_use]
pub fn configured_socket_path() -> PathBuf {
    std::env::var_os("LIBREWAVE_SOCKET").map_or_else(default_socket_path, PathBuf::from)
}

/// The peer credentials observed on a Linux Unix socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    /// Process ID of the peer.
    pub pid: i32,
    /// Effective user ID of the peer.
    pub uid: u32,
    /// Effective group ID of the peer.
    pub gid: u32,
}

/// A local-user peer policy for the daemon socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentialPolicy {
    allowed_uid: u32,
}

impl PeerCredentialPolicy {
    /// Creates a policy allowing exactly one user ID.
    #[must_use]
    pub const fn for_uid(uid: u32) -> Self {
        Self { allowed_uid: uid }
    }

    /// Creates a policy for the current effective user.
    #[must_use]
    pub fn current_user() -> Self {
        Self::for_uid(current_uid())
    }

    /// Checks credentials against this policy.
    ///
    /// # Errors
    ///
    /// Returns a permission error when the peer user ID differs from the configured user ID.
    pub fn check(self, credentials: PeerCredentials) -> Result<(), IpcError> {
        if credentials.uid == self.allowed_uid {
            Ok(())
        } else {
            Err(IpcError::permission_denied())
        }
    }
}

/// Reads the kernel peer credentials for a Linux Unix stream.
///
/// # Errors
///
/// Returns the operating-system error when peer credentials cannot be read.
pub fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut length =
        libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "peer credential size overflow")
        })?;
    // SAFETY: `credentials` is a valid writable buffer with its exact size,
    // and the descriptor is borrowed from the caller's live UnixStream.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCredentials { pid: credentials.pid, uid: credentials.uid, gid: credentials.gid })
}

/// Handles one accepted connection with a daemon command callback.
///
/// # Errors
///
/// Returns an I/O error when the frame cannot be read or the response cannot be written.
pub fn handle_connection<F>(
    stream: UnixStream,
    policy: PeerCredentialPolicy,
    handler: F,
) -> io::Result<()>
where
    F: FnOnce(Command) -> Result<Response, IpcError>,
{
    handle_connection_with_timeout(stream, policy, CONNECTION_TIMEOUT, handler)
}

fn handle_connection_with_timeout<F>(
    stream: UnixStream,
    policy: PeerCredentialPolicy,
    timeout: Duration,
    handler: F,
) -> io::Result<()>
where
    F: FnOnce(Command) -> Result<Response, IpcError>,
{
    handle_connection_with_tick(stream, policy, timeout, handler, &mut || {})
}

fn handle_connection_with_tick<F, T>(
    mut stream: UnixStream,
    policy: PeerCredentialPolicy,
    timeout: Duration,
    handler: F,
    tick: &mut T,
) -> io::Result<()>
where
    F: FnOnce(Command) -> Result<Response, IpcError>,
    T: FnMut(),
{
    stream.set_nonblocking(true)?;
    peer_credentials(&stream).and_then(|credentials| {
        policy
            .check(credentials)
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))
    })?;
    let payload = read_frame(&mut stream, timeout, tick)?;
    let request = match decode_request(&payload) {
        Ok(request) => request,
        Err(error) => {
            let response = ResponseEnvelope::failure(
                0,
                IpcError { kind: IpcErrorKind::InvalidRequest, message: error.to_string() },
            );
            let frame = encode_response(&response).map_err(|error| codec_io_error(&error))?;
            write_frame(&mut stream, &frame, timeout, tick)?;
            return Ok(());
        }
    };
    let response = process_request(request, handler);
    let frame = encode_response(&response).map_err(|error| codec_io_error(&error))?;
    write_frame(&mut stream, &frame, timeout, tick)
}

/// Serves local IPC and a bounded control-side audio tick on one owner thread.
/// Frame transfers have absolute deadlines and run the tick between I/O
/// attempts. Partial traffic cannot extend a frame deadline.
///
/// # Errors
///
/// Returns a socket bind or accept error.
pub fn serve_with_tick<F, T, R>(
    socket_path: &Path,
    mut handler: F,
    mut tick: T,
    mut report_connection_error: R,
) -> io::Result<()>
where
    F: FnMut(Command) -> Result<Response, IpcError>,
    T: FnMut(),
    R: FnMut(io::Error),
{
    let listener = bind_private_socket(socket_path)?;
    listener.listener.set_nonblocking(true)?;
    let policy = PeerCredentialPolicy::current_user();
    loop {
        tick();
        match listener.listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle_connection_with_tick(
                    stream,
                    policy,
                    Duration::from_millis(250),
                    &mut handler,
                    &mut tick,
                ) {
                    report_connection_error(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Serves the private user-session socket until an unrecoverable accept error occurs.
///
/// Per-connection errors are reported through `report_connection_error` and do not stop the
/// daemon. An interrupted accept is retried explicitly.
///
/// # Errors
///
/// Returns an error when the socket cannot be bound or an unrecoverable accept error occurs.
pub fn serve<F, R>(
    socket_path: &Path,
    mut handler: F,
    mut report_connection_error: R,
) -> io::Result<()>
where
    F: FnMut(Command) -> Result<Response, IpcError>,
    R: FnMut(io::Error),
{
    let listener = bind_private_socket(socket_path)?;
    let policy = PeerCredentialPolicy::current_user();
    loop {
        let (stream, _) = match listener.listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = handle_connection(stream, policy, &mut handler) {
            report_connection_error(error);
        }
    }
}

fn read_frame<T: FnMut()>(
    stream: &mut UnixStream,
    timeout: Duration,
    tick: &mut T,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut length = [0; 4];
    read_exact_until(stream, &mut length, deadline, tick, Instant::now).map_err(map_eof)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            CodecError::FrameTooLarge(length).to_string(),
        ));
    }
    let mut payload = vec![0; length];
    read_exact_until(stream, &mut payload, deadline, tick, Instant::now).map_err(map_eof)?;
    Ok(payload)
}

fn remaining(deadline: Instant, now: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC frame deadline expired"))
}

fn read_exact_until<R: Read, T: FnMut(), N: FnMut() -> Instant>(
    reader: &mut R,
    mut buffer: &mut [u8],
    deadline: Instant,
    tick: &mut T,
    mut now: N,
) -> io::Result<()> {
    while !buffer.is_empty() {
        let left = remaining(deadline, now())?;
        match reader.read(buffer) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => buffer = &mut buffer[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                tick();
                std::thread::sleep(left.min(Duration::from_millis(5)));
                continue;
            }
            Err(error) => return Err(error),
        }
        tick();
    }
    remaining(deadline, now())?;
    Ok(())
}

fn write_all_until<W: Write, T: FnMut(), N: FnMut() -> Instant>(
    writer: &mut W,
    mut buffer: &[u8],
    deadline: Instant,
    tick: &mut T,
    mut now: N,
) -> io::Result<()> {
    while !buffer.is_empty() {
        let left = remaining(deadline, now())?;
        match writer.write(buffer) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => buffer = &buffer[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                tick();
                std::thread::sleep(left.min(Duration::from_millis(5)));
                continue;
            }
            Err(error) => return Err(error),
        }
        tick();
    }
    remaining(deadline, now())?;
    Ok(())
}

fn map_eof(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        io::Error::new(io::ErrorKind::UnexpectedEof, CodecError::Truncated.to_string())
    } else {
        error
    }
}

fn write_frame<T: FnMut()>(
    stream: &mut UnixStream,
    frame: &[u8],
    timeout: Duration,
    tick: &mut T,
) -> io::Result<()> {
    write_all_until(stream, frame, Instant::now() + timeout, tick, Instant::now)
}

fn codec_io_error(error: &CodecError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

struct OwnedListener {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

fn socket_identity(metadata: &std::fs::Metadata) -> SocketIdentity {
    SocketIdentity { device: metadata.dev(), inode: metadata.ino() }
}

impl OwnedListener {
    fn bind(path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        let identity = socket_identity(&fs::symlink_metadata(path)?);
        let owned = Self { listener, path: path.to_owned(), identity };
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            drop(owned);
            return Err(error);
        }
        Ok(owned)
    }
}

impl Drop for OwnedListener {
    fn drop(&mut self) {
        let _ = remove_owned_socket(&self.path, self.identity);
    }
}

fn bind_private_socket(path: &Path) -> io::Result<OwnedListener> {
    match OwnedListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if reclaim_stale_socket(path)? {
                OwnedListener::bind(path)
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn reclaim_stale_socket(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != current_uid() {
        return Ok(false);
    }
    match UnixStream::connect(path) {
        Err(error) if is_stale_connection_error(&error) => {
            remove_owned_socket(path, socket_identity(&metadata))?;
            Ok(true)
        }
        Ok(_) | Err(_) => Ok(false),
    }
}

fn current_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not dereference a pointer.
    unsafe { libc::geteuid() }
}

fn remove_owned_socket(path: &Path, identity: SocketIdentity) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_socket() && socket_identity(&metadata) == identity {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn is_stale_connection_error(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Trickle<'a>(&'a std::cell::Cell<Instant>);

    impl Read for Trickle<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.0.set(self.0.get() + Duration::from_millis(100));
            bytes[0] = b' ';
            Ok(1)
        }
    }

    impl Write for Trickle<'_> {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            self.0.set(self.0.get() + Duration::from_millis(100));
            Ok(1)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn trickled_reads_and_writes_keep_ticking_and_cannot_extend_deadlines() {
        for write in [false, true] {
            let start = Instant::now();
            let clock = std::cell::Cell::new(start);
            let mut trickle = Trickle(&clock);
            let mut ticks = 0;
            let deadline = start + Duration::from_millis(250);
            let mut bytes = [0; 8];
            let result = if write {
                write_all_until(&mut trickle, &bytes, deadline, &mut || ticks += 1, || clock.get())
            } else {
                read_exact_until(&mut trickle, &mut bytes, deadline, &mut || ticks += 1, || {
                    clock.get()
                })
            };
            assert_eq!(result.expect_err("absolute timeout").kind(), io::ErrorKind::TimedOut);
            assert_eq!(ticks, 3);
            assert_eq!(clock.get() - start, Duration::from_millis(300));
        }
    }

    #[test]
    fn complete_private_request_preserves_response_and_runs_supervision() {
        let (server, mut client) = UnixStream::pair().expect("socket pair");
        let frame = encode_request(&RequestEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 7,
            command: Command::GetStatus,
        })
        .expect("request");
        client.write_all(&frame).expect("send");
        let mut ticks = 0;
        handle_connection_with_tick(
            server,
            PeerCredentialPolicy::current_user(),
            Duration::from_secs(1),
            |_| Ok(Response::Status { snapshot: librewave_core::Snapshot::empty() }),
            &mut || ticks += 1,
        )
        .expect("serve");
        assert!(ticks >= 3);
        client.set_nonblocking(true).expect("nonblocking");
        let payload =
            read_frame(&mut client, Duration::from_secs(1), &mut || {}).expect("response");
        assert_eq!(decode_response(&payload).expect("decode").request_id, 7);
    }

    #[test]
    fn peer_policy_rejects_a_different_uid() {
        let credentials = PeerCredentials { pid: 1, uid: 42, gid: 42 };
        assert_eq!(
            PeerCredentialPolicy::for_uid(43).check(credentials),
            Err(IpcError::permission_denied())
        );
    }

    #[test]
    fn rejected_peer_is_authenticated_before_request_read() {
        let (server, _client) = UnixStream::pair().expect("create socket pair");
        let denied_uid = u32::from(current_uid() == 0);
        let error = handle_connection(server, PeerCredentialPolicy::for_uid(denied_uid), |_| {
            panic!("a rejected peer must not reach the request handler")
        })
        .expect_err("peer should be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn only_refused_or_missing_connections_are_stale() {
        assert!(is_stale_connection_error(&io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "refused",
        )));
        assert!(is_stale_connection_error(&io::Error::new(io::ErrorKind::NotFound, "missing",)));
        assert!(!is_stale_connection_error(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "denied",
        )));
    }

    #[test]
    fn socket_cleanup_does_not_remove_a_non_socket_path() {
        let path = std::env::temp_dir().join(format!(
            "librewave-ipc-guard-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, b"user file").expect("create sentinel");
        let identity = socket_identity(&std::fs::symlink_metadata(&path).expect("stat sentinel"));
        remove_owned_socket(&path, identity).expect("inspect sentinel");
        assert!(path.exists());
        std::fs::remove_file(path).expect("remove sentinel");
    }

    #[test]
    fn socket_cleanup_requires_matching_device_and_inode() {
        let path = std::env::temp_dir().join(format!(
            "librewave-ipc-identity-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, b"user file").expect("create sentinel");
        let actual = socket_identity(&std::fs::symlink_metadata(&path).expect("stat sentinel"));
        let wrong_device = SocketIdentity { device: actual.device.saturating_add(1), ..actual };
        remove_owned_socket(&path, wrong_device).expect("inspect sentinel");
        assert!(path.exists());
        std::fs::remove_file(path).expect("remove sentinel");
    }
}
