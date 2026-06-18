//! In-memory registry of connected devices and their call state.
//!
//! For M1 the only device is the in-process [`crate::loopback`] phone. M2 adds real
//! phones over WebSocket; they register/unregister here on connect/disconnect.

use std::collections::HashMap;

use serde::Serialize;

use crate::protocol::{CallState, Direction};

/// How a device is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// In-process fake phone (M1, no hardware).
    Loopback,
    /// Real phone over WebSocket (M2+).
    Phone,
}

/// State of the device's current call, if any.
#[derive(Debug, Clone, Serialize)]
pub struct CallInfo {
    pub call_id: String,
    pub number: Option<String>,
    pub state: CallState,
    pub direction: Direction,
}

/// A registered device.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
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

    /// Number of registered devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
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
