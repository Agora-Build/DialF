//! Interactive host precheck for `dialf import`: verify the imported config against THIS
//! machine — shared key, capture/playback devices, tools, and record_dir — and prompt the
//! user to fix mismatches so an imported bundle actually runs, not just installs.
//!
//! Detection uses only what the OS ships: `system_profiler` (macOS) and `/proc/asound`
//! (Linux). Prompts default to "keep as-is" on Enter, and everything here degrades to the
//! plain warnings path when stdin isn't a terminal.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::audio::diagnose::NameClash;
pub use crate::audio::tool_detect::SoxFlavor;
use crate::audio::tool_detect::SoxIdentity;
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

/// Advice when separate devices share `name` (twin cards). Split cards — one card listed as
/// same-named input and output halves — get none here: whether they work depends on which sox
/// runs them, so [`sox_check`] judges them once the tool paths are final.
fn twin_advice(name: &str, devices: &[AudioDevice]) -> Option<String> {
    (clash(name, devices) == NameClash::Duplicates)
        .then(|| crate::audio::diagnose::duplicate_device_fix(name))
}

/// How the devices called `name` relate (see [`crate::audio::diagnose::NameClash`]).
fn clash(name: &str, devices: &[AudioDevice]) -> crate::audio::diagnose::NameClash {
    let seen: Vec<(&str, bool, bool)> =
        devices.iter().map(|d| (d.value.as_str(), d.input, d.output)).collect();
    crate::audio::diagnose::name_clash(name, &seen)
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

/// Parse a `/proc/asound` tree: the `cards` file names the cards; each `card<N>/pcm<M>c`
/// dir is a capture-capable pcm device (a "port"), `pcm<M>p` playback-capable. Every port
/// gets its own entry — multi-port cards (`plughw:1,1`) must be matchable and pickable,
/// not collapsed to device 0. Config value is ALSA `plughw:N,M`.
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
        // pcm device index -> (capture, playback)
        let mut pcms: std::collections::BTreeMap<u32, (bool, bool)> = Default::default();
        if let Ok(entries) = std::fs::read_dir(root.join(format!("card{n}"))) {
            for e in entries.flatten() {
                let f = e.file_name().to_string_lossy().into_owned();
                let Some(mid) = f.strip_prefix("pcm") else { continue };
                let Ok(m) = mid[..mid.len().saturating_sub(1)].parse::<u32>() else { continue };
                let slot = pcms.entry(m).or_default();
                slot.0 |= f.ends_with('c');
                slot.1 |= f.ends_with('p');
            }
        }
        if pcms.is_empty() {
            pcms.insert(0, (false, false));
        }
        for (m, (input, output)) in pcms {
            out.push(AudioDevice {
                value: format!("plughw:{n},{m}"),
                label: format!("{name} (card {n}, device {m})"),
                input,
                output,
            });
        }
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

    check_shared_key(doc, edits, p)?;
    for key in rewrite_linux_sox_commands(doc, edits) {
        writeln!(
            p.out,
            "{key}: changed SoX audio driver from coreaudio to alsa for Linux"
        )?;
    }

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
            if let Some(advice) = twin_advice(&current, &devices) {
                writeln!(p.out, "{key} \"{current}\": {advice}")?;
            } else if clash(&current, &devices) == NameClash::None {
                writeln!(p.out, "{key} \"{current}\" ✓")?;
            }
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
            if let Some(advice) = twin_advice(&new, &devices) {
                writeln!(p.out, "{key} \"{new}\": {advice}")?;
            }
            if new != current {
                set_audio_str(doc, edits, key, &new);
                renames.push((current, new));
            }
        }
    }

    // Pinned capture/playback commands: fix device literals renamed above, then make sure
    // their audio backend fits this OS and the tool itself exists (auto-fix its path, or
    // offer to install it).
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
            "playback (sox/afplay/ffplay)"
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

    if let Some(check) = sox_check(&audio_config(doc), || devices.clone()) {
        if let Some(report) = check.report() {
            writeln!(p.out, "{report}")?;
        }
        if check.problem.is_some() {
            offer_sox_ng(doc, edits, p, &check)?;
        }
    }
    Ok(())
}

/// The doc's `audio:` section as the daemon will read it, so import judges the config the
/// same way the running daemon does (missing keys take the daemon's defaults).
pub(crate) fn audio_config(doc: &serde_yaml::Value) -> crate::config::AudioConfig {
    doc.get("audio")
        .and_then(|a| serde_yaml::from_value(a.clone()).ok())
        .unwrap_or_default()
}

/// The names that are split cards on this machine (macOS lists them as an input-only and an
/// output-only device under one name).
pub fn split_cards_among(names: &[String], devices: &[AudioDevice]) -> Vec<String> {
    names.iter().filter(|n| clash(n, devices) == NameClash::Halves).cloned().collect()
}

/// Which sox an audio config runs and whether a device it names is a split card that sox
/// can't handle. One check for every caller — `dialf import` (both modes), the daemon at
/// start (`server.info.sox`) and `dialf --version` — so they can't disagree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SoxCheck {
    pub path: String,
    pub flavor: SoxFlavor,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub split_cards: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl SoxCheck {
    /// Combine what was found. The pair is the problem — a split card with original sox;
    /// either alone is fine. Pure, so every case is testable without sox or a card.
    pub fn judge(path: String, flavor: SoxFlavor, split_cards: Vec<String>) -> Self {
        let which = match flavor {
            SoxFlavor::SoxNg => None,
            SoxFlavor::Original => Some("original sox"),
            SoxFlavor::Unknown => Some("a sox whose version couldn't be read"),
        };
        let (problem, fix) = match (which, split_cards.first()) {
            (Some(which), Some(name)) => (
                Some(format!(
                    "{which} with split card \"{name}\" — one direction will fail (\"can not \
                     get audio device properties\")"
                )),
                Some(crate::audio::diagnose::split_device_fix(name)),
            ),
            _ => (None, None),
        };
        Self { path, flavor, split_cards, problem, fix }
    }

    /// What to print: the problem and its fix, or a ✓ when sox_ng handles a split card;
    /// `None` when there is nothing worth saying.
    pub fn report(&self) -> Option<String> {
        match &self.problem {
            Some(problem) => Some(crate::audio::diagnose::with_fix(problem, self.fix.as_deref())),
            None => self.split_cards.first().map(|name| {
                format!("\"{name}\" is a split card — fine with sox_ng, which picks the half matching each direction ✓")
            }),
        }
    }
}

/// The sox binary `audio` runs, if any: a pinned command whose tool is from the SoX family,
/// else — for a direction left unpinned on macOS, where auto-detection prefers sox —
/// `auto_detected` (the sox auto-detection would pick: sox_ng first).
pub fn configured_sox(
    audio: &crate::config::AudioConfig,
    macos: bool,
    auto_detected: Option<String>,
) -> Option<String> {
    use crate::audio::tool_detect::is_sox_family;
    let is_sox = |p: &str| Path::new(p).file_name().and_then(|n| n.to_str()).is_some_and(is_sox_family);
    for cmd in [&audio.capture_cmd, &audio.playback_cmd].into_iter().flatten() {
        if let Some(tool) = cmd.first().filter(|t| is_sox(t)) {
            return Some(tool.clone());
        }
    }
    let unpinned = audio.capture_cmd.is_none() || audio.playback_cmd.is_none();
    if unpinned && macos { auto_detected } else { None }
}

/// Every CoreAudio device name `audio` can make sox open: the device keys, and the name after
/// `coreaudio` in a pinned command — normally the same, but the command's is the one sox
/// actually uses if someone edited only one of them.
pub fn configured_device_names(audio: &crate::config::AudioConfig) -> Vec<String> {
    let mut names: Vec<String> = [&audio.capture_device, &audio.playback_device]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    for cmd in [&audio.capture_cmd, &audio.playback_cmd].into_iter().flatten() {
        if let Some(i) = cmd.iter().position(|a| a == "coreaudio") {
            if let Some(name) = cmd.get(i + 1) {
                names.push(name.clone());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The sox check for `audio`; `None` when it runs no sox. `devices` is only called when there
/// is a sox and a device name to look up (scanning takes about a second on macOS).
pub fn sox_check(
    audio: &crate::config::AudioConfig,
    devices: impl FnOnce() -> Vec<AudioDevice>,
) -> Option<SoxCheck> {
    let auto = crate::audio::tool_detect::find_tool("sox").map(|p| p.to_string_lossy().into_owned());
    let path = configured_sox(audio, cfg!(target_os = "macos"), auto)?;
    let flavor = SoxIdentity::probe(Path::new(&path)).flavor;
    let names = configured_device_names(audio);
    let split = if names.is_empty() { Vec::new() } else { split_cards_among(&names, &devices()) };
    Some(SoxCheck::judge(path, flavor, split))
}

/// Offer the sox_ng fix for a split card. A source-built sox_ng (installed as `sox_ng`, next
/// to original sox) just needs the commands pointed at it; otherwise offer Homebrew's swap.
/// Either way, re-check rather than assume it worked.
fn offer_sox_ng(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
    p: &mut Prompter<'_>,
    check: &SoxCheck,
) -> Result<()> {
    let installed_ng = crate::audio::tool_detect::best_sox("sox")
        .filter(|(_, id)| id.flavor == SoxFlavor::SoxNg)
        .map(|(path, _)| path.to_string_lossy().into_owned());
    if let Some(ng) = installed_ng.filter(|ng| *ng != check.path) {
        if p.yes(&format!("sox_ng is installed at {ng} — use it instead of {}?", check.path))? {
            for key in ["capture_cmd", "playback_cmd"] {
                if let Some(mut argv) = audio_argv(doc, key).filter(|a| a.first() == Some(&check.path)) {
                    argv[0] = ng.clone();
                    set_audio_argv(doc, edits, key, &argv);
                    writeln!(p.out, "{key} tool → {ng} ✓")?;
                }
            }
        }
        return Ok(());
    }
    if !cfg!(target_os = "macos") || which::which("brew").is_err() {
        return Ok(());
    }
    if !p.yes("replace sox with sox_ng now? (`brew uninstall sox && brew install sox_ng`)")? {
        return Ok(());
    }
    for args in [&["uninstall", "sox"][..], &["install", "sox_ng"][..]] {
        let status = Command::new("brew").args(args).status().context("run brew")?;
        if !status.success() {
            writeln!(
                p.out,
                "`brew {}` failed ({status}) — sox is unchanged; the aggregate device above still works",
                args.join(" ")
            )?;
            return Ok(());
        }
    }
    match SoxIdentity::probe(Path::new(&check.path)).flavor {
        SoxFlavor::SoxNg => writeln!(p.out, "sox is now sox_ng — split cards will work as configured ✓")?,
        _ => writeln!(
            p.out,
            "sox_ng installed, but `{}` still isn't it — check which sox that path runs",
            check.path
        )?,
    }
    Ok(())
}

/// SoX names its host audio driver in the argv (`-t coreaudio` on macOS, `-t alsa` on
/// Linux). Device and executable rewrites alone are not enough when importing across OSes.
fn rewrite_sox_coreaudio_for_alsa(argv: &mut [String]) -> bool {
    let Some(tool) = argv
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if !crate::audio::tool_detect::is_sox_family(tool) {
        return false;
    }

    let mut changed = false;
    for i in 1..argv.len() {
        if argv[i] == "coreaudio" && argv.get(i - 1).is_some_and(|arg| arg == "-t") {
            argv[i] = "alsa".to_string();
            changed = true;
        }
    }
    changed
}

/// Apply the cross-platform SoX backend migration. Returns the config keys that changed for
/// interactive status or non-interactive warning output.
pub(crate) fn rewrite_linux_sox_commands(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
) -> Vec<&'static str> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }

    let mut changed = Vec::new();
    for key in ["capture_cmd", "playback_cmd"] {
        let Some(mut argv) = audio_argv(doc, key) else {
            continue;
        };
        if rewrite_sox_coreaudio_for_alsa(&mut argv) {
            set_audio_argv(doc, edits, key, &argv);
            changed.push(key);
        }
    }
    changed
}

/// Drop `argv[0]` paths that cannot work on this machine down to the bare tool name.
///
/// An imported bundle carries the tool path from the machine that built it, so a macOS
/// config landing on Linux keeps `/opt/homebrew/bin/sox` — which fails with "No such file or
/// directory" forever, even after the user installs sox. A bare name resolves through PATH
/// as soon as the tool exists. Only absolute paths that are missing *here* are touched.
///
/// Returns `(key, old argv[0], new argv[0])` for each command changed.
pub(crate) fn rewrite_unusable_tool_paths(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
) -> Vec<(&'static str, String, String)> {
    let mut changed = Vec::new();
    for key in ["capture_cmd", "playback_cmd"] {
        let Some(mut argv) = audio_argv(doc, key) else {
            continue;
        };
        let Some(argv0) = argv.first().cloned() else {
            continue;
        };
        let path = Path::new(&argv0);
        if !path.is_absolute() || path.exists() {
            continue;
        }
        let Some(base) = path.file_name().map(|b| b.to_string_lossy().into_owned()) else {
            continue;
        };
        // A tool present under a different prefix (or, for sox, sox_ng) is better named outright.
        let replacement = crate::audio::tool_detect::find_tool(&base)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(base);
        if replacement == argv0 {
            continue;
        }
        argv[0] = replacement.clone();
        set_audio_argv(doc, edits, key, &argv);
        changed.push((key, argv0, replacement));
    }
    changed
}

fn check_shared_key(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
    p: &mut Prompter<'_>,
) -> Result<()> {
    let current = doc
        .get("shared_key")
        .and_then(|v| v.as_str())
        .unwrap_or("change-me");
    if !shared_key_is_placeholder(current) {
        writeln!(p.out, "shared_key is set ✓")?;
        return Ok(());
    }

    let ans = p.ask(
        "shared_key is missing or still the insecure `change-me` placeholder. Type a new key \
         that will also be entered in the phone app, or Enter to keep it: ",
    )?;
    if ans.is_empty() || shared_key_is_placeholder(&ans) {
        writeln!(p.out, "warning: shared_key remains `change-me`")?;
    } else {
        set_top_level_str(doc, edits, "shared_key", &ans);
    }
    Ok(())
}

pub(crate) fn shared_key_is_placeholder(key: &str) -> bool {
    matches!(key.trim(), "" | "change-me")
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
    let find = |b: &str| crate::audio::tool_detect::find_tool(b);
    if let Some(found) = base.as_deref().and_then(find) {
        // Same tool, different home (/opt/homebrew vs /usr/local) — or, for sox, sox_ng.
        let found = found.to_string_lossy().into_owned();
        if p.yes(&format!("{key} tool {argv0} not found, but {found} exists — use it?"))? {
            return Ok(Some(found));
        }
        return Ok(None);
    }
    writeln!(p.out, "{key} tool {argv0} not found on this machine")?;
    let Some(pkg) = base.as_deref().and_then(package_for) else {
        writeln!(p.out, "no known package provides `{argv0}` — install it manually")?;
        return fallback_to_basename(argv0, base, key, p);
    };
    if offer_install(p, pkg)? {
        // Re-verify after the install and say so: the pinned path itself may now exist
        // (brew lands sox exactly at /opt/homebrew/bin/sox); otherwise point argv0 at
        // wherever the tool actually landed.
        if path.is_absolute() && path.exists() {
            writeln!(p.out, "{key} tool {argv0} ✓ (installed)")?;
            return Ok(None);
        }
        if let Some(found) = base.as_deref().and_then(find) {
            let found = found.to_string_lossy().into_owned();
            writeln!(p.out, "{key} tool installed at {found} ✓")?;
            return Ok(Some(found));
        }
        writeln!(p.out, "{key} tool still not found after the install — fix the config manually")?;
    }
    fallback_to_basename(argv0, base, key, p)
}

/// Drop an unusable absolute path down to the bare tool name.
///
/// A path from the machine that built the bundle (`/opt/homebrew/bin/sox` imported onto
/// Linux) cannot work here however the tool is later installed — the run fails with "No such
/// file or directory" even once sox is present. The bare name resolves through PATH the
/// moment it is. Only worth doing for an absolute path: a name is already a PATH lookup, and
/// rewriting it to itself would just print a confusing no-op.
fn fallback_to_basename(
    argv0: &str,
    base: Option<String>,
    key: &str,
    p: &mut Prompter<'_>,
) -> Result<Option<String>> {
    let Some(base) = base.filter(|b| Path::new(argv0).is_absolute() && b != argv0) else {
        return Ok(None);
    };
    writeln!(
        p.out,
        "{key} tool: using `{base}` instead of {argv0}, so it resolves via PATH once installed"
    )?;
    Ok(Some(base))
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

/// What to tell someone when no package manager we can drive is available.
///
/// Nix comes first and on either platform: a machine with nix has a working way to get the
/// tool, and nix-darwin boxes have nix without Homebrew — telling those to install Homebrew
/// would be advice for a system they deliberately don't run. Packages there are usually
/// declared rather than installed ad hoc, so both routes are named.
fn manual_install_hint(pkg: &str, has_nix: bool, is_macos: bool) -> String {
    if has_nix {
        return format!(
            "install it manually — `nix-shell -p {pkg}` for a one-off, or add `{pkg}` to your \
             Nix configuration (configuration.nix / home.packages) to keep it"
        );
    }
    if is_macos {
        return format!("install it manually (e.g. install Homebrew, then: brew install {pkg})");
    }
    // Being specific about what was looked for beats "use your distro's package manager",
    // which is unhelpful right after failing to find one.
    format!(
        "install it manually: no supported package manager found \
         (apt-get/dnf/pacman/zypper) — install `{pkg}` however this system does it"
    )
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
        let has_nix = which::which("nix").is_ok() || which::which("nix-env").is_ok();
        writeln!(p.out, "{}", manual_install_hint(pkg, has_nix, cfg!(target_os = "macos")))?;
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

fn set_top_level_str(
    doc: &mut serde_yaml::Value,
    edits: &mut Vec<(String, String)>,
    key: &str,
    val: &str,
) {
    let Some(map) = doc.as_mapping_mut() else {
        return;
    };
    map.insert(serde_yaml::Value::String(key.to_string()), val.into());
    edits.retain(|(k, _)| k != key);
    edits.push((key.to_string(), yaml_quote(val)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `ensure_tool` with scripted answers and capture what it printed.
    fn run_ensure_tool(argv0: &str, answers: &str) -> (Option<String>, String) {
        let mut input = std::io::Cursor::new(answers.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        let mut p = Prompter {
            input: &mut input,
            out: &mut out,
        };
        let got = ensure_tool(argv0, "capture_cmd", &mut p).unwrap();
        (got, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn the_install_hint_suits_the_machine() {
        // Nix present: same advice on either platform, because nix-darwin exists and those
        // machines deliberately have no Homebrew.
        for is_macos in [true, false] {
            let hint = manual_install_hint("sox", true, is_macos);
            assert!(hint.contains("nix-shell -p sox"), "{hint}");
            assert!(hint.contains("configuration.nix"), "{hint}");
            assert!(!hint.contains("Homebrew"), "nix box told to use brew: {hint}");
        }

        // macOS without nix: Homebrew is the reasonable suggestion.
        let mac = manual_install_hint("sox", false, true);
        assert!(mac.contains("brew install sox"), "{mac}");

        // Linux with nothing we can drive: say what was looked for rather than waving at
        // "your distro's package manager", which we just failed to find.
        let linux = manual_install_hint("sox", false, false);
        assert!(linux.contains("apt-get/dnf/pacman/zypper"), "{linux}");
        assert!(linux.contains("sox"), "{linux}");
        assert!(!linux.contains("nix-shell"), "{linux}");
    }

    #[test]
    fn a_foreign_tool_path_is_reduced_to_the_bare_name() {
        // Importing a macOS bundle onto Linux: /opt/homebrew/bin/sox cannot exist here, and
        // no package manager offer can make it exist. Keeping the path leaves a config that
        // fails with "No such file or directory" even after the user installs sox; the bare
        // name resolves through PATH the moment it is there.
        let missing = "/opt/homebrew/bin/definitely-not-a-real-tool-xyz";
        // "n" declines any install offer that may be made.
        let (fixed, printed) = run_ensure_tool(missing, "n\nn\nn\n");
        assert_eq!(fixed.as_deref(), Some("definitely-not-a-real-tool-xyz"));
        assert!(
            printed.contains("resolves via PATH"),
            "should explain the rewrite, got: {printed}"
        );
    }

    #[test]
    fn a_tool_that_is_present_is_left_alone() {
        // Nothing to fix: no rewrite, no noise.
        let (fixed, printed) = run_ensure_tool("/bin/sh", "");
        assert_eq!(fixed, None);
        assert!(printed.contains("✓"), "got: {printed}");
    }

    #[test]
    fn a_bare_name_is_not_rewritten_to_itself() {
        // Already a PATH lookup; there is nothing better to offer, so leave it be rather
        // than emit a confusing "using `x` instead of x".
        let (fixed, _) = run_ensure_tool("definitely-not-a-real-tool-xyz", "n\nn\nn\n");
        assert_eq!(fixed, None);
    }

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
    fn parses_proc_asound_tree_with_ports() {
        let root = std::env::temp_dir().join(format!("dialf-asound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("card0/pcm0p")).unwrap();
        std::fs::create_dir_all(root.join("card1/pcm0c")).unwrap();
        std::fs::create_dir_all(root.join("card1/pcm0p")).unwrap();
        // Multi-port card: device 1 is a second, capture-only port.
        std::fs::create_dir_all(root.join("card1/pcm1c")).unwrap();
        std::fs::write(
            root.join("cards"),
            " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n\
             \x20                     HDA Intel PCH at 0xf7f10000 irq 31\n\
             \x201 [Device         ]: USB-Audio - USB Audio Device\n\
             \x20                     USB Audio Device at usb-0000:00:14.0-2\n",
        )
        .unwrap();
        let devs = parse_asound_dir(&root);
        assert_eq!(devs.len(), 3);
        assert_eq!(devs[0].value, "plughw:0,0");
        assert!(!devs[0].input && devs[0].output);
        assert_eq!(devs[1].value, "plughw:1,0");
        assert_eq!(devs[1].label, "USB Audio Device (card 1, device 0)");
        assert!(devs[1].input && devs[1].output);
        // The second port is its own matchable/pickable entry.
        assert_eq!(devs[2].value, "plughw:1,1");
        assert!(devs[2].input && !devs[2].output);
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
    fn rewrites_only_sox_coreaudio_driver_tokens_for_alsa() {
        let mut argv = vec![
            "/opt/homebrew/bin/sox",
            "-q",
            "-t",
            "coreaudio",
            "BlackHole 16ch",
            "-t",
            "raw",
            "-",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert!(rewrite_sox_coreaudio_for_alsa(&mut argv));
        assert_eq!(argv[3], "alsa");
        assert!(!rewrite_sox_coreaudio_for_alsa(&mut argv));

        let mut unrelated = vec!["ffmpeg".into(), "-t".into(), "coreaudio".into()];
        assert!(!rewrite_sox_coreaudio_for_alsa(&mut unrelated));
        assert_eq!(unrelated[2], "coreaudio");
    }

    #[test]
    fn precheck_replaces_placeholder_shared_key() {
        let mut doc: serde_yaml::Value = serde_yaml::from_str("shared_key: change-me\n").unwrap();
        let mut edits = Vec::new();
        let mut input = std::io::Cursor::new(b"latency-eval-key\n".to_vec());
        let mut out = Vec::new();
        let mut p = Prompter {
            input: &mut input,
            out: &mut out,
        };

        check_shared_key(&mut doc, &mut edits, &mut p).unwrap();

        assert_eq!(doc["shared_key"].as_str(), Some("latency-eval-key"));
        assert_eq!(
            edits,
            vec![("shared_key".into(), "\"latency-eval-key\"".into())]
        );
    }

    #[test]
    fn noninteractive_linux_rewrite_updates_document_and_edits() {
        let yaml = "audio:\n  capture_cmd: [\"sox\", \"-t\", \"coreaudio\", \"BlackHole\", \"-\"]\n  playback_cmd: [\"sox\", \"x.wav\", \"-t\", \"coreaudio\", \"BlackHole\"]\n";
        let mut doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let mut edits = Vec::new();

        let changed = rewrite_linux_sox_commands(&mut doc, &mut edits);

        if cfg!(target_os = "linux") {
            assert_eq!(changed, vec!["capture_cmd", "playback_cmd"]);
            assert_eq!(doc["audio"]["capture_cmd"][2].as_str(), Some("alsa"));
            assert_eq!(doc["audio"]["playback_cmd"][3].as_str(), Some("alsa"));
            assert_eq!(edits.len(), 2);
        } else {
            assert!(changed.is_empty());
            assert!(edits.is_empty());
        }
    }

    #[test]
    fn precheck_picks_devices_and_patches_pinned_cmd() {
        // Config pins BlackHole devices + a sox path that don't exist "here"; the user picks
        // detected devices — the cmd argv literals must follow the rename.
        let yaml = "shared_key: test-key\naudio:\n  capture_device: \"BlackHole 2ch\"\n  playback_device: \"BlackHole 16ch\"\n  capture_cmd: [\"/opt/homebrew/bin/sox\", \"-q\", \"-t\", \"coreaudio\", \"BlackHole 16ch\", \"-\"]\n";
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
        if cfg!(target_os = "linux") {
            assert!(argv.contains(&"alsa"), "{argv:?}");
            assert!(!argv.contains(&"coreaudio"));
        }
        assert!(edits.iter().any(|(k, _)| k == "capture_cmd"));
    }

    #[test]
    fn precheck_keeps_everything_on_enter() {
        let yaml = "shared_key: test-key\naudio:\n  record_dir: /x/recordings\n  capture_device: \"MiniFuse 2\"\n";
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

    /// A stand-in `sox` that answers `--version` like the given flavor, so import can be
    /// tested against both without installing either.
    fn fake_sox(tag: &str, version_line: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dialf-fakesox-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("sox");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// GitHub issue #2, replayed: a BlackHole bundle lands on a Mac whose TI codec is split
    /// into same-named halves; the user picks it from the list for both directions.
    fn import_split_card(sox_version: &str, tag: &str) -> String {
        let sox = fake_sox(tag, sox_version);
        let yaml = format!(
            "shared_key: test-key\naudio:\n  capture_device: \"BlackHole 2ch\"\n  playback_device: \"BlackHole 16ch\"\n  capture_cmd: [\"{s}\", \"-q\", \"-t\", \"coreaudio\", \"BlackHole 2ch\", \"-\"]\n  playback_cmd: [\"{s}\", \"-q\", \"{{file}}\", \"-t\", \"coreaudio\", \"BlackHole 16ch\"]\n",
            s = sox.display()
        );
        let mut doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let mut edits = Vec::new();
        let codec = "USB AUDIO  CODEC";
        let dev = |input, output| AudioDevice { value: codec.into(), label: codec.into(), input, output };
        // Picks 1 and 1; every later question (the sox_ng offer, if this host has brew) gets
        // "n" — a test must never run brew.
        let mut input = std::io::Cursor::new(b"1\n1\nn\nn\nn\nn\n".to_vec());
        let mut out = Vec::new();
        let mut p = Prompter { input: &mut input, out: &mut out };
        run_with(&mut doc, &mut edits, Path::new("/tmp"), vec![dev(false, true), dev(true, false)], &mut p).unwrap();
        let _ = std::fs::remove_dir_all(sox.parent().unwrap());
        assert_eq!(doc["audio"]["capture_device"].as_str(), Some(codec), "the pick itself still applies");
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn original_sox_with_a_split_card_gets_the_fix_once() {
        let said = import_split_card("sox:      SoX v14.4.2", "orig");
        assert!(said.contains("original sox with split card \"USB AUDIO  CODEC\""), "{said}");
        assert!(said.contains("↳ ") && said.contains("brew install sox_ng") && said.contains("aggregate"), "{said}");
        assert_eq!(said.matches("input-only and an output-only half").count(), 1, "one card, one message:\n{said}");
    }

    #[test]
    fn sox_ng_with_a_split_card_is_fine() {
        let said = import_split_card("sox:      SoX_ng v14.8.1", "ng");
        assert!(said.contains("\"USB AUDIO  CODEC\" is a split card — fine with sox_ng"), "{said}");
        assert!(!said.contains("brew install sox_ng"), "no nagging once sox_ng is in place:\n{said}");
        assert!(!said.contains("replace sox with sox_ng now?"), "{said}");
    }


    /// Two identical full-duplex cards: no halves to combine, so different advice, given at
    /// pick time and independent of the sox flavor.
    #[test]
    fn twin_cards_get_pick_one_advice() {
        let yaml = "shared_key: test-key\naudio:\n  capture_device: \"MiniFuse 2\"\n";
        let mut doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let mut edits = Vec::new();
        let d = || AudioDevice { value: "MiniFuse 2".into(), label: "MiniFuse 2".into(), input: true, output: true };
        let mut input = std::io::Cursor::new(b"n\nn\nn\n".to_vec());
        let mut out = Vec::new();
        let mut p = Prompter { input: &mut input, out: &mut out };
        run_with(&mut doc, &mut edits, Path::new("/tmp"), vec![d(), d()], &mut p).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("more than one device is named \"MiniFuse 2\""), "{said}");
        assert!(!said.contains("half"), "{said}");
    }

    /// The pair is the problem: a split card with original sox. Either alone is fine.
    #[test]
    fn the_check_flags_only_original_sox_with_a_split_card() {
        use SoxFlavor::*;
        let split = || vec!["USB AUDIO  CODEC".to_string()];
        let bad = SoxCheck::judge("/usr/local/bin/sox".into(), Original, split());
        assert!(bad.problem.as_deref().unwrap().starts_with("original sox with split card \"USB AUDIO  CODEC\""));
        assert!(bad.report().unwrap().contains("\n  ↳ ") && bad.fix.as_deref().unwrap().contains("brew install sox_ng"));
        assert!(SoxCheck::judge("sox".into(), Unknown, split()).problem.is_some(), "can't tell: still worth saying");
        let ng = SoxCheck::judge("sox".into(), SoxNg, split());
        assert_eq!(ng.problem, None);
        assert!(ng.report().unwrap().ends_with("✓"));
        assert_eq!(SoxCheck::judge("sox".into(), Original, vec![]).report(), None, "nothing to say");
    }

    /// What the daemon serves is what the CLI decodes — same struct, same wording.
    #[test]
    fn the_check_survives_the_trip_through_server_info() {
        let c = SoxCheck::judge("/usr/local/bin/sox".into(), SoxFlavor::Original, vec!["X".into()]);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["flavor"], "original");
        let back: SoxCheck = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
        let quiet = serde_json::to_value(SoxCheck::judge("sox".into(), SoxFlavor::SoxNg, vec![])).unwrap();
        assert_eq!(quiet, serde_json::json!({"path": "sox", "flavor": "sox_ng"}));
    }

    #[test]
    fn the_sox_in_use_is_found_pinned_or_auto() {
        let mut a = crate::config::AudioConfig::default();
        let auto = |p: &str| Some(p.to_string());
        // Unpinned on macOS: whatever detection picks — sox_ng first (find_tool's job).
        assert_eq!(configured_sox(&a, true, auto("/usr/local/bin/sox_ng")).as_deref(), Some("/usr/local/bin/sox_ng"));
        assert_eq!(configured_sox(&a, true, None), None);
        assert_eq!(configured_sox(&a, false, auto("sox")), None, "Linux auto-detects arecord first");
        // A pinned SoX-family tool wins, original or sox_ng, path and all.
        for tool in ["/usr/local/bin/sox", "/usr/local/bin/sox_ng", "play_ng"] {
            a.capture_cmd = Some(vec![tool.into(), "-q".into()]);
            a.playback_cmd = Some(vec![tool.into()]);
            assert_eq!(configured_sox(&a, true, None).as_deref(), Some(tool));
        }
        a.capture_cmd = Some(vec!["ffmpeg".into()]);
        a.playback_cmd = Some(vec!["afplay".into()]);
        assert_eq!(configured_sox(&a, true, auto("sox")), None, "no sox involved");
    }

    #[test]
    fn device_names_come_from_keys_and_pinned_commands() {
        let mut a = crate::config::AudioConfig::default();
        a.capture_device = Some("USB AUDIO  CODEC".into());
        a.capture_cmd = Some(["sox", "-q", "-t", "coreaudio", "Edited Name", "-"].map(String::from).to_vec());
        a.playback_cmd = Some(["sox", "{file}", "-t", "coreaudio", "USB AUDIO  CODEC"].map(String::from).to_vec());
        assert_eq!(configured_device_names(&a), vec!["Edited Name", "USB AUDIO  CODEC"]);
    }

    /// Import reads its YAML through the daemon's own AudioConfig, so both judge one config.
    #[test]
    fn import_reads_the_audio_section_like_the_daemon() {
        let doc: serde_yaml::Value = serde_yaml::from_str(
            "audio:\n  capture_device: \"X\"\n  capture_cmd: [\"/usr/local/bin/sox\", \"-q\"]\n",
        )
        .unwrap();
        let a = audio_config(&doc);
        assert_eq!(a.capture_device.as_deref(), Some("X"));
        assert_eq!(a.capture_cmd.unwrap()[0], "/usr/local/bin/sox");
        assert!(audio_config(&serde_yaml::from_str("{}").unwrap()).capture_cmd.is_none());
    }
}
