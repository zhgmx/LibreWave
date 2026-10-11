use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 32 * 1024;
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_millis(100);

struct OwnedChild {
    child: Child,
    reaped: bool,
}

impl OwnedChild {
    fn finish(&mut self) -> io::Result<std::process::ExitStatus> {
        let pid = i32::try_from(self.child.id()).map_err(io::Error::other)?;
        // SAFETY: this process group was created for this child. The child is
        // not reaped until after this signal, so its PID cannot be reused.
        let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.finish();
        }
    }
}

fn exited(child: &Child) -> io::Result<bool> {
    // SAFETY: siginfo_t is a C output structure; zero initializes its scalar
    // storage. waitid writes within that storage for our live child PID.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid initialized the SIGCHLD layout, or left the zero PID.
    Ok(unsafe { info.si_pid() } != 0)
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: the descriptor is borrowed from an owned, live child pipe.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL accepts the existing flags plus O_NONBLOCK.
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_pipe(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 4096];
    match pipe.read(&mut buffer) {
        Ok(0) => Ok(true),
        Ok(count) => {
            if output.len() + count > OUTPUT_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "service probe output limit exceeded",
                ));
            }
            output.extend_from_slice(&buffer[..count]);
            Ok(false)
        }
        Err(error)
            if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn run(command: &mut Command, cancel: &AtomicBool) -> io::Result<Output> {
    if cancel.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "service probe cancelled"));
    }
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let child = command.process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    collect(child, deadline, cancel)
}

fn collect(child: Child, deadline: Instant, cancel: &AtomicBool) -> io::Result<Output> {
    let mut owned = OwnedChild { child, reaped: false };
    let mut stdout =
        owned.child.stdout.take().ok_or_else(|| io::Error::other("missing probe stdout"))?;
    let mut stderr =
        owned.child.stderr.take().ok_or_else(|| io::Error::other("missing probe stderr"))?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut status = None;
    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            if !owned.reaped {
                owned.finish()?;
            }
            return Err(io::Error::new(io::ErrorKind::TimedOut, "service probe deadline expired"));
        }
        let out_done = read_pipe(&mut stdout, &mut out)?;
        let err_done = read_pipe(&mut stderr, &mut err)?;
        if status.is_none() && exited(&owned.child)? {
            status = Some(owned.finish()?);
        }
        if let Some(status) = status
            && out_done
            && err_done
        {
            return Ok(Output { status, stdout: out, stderr: err });
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_and_cancelled_probes_kill_and_reap_the_child() {
        for cancelled in [false, true] {
            let child = Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("fake hung probe");
            let pid = i32::try_from(child.id()).expect("PID");
            let deadline =
                if cancelled { Instant::now() + Duration::from_secs(30) } else { Instant::now() };
            let error = collect(child, deadline, &AtomicBool::new(cancelled)).expect_err("timeout");
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            // SAFETY: waitpid with WNOHANG and a null status only inspects this
            // test's child. ECHILD proves that collect already reaped it.
            assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        }
    }

    #[test]
    fn captures_completed_output_and_bounds_a_flooding_probe() {
        let output = run(
            Command::new("/bin/sh").args(["-c", "printf active; printf warning >&2"]),
            &AtomicBool::new(false),
        )
        .expect("completed probe");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"active");
        assert_eq!(output.stderr, b"warning");
        let error = run(
            Command::new("/bin/sh")
                .args(["-c", "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done"]),
            &AtomicBool::new(false),
        )
        .expect_err("bounded output");
        assert!(matches!(error.kind(), io::ErrorKind::InvalidData | io::ErrorKind::TimedOut));
        let mut pipe = io::Cursor::new(vec![0; OUTPUT_LIMIT + 1]);
        let mut bytes = Vec::new();
        while read_pipe(&mut pipe, &mut bytes).is_ok() {}
        assert_eq!(bytes.len(), OUTPUT_LIMIT);
    }
}
