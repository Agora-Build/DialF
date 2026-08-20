//! Export / import a DialF setup as a portable bundle.
//!
//! A "setup" is a project folder of job scripts + audio samples (plus a recordings output
//! dir), and the daemon config (`~/.config/dialf/config.yaml`). `export` zips the folder's
//! scripts and samples together with the config — recordings excluded — rewriting the
//! config's paths to be bundle-relative so the zip is self-contained. `import` takes an
//! already-extracted bundle folder, installs its `config.yaml` to the default config path
//! with paths rewritten to absolute paths under that folder, and leaves the folder in place
//! as the live content location. (Importing straight from a `.zip` is future work.)

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
    let cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(&config_path)?)
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
    let zip_abs = zip_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.canonicalize())
        .transpose()?
        .unwrap_or_else(|| PathBuf::from("."))
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

    for f in &files {
        let name = zip_name(f.strip_prefix(&dir).unwrap());
        if is_yaml(f) {
            match normalize_script(f, &name, &dir, &mut entries, &mut pulled, &mut warnings)? {
                Some(job) => {
                    warnings.push(format!(
                        "{}: rewritten for the bundle (comments dropped)",
                        f.display()
                    ));
                    entries.insert(name, Src::Bytes(serde_yaml::to_string(&job)?.into_bytes()));
                }
                None => {
                    entries.insert(name, Src::File(f.clone()));
                }
            }
        } else {
            entries.insert(name, Src::File(f.clone()));
        }
    }

    // Config: rewrite autoanswer job paths bundle-relative (pulling out-of-tree jobs in),
    // and point record_dir at a bundle-relative `recordings`.
    let mut bundle_cfg = cfg.clone();
    for (number, job) in bundle_cfg.autoanswer.iter_mut() {
        let Some(path) = job else { continue };
        let resolved = resolve_path_under(config_dir.as_deref(), Path::new(&*path));
        let Ok(target) = resolved.canonicalize() else {
            warnings.push(format!(
                "config autoanswer {number}: job not found: {path} (kept as-is)"
            ));
            continue;
        };
        if target.starts_with(&dir) {
            *path = zip_name(target.strip_prefix(&dir).unwrap());
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
            *path = name;
        }
    }
    if bundle_cfg.audio.record_dir.is_some() {
        bundle_cfg.audio.record_dir = Some(PathBuf::from("recordings"));
    }
    entries.insert(
        "config.yaml".to_string(),
        Src::Bytes(serde_yaml::to_string(&bundle_cfg)?.into_bytes()),
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
            continue; // missing ref: leave it; export already warns on the config path level
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
/// default config path with relative paths rewritten absolute under `folder`.
pub fn import(folder: &Path, override_existing: bool) -> Result<ImportReport> {
    import_to(folder, &Config::default_path(), override_existing)
}

/// [`import`] with an explicit destination config path (separated for tests).
pub fn import_to(folder: &Path, dest: &Path, override_existing: bool) -> Result<ImportReport> {
    if folder.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
        bail!(
            "direct .zip import isn't supported yet — unzip it first, then: dialf import <folder>"
        );
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
    let mut cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(&cfg_file)?)
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
    // config is installed to the default path but the content stays in the folder.
    for (number, job) in cfg.autoanswer.iter_mut() {
        let Some(path) = job else { continue };
        let abs = resolve_path_under(Some(&folder), Path::new(&*path));
        if !abs.is_file() {
            warnings.push(format!(
                "config autoanswer {number}: job not found at {}",
                abs.display()
            ));
        }
        *path = abs.to_string_lossy().into_owned();
    }
    if let Some(rd) = cfg.audio.record_dir.take() {
        cfg.audio.record_dir = Some(resolve_path_under(Some(&folder), &rd));
    }

    // Host-compat heads-up: the bundle may pin tools/devices from the exporting machine.
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

    // Install, confirming before replacing an existing config.
    let mut backup_path = None;
    if dest.exists() {
        if !override_existing {
            confirm_override(dest)?;
        }
        let bak = dest.with_extension("yaml.bak");
        std::fs::copy(dest, &bak)
            .with_context(|| format!("back up {} to {}", dest.display(), bak.display()))?;
        backup_path = Some(bak);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, serde_yaml::to_string(&cfg)?)
        .with_context(|| format!("write {}", dest.display()))?;

    Ok(ImportReport {
        config_path: dest.to_path_buf(),
        backup_path,
        folder,
        scripts,
        warnings,
    })
}

/// Interactive gate for replacing an existing config: the user must type `override`.
fn confirm_override(dest: &Path) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "{} already exists — re-run with --override to replace it",
            dest.display()
        );
    }
    eprint!(
        "{} already exists and will be replaced (a .bak backup is kept).\ntype \"override\" to continue: ",
        dest.display()
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim() != "override" {
        bail!("aborted — existing config left untouched");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively collect regular files under `dir`, skipping excluded dirs, dotfiles, and
/// `skip_file` (the zip being written).
fn walk(dir: &Path, excluded: &[PathBuf], skip_file: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
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
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if excluded.iter().any(|x| path.canonicalize().map(|c| c == *x).unwrap_or(false)) {
                continue;
            }
            walk(&path, excluded, skip_file, out)?;
        } else if ft.is_file() && path != skip_file {
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
    fn import_refuses_zip_and_invalid_bundles() {
        let root = tempdir("refuse");
        // A .zip argument gets the "unzip first" error.
        let err = import_to(&root.join("bundle.zip"), &root.join("c.yaml"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unzip it first"), "{err}");

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
    }

    #[test]
    fn relative_zip_paths() {
        assert_eq!(relative_zip_path("", "samples/x.wav"), "samples/x.wav");
        assert_eq!(relative_zip_path("scripts", "samples/x.wav"), "../samples/x.wav");
        assert_eq!(relative_zip_path("a/b", "a/c/x.wav"), "../c/x.wav");
        assert_eq!(relative_zip_path("scripts", "scripts/x.yaml"), "x.yaml");
    }
}
