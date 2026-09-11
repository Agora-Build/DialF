//! External-tool audio backend (subprocess).
//!
//! Capture: spawn the detected tool, read raw little-endian s16 PCM from its stdout,
//! interleaved at the configured channel count (whole frames only — see `read`).
//! Playback (file): spawn the detected tool with the file path and wait for it to exit.
//!
//! Synchronous on purpose — the daemon drives these from `tokio::task::spawn_blocking`.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::backend::CaptureSource;
use super::tool_detect::{CaptureCommand, PlaybackCommand};

/// A capture source backed by an external recording tool.
pub struct CommandCaptureSource {
    /// Shared so the source can be stopped from another thread (kill -> stdout EOF).
    child: Arc<Mutex<Child>>,
    stdout: ChildStdout,
    sample_rate: u32,
    channels: u16,
    /// Bytes of an incomplete frame carried between reads. A pipe read can split anywhere,
    /// so without this a short read would either look like EOF or swap the channels for the
    /// rest of the stream.
    leftover: Vec<u8>,
    byte_buf: Vec<u8>,
}

impl CommandCaptureSource {
    /// Spawn the capture tool. `sample_rate`/`channels` must match what the command emits.
    pub fn spawn(cmd: &CaptureCommand, sample_rate: u32, channels: u16) -> io::Result<Self> {
        let argv = &cmd.argv;
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty capture argv"));
        }
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdout(Stdio::piped())
            // Piped, not discarded: the tool's stderr is often the ONLY clue for a dead
            // capture (e.g. sox "can't open input device" on a device-name mismatch) —
            // relay it into the daemon log below.
            .stderr(Stdio::piped());
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
        // Name the tool in the error — a bare ENOENT ("No such file or directory") from a
        // config pinning a tool this machine doesn't have is undiagnosable otherwise.
        let mut child = command
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("spawn capture tool `{}`: {e}", argv[0])))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "capture: no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            let tool = argv[0].clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in io::BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    tracing::warn!(target: "capture_tool", tool = %tool, "{line}");
                }
            });
        }
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdout,
            sample_rate,
            channels: channels.max(1),
            leftover: Vec::new(),
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
        let ch = self.channels as usize;
        if out.len() < ch {
            return Ok(0); // can't hold even one frame
        }
        let frame_bytes = ch * 2;
        // Whole frames only, and never report EOF for a short read: a pipe can hand us one
        // byte at a time, and `Ok(0)` means end-of-stream to every caller (which would end
        // the recording mid-call). Loop until a full frame is available or the pipe closes.
        let want_bytes = (out.len() / ch) * frame_bytes;
        loop {
            if self.leftover.len() >= frame_bytes {
                break;
            }
            self.byte_buf.resize(want_bytes.max(frame_bytes), 0);
            let n = self.stdout.read(&mut self.byte_buf[..])?;
            if n == 0 {
                // True EOF. Any trailing partial frame is dropped: emitting it would
                // desynchronize the channels of every frame after it.
                return Ok(0);
            }
            self.leftover.extend_from_slice(&self.byte_buf[..n]);
        }
        let frames = (self.leftover.len() / frame_bytes).min(out.len() / ch);
        let used = frames * frame_bytes;
        for i in 0..frames * ch {
            let lo = self.leftover[i * 2] as u16;
            let hi = self.leftover[i * 2 + 1] as u16;
            out[i] = (lo | (hi << 8)) as i16;
        }
        self.leftover.drain(..used);
        Ok(frames * ch)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
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

/// Play a file to completion, but stop early if `force` flips true (a second Ctrl+C on
/// `dialf run`). We poll the child rather than block on `status()` so a force-cancel can kill the
/// playback mid-file; a deliberate force-kill returns `Ok` (it's an interrupt, not a failure) so
/// the runner stops cleanly and the recording is still finalized.
pub fn play_file_blocking(cmd: &PlaybackCommand, force: &AtomicBool) -> io::Result<()> {
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
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("spawn playback tool `{}`: {e}", argv[0])))?;
    loop {
        if force.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(()); // deliberate force-cancel — not a playback failure
        }
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("playback tool {:?} exited with {status}", argv[0]),
                ))
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
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
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("spawn playback tool `{}`: {e}", argv[0])))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn cmd(argv: &[&str]) -> PlaybackCommand {
        PlaybackCommand {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            via_stdin: false,
        }
    }

    #[test]
    fn play_file_blocking_runs_to_completion() {
        // A quick command completes normally with force never set.
        let force = AtomicBool::new(false);
        play_file_blocking(&cmd(&["true"]), &force).expect("`true` should succeed");
    }

    #[test]
    fn play_file_blocking_reports_nonzero_exit() {
        let force = AtomicBool::new(false);
        let err = play_file_blocking(&cmd(&["false"]), &force)
            .expect_err("`false` exits non-zero -> error");
        assert!(err.to_string().contains("exited"), "got: {err}");
    }

    /// The tool's stdout is a pipe: reads split anywhere. `read` must return whole frames,
    /// must never report `Ok(0)` (= EOF, which ends the recording) for a short read, and
    /// must not drop or reorder samples across the splits.
    #[test]
    fn capture_reads_whole_frames_across_awkward_pipe_splits() {
        // 6 frames of stereo (12 samples, 24 bytes) dribbled out in 3- and 5-byte writes:
        // every boundary lands mid-frame. Values are the sample index so order is checkable.
        let script = "for i in $(seq 0 11); do printf \"\\\\$(printf '%03o' $i)\\\\000\"; done";
        let cmd = CaptureCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
        };
        let mut src = CommandCaptureSource::spawn(&cmd, 48_000, 2).expect("spawn");
        let mut got: Vec<i16> = Vec::new();
        let mut buf = [0i16; 4];
        loop {
            let n = src.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            assert_eq!(n % 2, 0, "reads must be whole stereo frames, got {n} samples");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, (0i16..12).collect::<Vec<_>>(), "samples in order, none lost");
    }

    #[test]
    fn spawn_errors_name_the_missing_tool() {
        // A config pinning a tool this machine lacks must say WHICH tool — a bare
        // "No such file or directory" is undiagnosable.
        let force = AtomicBool::new(false);
        let err = play_file_blocking(&cmd(&["/nonexistent/sox", "x"]), &force).unwrap_err();
        assert!(err.to_string().contains("/nonexistent/sox"), "{err}");
        let err = CommandCaptureSource::spawn(
            &CaptureCommand { argv: vec!["/nonexistent/rec".into(), "-q".into()] },
            48_000,
            1,
        )
        .err()
        .expect("spawning a missing capture tool must fail");
        assert!(err.to_string().contains("/nonexistent/rec"), "{err}");
    }

    #[test]
    fn play_file_blocking_force_kills_playback() {
        // force preset true: a `sleep 30` must be killed on the first poll and return Ok (a
        // deliberate interrupt, not a failure) — well under the sleep duration.
        let force = AtomicBool::new(true);
        let start = Instant::now();
        play_file_blocking(&cmd(&["sleep", "30"]), &force).expect("force-cancel returns Ok");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "force should kill playback promptly, took {:?}",
            start.elapsed()
        );
    }
}
