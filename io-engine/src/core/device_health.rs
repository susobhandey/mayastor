//! Device SMART / health.
//!
//! This is implemented at the pool layer rather than inside the SPDK bdev
//! abstraction. Pools are backed by kernel block devices opened as `aio://` or
//! `uring://` bdevs — including NVMe used without VFIO, e.g.
//! `uring:///dev/nvme4n1` — which always have a `/dev` node. Health for such a
//! device is obtained by invoking `smartctl --json` on that node; this single
//! path covers SAS, SATA and kernel NVMe uniformly.
//!
//! VFIO/SPDK-native devices (`pcie://`) have no `/dev` node and are out of
//! scope for this approach (callers resolve only `/dev/...` paths and treat
//! anything else as unsupported).
//!
//! Runtime requirements: the `smartctl` binary must be present in the engine
//! image, the `/dev` node must be visible to the process, and the process needs
//! `CAP_SYS_RAWIO` (smartctl issues SG_IO/ATA pass-through ioctls internally).
//!
//! NOTE: this spawns a subprocess and blocks until it completes. It should be
//! driven from an off-reactor health collector, not from a hot path.

use std::{
    collections::HashMap,
    process::Command,
    sync::Mutex,
};

use nix::errno::Errno;
use once_cell::sync::Lazy;
use serde_json::Value;

use crate::core::CoreError;

/// Static device identity/inventory data. Unlike [`DeviceHealth`]'s counters,
/// this never changes for the device's lifetime, so callers should fetch it
/// once and cache it (see [`cached_nvme_identity`] for the VFIO path) rather
/// than re-reading it on every health poll. `Option` fields are unset when a
/// given device or transport does not report them.
#[derive(Debug, Default, Clone)]
pub struct DeviceIdentity {
    pub model: Option<String>,
    pub model_family: Option<String>,
    pub serial_number: Option<String>,
    pub firmware_revision: Option<String>,
    pub wwn: Option<String>,
    pub capacity_bytes: Option<u64>,
    pub logical_sector_size: Option<u32>,
    pub physical_sector_size: Option<u32>,
    /// RPM, or `Some(0)` for non-rotating media (SSD/NVMe).
    pub rotation_rate: Option<u32>,
    pub form_factor: Option<String>,
    pub transport: Option<String>,
    pub link_speed: Option<String>,
}

/// A single SMART attribute table entry (ATA devices only -- SAS/NVMe report
/// an empty list here since their equivalent thresholds are already
/// first-class fields on [`DeviceHealth`]).
#[derive(Debug, Clone)]
pub struct SmartAttribute {
    pub id: u8,
    pub name: String,
    pub value: u8,
    pub worst: u8,
    pub threshold: u8,
    pub raw_value: u64,
}

/// Cache of [`DeviceIdentity`] for VFIO-bound NVMe devices, keyed by bdev
/// name. Identity requires a separate NVMe Identify Controller admin command
/// from the health log page, and — unlike health — never changes, so it's
/// fetched once per device and reused rather than re-issued on every
/// `GetPoolHealth` call.
static NVME_IDENTITY_CACHE: Lazy<Mutex<HashMap<String, DeviceIdentity>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Look up a cached VFIO NVMe device identity, if one has already been
/// fetched for `name`.
pub fn cached_nvme_identity(name: &str) -> Option<DeviceIdentity> {
    NVME_IDENTITY_CACHE.lock().unwrap().get(name).cloned()
}

/// Cache a freshly-fetched VFIO NVMe device identity for `name`.
pub fn cache_nvme_identity(name: &str, identity: DeviceIdentity) {
    NVME_IDENTITY_CACHE
        .lock()
        .unwrap()
        .insert(name.to_string(), identity);
}

/// Normalised device SMART / health information. `Option` fields are unset when
/// a given device or transport does not report them.
#[derive(Debug, Default, Clone)]
pub struct DeviceHealth {
    /// Raw NVMe critical-warning bit flags (0 == no critical condition). For
    /// SAS/SATA a failed overall SMART status is mapped onto bit 0x04.
    pub critical_warning: u8,
    /// Temperature in degrees Celsius.
    pub temperature_celsius: Option<i16>,
    /// Remaining spare capacity, percent.
    pub available_spare_percent: Option<u8>,
    /// Spare threshold below which the device warns, percent.
    pub available_spare_threshold_percent: Option<u8>,
    /// Estimated device life used, percent (may exceed 100).
    pub percentage_used: Option<u8>,
    pub data_units_read: Option<u128>,
    pub data_units_written: Option<u128>,
    /// Number of host read commands completed.
    pub host_reads: Option<u128>,
    /// Number of host write commands completed.
    pub host_writes: Option<u128>,
    /// Time the controller was busy with I/O, in minutes.
    pub controller_busy_minutes: Option<u128>,
    pub power_cycles: Option<u128>,
    pub power_on_hours: Option<u128>,
    pub unsafe_shutdowns: Option<u128>,
    /// Media / data-integrity (uncorrectable) error count.
    pub media_errors: Option<u128>,
    /// Number of entries in the error-information log.
    pub num_error_log_entries: Option<u128>,
    /// Static device identity/inventory data, see [`DeviceIdentity`].
    pub identity: Option<DeviceIdentity>,
    /// SMART attribute table (ATA only; empty for SAS/NVMe).
    pub smart_attributes: Vec<SmartAttribute>,
}

impl DeviceHealth {
    /// True when no critical-warning bit is set.
    pub fn is_healthy(&self) -> bool {
        self.critical_warning == 0
    }
    pub fn spare_below_threshold(&self) -> bool {
        self.critical_warning & 0x01 != 0
    }
    pub fn temperature_critical(&self) -> bool {
        self.critical_warning & 0x02 != 0
    }
    pub fn reliability_degraded(&self) -> bool {
        self.critical_warning & 0x04 != 0
    }
    pub fn media_read_only(&self) -> bool {
        self.critical_warning & 0x08 != 0
    }
    pub fn volatile_backup_failed(&self) -> bool {
        self.critical_warning & 0x10 != 0
    }

    /// Parse a 512-byte NVMe SMART / Health Information log page (LID 02h),
    /// as returned by a Get Log Page admin command. Used for VFIO-bound NVMe,
    /// which has no `/dev` node for smartctl. Returns `None` if too short.
    pub fn from_nvme_smart(page: &[u8]) -> Option<Self> {
        if page.len() < 512 {
            return None;
        }
        let u16le = |o: usize| u16::from_le_bytes([page[o], page[o + 1]]);
        let u128le = |o: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&page[o..o + 16]);
            u128::from_le_bytes(b)
        };
        let temp_k = u16le(1);
        let temperature_celsius = if temp_k == 0 {
            None
        } else {
            Some(temp_k as i16 - 273)
        };
        Some(Self {
            critical_warning: page[0],
            temperature_celsius,
            available_spare_percent: Some(page[3]),
            available_spare_threshold_percent: Some(page[4]),
            percentage_used: Some(page[5]),
            data_units_read: Some(u128le(32)),
            data_units_written: Some(u128le(48)),
            host_reads: Some(u128le(64)),
            host_writes: Some(u128le(80)),
            controller_busy_minutes: Some(u128le(96)),
            power_cycles: Some(u128le(112)),
            power_on_hours: Some(u128le(128)),
            unsafe_shutdowns: Some(u128le(144)),
            media_errors: Some(u128le(160)),
            num_error_log_entries: Some(u128le(176)),
            // Identity comes from a separate Identify Controller admin
            // command; the caller (device.rs) fills this in from its own
            // cache, keyed by device name, since it never changes.
            identity: None,
            smart_attributes: Vec::new(),
        })
    }
}

/// Parse a 4096-byte NVMe Identify Controller data structure (as returned by
/// an Identify admin command with CNS=1) into [`DeviceIdentity`]. Used for
/// VFIO-bound NVMe, which has no Identify-derived info surfaced any other way.
/// Layout is fixed by the NVMe base spec: SN at byte offset 4 (20 bytes), MN
/// at offset 24 (40 bytes), FR at offset 64 (8 bytes), all ASCII space-padded.
/// Returns `None` if the buffer is too short.
pub fn identity_from_nvme_identify(data: &[u8]) -> Option<DeviceIdentity> {
    if data.len() < 72 {
        return None;
    }
    let ascii_field = |start: usize, len: usize| -> Option<String> {
        let s = std::str::from_utf8(&data[start..start + len]).ok()?;
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    Some(DeviceIdentity {
        model: ascii_field(24, 40),
        model_family: None,
        serial_number: ascii_field(4, 20),
        firmware_revision: ascii_field(64, 8),
        wwn: None,
        capacity_bytes: None,
        logical_sector_size: None,
        physical_sector_size: None,
        rotation_rate: Some(0), // NVMe is always non-rotating media.
        form_factor: None,
        transport: Some("PCIe".to_string()),
        link_speed: None,
    })
}

/// Read SMART / health for a kernel block device by its `/dev` path, via
/// `smartctl --json --all`.
///
/// smartctl uses a bitmask exit status where non-zero frequently just signals
/// SMART warnings while still emitting complete JSON, so the exit code is
/// ignored and the JSON on stdout is parsed regardless.
pub fn read_device_health(path: &str) -> Result<DeviceHealth, CoreError> {
    let output = Command::new("smartctl")
        .args(["--json", "--all", path])
        .output()
        .map_err(|_| CoreError::NotSupported {
            // smartctl binary not found / not executable.
            source: Errno::ENOENT,
        })?;

    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| CoreError::NotSupported { source: Errno::EIO })?;

    // If smartctl could not open / identify the device there is no usable data.
    if json.get("device").is_none() {
        return Err(CoreError::NotSupported {
            source: Errno::ENXIO,
        });
    }

    Ok(parse_smartctl(&json))
}

/// Extract device identity/inventory data (model, serial, capacity, ...) from
/// smartctl JSON. Available uniformly across NVMe, SATA and SAS -- smartctl
/// normalises this regardless of transport, unlike the health counters below.
fn parse_smartctl_identity(j: &Value) -> DeviceIdentity {
    let str_field = |ptr: &str| j.pointer(ptr).and_then(Value::as_str).map(str::to_string);

    let wwn = j.pointer("/wwn").and_then(|w| {
        let naa = w.get("naa").and_then(Value::as_u64)?;
        let oui = w.get("oui").and_then(Value::as_u64)?;
        let id = w.get("id").and_then(Value::as_u64)?;
        Some(format!("{naa:x}{oui:06x}{id:010x}"))
    });

    DeviceIdentity {
        model: str_field("/model_name"),
        model_family: str_field("/model_family"),
        serial_number: str_field("/serial_number"),
        firmware_revision: str_field("/firmware_version"),
        wwn,
        capacity_bytes: j.pointer("/user_capacity/bytes").and_then(Value::as_u64),
        logical_sector_size: j
            .pointer("/logical_block_size")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        physical_sector_size: j
            .pointer("/physical_block_size")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        rotation_rate: j
            .pointer("/rotation_rate")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        form_factor: str_field("/form_factor/name"),
        transport: str_field("/device/protocol"),
        link_speed: str_field("/interface_speed/current/string"),
    }
}

/// Map smartctl JSON (NVMe / SATA / SAS) into [`DeviceHealth`].
fn parse_smartctl(j: &Value) -> DeviceHealth {
    let mut h = DeviceHealth::default();
    h.identity = Some(parse_smartctl_identity(j));

    // Overall pass/fail (present for SATA and SAS). A failed status maps onto
    // the reliability-degraded flag for a uniform `is_healthy()`.
    if let Some(false) = j.pointer("/smart_status/passed").and_then(Value::as_bool) {
        h.critical_warning |= 0x04;
    }

    // Temperature, power-on hours and power cycles are reported uniformly.
    if let Some(t) = j.pointer("/temperature/current").and_then(Value::as_i64) {
        h.temperature_celsius = Some(t as i16);
    }
    if let Some(v) = j.pointer("/power_on_time/hours").and_then(Value::as_u64) {
        h.power_on_hours = Some(v as u128);
    }
    if let Some(v) = j.pointer("/power_cycle_count").and_then(Value::as_u64) {
        h.power_cycles = Some(v as u128);
    }

    // Kernel NVMe: the SMART/health log is reported directly by smartctl. This
    // is the primary NVMe path (NVMe attached via uring/aio, not VFIO).
    if let Some(n) = j.get("nvme_smart_health_information_log") {
        let g = |k: &str| n.get(k).and_then(Value::as_u64);
        if let Some(v) = g("critical_warning") {
            h.critical_warning |= v as u8;
        }
        if let Some(v) = g("available_spare") {
            h.available_spare_percent = Some(v as u8);
        }
        if let Some(v) = g("available_spare_threshold") {
            h.available_spare_threshold_percent = Some(v as u8);
        }
        if let Some(v) = g("percentage_used") {
            h.percentage_used = Some(v as u8);
        }
        if let Some(v) = g("data_units_read") {
            h.data_units_read = Some(v as u128);
        }
        if let Some(v) = g("data_units_written") {
            h.data_units_written = Some(v as u128);
        }
        if let Some(v) = g("host_reads") {
            h.host_reads = Some(v as u128);
        }
        if let Some(v) = g("host_writes") {
            h.host_writes = Some(v as u128);
        }
        if let Some(v) = g("controller_busy_time") {
            h.controller_busy_minutes = Some(v as u128);
        }
        if let Some(v) = g("power_cycles") {
            h.power_cycles = Some(v as u128);
        }
        if let Some(v) = g("power_on_hours") {
            h.power_on_hours = Some(v as u128);
        }
        if let Some(v) = g("unsafe_shutdowns") {
            h.unsafe_shutdowns = Some(v as u128);
        }
        if let Some(v) = g("media_errors") {
            h.media_errors = Some(v as u128);
        }
        if let Some(v) = g("num_err_log_entries") {
            h.num_error_log_entries = Some(v as u128);
        }
        if let Some(t) = n.get("temperature").and_then(Value::as_i64) {
            h.temperature_celsius = Some(t as i16);
        }
    }

    // SATA: vendor attribute table. Fill only fields not already set from the
    // cleaner top-level values above.
    if let Some(table) = j
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
    {
        for attr in table {
            let id = attr.get("id").and_then(Value::as_u64).unwrap_or(0);
            let raw = attr.pointer("/raw/value").and_then(Value::as_u64);

            h.smart_attributes.push(SmartAttribute {
                id: id as u8,
                name: attr
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                value: attr.get("value").and_then(Value::as_u64).unwrap_or(0) as u8,
                worst: attr.get("worst").and_then(Value::as_u64).unwrap_or(0) as u8,
                threshold: attr.get("thresh").and_then(Value::as_u64).unwrap_or(0) as u8,
                raw_value: raw.unwrap_or(0),
            });

            match id {
                5 => {
                    if let Some(v) = raw {
                        h.media_errors = Some(v as u128); // reallocated sectors
                    }
                }
                9 => {
                    if h.power_on_hours.is_none() {
                        if let Some(v) = raw {
                            h.power_on_hours = Some(v as u128);
                        }
                    }
                }
                12 => {
                    if h.power_cycles.is_none() {
                        if let Some(v) = raw {
                            h.power_cycles = Some(v as u128);
                        }
                    }
                }
                190 | 194 => {
                    if h.temperature_celsius.is_none() {
                        if let Some(v) = raw {
                            h.temperature_celsius = Some((v & 0xff) as i16);
                        }
                    }
                }
                187 | 198 => {
                    if let Some(v) = raw {
                        h.num_error_log_entries = Some(v as u128);
                    }
                }
                _ => {}
            }
        }
    }

    // SAS: grown defect list as a media-error signal when nothing else set it.
    if h.media_errors.is_none() {
        if let Some(v) = j.pointer("/scsi_grown_defect_list").and_then(Value::as_u64) {
            h.media_errors = Some(v as u128);
        }
    }

    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_nvme_log() {
        let j: Value = serde_json::from_str(
            r#"{
              "device": {"name": "/dev/nvme4n1", "type": "nvme"},
              "nvme_smart_health_information_log": {
                "critical_warning": 0,
                "temperature": 41,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "percentage_used": 3,
                "data_units_read": 1000,
                "data_units_written": 2000,
                "host_reads": 3000,
                "host_writes": 4000,
                "controller_busy_time": 5,
                "power_cycles": 12,
                "power_on_hours": 4500,
                "unsafe_shutdowns": 1,
                "media_errors": 0,
                "num_err_log_entries": 5
              }
            }"#,
        )
        .unwrap();
        let h = parse_smartctl(&j);
        assert!(h.is_healthy());
        assert_eq!(h.temperature_celsius, Some(41));
        assert_eq!(h.available_spare_percent, Some(100));
        assert_eq!(h.percentage_used, Some(3));
        assert_eq!(h.data_units_written, Some(2000));
        assert_eq!(h.host_reads, Some(3000));
        assert_eq!(h.host_writes, Some(4000));
        assert_eq!(h.controller_busy_minutes, Some(5));
        assert_eq!(h.power_on_hours, Some(4500));
        assert_eq!(h.num_error_log_entries, Some(5));
    }

    #[test]
    fn parses_sata_attributes() {
        let j: Value = serde_json::from_str(
            r#"{
              "device": {"name": "/dev/sda", "type": "sat"},
              "smart_status": {"passed": true},
              "temperature": {"current": 38},
              "power_on_time": {"hours": 1200},
              "power_cycle_count": 44,
              "ata_smart_attributes": {"table": [
                {"id": 5,   "raw": {"value": 0}},
                {"id": 198, "raw": {"value": 3}}
              ]}
            }"#,
        )
        .unwrap();
        let h = parse_smartctl(&j);
        assert!(h.is_healthy());
        assert_eq!(h.temperature_celsius, Some(38));
        assert_eq!(h.power_on_hours, Some(1200));
        assert_eq!(h.power_cycles, Some(44));
        assert_eq!(h.media_errors, Some(0));
        assert_eq!(h.num_error_log_entries, Some(3));
    }

    #[test]
    fn parses_device_identity_and_attribute_table() {
        let j: Value = serde_json::from_str(
            r#"{
              "device": {"name": "/dev/sda", "type": "sat", "protocol": "ATA"},
              "model_name": "ACME SuperDrive 1TB",
              "model_family": "ACME SuperDrive series",
              "serial_number": "S3R14LNUM",
              "firmware_version": "1.0",
              "user_capacity": {"bytes": 1000204886016},
              "logical_block_size": 512,
              "physical_block_size": 4096,
              "rotation_rate": 0,
              "form_factor": {"ata_value": 3, "name": "2.5 inches"},
              "interface_speed": {"current": {"string": "6.0 Gb/s"}},
              "smart_status": {"passed": true},
              "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "value": 100, "worst": 100, "thresh": 10, "raw": {"value": 0}}
              ]}
            }"#,
        )
        .unwrap();
        let h = parse_smartctl(&j);
        let id = h.identity.expect("identity should be populated");
        assert_eq!(id.model, Some("ACME SuperDrive 1TB".to_string()));
        assert_eq!(id.model_family, Some("ACME SuperDrive series".to_string()));
        assert_eq!(id.serial_number, Some("S3R14LNUM".to_string()));
        assert_eq!(id.firmware_revision, Some("1.0".to_string()));
        assert_eq!(id.capacity_bytes, Some(1000204886016));
        assert_eq!(id.logical_sector_size, Some(512));
        assert_eq!(id.physical_sector_size, Some(4096));
        assert_eq!(id.rotation_rate, Some(0));
        assert_eq!(id.form_factor, Some("2.5 inches".to_string()));
        assert_eq!(id.transport, Some("ATA".to_string()));
        assert_eq!(id.link_speed, Some("6.0 Gb/s".to_string()));

        assert_eq!(h.smart_attributes.len(), 1);
        let attr = &h.smart_attributes[0];
        assert_eq!(attr.id, 5);
        assert_eq!(attr.name, "Reallocated_Sector_Ct");
        assert_eq!(attr.value, 100);
        assert_eq!(attr.worst, 100);
        assert_eq!(attr.threshold, 10);
        assert_eq!(attr.raw_value, 0);
    }

    #[test]
    fn nvme_smart_log_reports_no_attribute_table() {
        // SAS/NVMe report an empty attribute list -- their thresholds are
        // already first-class fields on DeviceHealth.
        let j: Value = serde_json::from_str(
            r#"{"device": {"name": "/dev/nvme4n1", "type": "nvme"},
                "nvme_smart_health_information_log": {"critical_warning": 0}}"#,
        )
        .unwrap();
        let h = parse_smartctl(&j);
        assert!(h.smart_attributes.is_empty());
    }

    #[test]
    fn parses_nvme_identify_controller_data() {
        let mut buf = vec![0u8; 4096];
        buf[4..24].copy_from_slice(b"SERIAL1234          ");
        buf[24..64].copy_from_slice(b"Intel Optane P4800X 375GB              ");
        buf[64..72].copy_from_slice(b"E201DP35");

        let id = identity_from_nvme_identify(&buf).expect("should parse");
        assert_eq!(id.serial_number, Some("SERIAL1234".to_string()));
        assert_eq!(id.model, Some("Intel Optane P4800X 375GB".to_string()));
        assert_eq!(id.firmware_revision, Some("E201DP35".to_string()));
        assert_eq!(id.rotation_rate, Some(0));
        assert_eq!(id.transport, Some("PCIe".to_string()));
    }

    #[test]
    fn nvme_identify_too_short_returns_none() {
        assert!(identity_from_nvme_identify(&[0u8; 10]).is_none());
    }

    #[test]
    fn maps_failed_status_to_reliability_flag() {
        let j: Value = serde_json::from_str(
            r#"{"device":{"name":"/dev/sdb"},"smart_status":{"passed":false},
                "temperature":{"current":55}}"#,
        )
        .unwrap();
        let h = parse_smartctl(&j);
        assert!(!h.is_healthy());
        assert!(h.reliability_degraded());
        assert_eq!(h.temperature_celsius, Some(55));
    }
}
