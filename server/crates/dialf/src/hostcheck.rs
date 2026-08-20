//! Interactive host precheck for `dialf import`: verify the imported config's audio setup
//! against THIS machine — capture/playback devices, the capture tool, record_dir — and
//! prompt the user to fix mismatches (pick a detected device, auto-fix a tool path, install
//! the tool) so an imported bundle actually runs, not just installs.
//!
//! Detection uses only what the OS ships: `system_profiler` (macOS) and `/proc/asound`
//! (Linux). Prompts default to "keep as-is" on Enter, and everything here degrades to the
//! plain warnings path when stdin isn't a terminal.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::bundle::{yaml_flow, yaml_quote};

/// One audio endpoint on this machine. `value` is what goes in the config
/// (CoreAudio device name on macOS, `plughw:N,0` on Linux); `label` is for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub value: String,
    pub label: String,
    pub input: bool,
    pub output: bool,
}

/// Detect this machine's audio devices (empty when detection isn't possible).
pub fn detect_devices() -> Vec<AudioDevice> {
    if cfg!(target_os = "macos") {
        detect_macos()
    } else {
        parse_asound_dir(Path::new("/proc/asound"))
    }
}

fn detect_macos() -> Vec<AudioDevice> {
    let Ok(out) = Command::new("system_profiler").args(["SPAudioDataType", "-json"]).output()
    else {
        return Vec::new();
    };
    parse_system_profiler(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `system_profiler SPAudioDataType -json` output.
pub(crate) fn parse_system_profiler(json: &str) -> Vec<AudioDevice> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let items = v
        .get("SPAudioDataType")
        .and_then(|a| a.get(0))
        .and_then(|d| d.get("_items"))
        .and_then(|i| i.as_array());
    let Some(items) = items else { return Vec::new() };
    items
        .iter()
        .filter_map(|it| {
            let name = it.get("_name")?.as_str()?.to_string();
            let chans = |key: &str| it.get(key).and_then(|c| c.as_i64()).unwrap_or(0) > 0;
            Some(AudioDevice {
                label: name.clone(),
                value: name,
                input: chans("coreaudio_device_input"),
                output: chans("coreaudio_device_output"),
            })
        })
        .collect()
}

/// Parse a `/proc/asound` tree: the `cards` file names the cards; a `card<N>/pcm*c` dir
/// means capture-capable, `pcm*p` playback-capable. Config value is ALSA `plughw:N,0`.
pub(crate) fn parse_asound_dir(root: &Path) -> Vec<AudioDevice> {
    let Ok(cards) = std::fs::read_to_string(root.join("cards")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in cards.lines() {
        // " 1 [Device         ]: USB-Audio - USB Audio Device"
        let t = line.trim_start();
        let Some((idx, rest)) = t.split_once(' ') else { continue };
        let Ok(n) = idx.parse::<u32>() else { continue };
        let name = rest.rsplit_once(" - ").map(|(_, n)| n.trim()).unwrap_or(rest).to_string();
        let card_dir = root.join(format!("card{n}"));
        let (mut input, mut output) = (false, false);
        if let Ok(entries) = std::fs::read_dir(&card_dir) {
            for e in entries.flatten() {
                let f = e.file_name().to_string_lossy().into_owned();
                if f.starts_with("pcm") {
                    input |= f.ends_with('c');
                    output |= f.ends_with('p');
                }
            }
        }
        out.push(AudioDevice {
            value: format!("plughw:{n},0"),
            label: format!("{name} (card {n})"),
            input,
            output,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Interactive flow
// ---------------------------------------------------------------------------

/// Test-injectable prompt I/O (stdin + stderr in production).
pub struct Prompter<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
}

impl Prompter<'_> {
    fn ask(&mut self, q: &str) -> Result<String> {
        write!(self.out, "{q}")?;
        self.out.flush()?;
        let mut line = String::new();
        self.input.read_line(&mut line)?;
        Ok(line.trim().to_string())
    }

    fn yes(&mut self, q: &str) -> Result<bool> {
        Ok(matches!(self.ask(&format!("{q} [y/N]: "))?.to_lowercase().as_str(), "y" | "yes"))
    }

    /// Numbered menu; Enter keeps the current value. Returns the picked option value,
    /// a free-typed value, or None to keep.
    fn pick(&mut self, title: &str, options: &[&AudioDevice]) -> Result<Option<String>> {
        writeln!(self.out, "{title}")?;
        for (i, d) in options.iter().enumerate() {
            writeln!(self.out, "  {}) {}", i + 1, d.label)?;
        }
        let ans = self.ask("pick a number, type a device name, or Enter to keep as-is: ")?;
        if ans.is_empty() {
            return Ok(None);
        }
        if let Ok(n) = ans.parse::<usize>() {
            if (1..=options.len()).contains(&n) {
                return Ok(Some(options[n - 1].value.clone()));
            }
        }
        Ok(Some(ans))
    }
}

/// Run the interactive precheck over the imported config document, recording fixes as
/// (key, pre-rendered-YAML-value) line edits. `folder` is the bundle folder (for resolving
/// a relative record_dir the user types).
pub fn run(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
    folder: &Path,
) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut err = std::io::stderr();
    let mut p = Prompter { input: &mut input, out: &mut err };
    run_with(doc, edits, folder, detect_devices(), &mut p)
}

pub(crate) fn run_with(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
    folder: &Path,
    devices: Vec<AudioDevice>,
    p: &mut Prompter<'_>,
) -> Result<()> {
    writeln!(p.out, "\nchecking this machine against the imported config…")?;

    // record_dir: confirm or redirect.
    if let Some(current) = audio_str(doc, "record_dir") {
        let ans = p.ask(&format!("record_dir [{current}] (Enter to keep, or type a new path): "))?;
        if !ans.is_empty() && ans != current {
            let abs = crate::daemon::resolve_path_under(Some(folder), Path::new(&ans));
            set_audio_str(doc, edits, "record_dir", &abs.to_string_lossy());
        }
    }

    // Devices: verify the configured names exist here; offer the detected list otherwise.
    if devices.is_empty() {
        writeln!(
            p.out,
            "no audio devices detected — plug in the USB sound card{}",
            if cfg!(target_os = "macos") {
                ", or for virtual audio: brew install blackhole-2ch blackhole-16ch"
            } else {
                "; check `cat /proc/asound/cards`, load snd-usb-audio, and make sure your \
                 user is in the `audio` group"
            }
        )?;
    }
    let mut renames: Vec<(String, String)> = Vec::new();
    for (key, dir_input) in [("capture_device", true), ("playback_device", false)] {
        let Some(current) = audio_str(doc, key) else { continue };
        let usable: Vec<&AudioDevice> = devices
            .iter()
            .filter(|d| if dir_input { d.input } else { d.output })
            .collect();
        if usable.iter().any(|d| d.value == current) {
            writeln!(p.out, "{key} \"{current}\" ✓")?;
            continue;
        }
        if cfg!(target_os = "macos") && current.contains("BlackHole") {
            writeln!(
                p.out,
                "\"{current}\" is a BlackHole virtual device — install with: brew install {}",
                if current.contains("16ch") { "blackhole-16ch" } else { "blackhole-2ch" }
            )?;
        }
        if usable.is_empty() {
            writeln!(p.out, "{key} \"{current}\" not found (no usable device detected — kept)")?;
            continue;
        }
        let picked = p.pick(
            &format!(
                "{key} \"{current}\" not found on this machine. Detected {}:",
                if dir_input { "input devices" } else { "output devices" }
            ),
            &usable,
        )?;
        if let Some(new) = picked {
            if new != current {
                set_audio_str(doc, edits, key, &new);
                renames.push((current, new));
            }
        }
    }

    // Pinned capture/playback commands: fix device literals renamed above, then make sure
    // the tool itself exists (auto-fix its path, or offer to install it).
    for key in ["capture_cmd", "playback_cmd"] {
        let Some(mut argv) = audio_argv(doc, key) else { continue };
        let mut changed = false;
        for (old, new) in &renames {
            for item in argv.iter_mut().filter(|i| *i == old) {
                *item = new.clone();
                changed = true;
            }
        }
        if let Some(fixed) = ensure_tool(&argv[0], key, p)? {
            argv[0] = fixed;
            changed = true;
        }
        if changed {
            set_audio_argv(doc, edits, key, &argv);
        }
    }

    // No pinned command: dialfd auto-detects a tool at run time — make sure each unpinned
    // direction has one.
    let (capture, playback) = crate::audio::tool_detect::present_tools();
    let mut missing = Vec::new();
    if audio_argv(doc, "capture_cmd").is_none() && capture.is_empty() {
        missing.push(if cfg!(target_os = "macos") {
            "capture (sox/ffmpeg)"
        } else {
            "capture (arecord/ffmpeg/sox)"
        });
    }
    if audio_argv(doc, "playback_cmd").is_none() && playback.is_empty() {
        missing.push(if cfg!(target_os = "macos") {
            "playback (afplay/ffplay/play)"
        } else {
            "playback (aplay/ffplay/play)"
        });
    }
    if !missing.is_empty() {
        writeln!(p.out, "no audio {} tool found on this machine", missing.join(" or "))?;
        // One package covers whatever is missing: sox on macOS (capture + `play`),
        // alsa-utils on Linux (arecord + aplay).
        offer_install(p, if cfg!(target_os = "macos") { "sox" } else { "alsa-utils" })?;
    }
    Ok(())
}

/// The pinned tool at `argv[0]`: return a corrected path if the user accepts one, `None` if
/// it's fine (or left alone).
fn ensure_tool(argv0: &str, key: &str, p: &mut Prompter<'_>) -> Result<Option<String>> {
    let path = Path::new(argv0);
    let exists = if path.is_absolute() { path.exists() } else { which::which(argv0).is_ok() };
    if exists {
        writeln!(p.out, "{key} tool {argv0} ✓")?;
        return Ok(None);
    }
    let base = path.file_name().map(|b| b.to_string_lossy().into_owned());
    if let Some(found) = base.as_deref().and_then(|b| which::which(b).ok()) {
        // Same tool, different home (e.g. /opt/homebrew vs /usr/local).
        let found = found.to_string_lossy().into_owned();
        if p.yes(&format!("{key} tool {argv0} not found, but {found} exists — use it?"))? {
            return Ok(Some(found));
        }
        return Ok(None);
    }
    writeln!(p.out, "{key} tool {argv0} not found on this machine")?;
    let Some(pkg) = base.as_deref().and_then(package_for) else {
        writeln!(p.out, "no known package provides `{argv0}` — install it manually")?;
        return Ok(None);
    };
    if offer_install(p, pkg)? {
        // Re-verify after the install and say so: the pinned path itself may now exist
        // (brew lands sox exactly at /opt/homebrew/bin/sox); otherwise point argv0 at
        // wherever the tool actually landed.
        if path.is_absolute() && path.exists() {
            writeln!(p.out, "{key} tool {argv0} ✓ (installed)")?;
            return Ok(None);
        }
        if let Some(found) = base.as_deref().and_then(|b| which::which(b).ok()) {
            let found = found.to_string_lossy().into_owned();
            writeln!(p.out, "{key} tool installed at {found} ✓")?;
            return Ok(Some(found));
        }
        writeln!(p.out, "{key} tool still not found after the install — fix the config manually")?;
    }
    Ok(None)
}

/// Which package provides `tool` on this platform, for the install offer. `None` = not
/// installable via a package manager (a custom script, or a macOS builtin like afplay).
fn package_for(tool: &str) -> Option<&'static str> {
    match tool {
        "sox" | "play" | "rec" => Some("sox"),
        "ffmpeg" | "ffplay" => Some("ffmpeg"),
        "arecord" | "aplay" => (!cfg!(target_os = "macos")).then_some("alsa-utils"),
        _ => None,
    }
}

/// Offer to install `pkg` via the native package manager.
/// Returns true if an install command ran successfully.
fn offer_install(p: &mut Prompter<'_>, pkg: &str) -> Result<bool> {
    let cmd: Option<Vec<&str>> = if cfg!(target_os = "macos") {
        which::which("brew").is_ok().then_some(vec!["brew", "install", pkg])
    } else {
        [
            vec!["apt-get", "install", "-y", pkg],
            vec!["dnf", "install", "-y", pkg],
            vec!["pacman", "-S", "--noconfirm", pkg],
            vec!["zypper", "install", "-y", pkg],
        ]
        .into_iter()
        .find(|c| which::which(c[0]).is_ok())
        .map(|mut c| {
            // Package managers need root; the user running the import usually isn't.
            if unsafe { libc::getuid() } != 0 {
                c.insert(0, "sudo");
            }
            c
        })
    };
    let Some(cmd) = cmd else {
        writeln!(
            p.out,
            "{}",
            if cfg!(target_os = "macos") {
                format!("install it manually (e.g. install Homebrew, then: brew install {pkg})")
            } else {
                format!("install it manually with your distro's package manager (package: {pkg})")
            }
        )?;
        return Ok(false);
    };
    if !p.yes(&format!("install now with `{}`?", cmd.join(" ")))? {
        return Ok(false);
    }
    let status = Command::new(cmd[0])
        .args(&cmd[1..])
        .status()
        .with_context(|| format!("run {}", cmd.join(" ")))?;
    if !status.success() {
        writeln!(p.out, "install failed ({status}) — continuing; fix the config later")?;
    }
    Ok(status.success())
}

// ---------------------------------------------------------------------------
// YAML doc accessors (audio.<key>), keeping the line-edit list in sync
// ---------------------------------------------------------------------------

fn audio_str(doc: &serde_yaml::Value, key: &str) -> Option<String> {
    doc.get("audio")?.get(key)?.as_str().map(str::to_string)
}

fn set_audio_str(doc: &mut serde_yaml::Value, edits: &mut Vec<(String, String)>, key: &str, val: &str) {
    if let Some(slot) = doc.get_mut("audio").and_then(|a| a.get_mut(key)) {
        *slot = val.into();
        // Replace any earlier edit for the same key (e.g. record_dir already rewritten).
        edits.retain(|(k, _)| k != key);
        edits.push((key.to_string(), yaml_quote(val)));
    }
}

fn audio_argv(doc: &serde_yaml::Value, key: &str) -> Option<Vec<String>> {
    let seq = doc.get("audio")?.get(key)?.as_sequence()?;
    let argv: Vec<String> = seq.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    (!argv.is_empty()).then_some(argv)
}

fn set_audio_argv(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
    key: &str,
    argv: &[String],
) {
    if let Some(slot) = doc.get_mut("audio").and_then(|a| a.get_mut(key)) {
        *slot = serde_yaml::Value::Sequence(argv.iter().map(|s| s.clone().into()).collect());
        edits.retain(|(k, _)| k != key);
        edits.push((key.to_string(), yaml_flow(argv)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_profiler_devices() {
        let json = r#"{"SPAudioDataType":[{"_items":[
            {"_name":"MiniFuse 2","coreaudio_device_input":4,"coreaudio_device_output":4},
            {"_name":"MacBook Pro Microphone","coreaudio_device_input":1},
            {"_name":"MacBook Pro Speakers","coreaudio_device_output":2}
        ]}]}"#;
        let devs = parse_system_profiler(json);
        assert_eq!(devs.len(), 3);
        assert!(devs[0].input && devs[0].output);
        assert!(devs[1].input && !devs[1].output);
        assert!(!devs[2].input && devs[2].output);
    }

    #[test]
    fn parses_proc_asound_tree() {
        let root = std::env::temp_dir().join(format!("dialf-asound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("card0/pcm0p")).unwrap();
        std::fs::create_dir_all(root.join("card1/pcm0c")).unwrap();
        std::fs::create_dir_all(root.join("card1/pcm0p")).unwrap();
        std::fs::write(
            root.join("cards"),
            " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n\
             \x20                     HDA Intel PCH at 0xf7f10000 irq 31\n\
             \x201 [Device         ]: USB-Audio - USB Audio Device\n\
             \x20                     USB Audio Device at usb-0000:00:14.0-2\n",
        )
        .unwrap();
        let devs = parse_asound_dir(&root);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].value, "plughw:0,0");
        assert!(!devs[0].input && devs[0].output);
        assert_eq!(devs[1].value, "plughw:1,0");
        assert_eq!(devs[1].label, "USB Audio Device (card 1)");
        assert!(devs[1].input && devs[1].output);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn package_for_maps_tools_to_their_packages() {
        assert_eq!(package_for("sox"), Some("sox"));
        assert_eq!(package_for("play"), Some("sox"));
        assert_eq!(package_for("ffmpeg"), Some("ffmpeg"));
        assert_eq!(package_for("ffplay"), Some("ffmpeg"));
        assert_eq!(package_for("afplay"), None); // macOS builtin — not installable
        assert_eq!(package_for("my-custom-capture.sh"), None);
        if cfg!(target_os = "macos") {
            assert_eq!(package_for("arecord"), None);
        } else {
            assert_eq!(package_for("arecord"), Some("alsa-utils"));
        }
    }

    #[test]
    fn precheck_picks_devices_and_patches_pinned_cmd() {
        // Config pins BlackHole devices + a sox path that don't exist "here"; the user picks
        // detected devices — the cmd argv literals must follow the rename.
        let yaml = "audio:\n  capture_device: \"BlackHole 2ch\"\n  playback_device: \"BlackHole 16ch\"\n  capture_cmd: [\"/opt/homebrew/bin/sox\", \"-q\", \"-t\", \"coreaudio\", \"BlackHole 16ch\", \"-\"]\n";
        let mut doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let mut edits = Vec::new();
        let devices = vec![AudioDevice {
            value: "MiniFuse 2".into(),
            label: "MiniFuse 2".into(),
            input: true,
            output: true,
        }];
        // Answers: capture pick 1, playback pick 1, then decline whatever tool prompts come.
        let mut input = std::io::Cursor::new(b"1\n1\nn\nn\n".to_vec());
        let mut out = Vec::new();
        let mut p = Prompter { input: &mut input, out: &mut out };
        run_with(&mut doc, &mut edits, Path::new("/tmp"), devices, &mut p).unwrap();

        assert_eq!(doc["audio"]["capture_device"].as_str(), Some("MiniFuse 2"));
        assert_eq!(doc["audio"]["playback_device"].as_str(), Some("MiniFuse 2"));
        let argv: Vec<&str> = doc["audio"]["capture_cmd"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(argv.contains(&"MiniFuse 2"), "{argv:?}"); // "BlackHole 16ch" literal renamed
        assert!(!argv.contains(&"BlackHole 16ch"));
        assert!(edits.iter().any(|(k, _)| k == "capture_cmd"));
    }

    #[test]
    fn precheck_keeps_everything_on_enter() {
        let yaml = "audio:\n  record_dir: /x/recordings\n  capture_device: \"MiniFuse 2\"\n";
        let mut doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let orig = doc.clone();
        let mut edits = Vec::new();
        let devices = vec![AudioDevice {
            value: "MiniFuse 2".into(),
            label: "MiniFuse 2".into(),
            input: true,
            output: true,
        }];
        let mut input = std::io::Cursor::new(b"\n\n\n\n".to_vec());
        let mut out = Vec::new();
        let mut p = Prompter { input: &mut input, out: &mut out };
        run_with(&mut doc, &mut edits, Path::new("/tmp"), devices, &mut p).unwrap();
        assert_eq!(doc, orig);
        assert!(edits.is_empty());
        assert!(String::from_utf8(out).unwrap().contains("\"MiniFuse 2\" ✓"));
    }
}
