//! Child output that is rewritten on its way to the user, without making
//! the child think it isn't talking to a terminal.

use anyhow::Result;
use nix::pty::{Winsize, openpty};
use nix::sys::termios::{OutputFlags, SetArg, tcgetattr, tcsetattr};
use std::fs::File;
use std::process::Stdio;

/// Output handle for a child. If our own stream is a terminal, the child
/// gets a pseudo-terminal of the same size, so cargo, rustc and test
/// harnesses keep colors and progress bars. Otherwise a plain pipe.
///
/// Returns the child's end and, for a pty, our reading end. For a pipe the
/// reading end comes from the spawned `Child`.
pub fn output(is_tty: bool) -> Result<(Stdio, Option<File>)> {
    if !is_tty {
        return Ok((Stdio::piped(), None));
    }
    let size = terminal_size::terminal_size_of(std::io::stderr()).or_else(terminal_size::terminal_size);
    let ws = size.map(|(w, h)| Winsize { ws_row: h.0, ws_col: w.0, ws_xpixel: 0, ws_ypixel: 0 });
    let pty = openpty(ws.as_ref(), None::<&nix::sys::termios::Termios>)?;
    // Pass bytes through as written. Our own terminal does any \n -> \r\n
    // translation; doing it here too would double it.
    let mut tio = tcgetattr(&pty.slave)?;
    tio.output_flags.remove(OutputFlags::ONLCR);
    tcsetattr(&pty.slave, SetArg::TCSANOW, &tio)?;
    Ok((Stdio::from(pty.slave), Some(File::from(pty.master))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::Command;

    #[test]
    fn child_sees_a_terminal_through_pty() {
        let (stdio, master) = output(true).unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "test -t 1 && echo tty || echo notty"]).stdout(stdio);
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        let mut out = String::new();
        // Linux reports EIO at end of pty output instead of EOF.
        let _ = master.unwrap().read_to_string(&mut out);
        child.wait().unwrap();
        assert_eq!(out, "tty\n", "no \\r added");
    }

    #[test]
    fn pipe_when_not_a_terminal() {
        assert!(output(false).unwrap().1.is_none());
    }
}
