//! Detect and template the external audio tool used for capture/playback.
//!
//! No audio library is bound; `dialfd` spawns a CLI tool and pipes raw `s16le` mono PCM.
//! Defaults are auto-detected per OS via `which`, and every command is overridable in
//! [`crate::config::AudioConfig`]. Placeholders in templates: `{rate}`, `{channels}`,
//! `{device}`, `{file}`.

use anyhow::{anyhow, Result};

/// Which OS family we're templating for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Other,
}

/// The host OS at compile time.
pub fn current_os() -> Os {
    if cfg!(target_os = "linux") {
        Os::Linux
    } else if cfg!(target_os = "macos") {
        Os::MacOs
    } else {
        Os::Other
    }
}

/// A resolved capture command. The tool MUST emit raw little-endian s16 mono PCM on
/// stdout at `rate`.
#[derive(Debug, Clone)]
pub struct CaptureCommand {
    pub argv: Vec<String>,
}

/// A resolved playback command for a file. If `via_stdin` is true the engine streams
/// PCM to the child's stdin; otherwise the file path is embedded in `argv`.
#[derive(Debug, Clone)]
pub struct PlaybackCommand {
    pub argv: Vec<String>,
    pub via_stdin: bool,
}

/// Parameters used to fill command templates.
#[derive(Debug, Clone)]
pub struct AudioParams {
    pub rate: u32,
    pub channels: u16,
    pub device: Option<String>,
}

/// Resolve a capture command: config override if present, else auto-detect.
pub fn resolve_capture(params: &AudioParams, override_cmd: Option<&[String]>) -> Result<CaptureCommand> {
    if let Some(tpl) = override_cmd {
        return Ok(CaptureCommand {
            argv: fill(tpl, params, None),
        });
    }
    let os = current_os();
    for cand in capture_candidates(os) {
        if tool_present(cand.bin) {
            return Ok(CaptureCommand {
                argv: fill(&cand.template, params, None),
            });
        }
    }
    Err(anyhow!(
        "no capture tool found (looked for {:?}); set audio.capture_cmd in config",
        capture_candidates(os).iter().map(|c| c.bin).collect::<Vec<_>>()
    ))
}

/// Resolve a file-playback command: config override if present, else auto-detect.
pub fn resolve_playback_file(
    file: &str,
    params: &AudioParams,
    override_cmd: Option<&[String]>,
) -> Result<PlaybackCommand> {
    if let Some(tpl) = override_cmd {
        let via_stdin = !tpl.iter().any(|a| a.contains("{file}"));
        return Ok(PlaybackCommand {
            argv: fill(tpl, params, Some(file)),
            via_stdin,
        });
    }
    let os = current_os();
    for cand in playback_candidates(os) {
        if tool_present(cand.bin) {
            return Ok(PlaybackCommand {
                argv: fill(&cand.template, params, Some(file)),
                via_stdin: false,
            });
        }
    }
    Err(anyhow!(
        "no playback tool found (looked for {:?}); set audio.playback_cmd in config",
        playback_candidates(os).iter().map(|c| c.bin).collect::<Vec<_>>()
    ))
}

struct Candidate {
    bin: &'static str,
    template: Vec<String>,
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

fn capture_candidates(os: Os) -> Vec<Candidate> {
    match os {
        Os::Linux => vec![
            Candidate {
                bin: "arecord",
                template: s(&[
                    "arecord", "-q", "-t", "raw", "-f", "S16_LE", "-r", "{rate}", "-c",
                    "{channels}", "{device:-D}", "-",
                ]),
            },
            Candidate {
                bin: "ffmpeg",
                template: s(&[
                    "ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "alsa", "-i",
                    "{device|default}", "-ac", "{channels}", "-ar", "{rate}", "-f", "s16le",
                    "-",
                ]),
            },
            Candidate {
                bin: "rec",
                template: s(&[
                    "rec", "-q", "-t", "raw", "-b", "16", "-e", "signed-integer", "-r",
                    "{rate}", "-c", "{channels}", "-",
                ]),
            },
        ],
        Os::MacOs => vec![
            // Preferred: sox selects the CoreAudio device by name ({ca_dev}).
            Candidate {
                bin: "sox",
                template: s(&[
                    "sox", "-q", "{ca_dev}", "-t", "raw", "-b", "16", "-e", "signed-integer",
                    "-r", "{rate}", "-c", "{channels}", "-",
                ]),
            },
            Candidate {
                bin: "ffmpeg",
                template: s(&[
                    "ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "avfoundation",
                    "-i", "{device|:0}", "-ac", "{channels}", "-ar", "{rate}", "-f", "s16le",
                    "-",
                ]),
            },
        ],
        Os::Other => vec![],
    }
}

fn playback_candidates(os: Os) -> Vec<Candidate> {
    match os {
        Os::Linux => vec![
            Candidate {
                bin: "aplay",
                template: s(&["aplay", "-q", "{device:-D}", "{file}"]),
            },
            Candidate {
                bin: "ffplay",
                template: s(&[
                    "ffplay", "-autoexit", "-nodisp", "-loglevel", "error", "{file}",
                ]),
            },
            Candidate {
                bin: "play",
                template: s(&["play", "-q", "{file}"]),
            },
        ],
        Os::MacOs => vec![
            // Preferred: sox plays the file to the CoreAudio device by name ({ca_dev}).
            Candidate {
                bin: "sox",
                template: s(&["sox", "-q", "-V1", "{file}", "{ca_dev}"]),
            },
            // afplay/ffplay go to the default output only (no device selection).
            Candidate {
                bin: "afplay",
                template: s(&["afplay", "{file}"]),
            },
            Candidate {
                bin: "ffplay",
                template: s(&[
                    "ffplay", "-autoexit", "-nodisp", "-loglevel", "error", "{file}",
                ]),
            },
        ],
        Os::Other => vec![],
    }
}

fn tool_present(bin: &str) -> bool {
    which::which(bin).is_ok()
}

/// Substitute placeholders in a template into a concrete argv.
///
/// Supported tokens:
/// - `{rate}`, `{channels}` -> numeric values
/// - `{file}` -> the audio file path (dropped if `file` is None)
/// - `{device|DEFAULT}` -> device value, or DEFAULT if no device set
/// - `{device:-D}` -> expands to two args `["-D", <device>]` if a device is set, else drops
/// - `{ca_dev}` -> sox CoreAudio device: `["-t","coreaudio",<device>]` if set, else `["-d"]`
fn fill(template: &[String], p: &AudioParams, file: Option<&str>) -> Vec<String> {
    let mut out = Vec::with_capacity(template.len());
    for tok in template {
        match tok.as_str() {
            "{rate}" => out.push(p.rate.to_string()),
            "{channels}" => out.push(p.channels.to_string()),
            "{file}" => {
                if let Some(f) = file {
                    out.push(f.to_string());
                }
            }
            "{ca_dev}" => match &p.device {
                Some(dev) => {
                    out.push("-t".to_string());
                    out.push("coreaudio".to_string());
                    out.push(dev.clone());
                }
                None => out.push("-d".to_string()),
            },
            t if t == "{device:-D}" => {
                if let Some(dev) = &p.device {
                    out.push("-D".to_string());
                    out.push(dev.clone());
                }
            }
            t if t.starts_with("{device|") && t.ends_with('}') => {
                let default = &t["{device|".len()..t.len() - 1];
                out.push(p.device.clone().unwrap_or_else(|| default.to_string()));
            }
            other => {
                // Generic single-token substitutions for custom templates.
                let replaced = other
                    .replace("{rate}", &p.rate.to_string())
                    .replace("{channels}", &p.channels.to_string())
                    .replace("{device}", p.device.as_deref().unwrap_or(""))
                    .replace("{file}", file.unwrap_or(""));
                out.push(replaced);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> AudioParams {
        AudioParams {
            rate: 48_000,
            channels: 1,
            device: None,
        }
    }

    #[test]
    fn fills_rate_and_channels() {
        let tpl = s(&["arecord", "-r", "{rate}", "-c", "{channels}", "-"]);
        let argv = fill(&tpl, &params(), None);
        assert_eq!(argv, vec!["arecord", "-r", "48000", "-c", "1", "-"]);
    }

    #[test]
    fn device_dash_d_drops_when_absent_and_expands_when_present() {
        let tpl = s(&["arecord", "{device:-D}", "-"]);
        assert_eq!(fill(&tpl, &params(), None), vec!["arecord", "-"]);

        let mut p = params();
        p.device = Some("plughw:1,0".into());
        assert_eq!(
            fill(&tpl, &p, None),
            vec!["arecord", "-D", "plughw:1,0", "-"]
        );
    }

    #[test]
    fn device_default_token() {
        let tpl = s(&["ffmpeg", "-i", "{device|default}"]);
        assert_eq!(fill(&tpl, &params(), None), vec!["ffmpeg", "-i", "default"]);

        let mut p = params();
        p.device = Some("hw:0".into());
        assert_eq!(fill(&tpl, &p, None), vec!["ffmpeg", "-i", "hw:0"]);
    }

    #[test]
    fn file_token() {
        let tpl = s(&["afplay", "{file}"]);
        assert_eq!(
            fill(&tpl, &params(), Some("a.wav")),
            vec!["afplay", "a.wav"]
        );
    }

    #[test]
    fn override_via_stdin_detected_by_absence_of_file_token() {
        let tpl = s(&["aplay", "-t", "raw", "-"]);
        let cmd = resolve_playback_file("x.wav", &params(), Some(&tpl)).unwrap();
        assert!(cmd.via_stdin);
    }
}
