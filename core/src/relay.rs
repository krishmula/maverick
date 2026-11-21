//! The native messaging host Chrome starts for the extension.
//!
//! Chrome talks to a host over its stdin and stdout. The relay copies those
//! bytes to and from the app's socket. Both sides use the same framing, so the
//! relay never parses a message.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;

pub fn run() -> io::Result<()> {
    let path = crate::socket_path().ok_or_else(|| io::Error::other("no application data directory"))?;
    pump(io::stdin(), io::stdout(), UnixStream::connect(path)?)
}

/// Returns when either side closes, so Chrome sees the host exit when the app quits.
pub fn pump(
    from_browser: impl Read + Send + 'static,
    to_browser: impl Write + Send + 'static,
    app: UnixStream,
) -> io::Result<()> {
    let from_app = app.try_clone()?;
    let to_app = app.try_clone()?;
    let (finished, first_finished) = mpsc::channel();
    let browser_finished = finished.clone();
    thread::spawn(move || browser_finished.send(copy(from_browser, to_app)));
    thread::spawn(move || finished.send(copy(from_app, to_browser)));
    let outcome = first_finished.recv().unwrap_or(Ok(()));
    let _ = app.shutdown(Shutdown::Both);
    outcome
}

fn copy(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
        // Stdout buffers until a newline, and a frame has none.
        to.flush()?;
    }
}
