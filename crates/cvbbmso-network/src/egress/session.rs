//! One administrator-started macOS PF session and its authenticated client.
#![allow(unsafe_code, reason = "the trial entry points check the effective UID")]

mod console;

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Stdout, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout, timeout_at};

use super::{
    EgressPolicy, LocalGuard,
    backend::macos,
    commands::{Commands, REAP_TIMEOUT, SystemCommands},
    state,
};
use crate::NetworkError;

pub(super) const HELPER: &str =
    "/Library/Application Support/com.cvbbmso-client/CVBBMSO.app/Contents/Helpers/cvbbmso-pf";
const DIRECTORY: &str = "/Library/Application Support/com.cvbbmso-client";
const SOCKET: &str = "/Library/Application Support/com.cvbbmso-client/session.sock";
const LOCK: &str = "/Library/Application Support/com.cvbbmso-client/session.lock";
const STARTUP: Duration = Duration::from_secs(120);
const ADMISSION: Duration = Duration::from_secs(45);
const OPERATION: Duration = Duration::from_secs(60);
const REPLY_WRITE: Duration = Duration::from_secs(2);
// A request can arrive just after a health check starts. Allow time for
// that check, the requested operation, and the reply.
const ROUND_TRIP: Duration =
    Duration::from_secs(2 * OPERATION.as_secs() + REPLY_WRITE.as_secs() + 3);
const HEALTH: Duration = Duration::from_secs(15);
const HANDOFF: Duration =
    Duration::from_secs(STARTUP.as_secs() + ADMISSION.as_secs() + OPERATION.as_secs() + 5);
const STARTED: &str = "CVBBMSO-STARTED\n";
const VERIFY: u8 = b'V';
const SHUTDOWN: u8 = b'S';
const READY: u8 = b'R';
const OK: u8 = b'K';
const ERROR: u8 = b'E';
const POLICY: u8 = b'P';

pub(super) struct Session(mpsc::Sender<Request>);

struct Request {
    operation: u8,
    deadline: Instant,
    result: oneshot::Sender<Result<(), NetworkError>>,
}

impl Session {
    pub(super) async fn install(policy: EgressPolicy) -> Result<Self, NetworkError> {
        let sealed = build_policy()?;
        if policy.validate()? != sealed.validate()? {
            return Err(NetworkError::Policy);
        }
        console::protected(Path::new(DIRECTORY), true).map_err(|_| unavailable())?;
        let socket = bounded(OPERATION, async {
            let mut socket = UnixStream::connect(SOCKET)
                .await
                .map_err(|_| unavailable())?;
            require_root_peer(&socket)?;
            let message = policy_message(&sealed);
            let length = u16::try_from(message.len()).map_err(|_| NetworkError::Policy)?;
            socket.write_u16(length).await.map_err(|_| unavailable())?;
            socket
                .write_all(&message)
                .await
                .map_err(|_| unavailable())?;
            match socket.read_u8().await.map_err(|_| unavailable())? {
                READY => Ok(socket),
                POLICY => Err(NetworkError::Policy),
                ERROR => Err(NetworkError::Platform),
                _ => Err(unavailable()),
            }
        })
        .await?;
        let (sender, requests) = mpsc::channel(1);
        tokio::spawn(client_worker(socket, requests));
        Ok(Self(sender))
    }

    pub(super) async fn verify(&self) -> Result<(), NetworkError> {
        self.request(VERIFY).await
    }

    pub(super) async fn shutdown(self) -> Result<(), NetworkError> {
        self.request(SHUTDOWN).await
    }

    async fn request(&self, operation: u8) -> Result<(), NetworkError> {
        let (result, reply) = oneshot::channel();
        let deadline = Instant::now() + ROUND_TRIP;
        timeout_at(deadline, async {
            self.0
                .send(Request {
                    operation,
                    deadline,
                    result,
                })
                .await
                .map_err(|_| unavailable())?;
            reply.await.map_err(|_| unavailable())?
        })
        .await
        .map_err(|_| unavailable())?
    }
}

async fn client_worker(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    mut requests: mpsc::Receiver<Request>,
) {
    while let Some(Request {
        operation,
        deadline,
        result,
    }) = requests.recv().await
    {
        if Instant::now() >= deadline {
            break;
        }
        let answer = timeout_at(deadline, async {
            stream
                .write_u8(operation)
                .await
                .map_err(|_| unavailable())?;
            match stream.read_u8().await.map_err(|_| unavailable())? {
                OK => Ok(()),
                ERROR => Err(NetworkError::Platform),
                _ => Err(unavailable()),
            }
        })
        .await
        .unwrap_or_else(|_| Err(unavailable()));
        let finished = operation == SHUTDOWN || answer.is_err();
        let _ = result.send(answer);
        if finished {
            break;
        }
    }
}

fn require_root_peer(socket: &UnixStream) -> Result<(), NetworkError> {
    if console::peer_uid(socket)? != 0 {
        return Err(unavailable());
    }
    Ok(())
}

fn build_policy() -> Result<EgressPolicy, NetworkError> {
    let origin = |value: &str| {
        let url = url::Url::parse(value).map_err(|_| NetworkError::Policy)?;
        if url.scheme() != "https"
            || url.host().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(NetworkError::Policy);
        }
        Ok(url)
    };
    Ok(EgressPolicy {
        accoding_origin: origin("https://accoding.buaa.edu.cn")?,
        server_origin: origin(
            option_env!("CVBBMSO_SERVER_ORIGIN").ok_or(NetworkError::MacosUntrustedInstallation)?,
        )?,
    })
}

fn policy_message(policy: &EgressPolicy) -> Vec<u8> {
    format!(
        "CVBBMSO-PF/1\n{}\n{}\n",
        policy.accoding_origin.origin().ascii_serialization(),
        policy.server_origin.origin().ascii_serialization(),
    )
    .into_bytes()
}

fn unavailable() -> NetworkError {
    NetworkError::MacosServiceUnavailable
}

async fn bounded<T>(
    limit: Duration,
    operation: impl Future<Output = Result<T, NetworkError>>,
) -> Result<T, NetworkError> {
    timeout(limit, operation).await.map_err(|_| unavailable())?
}

pub(super) fn check_helper() -> Result<(), NetworkError> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(NetworkError::PermissionDenied);
    }
    if std::env::current_exe().as_deref().ok() != Some(Path::new(HELPER)) {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    console::protected(Path::new(HELPER), false)
}

fn check_installation() -> Result<EgressPolicy, NetworkError> {
    let version = SystemCommands::new(None).output("/usr/bin/sw_vers", &["-productVersion"])?;
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
        || version
            .trim()
            .split('.')
            .next()
            .and_then(|part| part.parse::<u32>().ok())
            .is_none_or(|major| major < 14)
    {
        return Err(NetworkError::MacosUnsupported);
    }
    console::check_app()?;
    console::console_uid()?;
    build_policy()
}

fn take_session_lock() -> Result<File, NetworkError> {
    console::protected(Path::new(DIRECTORY), true)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(LOCK)
        .map_err(|_| NetworkError::Io)?;
    state::check_protected(Path::new(LOCK), false)?;
    state::lock_exclusive(&file, Duration::ZERO).map_err(|_| NetworkError::MacosBusy)?;
    Ok(file)
}

fn remove_socket() -> Result<(), NetworkError> {
    match fs::symlink_metadata(SOCKET) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == 0 => {
            fs::remove_file(SOCKET).map_err(|_| NetworkError::Io)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        _ => Err(NetworkError::MacosUntrustedInstallation),
    }
}

struct SessionSocket {
    _lock: File,
}

impl Drop for SessionSocket {
    fn drop(&mut self) {
        let _ = remove_socket();
    }
}

fn bind_session() -> Result<(SessionSocket, UnixListener), NetworkError> {
    let lock = take_session_lock()?;
    remove_socket()?;
    let listener = UnixListener::bind(SOCKET).map_err(|_| NetworkError::Io)?;
    let socket = SessionSocket { _lock: lock };
    // The directory protects the name. The helper authenticates each peer
    // before reading input, so other local users cannot join the session.
    fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o666)).map_err(|_| NetworkError::Io)?;
    Ok((socket, listener))
}

async fn welcome(
    socket: &mut UnixStream,
    expected: &[u8],
    verify: impl Future<Output = Result<(), NetworkError>>,
) -> Result<(), NetworkError> {
    if usize::from(socket.read_u16().await.map_err(|_| unavailable())?) != expected.len() {
        let _ = socket.write_u8(POLICY).await;
        return Err(NetworkError::Policy);
    }
    let mut received = vec![0; expected.len()];
    socket
        .read_exact(&mut received)
        .await
        .map_err(|_| unavailable())?;
    if received != expected {
        let _ = socket.write_u8(POLICY).await;
        return Err(NetworkError::Policy);
    }
    if let Err(error) = verify.await {
        let _ = socket.write_u8(ERROR).await;
        return Err(error);
    }
    socket.write_u8(READY).await.map_err(|_| unavailable())
}

async fn accept_peer(
    listener: &UnixListener,
    is_expected: impl Fn(&UnixStream) -> bool,
) -> Result<UnixStream, NetworkError> {
    loop {
        let (peer, _) = listener.accept().await.map_err(|_| unavailable())?;
        if is_expected(&peer) {
            return Ok(peer);
        }
    }
}

async fn serve(
    guard: &LocalGuard,
    listener: &UnixListener,
    uid: libc::uid_t,
    message: &[u8],
    handoff: &mut Option<Stdout>,
) -> Result<Option<UnixStream>, NetworkError> {
    let socket = bounded(ADMISSION, async {
        let (mut child, launched) = console::spawn(uid).await?;
        verify_console(uid)?;
        if let Some(mut pipe) = handoff.take() {
            // LocalGuard::install already verified live PF. This report
            // hands over the GUI, and never grants the App exam admission.
            pipe.write_all(STARTED.as_bytes())
                .and_then(|()| pipe.flush())
                .map_err(|_| NetworkError::Io)?;
        }
        let mut socket = tokio::select! {
            result = accept_peer(listener, |peer| {
                console::console_uid().ok() == Some(uid) && launched.matches_peer(peer, uid)
            }) => result?,
            _ = child.wait() => return Err(unavailable()),
        };
        welcome(&mut socket, message, guard.verify()).await?;
        Ok(socket)
    })
    .await?;
    serve_session(socket, async || {
        verify_console(uid)?;
        guard.verify().await
    })
    .await
}

async fn serve_session<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    verify: impl AsyncFn() -> Result<(), NetworkError>,
) -> Result<Option<S>, NetworkError> {
    loop {
        // Schedule a new check after the previous operation completes. A
        // slow check never leaves overdue ticks to process ahead of the App.
        let next_check = Instant::now() + HEALTH;
        tokio::select! {
            biased;
            command = stream.read_u8() => match command {
                Ok(VERIFY) => {
                    let verified = bounded(OPERATION, verify()).await;
                    let status = if verified.is_ok() { OK } else { ERROR };
                    let reply = timeout(REPLY_WRITE, stream.write_u8(status)).await;
                    verified?;
                    reply.map_err(|_| unavailable())?.map_err(|_| unavailable())?;
                }
                Ok(SHUTDOWN) => return Ok(Some(stream)),
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
                _ => return Err(unavailable()),
            },
            () = sleep_until(next_check) => {
                bounded(OPERATION, verify()).await?;
            }
        }
    }
}

fn verify_console(uid: libc::uid_t) -> Result<(), NetworkError> {
    if console::console_uid()? != uid {
        return Err(NetworkError::PermissionDenied);
    }
    Ok(())
}

async fn finish(
    mut outcome: Result<Option<impl AsyncWrite + Unpin>, NetworkError>,
    cleanup: impl Future<Output = Result<(), NetworkError>>,
) -> Result<(), NetworkError> {
    let result = bounded(OPERATION, cleanup).await;
    if let Ok(Some(stream)) = outcome.as_mut() {
        let status = if result.is_ok() { OK } else { ERROR };
        let _ = timeout(REPLY_WRITE, stream.write_u8(status)).await;
    }
    result?;
    outcome.map(drop)
}

async fn run(policy: EgressPolicy, handoff: &mut Option<Stdout>) -> Result<(), NetworkError> {
    let uid = console::console_uid()?;
    let message = policy_message(&policy);
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|_| NetworkError::Io)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|_| NetworkError::Io)?;
    let mut hangup = signal(SignalKind::hangup()).map_err(|_| NetworkError::Io)?;
    let stopped = async {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
    };
    tokio::pin!(stopped);
    let (_socket, listener) = bind_session()?;
    let guard = tokio::select! {
        () = &mut stopped => return Err(unavailable()),
        result = bounded(STARTUP, LocalGuard::install(policy)) => result?,
    };
    let outcome = tokio::select! {
        () = &mut stopped => Ok(None),
        result = serve(&guard, &listener, uid, &message, handoff) => result,
    };
    finish(outcome, guard.shutdown()).await
}

pub(crate) fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match check_helper().and_then(|()| run_command(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("macOS PF helper failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_command(args: &[String]) -> Result<(), NetworkError> {
    match args {
        [mode] if mode == "launch" => runtime()?.block_on(launch()),
        [mode] if mode == "run" => start(false),
        [mode, flag] if mode == "run" && flag == "--handoff" => start(true),
        [mode] if mode == "status" => check_installation().map(|policy| {
            println!("Installation verified. Monitor: {}", policy.server_origin);
        }),
        [mode] if mode == "recover" => {
            let _lock = take_session_lock()?;
            macos::prepare()?;
            remove_socket()?;
            println!("No session is active. Stale PF recovery completed.");
            Ok(())
        }
        [mode, uid] if mode == "--launch-client" => {
            console::exec_app(uid.parse().map_err(|_| NetworkError::PermissionDenied)?)
        }
        _ => macos::run_watchdog(args),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, NetworkError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| NetworkError::Platform)
}

fn start(detached: bool) -> Result<(), NetworkError> {
    let mut handoff = detached.then(std::io::stdout);
    let outcome = check_installation().and_then(|policy| {
        let runtime = runtime()?;
        let outcome = runtime.block_on(run(policy, &mut handoff));
        // Existing recovery jobs own the lease if a platform call outlives
        // its async deadline.
        runtime.shutdown_timeout(Duration::from_secs(1));
        outcome
    });
    if let Some(mut pipe) = handoff {
        let error = outcome.err().unwrap_or_else(unavailable);
        let _ = writeln!(pipe, "{error}");
        return Err(error);
    }
    outcome
}

async fn launch() -> Result<(), NetworkError> {
    let mut command = tokio::process::Command::new(HELPER);
    command
        .args(["run", "--handoff"])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: the hook runs after fork and only calls async-signal-safe
    // functions. setsid detaches the keepalive process from authorization.
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| NetworkError::Io)?;
    await_handoff(&mut child).await
}

async fn await_handoff(child: &mut Child) -> Result<(), NetworkError> {
    let pipe = child.stdout.take().ok_or(NetworkError::Io)?;
    let mut reply = String::new();
    let result = timeout(
        HANDOFF,
        BufReader::new(pipe.take(512)).read_line(&mut reply),
    )
    .await;
    if matches!(result, Ok(Ok(_))) && reply == STARTED {
        return Ok(());
    }
    if !reply.trim().is_empty() {
        eprintln!("{}", reply.trim());
    }
    if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: this is our unreaped direct child. Let it roll back before
        // resorting to a kill, which leaves recovery to the existing jobs.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    if !matches!(timeout(OPERATION, child.wait()).await, Ok(Ok(_))) {
        let _ = child.start_kill();
        let _ = timeout(REAP_TIMEOUT, child.wait()).await;
    }
    Err(unavailable())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::sync::Notify;
    use tokio::time::sleep;

    #[tokio::test]
    async fn helper_credentials_are_read_from_the_socket() {
        let (peer, _) = UnixStream::pair().unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        assert_eq!(console::peer_uid(&peer), Ok(uid));
        assert_eq!(require_root_peer(&peer).is_ok(), uid == 0);
    }

    #[tokio::test]
    async fn authorization_handoff_leaves_only_a_successful_supervisor_running() {
        // SAFETY: geteuid has no preconditions. Only a nonprivileged shell
        // stands in for the installed supervisor.
        assert_ne!(unsafe { libc::geteuid() }, 0);
        for (line, expected) in [
            (STARTED, Ok(())),
            ("could not start the fixture\n", Err(unavailable())),
        ] {
            let mut supervisor = tokio::process::Command::new("/bin/sh")
                .args(["-c", "printf '%s' \"$1\"; read -r release", "sh", line])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let control = supervisor.stdin.take().unwrap();
            assert_eq!(await_handoff(&mut supervisor).await, expected);
            assert_eq!(supervisor.id().is_some(), expected.is_ok());
            drop(control);
            timeout(Duration::from_secs(5), supervisor.wait())
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn ready_requires_both_the_sealed_policy_and_successful_pf_verification() {
        for (payload, verified, response) in [
            (b"policy".as_slice(), Ok(()), READY),
            (b"policy".as_slice(), Err(NetworkError::Platform), ERROR),
            (b"forged".as_slice(), Ok(()), POLICY),
            (b"short".as_slice(), Ok(()), POLICY),
        ] {
            let (mut client, mut root) = UnixStream::pair().unwrap();
            client
                .write_u16(u16::try_from(payload.len()).unwrap())
                .await
                .unwrap();
            client.write_all(payload).await.unwrap();
            let result = welcome(&mut root, b"policy", std::future::ready(verified)).await;
            assert_eq!(client.read_u8().await.unwrap(), response);
            assert_eq!(result.is_ok(), response == READY);
        }
    }

    #[tokio::test]
    async fn an_unexpected_peer_does_not_admit_and_timeout_still_cleans_up() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let rejected = UnixStream::connect(&path).await.unwrap();
        let outcome = bounded(Duration::ZERO, accept_peer(&listener, |_| false))
            .await
            .map(Some);
        let mut cleaned = false;
        assert_eq!(
            finish(outcome, async {
                cleaned = true;
                Ok(())
            })
            .await,
            Err(unavailable())
        );
        assert!(cleaned);
        drop(rejected);
    }

    #[tokio::test]
    async fn shutdown_acknowledges_only_confirmed_cleanup() {
        for cleanup in [Ok(()), Err(NetworkError::Io)] {
            let (mut client, root) = UnixStream::pair().unwrap();
            assert_eq!(
                finish(Ok(Some(root)), std::future::ready(cleanup)).await,
                cleanup
            );
            assert_eq!(
                client.read_u8().await.unwrap(),
                if cleanup.is_ok() { OK } else { ERROR }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_health_check_leaves_time_for_verify_and_shutdown_acknowledgements() {
        let slow = OPERATION.checked_sub(Duration::from_secs(1)).unwrap();
        for operation in [VERIFY, SHUTDOWN] {
            // With a paused clock, the runtime can advance to the next health
            // check while waiting for the kernel to report socket EOF. Keep
            // this timing test on a stream that wakes its peer on close.
            let (client, root) = tokio::io::duplex(1);
            let (sender, requests) = mpsc::channel(1);
            tokio::spawn(client_worker(client, requests));
            let guard = Session(sender);
            let checking = Notify::new();
            let checks = Cell::new(0);
            let cleaned = Cell::new(false);
            let helper = async {
                let outcome = serve_session(root, async || {
                    checks.set(checks.get() + 1);
                    checking.notify_one();
                    sleep(slow).await;
                    Ok(())
                })
                .await;
                finish(outcome, async {
                    if operation == SHUTDOWN {
                        sleep(slow).await;
                    }
                    cleaned.set(true);
                    Ok(())
                })
                .await
            };
            let app = async {
                checking.notified().await;
                let requested = Instant::now();
                if operation == VERIFY {
                    assert_eq!(guard.verify().await, Ok(()));
                } else {
                    assert_eq!(guard.shutdown().await, Ok(()));
                    assert!(cleaned.get());
                }
                assert!(requested.elapsed() >= slow * 2);
                assert!(requested.elapsed() < ROUND_TRIP);
            };
            let (outcome, ()) = tokio::join!(helper, app);
            assert_eq!(outcome, Ok(()));
            assert!(cleaned.get());
            assert_eq!(checks.get(), if operation == VERIFY { 2 } else { 1 });
        }
    }
}
