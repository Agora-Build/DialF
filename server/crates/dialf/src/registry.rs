//! In-memory registry of connected devices and their call state.
//!
//! Real phones register/unregister here over WebSocket on connect/disconnect.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::protocol::{CallState, Direction};

/// State of the device's current call, if any.
#[derive(Debug, Clone, Serialize)]
pub struct CallInfo {
    pub call_id: String,
    pub number: Option<String>,
    pub state: CallState,
    pub direction: Direction,
}

/// A text message seen by a device (received, or echoed on send).
#[derive(Debug, Clone, Serialize)]
pub struct SmsRecord {
    pub direction: Direction,
    pub from: Option<String>,
    pub to: Option<String>,
    pub body: String,
    pub ts: i64,
}

/// A call-log entry reported by a device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    /// The other party's number, if known.
    #[serde(default)]
    pub number: Option<String>,
    /// incoming | outgoing | missed | rejected | voicemail | blocked | unknown.
    pub kind: String,
    /// Epoch milliseconds.
    pub ts: i64,
    /// Call duration in seconds.
    pub duration: i64,
}

/// An active SIM / subscription on the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimInfo {
    /// Physical SIM slot index (0-based).
    pub slot: i32,
    /// Subscription id (use to place calls on this SIM).
    pub sub_id: i32,
    /// User-facing SIM name, if set.
    #[serde(default)]
    pub name: Option<String>,
    /// Carrier name, if known.
    #[serde(default)]
    pub carrier: Option<String>,
    /// The SIM's own number (often blank — carriers don't always provision it).
    #[serde(default)]
    pub number: Option<String>,
    /// True if this is the system default SIM for outgoing calls.
    #[serde(default)]
    pub is_default: bool,
}

/// The network's reply to a raw MMI / USSD request (low-level escape hatch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MmiResult {
    /// The code that was sent (e.g. `##002#`).
    pub code: String,
    /// Whether the network accepted the request.
    pub success: bool,
    /// The network's human-readable response, if any.
    #[serde(default)]
    pub response: Option<String>,
}

/// The result of a voicemail enable/disable request, as reported by the device.
/// How the change is applied is the device's concern (Android dials GSM MMI codes;
/// other platforms may do something else) — the host only expresses the intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicemailResult {
    /// The requested state (true = voicemail enabled).
    pub enabled: bool,
    /// Whether the device applied it successfully.
    pub success: bool,
    /// A human-readable detail from the device/network, if any.
    #[serde(default)]
    pub response: Option<String>,
}

/// A registered device.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// The phone's LAN IP, taken from the WebSocket peer — useful for reaching the phone over
    /// WiFi (`adb connect <ip>:<port>`) to debug the app without a USB cable. `None` until the
    /// phone connects.
    ///
    /// Only the IP is known here, never the adb port. `5555` is *not* a default — it's just the
    /// convention for classic `adb tcpip 5555`, which has to be enabled over USB and is lost on
    /// reboot. Android 11+ Wireless debugging instead uses a *random* port shown only on the phone
    /// screen (and gated behind one-time pairing). To get a fixed, known port unattended — no human
    /// reading the screen — you need root: set `service.adb.tcp.port 5555` and persist it across
    /// boots with a Magisk service script (plain `setprop` is wiped on reboot).
    #[serde(default)]
    pub addr: Option<String>,
    pub last_seen_ms: i64,
    pub current_call: Option<CallInfo>,
}

/// Registry of devices keyed by id.
#[derive(Debug, Default)]
pub struct Registry {
    devices: HashMap<String, DeviceInfo>,
}

impl Registry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a device.
    pub fn upsert(&mut self, dev: DeviceInfo) {
        self.devices.insert(dev.id.clone(), dev);
    }

    /// Remove a device by id.
    pub fn remove(&mut self, id: &str) -> Option<DeviceInfo> {
        self.devices.remove(id)
    }

    /// Look up a device by id.
    pub fn get(&self, id: &str) -> Option<&DeviceInfo> {
        self.devices.get(id)
    }

    /// Mutable access to a device by id.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut DeviceInfo> {
        self.devices.get_mut(id)
    }

    /// All devices, sorted by id for stable output.
    pub fn list(&self) -> Vec<DeviceInfo> {
        let mut v: Vec<DeviceInfo> = self.devices.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Remove every device whose `last_seen_ms` is strictly older than `cutoff_ms`; returns
    /// the removed ids. Used to reap phones whose connection went silent (half-open sockets).
    pub fn reap_older_than(&mut self, cutoff_ms: i64) -> Vec<String> {
        let stale: Vec<String> = self
            .devices
            .iter()
            .filter(|(_, d)| d.last_seen_ms < cutoff_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            self.devices.remove(id);
        }
        stale
    }

    /// Pick the sole device id if exactly one is registered (for `--device`-less calls).
    pub fn sole_device_id(&self) -> Option<String> {
        if self.devices.len() == 1 {
            self.devices.keys().next().cloned()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, last_seen_ms: i64) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: id.to_string(),
            addr: None,
            last_seen_ms,
            current_call: None,
        }
    }

    #[test]
    fn reap_older_than_removes_only_stale() {
        let mut reg = Registry::new();
        reg.upsert(dev("fresh", 10_000));
        reg.upsert(dev("stale", 1_000));
        reg.upsert(dev("edge", 5_000)); // exactly at cutoff → kept (strictly older only)

        let reaped = reg.reap_older_than(5_000);
        assert_eq!(reaped, vec!["stale".to_string()]);
        assert!(reg.get("fresh").is_some());
        assert!(reg.get("edge").is_some());
        assert!(reg.get("stale").is_none());
    }
}
