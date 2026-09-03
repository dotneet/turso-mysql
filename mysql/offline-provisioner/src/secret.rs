//! Bounded password input that never places a password in command arguments.

use std::{
    fmt,
    fs::File,
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::fd::{AsRawFd, FromRawFd, RawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 4096;
const MAX_POLL_WAIT: Duration = Duration::from_millis(100);
const TTY_PATH: &[u8] = b"/dev/tty\0";
const PROMPT_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
static PROMPT_CANCELLED: AtomicBool = AtomicBool::new(false);

/// The inherited descriptor from which to read a provisioning password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretSource {
    /// Prompt twice through the controlling terminal with echo disabled.
    Tty,
    /// Read raw bytes through standard input, which must not be a terminal.
    Stdin,
    /// Read raw bytes through a caller-supplied inherited descriptor.
    Fd(i32),
}

/// A redacted failure while collecting a provisioning password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretInputError {
    /// The selected input descriptor is not permitted.
    InvalidSource,
    /// The input descriptor or controlling terminal could not be used.
    Unavailable,
    /// The supplied value exceeded the fixed input bound.
    TooLong,
    /// Raw input contained a byte that cannot be unambiguously supplied by this boundary.
    InvalidBytes,
    /// Empty input was not permitted for this operation.
    Empty,
    /// The two terminal entries did not match exactly.
    Mismatch,
    /// The input did not complete before its absolute deadline.
    TimedOut,
    /// The terminal prompt was cancelled after terminal settings were restored.
    Cancelled,
}

impl fmt::Display for SecretInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource => f.write_str("password input source is invalid"),
            Self::Unavailable => f.write_str("password input is unavailable"),
            Self::TooLong => f.write_str("password input exceeds the allowed size"),
            Self::InvalidBytes => f.write_str("password input contains unsupported bytes"),
            Self::Empty => f.write_str("password input is empty"),
            Self::Mismatch => f.write_str("password confirmation did not match"),
            Self::TimedOut => f.write_str("password input timed out"),
            Self::Cancelled => f.write_str("password input was cancelled"),
        }
    }
}

impl std::error::Error for SecretInputError {}

/// Reads one bounded password from the selected source before an absolute deadline.
pub(crate) fn read_password(
    source: SecretSource,
    allow_empty: bool,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, SecretInputError> {
    match source {
        SecretSource::Tty => read_tty_password(allow_empty, deadline),
        SecretSource::Stdin => {
            let stdin_fd = std::io::stdin().as_raw_fd();
            if is_terminal(stdin_fd) {
                return Err(SecretInputError::InvalidSource);
            }
            let input = duplicate_fd(stdin_fd)?;
            verify_stream_input(&input)?;
            read_raw_password(input, allow_empty, deadline)
        }
        SecretSource::Fd(fd) => {
            if fd < 3 || is_terminal(fd) {
                return Err(SecretInputError::InvalidSource);
            }
            let input = duplicate_fd(fd)?;
            verify_stream_input(&input)?;
            read_raw_password(input, allow_empty, deadline)
        }
    }
}

fn read_tty_password(
    allow_empty: bool,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, SecretInputError> {
    let mut tty = open_tty()?;
    let signals = SignalGuard::install()?;
    let echo_guard = EchoGuard::disable(tty.as_raw_fd())?;
    let result = (|| {
        write_prompt(&mut tty, b"Password: ")?;
        let first = read_tty_line(&mut tty, deadline)?;
        write_prompt(&mut tty, b"\nConfirm password: ")?;
        let second = read_tty_line(&mut tty, deadline)?;
        write_prompt(&mut tty, b"\n")?;
        if first.as_slice() != second.as_slice() {
            return Err(SecretInputError::Mismatch);
        }
        validate_empty(first, allow_empty)
    })();
    let echo_restore = echo_guard.restore();
    let cancelled = signals.restore()?;
    echo_restore?;
    if cancelled {
        return Err(SecretInputError::Cancelled);
    }
    result
}

fn write_prompt(tty: &mut File, bytes: &[u8]) -> Result<(), SecretInputError> {
    tty.write_all(bytes)
        .and_then(|()| tty.flush())
        .map_err(|_| SecretInputError::Unavailable)
}

fn open_tty() -> Result<File, SecretInputError> {
    // SAFETY: TTY_PATH is a static NUL-terminated absolute pathname.
    let fd = unsafe {
        libc::open(
            TTY_PATH.as_ptr().cast(),
            libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(SecretInputError::Unavailable);
    }
    // SAFETY: open returned a new owned descriptor above.
    let file = unsafe { File::from_raw_fd(fd) };
    verify_character_device(&file)?;
    Ok(file)
}

fn verify_character_device(file: &File) -> Result<(), SecretInputError> {
    let metadata = metadata(file.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFCHR {
        return Err(SecretInputError::Unavailable);
    }
    Ok(())
}

fn duplicate_fd(fd: RawFd) -> Result<File, SecretInputError> {
    // SAFETY: fcntl only duplicates the supplied descriptor; it does not alter it.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(SecretInputError::Unavailable);
    }
    // SAFETY: fcntl returned a distinct owned descriptor above.
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

fn verify_stream_input(input: &File) -> Result<(), SecretInputError> {
    let kind = metadata(input.as_raw_fd())?.st_mode & libc::S_IFMT;
    if kind != libc::S_IFIFO && kind != libc::S_IFSOCK {
        return Err(SecretInputError::InvalidSource);
    }
    Ok(())
}

fn metadata(fd: RawFd) -> Result<libc::stat, SecretInputError> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to sufficient writable storage for fstat.
    if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
        return Err(SecretInputError::Unavailable);
    }
    // SAFETY: fstat returned success and initialized metadata.
    Ok(unsafe { metadata.assume_init() })
}

fn read_raw_password(
    mut input: File,
    allow_empty: bool,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, SecretInputError> {
    let password = read_raw_bytes(&mut input, deadline)?;
    validate_empty(password, allow_empty)
}

fn read_raw_bytes(
    input: &mut File,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, SecretInputError> {
    let mut password = Zeroizing::new(Vec::with_capacity(MAX_PASSWORD_BYTES));
    let mut bytes = Zeroizing::new([0u8; 512]);
    loop {
        wait_until_readable(input.as_raw_fd(), deadline, false)?;
        let read = match input.read(&mut bytes[..]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(SecretInputError::Unavailable),
        };
        if read == 0 {
            return Ok(password);
        }
        append_raw_bytes(&mut password, &bytes[..read])?;
    }
}

fn append_raw_bytes(
    password: &mut Zeroizing<Vec<u8>>,
    chunk: &[u8],
) -> Result<(), SecretInputError> {
    if password.len().saturating_add(chunk.len()) > MAX_PASSWORD_BYTES {
        return Err(SecretInputError::TooLong);
    }
    if chunk
        .iter()
        .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(SecretInputError::InvalidBytes);
    }
    password.extend_from_slice(chunk);
    Ok(())
}

fn read_tty_line(
    input: &mut File,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, SecretInputError> {
    let mut password = Zeroizing::new(Vec::with_capacity(MAX_PASSWORD_BYTES));
    let mut byte = Zeroizing::new([0u8; 1]);
    loop {
        wait_until_readable(input.as_raw_fd(), deadline, true)?;
        match input.read_exact(&mut byte[..]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(SecretInputError::Unavailable),
        }
        if matches!(byte[0], b'\r' | b'\n') {
            return Ok(password);
        }
        if byte[0] == 0 {
            return Err(SecretInputError::InvalidBytes);
        }
        if password.len() == MAX_PASSWORD_BYTES {
            return Err(SecretInputError::TooLong);
        }
        password.push(byte[0]);
    }
}

fn wait_until_readable(
    fd: RawFd,
    deadline: Instant,
    observe_cancellation: bool,
) -> Result<(), SecretInputError> {
    loop {
        if observe_cancellation && cancelled() {
            return Err(SecretInputError::Cancelled);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(SecretInputError::TimedOut)?;
        let wait = remaining.min(MAX_POLL_WAIT);
        let timeout = wait
            .as_millis()
            .max(1)
            .min(u128::try_from(libc::c_int::MAX).expect("c_int maximum is nonnegative"))
            as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd value.
        let outcome = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if outcome == 0 {
            continue;
        }
        if outcome < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(SecretInputError::Unavailable);
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(SecretInputError::Unavailable);
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            if observe_cancellation && cancelled() {
                return Err(SecretInputError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(SecretInputError::TimedOut);
            }
            return Ok(());
        }
        if descriptor.revents & libc::POLLERR != 0 {
            return Err(SecretInputError::Unavailable);
        }
    }
}

fn validate_empty(
    password: Zeroizing<Vec<u8>>,
    allow_empty: bool,
) -> Result<Zeroizing<Vec<u8>>, SecretInputError> {
    if password.is_empty() && !allow_empty {
        return Err(SecretInputError::Empty);
    }
    Ok(password)
}

fn is_terminal(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the supplied descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

fn cancelled() -> bool {
    PROMPT_CANCELLED.load(Ordering::Relaxed)
}

extern "C" fn request_prompt_cancellation(_: libc::c_int) {
    PROMPT_CANCELLED.store(true, Ordering::Relaxed);
}

struct SignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalGuard {
    fn install() -> Result<Self, SecretInputError> {
        PROMPT_CANCELLED.store(false, Ordering::Relaxed);
        // SAFETY: zeroed sigaction is initialized below before use.
        let mut handler = unsafe { std::mem::zeroed::<libc::sigaction>() };
        handler.sa_sigaction = request_prompt_cancellation as usize;
        // SAFETY: handler.sa_mask is valid writable storage for sigemptyset.
        if unsafe { libc::sigemptyset(&mut handler.sa_mask) } != 0 {
            return Err(SecretInputError::Unavailable);
        }

        let mut previous = Vec::with_capacity(PROMPT_SIGNALS.len());
        for signal in PROMPT_SIGNALS {
            let mut action = MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: handler and action point to initialized signal-action storage.
            if unsafe { libc::sigaction(signal, &handler, action.as_mut_ptr()) } != 0 {
                let mut guard = Self { previous };
                let _ = guard.restore_inner();
                return Err(SecretInputError::Unavailable);
            }
            // SAFETY: sigaction returned success and initialized action.
            previous.push((signal, unsafe { action.assume_init() }));
        }
        Ok(Self { previous })
    }

    fn restore(mut self) -> Result<bool, SecretInputError> {
        let was_cancelled = cancelled();
        self.restore_inner()?;
        PROMPT_CANCELLED.store(false, Ordering::Relaxed);
        Ok(was_cancelled)
    }

    fn restore_inner(&mut self) -> Result<(), SecretInputError> {
        for (signal, action) in self.previous.iter().rev() {
            // SAFETY: action was returned by sigaction for this signal.
            if unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) } != 0 {
                return Err(SecretInputError::Unavailable);
            }
        }
        self.previous.clear();
        Ok(())
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

struct EchoGuard {
    fd: RawFd,
    original: Option<libc::termios>,
}

impl EchoGuard {
    fn disable(fd: RawFd) -> Result<Self, SecretInputError> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: original points to sufficient writable storage for tcgetattr.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(SecretInputError::Unavailable);
        }
        // SAFETY: tcgetattr returned success and initialized original.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: hidden is a valid termios copied from tcgetattr for this descriptor.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(SecretInputError::Unavailable);
        }
        Ok(Self {
            fd,
            original: Some(original),
        })
    }

    fn restore(mut self) -> Result<(), SecretInputError> {
        let original = self
            .original
            .take()
            .expect("terminal settings are restored once");
        // SAFETY: original was read from this descriptor before echo was changed.
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &original) } == 0 {
            Ok(())
        } else {
            Err(SecretInputError::Unavailable)
        }
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            // SAFETY: original was read from this descriptor before echo was changed.
            let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, original) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::Write,
        mem::MaybeUninit,
        os::fd::{AsRawFd, FromRawFd},
        sync::Mutex,
        time::{Duration, Instant},
    };

    use super::{
        append_raw_bytes, read_password, read_raw_bytes, read_tty_line, validate_empty, EchoGuard,
        SecretInputError, SecretSource, SignalGuard, MAX_PASSWORD_BYTES,
    };

    static SIGNAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn raw_input_rejects_ambiguous_bytes() {
        for bytes in [b"a\0b".as_slice(), b"a\rb", b"a\nb"] {
            let mut password = zeroize::Zeroizing::new(Vec::new());
            assert_eq!(
                append_raw_bytes(&mut password, bytes),
                Err(SecretInputError::InvalidBytes)
            );
        }
    }

    #[test]
    fn raw_input_requires_eof_and_obeys_the_bound() {
        let exact = vec![b'a'; MAX_PASSWORD_BYTES];
        let mut exact_input = finished_pipe(&exact);
        assert_eq!(
            read_raw_bytes(&mut exact_input, deadline_after(Duration::from_secs(1)))
                .unwrap()
                .as_slice(),
            exact
        );
        let too_long = vec![b'a'; MAX_PASSWORD_BYTES + 1];
        let mut too_long_input = finished_pipe(&too_long);
        assert_eq!(
            read_raw_bytes(&mut too_long_input, deadline_after(Duration::from_secs(1))),
            Err(SecretInputError::TooLong)
        );
    }

    #[test]
    fn stalled_pipe_times_out() {
        let (reader, writer) = pipe();
        assert_eq!(
            read_password(
                SecretSource::Fd(reader.as_raw_fd()),
                true,
                deadline_after(Duration::from_millis(20)),
            ),
            Err(SecretInputError::TimedOut)
        );
        drop(writer);
        drop(reader);
    }

    #[test]
    fn regular_file_is_not_a_password_source() {
        let mut template = *b"/tmp/turso-mysql-secret-XXXXXX\0";
        // SAFETY: template is a mutable NUL-terminated mkstemp template.
        let fd = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
        assert!(fd >= 0);
        // SAFETY: mkstemp returned an owned descriptor on success.
        let file = unsafe { File::from_raw_fd(fd) };
        assert_eq!(
            read_password(
                SecretSource::Fd(file.as_raw_fd()),
                true,
                deadline_after(Duration::from_secs(1)),
            ),
            Err(SecretInputError::InvalidSource)
        );
        // SAFETY: template remains NUL-terminated and names the just-created file.
        assert_eq!(unsafe { libc::unlink(template.as_ptr().cast()) }, 0);
    }

    #[test]
    fn empty_input_requires_explicit_permission() {
        assert_eq!(
            validate_empty(zeroize::Zeroizing::new(Vec::new()), false),
            Err(SecretInputError::Empty)
        );
        assert!(validate_empty(zeroize::Zeroizing::new(Vec::new()), true).is_ok());
    }

    #[test]
    fn terminal_line_removes_its_terminator() {
        let _lock = SIGNAL_TEST_LOCK.lock().unwrap();
        let mut newline = finished_pipe(b"secret\n");
        assert_eq!(
            read_tty_line(&mut newline, deadline_after(Duration::from_secs(1)))
                .unwrap()
                .as_slice(),
            b"secret"
        );
        let mut carriage_return = finished_pipe(b"secret\r");
        assert_eq!(
            read_tty_line(&mut carriage_return, deadline_after(Duration::from_secs(1)))
                .unwrap()
                .as_slice(),
            b"secret"
        );
    }

    #[test]
    fn fd_source_does_not_close_the_inherited_descriptor() {
        let reader = finished_pipe(b"secret");
        let password = read_password(
            SecretSource::Fd(reader.as_raw_fd()),
            false,
            deadline_after(Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(password.as_slice(), b"secret");
        // SAFETY: reader remains owned by this test because read_password duplicated it.
        assert_ne!(
            unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFD) },
            -1
        );
    }

    #[test]
    fn standard_descriptors_are_not_accepted_as_explicit_password_fds() {
        for fd in [0, 1, 2, -1] {
            assert_eq!(
                read_password(
                    SecretSource::Fd(fd),
                    true,
                    deadline_after(Duration::from_secs(1)),
                ),
                Err(SecretInputError::InvalidSource)
            );
        }
    }

    #[test]
    fn source_debug_values_do_not_contain_password_material() {
        assert_eq!(format!("{:?}", SecretSource::Tty), "Tty");
        assert_eq!(format!("{:?}", SecretSource::Stdin), "Stdin");
    }

    #[test]
    fn signal_cancellation_restores_echo_before_old_handlers_return() {
        let _lock = SIGNAL_TEST_LOCK.lock().unwrap();
        let (_master, slave) = pty();
        let before = termios(slave.as_raw_fd());
        let signals = SignalGuard::install().unwrap();
        let echo = EchoGuard::disable(slave.as_raw_fd()).unwrap();
        // SAFETY: the temporary handler installed above only sets an atomic flag.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        echo.restore().unwrap();
        assert!(signals.restore().unwrap());
        let after = termios(slave.as_raw_fd());
        assert_eq!(after.c_lflag & libc::ECHO, before.c_lflag & libc::ECHO);
        assert_eq!(after.c_lflag & libc::ISIG, before.c_lflag & libc::ISIG);
    }

    fn deadline_after(duration: Duration) -> Instant {
        Instant::now().checked_add(duration).unwrap()
    }

    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: fds points to two writable descriptors for pipe.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned owned descriptors on success.
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        // SAFETY: pipe returned owned descriptors on success.
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        (reader, writer)
    }

    fn finished_pipe(bytes: &[u8]) -> File {
        let (reader, mut writer) = pipe();
        writer.write_all(bytes).unwrap();
        drop(writer);
        reader
    }

    fn pty() -> (File, File) {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: master and slave point to writable storage for openpty descriptors.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned owned descriptors on success.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: openpty returned owned descriptors on success.
        let slave = unsafe { File::from_raw_fd(slave) };
        (master, slave)
    }

    fn termios(fd: i32) -> libc::termios {
        let mut value = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: value points to sufficient writable storage for tcgetattr.
        assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
        // SAFETY: tcgetattr succeeded and initialized value.
        unsafe { value.assume_init() }
    }
}
