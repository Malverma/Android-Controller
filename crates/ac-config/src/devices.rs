//! `devices.toml` (spec §7.4): devices the user has connected before.

use std::collections::{BTreeMap, HashSet};

use ac_core::{SlotId, TransportKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Profile, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// `ro.serialno`, or `aid:<android_id>` when the ROM hides it.
    pub id: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub sdk: u32,
    /// 1..=max_slots, or 0 for saved but unassigned.
    #[serde(default)]
    pub slot: SlotId,
    pub transport: TransportKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdns_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi_mac: Option<String>,
    #[serde(default = "yes")]
    pub auto_connect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_codecs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub capture_orientation: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Partial profile overrides keyed by profile name. Kept as raw TOML so
    /// only the keys the user wrote are applied.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profile_overrides: BTreeMap<String, toml::Table>,
}

fn yes() -> bool {
    true
}

impl DeviceRecord {
    pub fn new(id: &str, transport: TransportKind, now: DateTime<Utc>) -> Self {
        Self {
            id: id.to_owned(),
            alias: String::new(),
            model: String::new(),
            sdk: 0,
            slot: 0,
            transport,
            mdns_instance: None,
            last_address: None,
            wifi_mac: None,
            auto_connect: true,
            target_dir: None,
            supported_codecs: Vec::new(),
            color: None,
            capture_orientation: String::new(),
            first_seen: now,
            last_seen: now,
            profile_overrides: BTreeMap::new(),
        }
    }

    pub fn display_name(&self) -> &str {
        if !self.alias.is_empty() {
            &self.alias
        } else if !self.model.is_empty() {
            &self.model
        } else {
            &self.id
        }
    }

    /// Applies this device's overrides for `name` on top of `base`.
    pub fn profile(&self, name: &str, base: &Profile) -> Result<Profile, toml::de::Error> {
        let Some(over) = self.profile_overrides.get(name) else {
            return Ok(base.clone());
        };
        let mut merged = toml::Table::try_from(base).expect("Profile serializes to a table");
        for (k, v) in over {
            merged.insert(k.clone(), v.clone());
        }
        merged.try_into()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    pub schema_version: u32,
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceRecord>,
}

impl Default for Devices {
    fn default() -> Self {
        Self { schema_version: SCHEMA_VERSION, devices: Vec::new() }
    }
}

impl Devices {
    pub fn get(&self, id: &str) -> Option<&DeviceRecord> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut DeviceRecord> {
        self.devices.iter_mut().find(|d| d.id == id)
    }

    pub fn by_slot(&self, slot: SlotId) -> Option<&DeviceRecord> {
        (slot != 0).then(|| self.devices.iter().find(|d| d.slot == slot)).flatten()
    }

    pub fn upsert(&mut self, record: DeviceRecord) {
        match self.get_mut(&record.id) {
            Some(existing) => *existing = record,
            None => self.devices.push(record),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<DeviceRecord> {
        let idx = self.devices.iter().position(|d| d.id == id)?;
        Some(self.devices.remove(idx))
    }

    /// Enforces uniqueness after loading: duplicate ids keep the first entry;
    /// duplicate slots keep the first and set later ones to 0.
    pub fn normalize(&mut self) {
        let mut ids = HashSet::new();
        self.devices.retain(|d| ids.insert(d.id.clone()));
        let mut slots = HashSet::new();
        for d in &mut self.devices {
            if d.slot as usize > ac_core::MAX_SLOTS {
                d.slot = 0;
            }
            if d.slot != 0 && !slots.insert(d.slot) {
                d.slot = 0;
            }
        }
    }
}
