//! `dialfd` configuration.
//!
//! Loaded from a YAML file (default `~/.config/dialf/config.yaml`); every field has a
//! sensible default so a zero-config run works. Audio command templates are overridable
//! here so no specific tool is hardcoded (see [`AudioConfig`]).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default WebSocket bind address for the phone control plane.
pub const DEFAULT_WS_BIND: &str = "0.0.0.0:8765";
/// Default mDNS service type advertised on the LAN.
pub const DEFAULT_SERVICE_TYPE: &str = "_dialfd._tcp.local.";
/// Native capture/playback rate used with the sound card before resampling to 16k.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Top-level daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Shared secret the phone must present in its `hello` frame.
    pub shared_key: String,
    /// Path to the local control socket (`dialf` <-> `dialfd`).
    pub control_socket: PathBuf,
    /// `host:port` to bind the phone WebSocket server on.
    pub ws_bind: String,
    /// Friendly instance name advertised via mDNS.
    pub instance_name: String,
    /// Inbound routing: number → optional job path. The number is auto-answered when it
    /// rings; `None` (a null/empty value) just answers, while `Some(path)` answers *and*
    /// runs that job (which should begin with `call.answer`). Relative job paths resolve
    /// against this config file's directory.
    pub autoanswer: BTreeMap<String, Option<String>>,
    /// Audio engine / sound-card settings.
    pub audio: AudioConfig,
}

/// Sound-card + external-tool settings.
/// Stereo channel assignment for the mixed recording (`*-mix.wav`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MixChannels {
    /// Left = tx (local / our prompts), right = rx (remote / far end). The default.
    #[default]
    TxRx,
    /// Left = rx (remote / far end), right = tx (local / our prompts).
    RxTx,
}

impl MixChannels {
    /// True when tx belongs in the left channel (the default layout).
    pub fn tx_left(self) -> bool {
        matches!(self, MixChannels::TxRx)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Native sample rate to capture/play at the card (resampled to 16k for VAD).
    pub sample_rate: u32,
    /// Channel count at the card (mono recommended for the call bridge).
    pub channels: u16,
    /// Capture device hint passed to the tool (e.g. ALSA `plughw:1,0`, ffmpeg index).
    pub capture_device: Option<String>,
    /// Playback device hint passed to the tool.
    pub playback_device: Option<String>,
    /// Override the capture command template (argv). `{rate}`, `{channels}`, `{device}`
    /// are substituted; the tool MUST emit raw little-endian s16 PCM on stdout.
    pub capture_cmd: Option<Vec<String>>,
    /// Override the playback command template (argv). `{rate}`, `{channels}`, `{device}`,
    /// `{file}` are substituted; the tool reads s16 PCM/WAV from stdin or `{file}`.
    pub playback_cmd: Option<Vec<String>>,
    /// Directory to write call recordings into; `None` disables recording.
    pub record_dir: Option<PathBuf>,
    /// When recording, mix played + captured audio into one file (else keep two legs).
    pub mix_recording: bool,
    /// Stereo channel layout for the mixed file. Default: tx (local) left, rx (remote) right.
    pub mix_channels: MixChannels,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shared_key: "change-me".to_string(),
            control_socket: default_control_socket(),
            ws_bind: DEFAULT_WS_BIND.to_string(),
            instance_name: "dialfd".to_string(),
            autoanswer: BTreeMap::new(),
            audio: AudioConfig::default(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: 1,
            capture_device: None,
            playback_device: None,
            capture_cmd: None,
            playback_cmd: None,
            record_dir: None,
            mix_recording: false,
            mix_channels: MixChannels::default(),
        }
    }
}

impl Config {
    /// Load config from `path`, falling back to defaults if it does not exist.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = serde_yaml::from_str(&text)?;
        Ok(cfg)
    }

    /// The conventional default config path (`~/.config/dialf/config.yaml`).
    pub fn default_path() -> PathBuf {
        config_dir().join("dialf").join("config.yaml")
    }
}

fn config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".")
}

fn default_control_socket() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("dialfd.sock");
    }
    std::env::temp_dir().join("dialfd.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoanswer_map_parses_jobs_and_answer_only() {
        // A path runs a job; `~` and an empty value both mean answer-only.
        let yaml = r#"
autoanswer:
  "+15551234": jobs/inbound.yaml
  "+15559876": ~
  "+15550000":
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.autoanswer.get("+15551234"),
            Some(&Some("jobs/inbound.yaml".to_string()))
        );
        assert_eq!(cfg.autoanswer.get("+15559876"), Some(&None)); // answer only
        assert_eq!(cfg.autoanswer.get("+15550000"), Some(&None)); // answer only
        assert_eq!(cfg.autoanswer.get("+10000000"), None); // not configured
    }

    #[test]
    fn mix_channels_defaults_to_tx_left() {
        // Default (unset in YAML) is tx-left / rx-right.
        assert_eq!(MixChannels::default(), MixChannels::TxRx);
        assert!(MixChannels::TxRx.tx_left());
        assert!(!MixChannels::RxTx.tx_left());
        // Omitting it in config leaves the default.
        let cfg: Config = serde_yaml::from_str("audio: {}").unwrap();
        assert_eq!(cfg.audio.mix_channels, MixChannels::TxRx);
    }

    #[test]
    fn mix_channels_parses_snake_case() {
        let cfg: Config = serde_yaml::from_str("audio:\n  mix_channels: rx_tx\n").unwrap();
        assert_eq!(cfg.audio.mix_channels, MixChannels::RxTx);
        assert!(!cfg.audio.mix_channels.tx_left());
    }
}
