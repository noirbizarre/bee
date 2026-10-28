//! Secret input keeps the existing buffered stdin while controlling terminal-driver echo.

use std::io;
use std::os::fd::AsFd;

use nix::sys::termios::{self, LocalFlags, SetArg, Termios};

pub(super) struct EchoGuard<Fd: AsFd> {
    fd: Fd,
    original: Option<Termios>,
}

impl<Fd: AsFd> EchoGuard<Fd> {
    pub(super) fn hide(fd: Fd) -> io::Result<Self> {
        let original = termios::tcgetattr(&fd)?;
        let mut hidden = original.clone();
        // Canonical line editing remains available without exposing pasted credential bytes.
        hidden
            .local_flags
            .remove(LocalFlags::ECHO | LocalFlags::ECHONL);
        termios::tcsetattr(&fd, SetArg::TCSANOW, &hidden)?;
        Ok(Self {
            fd,
            original: Some(original),
        })
    }

    pub(super) fn restore(mut self) -> io::Result<()> {
        if let Some(original) = &self.original {
            termios::tcsetattr(&self.fd, SetArg::TCSANOW, original)?;
        }
        self.original = None;
        Ok(())
    }
}

impl<Fd: AsFd> Drop for EchoGuard<Fd> {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            // Failed reads and early returns must not leave the user's terminal echo disabled.
            let _ = termios::tcsetattr(&self.fd, SetArg::TCSANOW, original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_entry_disables_terminal_echo_and_restores_it_on_success_and_early_return() {
        let terminal = nix::pty::openpty(None, None).unwrap();
        let original = termios::tcgetattr(&terminal.slave).unwrap();
        assert!(original.local_flags.contains(LocalFlags::ECHO));
        for restore in [true, false] {
            let guard = EchoGuard::hide(&terminal.slave).unwrap();
            let hidden = termios::tcgetattr(&terminal.slave).unwrap();
            assert!(
                !hidden
                    .local_flags
                    .intersects(LocalFlags::ECHO | LocalFlags::ECHONL)
            );
            assert_eq!(hidden.input_flags, original.input_flags);
            if restore {
                guard.restore().unwrap();
            } else {
                drop(guard);
            }
            assert_eq!(termios::tcgetattr(&terminal.slave).unwrap(), original);
        }
    }
}
