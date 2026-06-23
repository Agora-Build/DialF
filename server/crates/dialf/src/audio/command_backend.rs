//! External-tool audio backend (subprocess).
//!
//! Capture: spawn the detected tool, read raw little-endian s16 mono PCM from its stdout.
//! Playback (file): spawn the detected tool with the file path and wait for it to exit.
//!
//! Synchronous on purpose — the daemon drives these from `tokio::task::spawn_blocking`.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use super::backend::CaptureSource;
use super::tool_detect::{CaptureCommand, PlaybackCommand};

/// A capture source backed by an external recording tool.
pub struct CommandCaptureSource {
    /// Shared so the source can be stopped from another thread (kill -> stdout EOF).
    child: Arc<Mutex<Child>>,
    stdout: ChildStdout,
    sample_rate: u32,
    /// Carries a leftover odd byte between reads (PCM frames are 2 bytes).
    leftover: Option<u8>,
    byte_buf: Vec<u8>,
}

impl CommandCaptureSource {
    /// Spawn the capture tool. `sample_rate` must match what the command emits.
    pub fn spawn(cmd: &CaptureCommand, sample_rate: u32) -> io::Result<Self> {
        let argv = &cmd.argv;
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty capture argv"));
        }
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Best-effort: raise the capture tool's priority so it isn't starved on a loaded
        // host. Negative nice needs privilege (root / CAP_SYS_NICE); ignored otherwise.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                // SAFETY: setpriority is async-signal-safe, valid in a post-fork child.
                libc::setpriority(libc::PRIO_PROCESS, 0, -10);
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "capture: no stdout"))?;
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdout,
            sample_rate,
            leftover: None,
            byte_buf: Vec::new(),
        })
    }

    /// A handle that can kill the capture child from another thread. Killing the child
    /// makes the blocking stdout `read` return EOF, so a background reader loop exits.
    pub fn kill_handle(&self) -> Arc<Mutex<Child>> {
        self.child.clone()
    }
}

impl CaptureSource for CommandCaptureSource {
    fn read(&mut self, out: &mut [i16]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let want_bytes = out.len() * 2;
        self.byte_buf.resize(want_bytes, 0);
        // Prepend any leftover byte from a previous short read.
        let mut filled = 0;
        if let Some(b) = self.leftover.take() {
            self.byte_buf[0] = b;
            filled = 1;
        }
        let n = self.stdout.read(&mut self.byte_buf[filled..])?;
        let total = filled + n;
        if total == 0 {
            return Ok(0); // EOF
        }
        let pairs = total / 2;
        for i in 0..pairs {
            let lo = self.byte_buf[i * 2] as u16;
            let hi = self.byte_buf[i * 2 + 1] as u16;
            out[i] = (lo | (hi << 8)) as i16;
        }
        if total % 2 == 1 {
            self.leftover = Some(self.byte_buf[total - 1]);
        }
        Ok(pairs)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl Drop for CommandCaptureSource {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Play an audio file via the resolved playback command, blocking until it finishes.
pub fn play_file_blocking(cmd: &PlaybackCommand) -> io::Result<()> {
    let argv = &cmd.argv;
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty playback argv"));
    }
    if cmd.via_stdin {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "play_file_blocking requires a file-based command, not a stdin template",
        ));
    }
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("playback tool {:?} exited with {status}", argv[0]),
        ));
    }
    Ok(())
}

/// Stream raw mono s16 PCM to the playback tool's stdin, blocking until drained.
pub fn play_pcm_blocking(cmd: &PlaybackCommand, pcm: &[i16]) -> io::Result<()> {
    let argv = &cmd.argv;
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty playback argv"));
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "playback: no stdin"))?;
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for &s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        stdin.write_all(&bytes)?;
        // stdin dropped here -> EOF to the tool.
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("playback tool {:?} exited with {status}", argv[0]),
        ));
    }
    Ok(())
}
