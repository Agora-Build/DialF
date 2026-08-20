//! Export / import a DialF setup as a portable bundle.
//!
//! A "setup" is a project folder of job scripts + audio samples (plus a recordings output
//! dir), and the daemon config (`~/.config/dialf/config.yaml`). `export` zips the folder's
//! scripts and samples together with the config — recordings excluded — rewriting the
//! config's paths to be bundle-relative so the zip is self-contained. `import` takes an
//! bundle folder — or a bundle `.zip`, extracted flat into the current folder first —
//! installs its `config.yaml` to the default config path with paths rewritten to absolute
//! paths under that folder, and leaves the folder in place as the live content location.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::daemon::resolve_path_under;
use crate::jobs::schema::{self, Step, StepKind};

/// Directory names always excluded from an export, on top of the config's `record_dir`.
/// Recordings are machine outputs, not setup — a bundle should carry inputs only.
const RECORDING_DIR_NAMES: &[&str] = &["recordings"];

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// What `export` produced, for display: zip path + its entries (name, uncompressed size).
pub struct ExportReport {
    pub zip_path: PathBuf,
    pub entries: Vec<(String, u64)>,
    pub warnings: Vec<String>,
}

/// Bundle `dir` (scripts + samples) and the config into a zip at `out`.
///
/// Config resolution: `config` argument > `<dir>/config.yaml` > [`Config::default_path`].
/// The chosen file must exist — exporting a made-up default config would be misleading.
pub fn export(dir: &Path, out: Option<PathBuf>, config: Option<PathBuf>) -> Result<ExportReport> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("export folder not found: {}", dir.display()))?;
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }

    let config_path = match config {
        Some(p) => p,
        None => {
            let local = dir.join("config.yaml");
            if local.is_file() {
                local
            } else {
                Config::default_path()
            }
        }
    };
    if !config_path.is_file() {
        bail!(
            "config not found: {} — create one, or pass --config",
            config_path.display()
        );
    }
    let config_path = config_path.canonicalize()?;
    let config_text = std::fs::read_to_string(&config_path)?;
    let cfg: Config = serde_yaml::from_str(&config_text)
        .with_context(|| format!("parse config {}", config_path.display()))?;
    let config_dir = config_path.parent().map(Path::to_path_buf);

    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "dialf".to_string());
    let zip_path = match out {
        Some(p) => p,
        None => PathBuf::from(format!("{dir_name}.dialf.zip")),
    };

    let mut warnings = Vec::new();

    // Exclusions: the config's record_dir (resolved against the config file's dir, like the
    // daemon does), conventional recording folder names, and the output zip itself.
    let mut excluded_dirs: Vec<PathBuf> = Vec::new();
    if let Some(rd) = cfg.audio.record_dir.as_deref() {
        let resolved = resolve_path_under(config_dir.as_deref(), rd);
        if let Ok(c) = resolved.canonicalize() {
            excluded_dirs.push(c);
        }
    }
    for name in RECORDING_DIR_NAMES {
        if let Ok(c) = dir.join(name).canonicalize() {
            if !excluded_dirs.contains(&c) {
                excluded_dirs.push(c);
            }
        }
    }
    // Canonicalize the zip's own path (including the bare "-o out.zip" / default-name case,
    // whose parent is the cwd) so walk() reliably skips a stale zip inside the export dir —
    // otherwise a re-export would copy the old zip into the new one while overwriting it.
    let zip_parent_dir = zip_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let zip_abs = zip_parent_dir
        .canonicalize()
        .unwrap_or_else(|_| zip_parent_dir.to_path_buf())
        .join(zip_path.file_name().unwrap_or_default());

    // Walk the folder. Everything not excluded ships; scripts get their audio refs checked.
    let mut files = Vec::new();
    walk(&dir, &excluded_dirs, &zip_abs, &mut files)?;
    if let Some(local_cfg) = files.iter().position(|f| *f == dir.join("config.yaml")) {
        // The bundled config is generated below; don't also ship the folder's own copy.
        files.remove(local_cfg);
    }

    let mut entries: BTreeMap<String, Src> = BTreeMap::new();
    // Out-of-tree samples pulled into the bundle: original absolute path -> zip name.
    let mut pulled: BTreeMap<PathBuf, String> = BTreeMap::new();

    // Reserve every in-tree file's zip name first, so a later pull_external can never pick
    // (and be clobbered by / clobber) a name an in-tree file already owns.
    for f in &files {
        entries.insert(zip_name(f.strip_prefix(&dir).unwrap()), Src::File(f.clone()));
    }
    for f in &files {
        if !is_yaml(f) {
            continue;
        }
        let name = zip_name(f.strip_prefix(&dir).unwrap());
        if let Some(job) =
            normalize_script(f, &name, &dir, &excluded_dirs, &mut entries, &mut pulled, &mut warnings)?
        {
            warnings.push(format!(
                "{}: rewritten for the bundle (comments dropped)",
                f.display()
            ));
            entries.insert(name, Src::Bytes(serde_yaml::to_string(&job)?.into_bytes()));
        }
    }

    // Config: rewrite autoanswer job paths bundle-relative (pulling out-of-tree jobs in),
    // and point record_dir at a bundle-relative `recordings`. Edited as a YAML value tree,
    // not re-serialized from the typed struct, so the bundled config keeps exactly the
    // fields the user wrote (no injected defaults) in their original order — close enough
    // to the original to drop straight into ~/.config/dialf/ and run.
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&config_text)?;
    let mut edits: Vec<(String, String)> = Vec::new();
    if let Some(map) = doc.get_mut("autoanswer").and_then(|v| v.as_mapping_mut()) {
        for (k, v) in map.iter_mut() {
            let number = k.as_str().unwrap_or("?").to_string();
            let Some(path) = v.as_str().map(str::to_string) else {
                continue; // null = answer-only
            };
            let resolved = resolve_path_under(config_dir.as_deref(), Path::new(&path));
            let Ok(target) = resolved.canonicalize() else {
                warnings.push(format!(
                    "config autoanswer {number}: job not found: {path} (kept as-is)"
                ));
                continue;
            };
            let new = if target.starts_with(&dir) {
                zip_name(target.strip_prefix(&dir).unwrap())
            } else {
                let name = pull_external(&target, "scripts", &mut entries, &mut pulled);
                // The pulled job's own audio refs must work from its new scripts/ home.
                if let Some(job_steps) =
                    normalize_out_of_tree_job(&target, &name, &dir, &mut entries, &mut pulled)?
                {
                    entries.insert(name.clone(), Src::Bytes(job_steps.into_bytes()));
                }
                warnings.push(format!(
                    "config autoanswer {number}: pulled out-of-tree job {} into the bundle as {name}",
                    target.display()
                ));
                name
            };
            if new != path {
                *v = new.clone().into();
                edits.push((number, yaml_quote(&new)));
            }
        }
    }
    if let Some(rd) = doc.get_mut("audio").and_then(|a| a.get_mut("record_dir")) {
        if rd.as_str().is_some_and(|s| s != "recordings") {
            *rd = "recordings".into();
            edits.push(("record_dir".to_string(), yaml_quote("recordings")));
        }
    }
    entries.insert(
        "config.yaml".to_string(),
        Src::Bytes(render_config(&config_text, &edits, &doc)?.into_bytes()),
    );

    // Write the zip.
    let out_file = std::fs::File::create(&zip_path)
        .with_context(|| format!("create {}", zip_path.display()))?;
    let mut zw = zip::ZipWriter::new(out_file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, src) in &entries {
        zw.start_file(name, opts)?;
        match src {
            Src::File(p) => {
                let mut f = std::fs::File::open(p)
                    .with_context(|| format!("read {}", p.display()))?;
                std::io::copy(&mut f, &mut zw)?;
            }
            Src::Bytes(b) => zw.write_all(b)?,
        }
    }
    zw.finish()?;

    // Re-open and list what actually landed in the zip.
    let mut listing = Vec::new();
    let mut za = zip::ZipArchive::new(std::fs::File::open(&zip_path)?)?;
    for i in 0..za.len() {
        let e = za.by_index(i)?;
        listing.push((e.name().to_string(), e.size()));
    }
    listing.sort();

    Ok(ExportReport {
        zip_path,
        entries: listing,
        warnings,
    })
}

/// A zip entry's content source: a file on disk, or bytes generated during export.
enum Src {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// Render a rewritten config with maximum fidelity to the user's original file: apply each
/// `(key, new_value)` edit to its single `key: value` line in the original text — keeping
/// every other line byte-identical (comments, quoting, flow arrays) — and verify the result
/// parses to exactly `expected`. Any ambiguity (key not found once, multi-line value,
/// parse mismatch) falls back to a clean re-dump of `expected`.
fn render_config(
    original: &str,
    edits: &[(String, String)],
    expected: &serde_yaml::Value,
) -> Result<String> {
    if let Some(text) = apply_line_edits(original, edits, expected) {
        return Ok(text);
    }
    Ok(serde_yaml::to_string(expected)?)
}

fn apply_line_edits(
    original: &str,
    edits: &[(String, String)],
    expected: &serde_yaml::Value,
) -> Option<String> {
    let mut lines: Vec<String> = original.lines().map(String::from).collect();
    for (key, new_value) in edits {
        let hits: Vec<usize> = (0..lines.len())
            .filter(|&i| line_value_span(&lines[i], key).is_some())
            .collect();
        let [i] = hits[..] else { return None };
        let prefix_len = line_value_span(&lines[i], key)?;
        lines[i] = format!("{}{}", &lines[i][..prefix_len], new_value);
    }
    let text = lines.join("\n") + "\n";
    (serde_yaml::from_str::<serde_yaml::Value>(&text).ok()? == *expected).then_some(text)
}

/// A string as a double-quoted YAML scalar (edit values are pre-rendered YAML).
pub(crate) fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An argv as a single-line YAML flow sequence, every element quoted.
pub(crate) fn yaml_flow(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|i| yaml_quote(i)).collect();
    format!("[{}]", inner.join(", "))
}

/// If `line` is `<indent><key>: <scalar>` (key optionally quoted), return the byte length of
/// the prefix up to and including the colon+space — i.e. where the value starts.
fn line_value_span(line: &str, key: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    let after_key = rest
        .strip_prefix(&format!("\"{key}\""))
        .or_else(|| rest.strip_prefix(&format!("'{key}'")))
        .or_else(|| rest.strip_prefix(key))?;
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let value = after_colon.trim_start();
    if value.is_empty() || value.starts_with('#') {
        return None; // no inline scalar on this line (block value / answer-only null)
    }
    Some(line.len() - value.len())
}

/// Add an out-of-tree file to the bundle under `<under>/<filename>` (deduplicated; name
/// collisions get a numeric prefix). Returns its zip name.
fn pull_external(
    src: &Path,
    under: &str,
    entries: &mut BTreeMap<String, Src>,
    pulled: &mut BTreeMap<PathBuf, String>,
) -> String {
    if let Some(name) = pulled.get(src) {
        return name.clone();
    }
    let base = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let mut name = format!("{under}/{base}");
    let mut n = 1;
    while entries.contains_key(&name) {
        name = format!("{under}/{n}-{base}");
        n += 1;
    }
    entries.insert(name.clone(), Src::File(src.to_path_buf()));
    pulled.insert(src.to_path_buf(), name.clone());
    name
}

/// Normalize an in-tree script's audio refs for the bundle. Returns rewritten steps only
/// when a rewrite is required (absolute or out-of-tree refs — those get pulled into
/// `samples/`); self-contained scripts return `None` and ship byte-identical, preserving
/// their comments. Non-job YAML also returns `None` (shipped as plain content).
fn normalize_script(
    script_disk: &Path,
    script_zip: &str,
    dir: &Path,
    excluded_dirs: &[PathBuf],
    entries: &mut BTreeMap<String, Src>,
    pulled: &mut BTreeMap<PathBuf, String>,
    warnings: &mut Vec<String>,
) -> Result<Option<Vec<Step>>> {
    let text = std::fs::read_to_string(script_disk)?;
    let mut job = match schema::parse(&text) {
        Ok(j) => j,
        Err(_) => return Ok(None),
    };
    let script_dir = script_disk.parent().unwrap_or(Path::new(""));
    let zip_parent = parent_of_zip_name(script_zip);
    let mut rewritten = false;
    for step in &mut job {
        let StepKind::AudioPlay { file } = &mut step.kind else {
            continue;
        };
        let resolved = resolve_path_under(Some(script_dir), Path::new(&*file));
        let Ok(target) = resolved.canonicalize() else {
            warnings.push(format!(
                "{}: audio.play file not found: {file} (kept as-is)",
                script_disk.display()
            ));
            continue;
        };
        // An in-tree ref can still point at content the bundle won't carry (the excluded
        // recordings dir, or a hidden path the walk skips) — that must not pass silently.
        let bundled = target.starts_with(dir)
            && !excluded_dirs.iter().any(|x| target.starts_with(x))
            && !target
                .strip_prefix(dir)
                .unwrap()
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
        if target.starts_with(dir) && !bundled {
            warnings.push(format!(
                "{}: audio.play file {file} is under an excluded/hidden path — NOT in the bundle",
                script_disk.display()
            ));
            continue;
        }
        if Path::new(&*file).is_relative() && target.starts_with(dir) {
            continue; // same relative relationship holds inside the zip
        }
        let zip_target = if target.starts_with(dir) {
            // absolute ref into the folder -> its in-zip location
            zip_name(target.strip_prefix(dir).unwrap())
        } else {
            warnings.push(format!(
                "{}: pulled out-of-tree sample {} into the bundle",
                script_disk.display(),
                target.display()
            ));
            pull_external(&target, "samples", entries, pulled)
        };
        *file = relative_zip_path(&zip_parent, &zip_target);
        rewritten = true;
    }
    Ok(if rewritten { Some(job) } else { None })
}

/// Rewrite an out-of-tree autoanswer job's audio refs for its new `scripts/` location in the
/// bundle. In-tree refs become `../<relpath>`; out-of-tree samples are pulled into `samples/`.
/// Returns the rewritten YAML, or `None` if nothing needed rewriting (ship it verbatim).
fn normalize_out_of_tree_job(
    job_disk: &Path,
    job_zip: &str,
    dir: &Path,
    entries: &mut BTreeMap<String, Src>,
    pulled: &mut BTreeMap<PathBuf, String>,
) -> Result<Option<String>> {
    let text = std::fs::read_to_string(job_disk)?;
    let mut job = schema::parse(&text)
        .with_context(|| format!("parse autoanswer job {}", job_disk.display()))?;
    let job_dir = job_disk.parent().unwrap_or(Path::new(""));
    let zip_parent = parent_of_zip_name(job_zip);
    let mut rewritten = false;
    for step in &mut job {
        let StepKind::AudioPlay { file } = &mut step.kind else {
            continue;
        };
        let resolved = resolve_path_under(Some(job_dir), Path::new(&*file));
        let Ok(target) = resolved.canonicalize() else {
            continue; // missing ref: leave the path as-is
        };
        let zip_target = if target.starts_with(dir) {
            zip_name(target.strip_prefix(dir).unwrap())
        } else {
            pull_external(&target, "samples", entries, pulled)
        };
        *file = relative_zip_path(&zip_parent, &zip_target);
        rewritten = true;
    }
    Ok(if rewritten {
        Some(serde_yaml::to_string(&job)?)
    } else {
        None
    })
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// What `import` did, for display + the follow-up daemon restart.
#[derive(Debug)]
pub struct ImportReport {
    /// Where config.yaml was installed (the default config path).
    pub config_path: PathBuf,
    /// Backup of the previous config, when one was replaced.
    pub backup_path: Option<PathBuf>,
    /// The canonicalized bundle folder the config now points at.
    pub folder: PathBuf,
    pub scripts: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Import an extracted bundle `folder`: validate it, then install its config.yaml to the
/// default config path with relative paths rewritten absolute under `folder`. On a terminal,
/// runs the interactive host precheck (devices / capture tool / record_dir).
pub fn import(folder: &Path, override_existing: bool) -> Result<ImportReport> {
    let interactive = std::io::stdin().is_terminal();
    import_impl(folder, &Config::default_path(), override_existing, interactive)
}

/// [`import`] with an explicit destination config path and no prompts (separated for tests).
pub fn import_to(folder: &Path, dest: &Path, override_existing: bool) -> Result<ImportReport> {
    import_impl(folder, dest, override_existing, false)
}

fn import_impl(
    folder: &Path,
    dest: &Path,
    override_existing: bool,
    interactive: bool,
) -> Result<ImportReport> {
    // A .zip is extracted FLAT into the current folder (config.yaml's parent prefix inside
    // the zip, if any, is stripped) — the current folder then IS the bundle folder.
    if folder.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
        let workspace = std::env::current_dir()?;
        let n = extract_bundle_zip(folder, &workspace)?;
        eprintln!("extracted {n} file(s) from {} into {}", folder.display(), workspace.display());
        return import_impl(&workspace, dest, override_existing, interactive);
    }
    let folder = folder
        .canonicalize()
        .with_context(|| format!("bundle folder not found: {}", folder.display()))?;
    if !folder.is_dir() {
        bail!("not a directory: {}", folder.display());
    }

    // Validate the bundle shape: a config.yaml plus at least one job script.
    let cfg_file = folder.join("config.yaml");
    if !cfg_file.is_file() {
        bail!(
            "not a dialf bundle: {} has no config.yaml",
            folder.display()
        );
    }
    let cfg_text = std::fs::read_to_string(&cfg_file)?;
    let cfg: Config = serde_yaml::from_str(&cfg_text)
        .with_context(|| format!("parse {}", cfg_file.display()))?;

    let mut files = Vec::new();
    walk(&folder, &[], Path::new(""), &mut files)?;
    let mut scripts = Vec::new();
    let mut warnings = Vec::new();
    for f in &files {
        if !is_yaml(f) || *f == cfg_file {
            continue;
        }
        let Ok(job) = schema::parse(&std::fs::read_to_string(f)?) else {
            continue;
        };
        // A script's samples must be present for the setup to actually run.
        let script_dir = f.parent().unwrap_or(Path::new(""));
        for step in &job {
            if let StepKind::AudioPlay { file } = &step.kind {
                let p = resolve_path_under(Some(script_dir), Path::new(file));
                if !p.is_file() {
                    warnings.push(format!(
                        "{}: audio.play file missing: {}",
                        f.display(),
                        p.display()
                    ));
                }
            }
        }
        scripts.push(f.clone());
    }
    if scripts.is_empty() {
        bail!(
            "not a dialf bundle: {} has no job script (no YAML parsed as a job)",
            folder.display()
        );
    }

    // Rewrite the config's relative paths to absolute paths under the bundle folder: the
    // config is installed to the default path but the content stays in the folder. Edited
    // as a YAML value tree so the installed file keeps exactly the bundle's fields (no
    // injected defaults) in their original order.
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&cfg_text)?;
    let mut edits: Vec<(String, String)> = Vec::new();
    if let Some(map) = doc.get_mut("autoanswer").and_then(|v| v.as_mapping_mut()) {
        for (k, v) in map.iter_mut() {
            let Some(path) = v.as_str() else {
                continue; // null = answer-only
            };
            let abs = resolve_path_under(Some(&folder), Path::new(path));
            if !abs.is_file() {
                warnings.push(format!(
                    "config autoanswer {}: job not found at {}",
                    k.as_str().unwrap_or("?"),
                    abs.display()
                ));
            }
            let new = abs.to_string_lossy().into_owned();
            if new != path {
                let number = k.as_str().unwrap_or("?").to_string();
                *v = new.clone().into();
                edits.push((number, yaml_quote(&new)));
            }
        }
    }
    if let Some(rd) = doc.get_mut("audio").and_then(|a| a.get_mut("record_dir")) {
        if let Some(dir_str) = rd.as_str() {
            let new = resolve_path_under(Some(&folder), Path::new(dir_str))
                .to_string_lossy()
                .into_owned();
            if new != dir_str {
                *rd = new.clone().into();
                edits.push(("record_dir".to_string(), yaml_quote(&new)));
            }
        }
    }

    // Host precheck: interactively verify/fix devices, the capture tool, and record_dir
    // against THIS machine — or, without a terminal, fall back to plain warnings.
    if interactive {
        crate::hostcheck::run(&mut doc, &mut edits, &folder)?;
    } else {
        for (label, cmd) in [
            ("capture_cmd", cfg.audio.capture_cmd.as_ref()),
            ("playback_cmd", cfg.audio.playback_cmd.as_ref()),
        ] {
            let Some(argv0) = cmd.and_then(|c| c.first()) else {
                continue;
            };
            let found = if Path::new(argv0).is_absolute() {
                Path::new(argv0).exists()
            } else {
                which::which(argv0).is_ok()
            };
            if !found {
                warnings.push(format!(
                    "audio.{label} tool not found on this machine: {argv0} — install it or edit the config"
                ));
            }
        }
        for (label, dev) in [
            ("capture_device", cfg.audio.capture_device.as_deref()),
            ("playback_device", cfg.audio.playback_device.as_deref()),
        ] {
            if let Some(d) = dev {
                warnings.push(format!(
                    "audio.{label} is \"{d}\" — verify that device exists on this machine"
                ));
            }
        }
    }
    // These can stop the daemon from starting at all on a machine with a different setup.
    if cfg.control_socket.is_some()
        || cfg.control_socket_group.is_some()
        || cfg.control_socket_mode.is_some()
    {
        warnings.push(
            "config pins control_socket settings from the exporting machine \
             (control_socket/control_socket_group/control_socket_mode) — remove them unless \
             this machine uses the same shared-socket setup"
                .to_string(),
        );
    }
    if cfg.ws_bind != crate::config::DEFAULT_WS_BIND {
        warnings.push(format!(
            "config ws_bind is \"{}\" — verify it's bindable on this machine",
            cfg.ws_bind
        ));
    }

    // Install, confirming before replacing an existing config.
    let mut backup_path = None;
    if dest.exists() {
        if !override_existing {
            confirm_override(dest)?;
        }
        // Timestamped so backups sort by age and are never overwritten — a second import
        // must not destroy the backup of the user's original config. The numeric suffix
        // only disambiguates two imports within the same second.
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut bak = dest.with_extension(format!("yaml.bak.{stamp}"));
        let mut n = 1;
        while bak.exists() {
            bak = dest.with_extension(format!("yaml.bak.{stamp}-{n}"));
            n += 1;
        }
        std::fs::copy(dest, &bak)
            .with_context(|| format!("back up {} to {}", dest.display(), bak.display()))?;
        backup_path = Some(bak);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, render_config(&cfg_text, &edits, &doc)?)
        .with_context(|| format!("write {}", dest.display()))?;

    Ok(ImportReport {
        config_path: dest.to_path_buf(),
        backup_path,
        folder,
        scripts,
        warnings,
    })
}

/// Extract a bundle zip FLAT into `dest`: the parent prefix of the (shallowest) config.yaml
/// inside the zip is stripped, so `myEval/config.yaml` + `myEval/samples/x.wav` land as
/// `config.yaml` + `samples/x.wav`. Refuses to overwrite existing files, rejects entries
/// that would escape `dest` (zip-slip), and skips archiver junk (`__MACOSX/`, `.DS_Store`).
/// Returns the number of files written.
pub(crate) fn extract_bundle_zip(zip_path: &Path, dest: &Path) -> Result<usize> {
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("open {}", zip_path.display()))?;
    let mut za = zip::ZipArchive::new(file)
        .with_context(|| format!("read zip {}", zip_path.display()))?;

    // The strip prefix = the parent of the shallowest config.yaml in the archive.
    let mut prefix: Option<String> = None;
    for i in 0..za.len() {
        // by_index_raw: entry names are readable without decrypting, so a password-protected
        // zip can be reported clearly instead of failing mid-read with a cryptic error.
        let entry = za.by_index_raw(i)?;
        if entry.encrypted() {
            bail!(
                "{} is password-protected — dialf can't decrypt zips; unzip it yourself, \
                 then: dialf import <folder>",
                zip_path.display()
            );
        }
        let name = entry.name().to_string();
        if is_zip_junk(&name) {
            continue;
        }
        let (parent, base) = match name.rsplit_once('/') {
            Some((p, b)) => (format!("{p}/"), b.to_string()),
            None => (String::new(), name),
        };
        if base == "config.yaml"
            && prefix
                .as_ref()
                .map_or(true, |best| parent.matches('/').count() < best.matches('/').count())
        {
            prefix = Some(parent);
        }
    }
    let prefix = prefix
        .ok_or_else(|| anyhow::anyhow!("not a dialf bundle: no config.yaml in {}", zip_path.display()))?;

    // Plan all targets first so a conflict aborts before anything is written.
    let mut plan: Vec<(usize, PathBuf)> = Vec::new();
    let mut conflicts = Vec::new();
    for i in 0..za.len() {
        let entry = za.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() || is_zip_junk(&name) {
            continue;
        }
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue; // outside the bundle folder inside the zip
        };
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path.components().any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("refusing zip entry with an unsafe path: {name}");
        }
        let target = dest.join(rel_path);
        if target.exists() {
            conflicts.push(rel.to_string());
        }
        plan.push((i, target));
    }
    if !conflicts.is_empty() {
        bail!(
            "won't overwrite {} existing file(s) in {} (first: {}) — extract in an empty \
             folder, or remove them first",
            conflicts.len(),
            dest.display(),
            conflicts[0]
        );
    }
    for (i, target) in &plan {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut entry = za.by_index(*i)?;
        let mut out = std::fs::File::create(target)
            .with_context(|| format!("write {}", target.display()))?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(plan.len())
}

/// Archiver noise that must not become part of the bundle.
fn is_zip_junk(name: &str) -> bool {
    name.starts_with("__MACOSX/")
        || name.rsplit('/').next().is_some_and(|b| b == ".DS_Store" || b == "Thumbs.db")
}

/// Interactive gate for replacing an existing config: y/N prompt, defaulting to No.
fn confirm_override(dest: &Path) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "{} already exists — re-run with --override to replace it",
            dest.display()
        );
    }
    eprint!(
        "warning: {} already exists — replace it? (a .bak backup is kept) [y/N]: ",
        dest.display()
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
        bail!("aborted — existing config left untouched");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively collect regular files under `dir`, skipping excluded dirs, dotfiles, and
/// `skip_file` (the zip being written). Follows symlinks (a symlinked samples/ dir is real
/// content), with a visited-set guard against symlink cycles.
fn walk(dir: &Path, excluded: &[PathBuf], skip_file: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    walk_inner(dir, excluded, skip_file, out, &mut seen)
}

fn walk_inner(
    dir: &Path,
    excluded: &[PathBuf],
    skip_file: &Path,
    out: &mut Vec<PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> Result<()> {
    if let Ok(c) = dir.canonicalize() {
        if !seen.insert(c) {
            return Ok(()); // symlink cycle
        }
    }
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))?;
    let mut items: Vec<_> = entries.collect::<std::io::Result<_>>()?;
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        // fs::metadata (not DirEntry::file_type) so symlinks resolve to what they point at;
        // broken symlinks are skipped.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if excluded.iter().any(|x| path.canonicalize().map(|c| c == *x).unwrap_or(false)) {
                continue;
            }
            walk_inner(&path, excluded, skip_file, out, seen)?;
        } else if meta.is_file() && path != skip_file {
            out.push(path);
        }
    }
    Ok(())
}

fn is_yaml(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
}

/// A path relative to the bundle root, as a forward-slash zip entry name.
fn zip_name(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The directory part of a zip entry name ("" for root-level entries).
fn parent_of_zip_name(name: &str) -> String {
    match name.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Relative path from zip directory `base` to zip entry `target` (both zip-root-relative),
/// e.g. base "scripts", target "samples/x.wav" -> "../samples/x.wav".
fn relative_zip_path(base: &str, target: &str) -> String {
    let base_parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    let target_parts: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let common = base_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; base_parts.len() - common];
    parts.extend(&target_parts[common..]);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dialf-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A project folder shaped like the user's real one: script + samples/ + recordings/.
    fn make_project(root: &Path) -> PathBuf {
        let proj = root.join("myEval");
        write(&proj.join("job.yaml"), concat!(
            "# my job\n",
            "- type: audio.play\n  file: samples/prompt.wav\n",
            "- type: wait\n  ms: 100\n",
        ));
        write(&proj.join("samples/prompt.wav"), "RIFFfake");
        write(&proj.join("recordings/dialf-job-1-rx.wav"), "RIFFrec");
        proj
    }

    #[test]
    fn export_excludes_recordings_and_rewrites_config() {
        let root = tempdir("export");
        let proj = make_project(&root);
        write(
            &root.join("cfg/config.yaml"),
            &format!(
                "shared_key: k\nautoanswer:\n  \"+15551234\": {}/job.yaml\naudio:\n  record_dir: {}/recordings\n",
                proj.display(),
                proj.display()
            ),
        );
        let zip = root.join("out.zip");
        let report = export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        let names: Vec<&str> = report.entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"config.yaml"));
        assert!(names.contains(&"job.yaml"));
        assert!(names.contains(&"samples/prompt.wav"));
        assert!(!names.iter().any(|n| n.contains("recordings")));

        // The bundled config points at bundle-relative paths.
        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        za.by_name("config.yaml").unwrap().read_to_string(&mut text).unwrap();
        let cfg: Config = serde_yaml::from_str(&text).unwrap();
        assert_eq!(cfg.autoanswer.get("+15551234").unwrap().as_deref(), Some("job.yaml"));
        assert_eq!(cfg.audio.record_dir.as_deref(), Some(Path::new("recordings")));
        assert_eq!(cfg.shared_key, "k");
    }

    #[test]
    fn export_pulls_out_of_tree_job_and_samples() {
        let root = tempdir("outoftree");
        let proj = make_project(&root);
        // An autoanswer job outside the project folder, referencing its own sample by `..`.
        write(
            &root.join("elsewhere/jobs/inbound.yaml"),
            "- type: audio.play\n  file: ../voice/hello.wav\n",
        );
        write(&root.join("elsewhere/voice/hello.wav"), "RIFFhello");
        write(
            &root.join("cfg/config.yaml"),
            &format!(
                "autoanswer:\n  \"+15551234\": {}/elsewhere/jobs/inbound.yaml\n",
                root.display()
            ),
        );
        let zip = root.join("out.zip");
        let report = export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();
        let names: Vec<&str> = report.entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"scripts/inbound.yaml"));
        assert!(names.contains(&"samples/hello.wav"));

        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        za.by_name("config.yaml").unwrap().read_to_string(&mut text).unwrap();
        let cfg: Config = serde_yaml::from_str(&text).unwrap();
        assert_eq!(
            cfg.autoanswer.get("+15551234").unwrap().as_deref(),
            Some("scripts/inbound.yaml")
        );
        // The pulled job's ref was rewritten to reach the pulled sample from scripts/.
        let mut job_text = String::new();
        za.by_name("scripts/inbound.yaml").unwrap().read_to_string(&mut job_text).unwrap();
        let job = schema::parse(&job_text).unwrap();
        match &job[0].kind {
            StepKind::AudioPlay { file } => assert_eq!(file, "../samples/hello.wav"),
            other => panic!("unexpected step {other:?}"),
        }
    }

    #[test]
    fn export_import_round_trip() {
        let root = tempdir("roundtrip");
        let proj = make_project(&root);
        write(
            &root.join("cfg/config.yaml"),
            &format!(
                "shared_key: k\nautoanswer:\n  \"+15551234\": {}/job.yaml\naudio:\n  record_dir: {}/recordings\n",
                proj.display(),
                proj.display()
            ),
        );
        let zip = root.join("bundle.zip");
        export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        // "Move to the other machine": extract the zip to a fresh folder.
        let imported = root.join("imported");
        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        za.extract(&imported).unwrap();

        let dest = root.join("dest-config/config.yaml");
        let report = import_to(&imported, &dest, false).unwrap();
        assert_eq!(report.scripts.len(), 1);
        assert!(report.backup_path.is_none());

        let cfg: Config =
            serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        let imported_canon = imported.canonicalize().unwrap();
        assert_eq!(
            cfg.autoanswer.get("+15551234").unwrap().as_deref(),
            Some(imported_canon.join("job.yaml").to_str().unwrap())
        );
        assert_eq!(
            cfg.audio.record_dir.as_deref(),
            Some(imported_canon.join("recordings").as_path())
        );
        assert_eq!(cfg.shared_key, "k");
    }

    #[test]
    fn import_refuses_invalid_bundles() {
        let root = tempdir("refuse");
        // A .zip argument that doesn't exist errors on open.
        let err = import_to(&root.join("bundle.zip"), &root.join("c.yaml"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("open"), "{err}");

        // No config.yaml.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = import_to(&empty, &root.join("c.yaml"), false).unwrap_err().to_string();
        assert!(err.contains("no config.yaml"), "{err}");

        // Config but no job script.
        let noscript = root.join("noscript");
        write(&noscript.join("config.yaml"), "shared_key: k\n");
        let err = import_to(&noscript, &root.join("c.yaml"), false).unwrap_err().to_string();
        assert!(err.contains("no job script"), "{err}");
    }

    #[test]
    fn import_override_backs_up_existing_config() {
        let root = tempdir("override");
        let bundle = root.join("bundle");
        write(&bundle.join("config.yaml"), "shared_key: new\n");
        write(&bundle.join("job.yaml"), "- type: wait\n  ms: 1\n");

        let dest = root.join("cfg/config.yaml");
        write(&dest, "shared_key: old\n");

        // Without --override on a non-tty stdin (tests), it refuses.
        let err = import_to(&bundle, &dest, false).unwrap_err().to_string();
        assert!(err.contains("--override"), "{err}");
        assert!(std::fs::read_to_string(&dest).unwrap().contains("old"));

        // With --override: replaced, old config backed up.
        let report = import_to(&bundle, &dest, true).unwrap();
        let bak = report.backup_path.unwrap();
        assert!(std::fs::read_to_string(&bak).unwrap().contains("old"));
        assert!(std::fs::read_to_string(&dest).unwrap().contains("new"));

        // A further import must NOT clobber the original backup — it gets its own name
        // (same-second imports are disambiguated with a counter suffix).
        let report2 = import_to(&bundle, &dest, true).unwrap();
        let bak2 = report2.backup_path.unwrap();
        assert_ne!(bak, bak2);
        assert!(std::fs::read_to_string(&bak).unwrap().contains("old"));
    }

    #[test]
    fn import_override_accumulates_backups() {
        // `--override` replaces directly but every replaced config gets its own backup —
        // nothing is pruned or overwritten.
        let root = tempdir("bak-accumulate");
        let bundle = root.join("bundle");
        write(&bundle.join("config.yaml"), "shared_key: bundle\n");
        write(&bundle.join("job.yaml"), "- type: wait\n  ms: 1\n");
        let dest = root.join("cfg/config.yaml");
        for i in 0..7 {
            write(&dest, &format!("shared_key: v{i}\n"));
            import_to(&bundle, &dest, true).unwrap();
        }
        let baks: Vec<String> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.yaml.bak"))
            .collect();
        assert_eq!(baks.len(), 7, "{baks:?}");
    }

    #[test]
    fn export_intree_file_wins_name_over_pulled_external() {
        let root = tempdir("name-clash");
        let proj = make_project(&root);
        // a.yaml (sorts before samples/) plays an out-of-tree file whose basename collides
        // with the in-tree samples/prompt.wav — the external must get a distinct zip name,
        // not clobber (or be clobbered by) the in-tree file.
        write(&root.join("shared/prompt.wav"), "RIFFexternal");
        write(
            &proj.join("a.yaml"),
            &format!("- type: audio.play\n  file: {}/shared/prompt.wav\n", root.display()),
        );
        write(&root.join("cfg/config.yaml"), "shared_key: k\n");
        let zip = root.join("out.zip");
        export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut in_tree = String::new();
        za.by_name("samples/prompt.wav").unwrap().read_to_string(&mut in_tree).unwrap();
        assert_eq!(in_tree, "RIFFfake");
        let mut script = String::new();
        za.by_name("a.yaml").unwrap().read_to_string(&mut script).unwrap();
        let job = schema::parse(&script).unwrap();
        let StepKind::AudioPlay { file } = &job[0].kind else { panic!() };
        assert_ne!(file, "samples/prompt.wav", "external clobbered the in-tree name");
        let mut external = String::new();
        za.by_name(file).unwrap().read_to_string(&mut external).unwrap();
        assert_eq!(external, "RIFFexternal");
    }

    #[test]
    fn export_warns_on_refs_into_excluded_dirs() {
        let root = tempdir("excluded-ref");
        let proj = make_project(&root);
        // A script replaying a recording: the ref resolves in-tree, but recordings/ is
        // excluded from the bundle — that must produce a loud warning, not silence.
        write(
            &proj.join("replay.yaml"),
            "- type: audio.play\n  file: recordings/dialf-job-1-rx.wav\n",
        );
        write(&root.join("cfg/config.yaml"), "shared_key: k\n");
        let zip = root.join("out.zip");
        let report =
            export(&proj, Some(zip), Some(root.join("cfg/config.yaml"))).unwrap();
        assert!(
            report.warnings.iter().any(|w| w.contains("NOT in the bundle")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn export_follows_symlinked_sample_dirs() {
        let root = tempdir("symlink");
        let proj = root.join("proj");
        write(&proj.join("job.yaml"), "- type: audio.play\n  file: samples/prompt.wav\n");
        // samples/ is a symlink to a shared library elsewhere — its files must ship.
        write(&root.join("library/prompt.wav"), "RIFFshared");
        std::os::unix::fs::symlink(root.join("library"), proj.join("samples")).unwrap();
        write(&root.join("cfg/config.yaml"), "shared_key: k\n");
        let zip = root.join("out.zip");
        let report =
            export(&proj, Some(zip), Some(root.join("cfg/config.yaml"))).unwrap();
        let names: Vec<&str> = report.entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"samples/prompt.wav"), "{names:?}");
    }

    #[test]
    fn import_warns_on_pinned_socket_and_ws_bind() {
        let root = tempdir("socket-warn");
        let bundle = root.join("bundle");
        write(
            &bundle.join("config.yaml"),
            "shared_key: k\ncontrol_socket: /run/dialf/dialfd.sock\nws_bind: \"192.168.1.50:8765\"\n",
        );
        write(&bundle.join("job.yaml"), "- type: wait\n  ms: 1\n");
        let report = import_to(&bundle, &root.join("cfg/config.yaml"), false).unwrap();
        assert!(
            report.warnings.iter().any(|w| w.contains("control_socket")),
            "{:?}",
            report.warnings
        );
        assert!(report.warnings.iter().any(|w| w.contains("ws_bind")), "{:?}", report.warnings);
    }

    #[test]
    fn export_prefers_folder_config_and_skips_stale_zip_inside_dir() {
        let root = tempdir("folder-config");
        let proj = make_project(&root);
        // The folder carries its own config (answer-only entry included) — it wins over the
        // default config path, and the generated bundle config replaces the folder's copy.
        write(
            &proj.join("config.yaml"),
            "shared_key: local\nautoanswer:\n  \"+15551234\": job.yaml\n  \"+15559876\":\n",
        );
        // A stale zip from a previous export sits inside the folder being exported.
        let zip = proj.join("myEval.dialf.zip");
        write(&zip, "stale");
        let report = export(&proj, Some(zip.clone()), None).unwrap();

        let names: Vec<&str> = report.entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"myEval.dialf.zip"), "{names:?}");
        assert_eq!(names.iter().filter(|n| **n == "config.yaml").count(), 1);

        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        za.by_name("config.yaml").unwrap().read_to_string(&mut text).unwrap();
        let cfg: Config = serde_yaml::from_str(&text).unwrap();
        assert_eq!(cfg.shared_key, "local");
        assert_eq!(cfg.autoanswer.get("+15551234").unwrap().as_deref(), Some("job.yaml"));
        // Answer-only entries survive untouched.
        assert_eq!(cfg.autoanswer.get("+15559876"), Some(&None));
    }

    #[test]
    fn export_preserves_intree_relative_refs_and_comments() {
        let root = tempdir("intree-rel");
        let proj = make_project(&root);
        // A nested script escaping its dir with `..` but staying inside the folder: the same
        // relative relationship holds in the zip, so the file ships byte-identical.
        let script = "# keep this comment\n- type: audio.play\n  file: ../samples/prompt.wav\n";
        write(&proj.join("scripts/greet.yaml"), script);
        write(&root.join("cfg/config.yaml"), "shared_key: k\n");
        let zip = root.join("out.zip");
        export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        za.by_name("scripts/greet.yaml").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, script);
    }

    #[test]
    fn export_rewrites_absolute_intree_ref_to_relative() {
        let root = tempdir("abs-intree");
        let proj = make_project(&root);
        // A script pinned to an absolute path inside the folder would break on another
        // machine — it gets rewritten relative (comments dropped for that file).
        write(
            &proj.join("pinned.yaml"),
            &format!(
                "- type: audio.play\n  file: {}/samples/prompt.wav\n",
                proj.canonicalize().unwrap().display()
            ),
        );
        write(&root.join("cfg/config.yaml"), "shared_key: k\n");
        let zip = root.join("out.zip");
        let report =
            export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        za.by_name("pinned.yaml").unwrap().read_to_string(&mut text).unwrap();
        let job = schema::parse(&text).unwrap();
        match &job[0].kind {
            StepKind::AudioPlay { file } => assert_eq!(file, "samples/prompt.wav"),
            other => panic!("unexpected step {other:?}"),
        }
        assert!(report.warnings.iter().any(|w| w.contains("rewritten")), "{:?}", report.warnings);
    }

    #[test]
    fn import_accepts_nested_scripts_and_warns_on_missing_refs() {
        let root = tempdir("import-warn");
        let bundle = root.join("bundle");
        // Config points at an absolute job that doesn't exist on "this machine", plus a
        // pinned capture tool that isn't installed.
        write(
            &bundle.join("config.yaml"),
            "shared_key: k\nautoanswer:\n  \"+15551234\": /gone/machine1/job.yaml\naudio:\n  capture_cmd: [\"/opt/nowhere/sox\", \"-q\"]\n",
        );
        // The only script lives in a subfolder and references a sample that's missing.
        write(
            &bundle.join("scripts/inbound.yaml"),
            "- type: audio.play\n  file: ../samples/missing.wav\n",
        );

        let dest = root.join("cfg/config.yaml");
        let report = import_to(&bundle, &dest, false).unwrap();
        assert_eq!(report.scripts.len(), 1);
        assert!(report.warnings.iter().any(|w| w.contains("missing.wav")), "{:?}", report.warnings);
        assert!(
            report.warnings.iter().any(|w| w.contains("/gone/machine1/job.yaml")),
            "{:?}",
            report.warnings
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("/opt/nowhere/sox")),
            "{:?}",
            report.warnings
        );
        // The absolute (already machine-specific) autoanswer path is left as-is.
        let cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(
            cfg.autoanswer.get("+15551234").unwrap().as_deref(),
            Some("/gone/machine1/job.yaml")
        );
    }

    #[test]
    fn imported_bundle_jobs_load_and_resolve() {
        // The practical end state: after export -> unzip -> import, the daemon must be able
        // to load the job via the installed config's path and find its samples.
        let root = tempdir("loadable");
        let proj = make_project(&root);
        write(
            &root.join("cfg/config.yaml"),
            &format!("autoanswer:\n  \"+15551234\": {}/job.yaml\n", proj.display()),
        );
        let zip = root.join("bundle.zip");
        export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        let extracted = root.join("machine2");
        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        za.extract(&extracted).unwrap();
        let dest = root.join("dest/config.yaml");
        import_to(&extracted, &dest, false).unwrap();

        let cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        let job_path = cfg.autoanswer.get("+15551234").unwrap().clone().unwrap();
        // What the daemon does on an inbound call: load the job, resolve audio refs.
        let steps = crate::daemon::load_job_file(&job_path).unwrap();
        let StepKind::AudioPlay { file } = &steps[0].kind else {
            panic!("expected audio.play first");
        };
        assert!(Path::new(file).is_file(), "sample not resolvable: {file}");
    }

    #[test]
    fn config_round_trip_keeps_only_the_users_fields() {
        // The bundled + installed configs must stay recognizable as the user's own file:
        // only the fields they wrote (in their order), with just the path values rewritten —
        // no injected defaults like ws_bind / instance_name / autoanswer: {}.
        let root = tempdir("fidelity");
        let proj = make_project(&root);
        write(
            &root.join("cfg/config.yaml"),
            &format!(
                "shared_key: change-me\naudio:\n  # my sound card\n  capture_device: \"BlackHole 2ch\"\n  record_dir: {}/recordings\n  mix_recording: true\n",
                proj.display()
            ),
        );
        let zip = root.join("bundle.zip");
        export(&proj, Some(zip.clone()), Some(root.join("cfg/config.yaml"))).unwrap();

        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        let mut text = String::new();
        za.by_name("config.yaml").unwrap().read_to_string(&mut text).unwrap();
        for absent in ["ws_bind", "instance_name", "autoanswer", "sample_rate", "playback_cmd"] {
            assert!(!text.contains(absent), "injected `{absent}` into:\n{text}");
        }
        assert!(text.contains(r#"record_dir: "recordings""#), "{text}");
        assert!(text.starts_with("shared_key:"), "field order changed:\n{text}");
        // Untouched lines keep their exact original formatting — quotes and comments included.
        assert!(text.contains(r#"capture_device: "BlackHole 2ch""#), "{text}");
        assert!(text.contains("# my sound card"), "comments dropped:\n{text}");

        // Same fidelity after import: only the bundle's fields, paths made absolute.
        let extracted = root.join("machine2");
        let mut za = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        za.extract(&extracted).unwrap();
        let dest = root.join("dest/config.yaml");
        import_to(&extracted, &dest, false).unwrap();
        let installed = std::fs::read_to_string(&dest).unwrap();
        for absent in ["ws_bind", "instance_name", "autoanswer", "sample_rate"] {
            assert!(!installed.contains(absent), "injected `{absent}` into:\n{installed}");
        }
        let dir = extracted.canonicalize().unwrap();
        assert!(
            installed.contains(&format!("record_dir: \"{}/recordings\"", dir.display())),
            "{installed}"
        );
    }

    fn make_zip(path: &Path, entries: &[(&str, &str)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut zw = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            zw.start_file(*name, opts).unwrap();
            std::io::Write::write_all(&mut zw, content.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }

    #[test]
    fn zip_extracts_flat_stripping_config_parent() {
        // A zip of a folder (`zip -r myEval.zip myEval/` / Finder compress): the config's
        // parent prefix is stripped so the files land flat, junk skipped.
        let root = tempdir("zip-flat");
        let zip = root.join("myEval.dialf.zip");
        make_zip(
            &zip,
            &[
                ("myEval/config.yaml", "shared_key: k\n"),
                ("myEval/job.yaml", "- type: wait\n  ms: 1\n"),
                ("myEval/samples/p.wav", "RIFFx"),
                ("__MACOSX/myEval/._config.yaml", "junk"),
                ("myEval/.DS_Store", "junk"),
            ],
        );
        let ws = root.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let n = extract_bundle_zip(&zip, &ws).unwrap();
        assert_eq!(n, 3);
        assert!(ws.join("config.yaml").is_file());
        assert!(ws.join("samples/p.wav").is_file());
        assert!(!ws.join("myEval").exists(), "prefix not stripped");
        assert!(!ws.join(".DS_Store").exists());

        // And the extracted workspace imports as a normal bundle.
        let report = import_to(&ws, &root.join("cfg/config.yaml"), false).unwrap();
        assert_eq!(report.scripts.len(), 1);
    }

    #[test]
    fn zip_with_root_config_extracts_as_is() {
        // Our own `dialf export` zips have config.yaml at the root — nothing to strip.
        let root = tempdir("zip-root");
        let zip = root.join("b.zip");
        make_zip(&zip, &[("config.yaml", "shared_key: k\n"), ("job.yaml", "- type: wait\n  ms: 1\n")]);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert_eq!(extract_bundle_zip(&zip, &ws).unwrap(), 2);
        assert!(ws.join("config.yaml").is_file());
        assert!(ws.join("job.yaml").is_file());
    }

    #[test]
    fn zip_extraction_rejects_escapes_and_conflicts() {
        let root = tempdir("zip-guard");
        // Zip-slip: an entry escaping the workspace is refused outright.
        let evil = root.join("evil.zip");
        make_zip(&evil, &[("config.yaml", "shared_key: k\n"), ("../outside.txt", "x")]);
        let ws = root.join("ws1");
        std::fs::create_dir_all(&ws).unwrap();
        let err = extract_bundle_zip(&evil, &ws).unwrap_err().to_string();
        assert!(err.contains("unsafe path"), "{err}");
        assert!(!root.join("outside.txt").exists());

        // Existing files are never overwritten — abort before writing anything.
        let zip = root.join("b.zip");
        make_zip(&zip, &[("config.yaml", "shared_key: k\n"), ("job.yaml", "- type: wait\n  ms: 1\n")]);
        let ws2 = root.join("ws2");
        write(&ws2.join("job.yaml"), "precious local edits");
        let err = extract_bundle_zip(&zip, &ws2).unwrap_err().to_string();
        assert!(err.contains("won't overwrite"), "{err}");
        assert_eq!(std::fs::read_to_string(ws2.join("job.yaml")).unwrap(), "precious local edits");
        assert!(!ws2.join("config.yaml").exists(), "nothing should be written on conflict");

        // No config.yaml anywhere in the zip -> not a bundle.
        let nocfg = root.join("nocfg.zip");
        make_zip(&nocfg, &[("job.yaml", "- type: wait\n  ms: 1\n")]);
        let ws3 = root.join("ws3");
        std::fs::create_dir_all(&ws3).unwrap();
        let err = extract_bundle_zip(&nocfg, &ws3).unwrap_err().to_string();
        assert!(err.contains("no config.yaml"), "{err}");
    }

    #[test]
    fn relative_zip_paths() {
        assert_eq!(relative_zip_path("", "samples/x.wav"), "samples/x.wav");
        assert_eq!(relative_zip_path("scripts", "samples/x.wav"), "../samples/x.wav");
        assert_eq!(relative_zip_path("a/b", "a/c/x.wav"), "../c/x.wav");
        assert_eq!(relative_zip_path("scripts", "scripts/x.yaml"), "x.yaml");
    }
}
