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
        // The recording is written with the shape declared by `audio.channels`, so a pinned
        // command that hardcodes a different channel count produces a file whose header
        // lies about its contents (half speed, channels interleaved wrong) with no error
        // anywhere. We can't read the tool's mind — say so instead.
        if params.channels > 1 && !tpl.iter().any(|a| a.contains("{channels}")) {
            tracing::warn!(
                channels = params.channels,
                "audio.capture_cmd is pinned and doesn't use {{channels}} — make sure it \
                 really emits {} channels, or the recording will be mislabeled",
                params.channels
            );
        }
        return Ok(CaptureCommand {
            argv: fill(tpl, params, None),
        });
    }
    let os = current_os();
    for cand in capture_candidates(os) {
        if let Some(bin) = locate(cand.bin) {
            let mut argv = fill(&cand.template, params, None);
            argv[0] = bin;
            return Ok(CaptureCommand { argv });
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
        if let Some(bin) = locate(cand.bin) {
            let mut argv = fill(&cand.template, params, Some(file));
            argv[0] = bin;
            return Ok(PlaybackCommand { argv, via_stdin: false });
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

/// Names of the known capture / playback tools that are present on this machine, in
/// preference order — for the import host precheck.
pub(crate) fn present_tools() -> (Vec<&'static str>, Vec<&'static str>) {
    let os = current_os();
    let present = |cands: Vec<Candidate>| {
        cands.into_iter().map(|c| c.bin).filter(|b| locate(b).is_some()).collect()
    };
    (present(capture_candidates(os)), present(playback_candidates(os)))
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
                    "-i", "{av_audio}", "-ac", "{channels}", "-ar", "{rate}", "-f", "s16le",
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

/// Which sox implementation a binary is. They differ where it matters for split cards:
/// original sox (14.4.2, 2015) takes the first CoreAudio name match in either direction; sox_ng
/// skips devices with no streams in the direction being opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoxFlavor {
    SoxNg,
    Original,
    /// Couldn't run it, or it said something else.
    Unknown,
}

/// What a SoX-family binary says it is, from `--version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoxIdentity {
    pub flavor: SoxFlavor,
    /// `None` when it printed no number — Homebrew's original sox says just "SoX v".
    pub version: Option<(u32, u32, u32)>,
}

impl SoxIdentity {
    /// Read `--version` output: sox_ng prints "SoX_ng v14.8.1" (sox_ng.c), original "SoX v14.4.2".
    pub fn parse(output: &str) -> Self {
        let (flavor, rest) = if let Some(i) = output.find("SoX_ng v") {
            (SoxFlavor::SoxNg, &output[i + "SoX_ng v".len()..])
        } else if let Some(i) = output.find("SoX v") {
            (SoxFlavor::Original, &output[i + "SoX v".len()..])
        } else {
            return Self { flavor: SoxFlavor::Unknown, version: None };
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let mut parts = digits.split('.').map(|p| p.parse::<u32>().ok());
        let version = match (parts.next().flatten(), parts.next().flatten(), parts.next().flatten()) {
            (Some(a), b, c) => Some((a, b.unwrap_or(0), c.unwrap_or(0))),
            _ => None,
        };
        Self { flavor, version }
    }

    /// Run `bin --version` and read it.
    pub fn probe(bin: &std::path::Path) -> Self {
        match std::process::Command::new(bin).arg("--version").output() {
            Ok(o) => Self::parse(&format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )),
            Err(_) => Self { flavor: SoxFlavor::Unknown, version: None },
        }
    }

    /// Ordering for choosing among binaries: any sox_ng beats original sox; then newer wins.
    fn rank(&self) -> (bool, Option<(u32, u32, u32)>) {
        (self.flavor == SoxFlavor::SoxNg, self.version)
    }
}

/// The SoX family by role, `(name, sox_ng's name)`. A source-built sox_ng installs under the
/// `_ng` names next to original sox, Homebrew's sox_ng under the plain ones, and `sox` may be a
/// symlink to either — so a name says nothing; `--version` does. `play`/`rec` act by the name
/// they're run as, so each role only considers its own two names.
const SOX_ROLES: [(&str, &str); 3] = [("sox", "sox_ng"), ("play", "play_ng"), ("rec", "rec_ng")];

/// Whether `tool` (a basename) is a SoX-family command, original or sox_ng.
pub fn is_sox_family(tool: &str) -> bool {
    SOX_ROLES.iter().any(|(plain, ng)| tool == *plain || tool == *ng)
}

/// The best binary for the SoX-family role of `tool` among `dirs` (searched in order): the
/// newest sox_ng under either name, else original sox. Each distinct file is asked once —
/// symlinks resolve to their target. `None` when there is none, or `tool` isn't SoX.
pub fn best_sox_in(
    tool: &str,
    dirs: impl IntoIterator<Item = std::path::PathBuf>,
) -> Option<(std::path::PathBuf, SoxIdentity)> {
    use std::os::unix::fs::PermissionsExt;
    let (plain, ng) = SOX_ROLES.iter().find(|(plain, ng)| tool == *plain || tool == *ng)?;
    let mut seen = std::collections::HashSet::new();
    let mut best: Option<(std::path::PathBuf, SoxIdentity)> = None;
    for dir in dirs {
        for name in [ng, plain] {
            let path = dir.join(name);
            let runnable = std::fs::metadata(&path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if !runnable || !seen.insert(std::fs::canonicalize(&path).unwrap_or(path.clone())) {
                continue;
            }
            let id = SoxIdentity::probe(&path);
            // Strictly better only: on a tie the earlier PATH entry stays, as `which` would.
            if best.as_ref().is_none_or(|(_, b)| id.rank() > b.rank()) {
                best = Some((path, id));
            }
        }
    }
    best
}

/// [`best_sox_in`] over `$PATH`.
pub fn best_sox(tool: &str) -> Option<(std::path::PathBuf, SoxIdentity)> {
    let path = std::env::var_os("PATH")?;
    best_sox_in(tool, std::env::split_paths(&path))
}

/// Where `tool` lives: for the SoX family the best binary by version (see [`best_sox_in`]),
/// otherwise the first on PATH.
pub fn find_tool(tool: &str) -> Option<std::path::PathBuf> {
    if is_sox_family(tool) {
        best_sox(tool).map(|(p, _)| p)
    } else {
        which::which(tool).ok()
    }
}

/// The binary to run for candidate `bin`: a SoX-family tool resolves to the best one by
/// version (it may live under another name); anything else runs by name.
fn locate(bin: &str) -> Option<String> {
    if is_sox_family(bin) {
        find_tool(bin).map(|p| p.to_string_lossy().into_owned())
    } else {
        tool_present(bin).then(|| bin.to_string())
    }
}

/// Substitute placeholders in a template into a concrete argv.
///
/// Supported tokens:
/// - `{rate}`, `{channels}` -> numeric values
/// - `{file}` -> the audio file path (dropped if `file` is None)
/// - `{device|DEFAULT}` -> device value, or DEFAULT if no device set
/// - `{device:-D}` -> expands to two args `["-D", <device>]` if a device is set, else drops
/// - `{ca_dev}` -> sox CoreAudio device: `["-t","coreaudio",<device>]` if set, else `["-d"]`
/// - `{av_audio}` -> ffmpeg AVFoundation audio-only input: `:<device>` if set, else `:0`
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
            // AVFoundation takes "<video>:<audio>"; an audio-only device must lead with the
            // colon, or ffmpeg looks for a *video* device by that name.
            "{av_audio}" => out.push(match &p.device {
                Some(dev) => format!(":{dev}"),
                None => ":0".to_string(),
            }),
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

    /// ffmpeg reads `-i NAME` on AVFoundation as a VIDEO device; audio-only must be `:NAME`.
    #[test]
    fn avfoundation_audio_device_leads_with_a_colon() {
        let tpl = s(&["ffmpeg", "-f", "avfoundation", "-i", "{av_audio}"]);
        assert_eq!(fill(&tpl, &params(), None)[4], ":0");
        let mut p = params();
        p.device = Some("USB AUDIO  CODEC".into());
        assert_eq!(fill(&tpl, &p, None)[4], ":USB AUDIO  CODEC");
    }

    /// Verbatim first lines: Homebrew's original sox prints no number at all ("SoX v").
    #[test]
    fn sox_identity_from_version_lines() {
        let id = |s: &str| SoxIdentity::parse(s);
        assert_eq!(id("/opt/homebrew/bin/sox:      SoX v\n"), SoxIdentity { flavor: SoxFlavor::Original, version: None });
        assert_eq!(id("sox:      SoX v14.4.2\n").version, Some((14, 4, 2)));
        assert_eq!(id("sox_ng:      SoX_ng v14.8.1\n"), SoxIdentity { flavor: SoxFlavor::SoxNg, version: Some((14, 8, 1)) });
        assert_eq!(id("sox:      SoX_ng v14.10\n").version, Some((14, 10, 0)), "numeric, not lexical");
        assert_eq!(id("ffmpeg version 7").flavor, SoxFlavor::Unknown);
    }

    /// A directory of stand-in binaries answering `--version` as given.
    fn bin_dir(tag: &str, bins: &[(&str, &str)]) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dialf-sox-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, line) in bins {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn the_newest_sox_ng_wins_whatever_its_name() {
        let orig = bin_dir("orig", &[("sox", "sox:      SoX v14.4.2")]);
        let src = bin_dir("src", &[("sox_ng", "sox_ng:      SoX_ng v14.8.1"), ("play_ng", "play_ng:      SoX_ng v14.8.1")]);
        let brew = bin_dir("brew", &[("sox", "sox:      SoX_ng v14.9.0"), ("play", "play:      SoX_ng v14.9.0")]);
        let pick = |tool: &str, dirs: &[&std::path::PathBuf]| {
            best_sox_in(tool, dirs.iter().map(|d| d.to_path_buf())).map(|(p, _)| p)
        };
        // Original first on PATH still loses to sox_ng; the newer sox_ng — named plain `sox`.
        assert_eq!(pick("sox", &[&orig, &src, &brew]), Some(brew.join("sox")));
        assert_eq!(pick("sox", &[&orig, &src]), Some(src.join("sox_ng")));
        // Asked as `sox_ng`, the role is the same.
        assert_eq!(pick("sox_ng", &[&orig, &src, &brew]), Some(brew.join("sox")));
        // Only original sox: use it.
        assert_eq!(pick("sox", &[&orig]), Some(orig.join("sox")));
        // `play` acts by its name: never a `sox`-named binary.
        assert_eq!(pick("play", &[&orig, &src]), Some(src.join("play_ng")));
        assert_eq!(pick("play", &[&orig]), None);
        assert_eq!(pick("ffmpeg", &[&orig]), None, "not SoX");
        for d in [orig, src, brew] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// `sox` symlinked to sox_ng is one binary, asked once; equal candidates keep PATH order.
    #[test]
    fn a_symlinked_sox_is_one_binary() {
        let src = bin_dir("link-src", &[("sox_ng", "sox_ng:      SoX_ng v14.8.1")]);
        let links = std::env::temp_dir().join(format!("dialf-sox-{}-links", std::process::id()));
        std::fs::create_dir_all(&links).unwrap();
        let _ = std::fs::remove_file(links.join("sox"));
        std::os::unix::fs::symlink(src.join("sox_ng"), links.join("sox")).unwrap();
        let (path, id) = best_sox_in("sox", [links.clone(), src.clone()]).unwrap();
        assert_eq!(path, links.join("sox"), "first on PATH wins a tie");
        assert_eq!(id.flavor, SoxFlavor::SoxNg);
        let _ = std::fs::remove_dir_all(src);
        let _ = std::fs::remove_dir_all(links);
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
