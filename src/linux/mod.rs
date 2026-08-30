mod dispatch;
mod remote;
mod seccomp;
mod sysno;

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

use crate::bind::{BindView, DirEntry};

static CHILD_PROCESS: AtomicI32 = AtomicI32::new(0);
const FORWARDED_SIGNALS: [i32; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];
const PROBE_TIMEOUT_MS: i32 = 5_000;
const SETUP_READY: u8 = 1;
const SETUP_UNAVAILABLE: u8 = 0;

pub(crate) enum RunOutcome {
    Exited(ChildStatus),
    Unavailable { command: Command, reason: String },
}

pub(crate) enum ChildStatus {
    Exited(i32),
    Signaled(i32),
}

pub(crate) fn exit_with_status(status: ChildStatus) -> ! {
    match status {
        ChildStatus::Exited(code) => std::process::exit(code),
        ChildStatus::Signaled(signal) => unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());

            let mut unblocked: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblocked);
            libc::sigaddset(&mut unblocked, signal);
            libc::sigprocmask(libc::SIG_UNBLOCK, &unblocked, std::ptr::null_mut());
            libc::kill(libc::getpid(), signal);
            libc::_exit(128 + signal);
        },
    }
}

pub(crate) struct State {
    view: BindView,
    directories: HashMap<(u32, i32), OpenDirectory>,
    virtual_cwds: HashMap<u32, PathBuf>,
}

pub(crate) struct OpenDirectory {
    path: PathBuf,
    entries: Vec<DirEntry>,
    cursor: usize,
}

impl State {
    fn new(view: BindView) -> Self {
        Self {
            view,
            directories: HashMap::new(),
            virtual_cwds: HashMap::new(),
        }
    }
}

pub(crate) fn run(view: BindView, mut command: Command, quiet: bool) -> io::Result<RunOutcome> {
    let supervisor_view = match view.reopen() {
        Ok(view) => view,
        Err(error) => {
            return Ok(RunOutcome::Unavailable {
                command,
                reason: format!("bind-view-setup-failed error={error}"),
            });
        }
    };
    let (parent_socket, child_socket) = match socket_pair() {
        Ok(sockets) => sockets,
        Err(error) => {
            return Ok(unavailable(command, "bind-view-socket-unavailable", error));
        }
    };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Ok(unavailable(
            command,
            "bind-view-fork-unavailable",
            io::Error::last_os_error(),
        ));
    }
    if pid == 0 {
        drop(parent_socket);
        // The caller owns the execution process group. Fork and exec preserve
        // that PGID; creating a nested group here would let the command escape
        // a supervisor that kills pathshim's group as one execution.
        child_main(child_socket, &mut command, view.probe_path());
    }
    drop(child_socket);

    let listener = match receive_setup(parent_socket.as_raw_fd()) {
        Err(error) => {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = wait_for_child(pid);
            return Ok(unavailable(
                command,
                "bind-view-listener-handshake-failed",
                error,
            ));
        }
        Ok(Setup::Ready(listener)) => listener,
        Ok(Setup::Unavailable(error)) => {
            let _ = wait_for_child(pid);
            return Ok(unavailable(command, "bind-view-unavailable", error));
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let supervisor_stop = Arc::clone(&stop);
    let supervisor = match std::thread::Builder::new()
        .name("pathshim-fs".to_owned())
        .spawn(move || {
            if let Err(error) = supervise(listener, supervisor_view, supervisor_stop) {
                if !quiet {
                    eprintln!("pathshim: filesystem supervisor stopped: {error}");
                }
            }
        }) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = wait_for_child(pid);
            return Ok(unavailable(
                command,
                "bind-view-supervisor-unavailable",
                error,
            ));
        }
    };
    let signal_forwarding = match SignalForwarding::install(pid) {
        Ok(forwarding) => forwarding,
        Err(error) => {
            stop_attempt(pid, &stop, supervisor);
            return Ok(RunOutcome::Unavailable {
                command,
                reason: format!("signal-forwarding-unavailable error={error}"),
            });
        }
    };
    if let Err(error) = write_byte(parent_socket.as_raw_fd(), 1) {
        drop(signal_forwarding);
        stop_attempt(pid, &stop, supervisor);
        return Ok(RunOutcome::Unavailable {
            command,
            reason: format!("bind-view-probe-start-failed error={error}"),
        });
    }

    let probe_error = match read_i32_timeout(parent_socket.as_raw_fd(), PROBE_TIMEOUT_MS) {
        Ok(error) => error,
        Err(error) => {
            drop(signal_forwarding);
            stop_attempt(pid, &stop, supervisor);
            return Ok(RunOutcome::Unavailable {
                command,
                reason: format!("bind-view-probe-unavailable error={error}"),
            });
        }
    };
    if probe_error != 0 {
        drop(signal_forwarding);
        stop_attempt(pid, &stop, supervisor);
        return Ok(RunOutcome::Unavailable {
            command,
            reason: format!(
                "bind-view-probe-failed error={}",
                io::Error::from_raw_os_error(probe_error)
            ),
        });
    }

    if !quiet {
        eprintln!(
            "pathshim: collect mode=bind-view projections={} features=replace,shared-source",
            view.projection_count()
        );
    }
    if let Err(error) = write_byte(parent_socket.as_raw_fd(), 1) {
        drop(signal_forwarding);
        stop_attempt(pid, &stop, supervisor);
        return Ok(unavailable(command, "bind-view-probe-finish-failed", error));
    }

    let status = wait_for_child(pid)?;
    drop(signal_forwarding);
    stop.store(true, Ordering::Release);
    let _ = supervisor.join();
    Ok(RunOutcome::Exited(status))
}

fn unavailable(command: Command, reason: &str, error: io::Error) -> RunOutcome {
    RunOutcome::Unavailable {
        command,
        reason: format!("{reason} error={error}"),
    }
}

fn stop_attempt(pid: i32, stop: &AtomicBool, supervisor: std::thread::JoinHandle<()>) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = wait_for_child(pid);
    stop.store(true, Ordering::Release);
    let _ = supervisor.join();
}

struct SignalForwarding {
    previous: Vec<(i32, libc::sigaction)>,
}

impl SignalForwarding {
    fn install(child_process: i32) -> io::Result<Self> {
        CHILD_PROCESS.store(child_process, Ordering::Release);
        let mut previous = Vec::with_capacity(FORWARDED_SIGNALS.len());
        for signal in FORWARDED_SIGNALS {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = forward_signal as usize;
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, &action, &mut old) } < 0 {
                for (installed_signal, installed_action) in previous.iter().rev() {
                    unsafe {
                        libc::sigaction(*installed_signal, installed_action, std::ptr::null_mut())
                    };
                }
                CHILD_PROCESS.store(0, Ordering::Release);
                return Err(io::Error::last_os_error());
            }
            previous.push((signal, old));
        }
        Ok(Self { previous })
    }
}

impl Drop for SignalForwarding {
    fn drop(&mut self) {
        CHILD_PROCESS.store(0, Ordering::Release);
        for (signal, action) in self.previous.iter().rev() {
            unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) };
        }
    }
}

extern "C" fn forward_signal(signal: i32) {
    let child = CHILD_PROCESS.load(Ordering::Acquire);
    if child > 0 {
        unsafe { libc::kill(child, signal) };
    }
}

fn child_main(socket: OwnedFd, command: &mut Command, probe_path: &Path) -> ! {
    let listener = match seccomp::install_listener() {
        Ok(listener) => listener,
        Err(error) => {
            let errno = error.raw_os_error().unwrap_or(libc::EIO);
            let _ = send_setup_error(socket.as_raw_fd(), errno);
            unsafe { libc::_exit(0) }
        }
    };
    if let Err(error) = send_fd(socket.as_raw_fd(), listener.as_raw_fd()) {
        child_error(format!("cannot pass filesystem listener: {error}"));
    }
    drop(listener);
    if let Err(error) = read_byte(socket.as_raw_fd()) {
        child_error(format!("cannot synchronize filesystem supervisor: {error}"));
    }
    if let Err(error) = probe_projection(probe_path) {
        let _ = write_i32(
            socket.as_raw_fd(),
            error.raw_os_error().unwrap_or(libc::EIO),
        );
        unsafe { libc::_exit(0) }
    }
    if let Err(error) = write_i32(socket.as_raw_fd(), 0) {
        child_error(format!("cannot report filesystem probe: {error}"));
    }
    if let Err(error) = read_byte(socket.as_raw_fd()) {
        child_error(format!("cannot finish filesystem probe: {error}"));
    }
    drop(socket);

    let error = command.exec();
    child_error(format!(
        "cannot execute command `{}`: {error}",
        command.get_program().to_string_lossy()
    ));
}

fn probe_projection(probe_path: &Path) -> io::Result<()> {
    let path = std::ffi::CString::new(probe_path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(libc::AT_FDCWD, path.as_ptr(), &mut stat, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::close(fd) };
    Ok(())
}

fn child_error(message: String) -> ! {
    eprintln!("pathshim: {message}");
    unsafe { libc::_exit(1) }
}

fn supervise(listener: OwnedFd, view: BindView, stop: Arc<AtomicBool>) -> io::Result<()> {
    let mut state = State::new(view);
    while !stop.load(Ordering::Acquire) {
        let mut poll_fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 100) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            break;
        }
        if poll_fd.revents & libc::POLLIN == 0 {
            continue;
        }
        let notification = match seccomp::receive(listener.as_raw_fd()) {
            Ok(notification) => notification,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(error) => return Err(error),
        };
        match dispatch::handle(listener.as_raw_fd(), &mut state, notification) {
            Ok(()) => {}
            // The tracee can exit between notification receipt and response. The kernel
            // then invalidates the notification; this is lifecycle cleanup, not loss of
            // filesystem projection for the remaining processes.
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_for_child(pid: i32) -> io::Result<ChildStatus> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if libc::WIFEXITED(status) {
            return Ok(ChildStatus::Exited(libc::WEXITSTATUS(status)));
        }
        if libc::WIFSIGNALED(status) {
            return Ok(ChildStatus::Signaled(libc::WTERMSIG(status)));
        }
    }
}

fn socket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut sockets = [0; 2];
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(sockets[0]),
            OwnedFd::from_raw_fd(sockets[1]),
        )
    })
}

fn send_fd(socket: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = SETUP_READY;
    let mut iov = libc::iovec {
        iov_base: &mut byte as *mut u8 as *mut libc::c_void,
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    message.msg_controllen = control.len();
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::write(libc::CMSG_DATA(header) as *mut RawFd, fd);
    }
    if unsafe { libc::sendmsg(socket, &message, 0) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

enum Setup {
    Ready(OwnedFd),
    Unavailable(io::Error),
}

fn send_setup_error(socket: RawFd, error: i32) -> io::Result<()> {
    let mut payload = [0u8; 5];
    payload[0] = SETUP_UNAVAILABLE;
    payload[1..].copy_from_slice(&error.to_ne_bytes());
    let result = unsafe {
        libc::send(
            socket,
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            0,
        )
    };
    if result == payload.len() as isize {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn receive_setup(socket: RawFd) -> io::Result<Setup> {
    let mut payload = [0u8; 5];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    message.msg_controllen = control.len();
    let received = unsafe { libc::recvmsg(socket, &mut message, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if received == 5 && payload[0] == SETUP_UNAVAILABLE {
        let error = i32::from_ne_bytes(payload[1..].try_into().unwrap());
        return Ok(Setup::Unavailable(io::Error::from_raw_os_error(error)));
    }
    if received != 1 || payload[0] != SETUP_READY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid filesystem listener setup response",
        ));
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null()
        || unsafe { (*header).cmsg_level } != libc::SOL_SOCKET
        || unsafe { (*header).cmsg_type } != libc::SCM_RIGHTS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "listener fd missing",
        ));
    }
    let fd = unsafe { std::ptr::read(libc::CMSG_DATA(header) as *const RawFd) };
    Ok(Setup::Ready(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn write_byte(fd: RawFd, byte: u8) -> io::Result<()> {
    let result = unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
    if result == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_byte(fd: RawFd) -> io::Result<()> {
    let mut byte = 0u8;
    let result = unsafe { libc::read(fd, &mut byte as *mut u8 as *mut libc::c_void, 1) };
    if result == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn write_i32(fd: RawFd, value: i32) -> io::Result<()> {
    let bytes = value.to_ne_bytes();
    let result = unsafe { libc::send(fd, bytes.as_ptr() as *const libc::c_void, bytes.len(), 0) };
    if result == bytes.len() as isize {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_i32_timeout(fd: RawFd, timeout_ms: i32) -> io::Result<i32> {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "filesystem capability probe timed out",
            ));
        }
        if poll_fd.revents & libc::POLLIN != 0 {
            let mut bytes = [0u8; 4];
            let received =
                unsafe { libc::recv(fd, bytes.as_mut_ptr() as *mut libc::c_void, bytes.len(), 0) };
            if received == bytes.len() as isize {
                return Ok(i32::from_ne_bytes(bytes));
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "filesystem capability probe result missing",
            ));
        }
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "filesystem capability probe process stopped",
        ));
    }
}
