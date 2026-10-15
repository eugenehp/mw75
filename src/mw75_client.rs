//! BLE scanning, connecting, activation, and the command API for MW75 Neuro.
//!
//! The MW75 uses a two-phase connection model:
//!
//! 1. **BLE activation** — discover the device, connect via BLE, write
//!    activation commands (enable EEG, enable raw mode, query battery),
//!    and verify responses on the status characteristic.
//!
//! 2. **RFCOMM data streaming** — after BLE activation the device starts
//!    broadcasting 63-byte EEG packets on RFCOMM channel 25.
//!
//! With the `rfcomm` Cargo feature enabled, use
//! `crate::rfcomm::start_rfcomm_stream` to automatically connect and
//! stream data into the [`Mw75Handle`] after BLE activation.
//!
//! Without the `rfcomm` feature, use [`Mw75Handle::feed_data`] to push
//! raw RFCOMM bytes from any external transport.
//!
//! # Full connection example (with `rfcomm` feature)
//!
//! ```no_run
//! use mw75::prelude::*;
//! use std::sync::Arc;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let client = Mw75Client::new(Mw75ClientConfig::default());
//! let (mut rx, handle) = client.connect().await?;
//! handle.start().await?;
//!
//! // Disconnect BLE before starting RFCOMM
//! let addr = handle.peripheral_id();
//! handle.disconnect_ble().await?;
//!
//! // Start RFCOMM data stream (requires `rfcomm` feature)
//! let handle = Arc::new(handle);
//! # #[cfg(feature = "rfcomm")]
//! let _rfcomm = start_rfcomm_stream(handle.clone(), &addr).await?;
//!
//! while let Some(event) = rx.recv().await {
//!     match event {
//!         Mw75Event::Eeg(pkt) => println!("counter={}", pkt.counter),
//!         Mw75Event::Disconnected => break,
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use uuid::Uuid;
use webbluetooth::stream::{select_all, StreamExt};
use webbluetooth::{
    Bluetooth, BluetoothDevice, BluetoothUuid, Candidate, DeviceFilter, Grant, LeScan,
    LeScanOptions, RemoteGattCharacteristic, RemoteGattServer,
};

use crate::parse::PacketProcessor;
use crate::protocol::{
    SampleRate, BATTERY_CMD, BLE_ACTIVATION_DELAY_MS, BLE_BATTERY_COMMAND, BLE_COMMAND_DELAY_MS,
    BLE_DISCOVERY_TIMEOUT_SECS, BLE_EEG_COMMAND, BLE_RAW_MODE_COMMAND, BLE_RFCOMM_STATUS_COMMAND,
    BLE_SUCCESS_CODE, BLE_UNKNOWN_E0_COMMAND, DISABLE_EEG_CMD, DISABLE_RAW_MODE_CMD,
    ENABLE_EEG_CMD, ENABLE_RAW_MODE_CMD, MW75_COMMAND_CHAR, MW75_DEVICE_NAME_PATTERN,
    MW75_SERVICE_UUID, MW75_STATUS_CHAR,
};
use crate::types::{ActivationStatus, BatteryInfo, Mw75Event};

// ── Mw75Device ────────────────────────────────────────────────────────────────

/// An MW75 device discovered during a BLE scan.
///
/// Returned by [`Mw75Client::scan_all`]; pass to [`Mw75Client::connect_to`]
/// to establish a connection.
#[derive(Clone, Debug)]
pub struct Mw75Device {
    /// Advertised device name (e.g. `"MW75 Neuro"`).
    pub name: String,
    /// Platform BLE identifier.
    pub id: String,
}

// ── Mw75ClientConfig ──────────────────────────────────────────────────────────

/// Configuration for [`Mw75Client`].
#[derive(Debug, Clone)]
pub struct Mw75ClientConfig {
    /// BLE scan duration in seconds before giving up. Default: `4`.
    pub scan_timeout_secs: u64,
    /// Match devices whose advertised name contains this string
    /// (case-insensitive). Default: `"MW75"`.
    pub name_pattern: String,
    /// EEG streaming sample rate. Default: [`SampleRate::Hz500`].
    ///
    /// * [`SampleRate::Hz500`] — enables raw mode (full ADC data).
    /// * [`SampleRate::Hz256`] — disables raw mode (processed data).
    pub sample_rate: SampleRate,
}

impl Default for Mw75ClientConfig {
    fn default() -> Self {
        Self {
            scan_timeout_secs: BLE_DISCOVERY_TIMEOUT_SECS,
            name_pattern: MW75_DEVICE_NAME_PATTERN.into(),
            sample_rate: SampleRate::default(),
        }
    }
}

// ── Mw75Client ────────────────────────────────────────────────────────────────

/// BLE client for MW75 Neuro EEG headphones.
///
/// Handles scanning, connecting, and the BLE activation handshake.
/// After activation, EEG data arrives over RFCOMM; use
/// [`Mw75Handle::feed_data`] to push raw RFCOMM bytes into the built-in
/// packet processor.
///
/// # Architecture
///
/// ```text
///   BLE scan → connect → activate (enable EEG + raw mode)
///                                    ↓
///                        RFCOMM data → feed_data() → Mw75Event::Eeg
/// ```
pub struct Mw75Client {
    config: Mw75ClientConfig,
}

impl Mw75Client {
    /// Create a new client with the given configuration.
    pub fn new(config: Mw75ClientConfig) -> Self {
        Self { config }
    }

    // ── Public: scan ─────────────────────────────────────────────────────────

    /// Scan for **all** nearby MW75 devices and return them.
    ///
    /// The scan runs for `config.scan_timeout_secs` seconds.
    pub async fn scan_all(&self) -> Result<Vec<Mw75Device>> {
        let bluetooth = Bluetooth::shared();

        info!(
            "scan_all: scanning for {} s …",
            self.config.scan_timeout_secs
        );

        // Use service UUID filter — on macOS CoreBluetooth this is required
        // to discover already-paired devices that are not actively advertising.
        let mut scan = bluetooth
            .request_le_scan(
                LeScanOptions::new()
                    .filter(DeviceFilter::new().service(MW75_SERVICE_UUID)?)
                    .keep_repeated_devices(true)
                    .accept_all_manufacturer_data(),
            )
            .await?;

        // A device's name can arrive in a later packet than the one that
        // first matched, so sightings are folded together per device.
        let seen = scan
            .collect_for(Duration::from_secs(self.config.scan_timeout_secs))
            .await;
        scan.stop();

        let upper_pattern = self.config.name_pattern.to_uppercase();
        let mut found = vec![];
        for candidate in &seen {
            let name = candidate_name(candidate);
            let id = candidate.id.clone();
            debug!("scan_all: saw peripheral: name={name:?}  id={id}");

            if match_candidate(candidate, &upper_pattern).is_some() {
                let display_name = if name.is_empty() {
                    "MW75 (matched by UUID/mfg)".to_owned()
                } else {
                    name
                };
                info!("scan_all: found {display_name}  id={id}");
                found.push(Mw75Device {
                    name: display_name,
                    id,
                });
            }
        }
        info!("scan_all: {} device(s) found", found.len());
        Ok(found)
    }

    // ── Public: connect_to ────────────────────────────────────────────────────

    /// Connect to a specific device returned by [`Mw75Client::scan_all`], run the BLE
    /// activation handshake, and return an event receiver + handle.
    pub async fn connect_to(
        &self,
        device: Mw75Device,
    ) -> Result<(mpsc::Receiver<Mw75Event>, Mw75Handle)> {
        let peripheral = Bluetooth::shared()
            .adopt_device(&device.id, Grant::all_services())
            .await?;
        self.setup_peripheral(peripheral, device.name).await
    }

    // ── Public: connect (convenience) ────────────────────────────────────────

    /// Scan for the first MW75 device, connect, and return an event channel.
    ///
    /// Starts a generic BLE scan (no service filter) and matches peripherals
    /// by name, service UUID, or manufacturer data.
    pub async fn connect(&self) -> Result<(mpsc::Receiver<Mw75Event>, Mw75Handle)> {
        let bluetooth = Bluetooth::shared();

        let timeout = self.config.scan_timeout_secs;
        let pattern = &self.config.name_pattern;

        info!("Scanning for MW75 devices (timeout: {timeout} s) …");
        let mut scan = bluetooth
            .request_le_scan(
                LeScanOptions::accept_all_advertisements()
                    .keep_repeated_devices(true)
                    .accept_all_manufacturer_data(),
            )
            .await?;
        let candidate = self.find_first(&mut scan, pattern, timeout).await?;
        scan.stop();

        let device_name = match candidate_name(&candidate) {
            name if name.is_empty() => format!("MW75 ({})", candidate.id),
            name => name,
        };
        info!("Found device: {device_name}");

        let peripheral = bluetooth
            .adopt_candidate(&candidate, Grant::all_services())
            .await?;
        self.setup_peripheral(peripheral, device_name).await
    }

    // ── Private: setup_peripheral ─────────────────────────────────────────────

    async fn setup_peripheral(
        &self,
        peripheral: BluetoothDevice,
        device_name: String,
    ) -> Result<(mpsc::Receiver<Mw75Event>, Mw75Handle)> {
        let gatt = peripheral.gatt();

        // Connect with timeout
        tokio::time::timeout(Duration::from_secs(10), gatt.connect())
            .await
            .map_err(|_| anyhow!("BLE connect() timed out after 10 s"))??;

        #[cfg(target_os = "linux")]
        tokio::time::sleep(Duration::from_millis(600)).await;

        let chars = tokio::time::timeout(Duration::from_secs(15), gatt.get_characteristics())
            .await
            .map_err(|_| anyhow!("service discovery timed out after 15 s"))??;
        info!("Connected and services discovered: {device_name}");

        // Log all discovered characteristics for diagnostics
        for c in &chars {
            info!(
                "  characteristic: uuid={} properties={:?}",
                c.uuid(),
                c.properties()
            );
        }

        let find_char = |uuid: Uuid| -> Result<RemoteGattCharacteristic> {
            chars
                .iter()
                .find(|c| c.uuid().as_uuid() == uuid)
                .cloned()
                .ok_or_else(|| anyhow!("Characteristic {uuid} not found"))
        };

        // Find required characteristics
        let command_char = find_char(MW75_COMMAND_CHAR)?;
        let status_char = find_char(MW75_STATUS_CHAR)?;

        // Subscribe to ALL notifiable characteristics (status + data chars).
        // EEG data arrives on the data characteristics (1103, 1106) via
        // BLE GATT notifications
        let mut subscriptions = Vec::new();
        let mut subscribed_chars: Vec<Uuid> = Vec::new();
        for c in &chars {
            if c.properties().notify() || c.properties().indicate() {
                match c.start_notifications().await {
                    Ok(notifications) => {
                        info!("BLE notifications enabled on {}", c.uuid());
                        subscribed_chars.push(c.uuid().as_uuid());
                        subscriptions.push(notifications.tagged());
                    }
                    Err(e) => {
                        warn!("⚠ Failed to subscribe to {}: {e}", c.uuid());
                    }
                }
            }
        }

        // Always ensure the status characteristic is subscribed
        if !subscribed_chars.contains(&MW75_STATUS_CHAR) {
            subscriptions.push(status_char.start_notifications().await?.tagged());
            info!("BLE notifications enabled on status characteristic");
            subscribed_chars.push(MW75_STATUS_CHAR);
        }

        info!(
            "Subscribed to {} characteristic(s): {}",
            subscribed_chars.len(),
            subscribed_chars
                .iter()
                .map(|u| {
                    let s = u.to_string();
                    // Short label for known chars
                    if s.contains("1102") {
                        "STATUS".to_string()
                    } else if s.contains("1103") {
                        "DATA_1103".to_string()
                    } else if s.contains("1105") {
                        "STATUS_ALT".to_string()
                    } else if s.contains("1106") {
                        "DATA_1106".to_string()
                    } else {
                        s[..8].to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Event channel
        let (tx, rx) = mpsc::channel::<Mw75Event>(256);
        let _ = tx.send(Mw75Event::Connected(device_name.clone())).await;

        // Shared flag: set before intentional BLE disconnects (e.g. pre-RFCOMM)
        let ble_disconnect_expected = Arc::new(AtomicBool::new(false));

        // Spawn notification handler for BLE responses AND data streaming.
        // It doubles as the disconnect watcher: the subscriptions end when
        // the link drops.
        let notification_tx = tx.clone();
        let expected_flag = ble_disconnect_expected.clone();
        let status_char_uuid = MW75_STATUS_CHAR;
        // Shared processor for BLE data notifications
        let ble_processor = Arc::new(std::sync::Mutex::new(PacketProcessor::new(false)));
        let ble_processor_clone = ble_processor.clone();
        tokio::spawn(async move {
            let mut notifications = select_all(subscriptions);
            info!("BLE notification stream active, waiting for data…");

            let mut notif_count: u64 = 0;
            let mut notif_counts_by_char: std::collections::HashMap<Uuid, u64> =
                std::collections::HashMap::new();

            while let Some((notif_uuid, value)) = notifications.next().await {
                let notif_uuid = notif_uuid.as_uuid();
                let data = &value;
                notif_count += 1;
                let char_count = notif_counts_by_char.entry(notif_uuid).or_insert(0);
                *char_count += 1;

                // Build hex string for logging
                let hex_str: String = if data.len() <= 64 {
                    data.iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    let head: String = data[..32]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("{head} … ({} bytes total)", data.len())
                };

                // Determine characteristic label
                let char_short = match notif_uuid {
                    u if u == status_char_uuid => "STATUS_1102",
                    u if u == Uuid::from_u128(0x00001103_d102_11e1_9b23_00025b00a5a5) => {
                        "DATA_1103"
                    }
                    u if u == Uuid::from_u128(0x00001105_d102_11e1_9b23_00025b00a5a6) => {
                        "STATUS_1105"
                    }
                    u if u == Uuid::from_u128(0x00001106_d102_11e1_9b23_00025b00a5a6) => {
                        "DATA_1106"
                    }
                    _ => "OTHER",
                };

                // Log first 50 notifications per characteristic at info level,
                // then switch to debug to avoid flooding
                if *char_count <= 50 {
                    info!(
                        "📨 {char_short} #{char_count} ({} B): {hex_str}",
                        data.len()
                    );
                } else if *char_count == 51 {
                    info!("📨 {char_short} — suppressing further raw logs (received 50+)");
                }

                // Status characteristic: parse command responses
                if notif_uuid == status_char_uuid
                    && data.len() >= 5
                    && data[0] == 0x09
                    && data[1] == 0x9A
                {
                    let cmd_type = data[3];
                    let status = data[4];

                    if cmd_type == BLE_EEG_COMMAND {
                        if status == BLE_SUCCESS_CODE {
                            info!("EEG mode confirmed enabled");
                        } else {
                            warn!("EEG mode response: status=0x{status:02x} (expected 0xF1)");
                        }
                    } else if cmd_type == BLE_RAW_MODE_COMMAND {
                        if status == BLE_SUCCESS_CODE {
                            info!("Raw mode confirmed enabled");
                        } else {
                            warn!("Raw mode response: status=0x{status:02x} (0xF0=failed, may need retry)");
                        }
                    } else if cmd_type == BLE_BATTERY_COMMAND {
                        let level = if status == BLE_SUCCESS_CODE && data.len() >= 6 {
                            data[5]
                        } else {
                            status
                        };
                        info!("Battery level: {level}%");
                        let _ = notification_tx
                            .send(Mw75Event::Battery(BatteryInfo { level }))
                            .await;
                    } else if cmd_type == BLE_RFCOMM_STATUS_COMMAND {
                        let state = match status {
                            0x00 => "not connected",
                            0x01 => "connected",
                            _ => "unknown",
                        };
                        debug!("RFCOMM status: 0x{status:02x} ({state})");
                    } else if cmd_type == BLE_UNKNOWN_E0_COMMAND {
                        debug!("Unknown E0 command response: status=0x{status:02x}");
                    } else {
                        debug!(
                            "Unhandled BLE response: cmd=0x{cmd_type:02x} status=0x{status:02x} (feature_id=0x{:02x})",
                            data[2]
                        );
                    }
                    continue;
                }

                // Any other notification: try to parse as EEG data
                let events = {
                    let mut proc = ble_processor_clone.lock().unwrap();
                    proc.process_data(data)
                };
                if !events.is_empty() {
                    info!(
                        "✅ Parsed {} EEG packet(s) from {char_short} ({} bytes)",
                        events.len(),
                        data.len()
                    );
                    for event in events {
                        let _ = notification_tx.send(event).await;
                    }
                } else if data.len() > 5 && *char_count <= 50 {
                    // Show first byte analysis for non-parsed data
                    let first = data[0];
                    let annotation = if first == 0xAA {
                        format!(
                            "sync=0xAA event_id={} len={} counter={}",
                            data.get(1).unwrap_or(&0),
                            data.get(2).unwrap_or(&0),
                            data.get(3).unwrap_or(&0)
                        )
                    } else {
                        format!("first_byte=0x{first:02x} (not 0xAA sync)")
                    };
                    info!("   ↳ no packet parsed: {annotation}");
                }

                // Periodic summary every 100 notifications
                if notif_count.is_multiple_of(100) {
                    let summary: String = notif_counts_by_char
                        .iter()
                        .map(|(uuid, count)| {
                            let lbl = match *uuid {
                                u if u == status_char_uuid => "STATUS_1102",
                                u if u
                                    == Uuid::from_u128(0x00001103_d102_11e1_9b23_00025b00a5a5) =>
                                {
                                    "DATA_1103"
                                }
                                u if u
                                    == Uuid::from_u128(0x00001105_d102_11e1_9b23_00025b00a5a6) =>
                                {
                                    "STATUS_1105"
                                }
                                u if u
                                    == Uuid::from_u128(0x00001106_d102_11e1_9b23_00025b00a5a6) =>
                                {
                                    "DATA_1106"
                                }
                                _ => "OTHER",
                            };
                            format!("{lbl}={count}")
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    info!("📊 Notification total: {notif_count} — {summary}");
                }
            }

            if expected_flag.load(Ordering::SeqCst) {
                info!("BLE notification stream ended: BLE disconnect expected (pre-RFCOMM), suppressing event");
            } else {
                info!("BLE notification stream ended: MW75 disconnected unexpectedly");
                let _ = notification_tx.send(Mw75Event::Disconnected).await;
            }
        });

        let handle = Mw75Handle {
            peripheral,
            command_char,
            processor: std::sync::Mutex::new(PacketProcessor::new(false)),
            tx,
            device_name: device_name.clone(),
            ble_disconnect_expected,
            sample_rate: self.config.sample_rate,
        };

        Ok((rx, handle))
    }

    // ── Private: find_first ───────────────────────────────────────────────────

    /// Discover the first MW75 peripheral using multiple heuristics:
    ///
    /// 1. **Name match** — `local_name` contains the pattern (e.g. "MW75").
    /// 2. **Service UUID match** — advertised services include `MW75_SERVICE_UUID`.
    /// 3. **Manufacturer data match** — mfg data bytes contain the pattern as ASCII.
    ///
    /// On macOS CoreBluetooth, `local_name` is often `None` for already-paired
    /// devices, so (2) and (3) are critical fallbacks.
    async fn find_first(
        &self,
        scan: &mut LeScan,
        pattern: &str,
        timeout_secs: u64,
    ) -> Result<Candidate> {
        let upper_pattern = pattern.to_uppercase();
        let mut seen: Vec<Candidate> = vec![];

        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            while let Some(sighting) = scan.next().await {
                let (candidate, first_sighting) = merge_sighting(&mut seen, sighting);
                let id = &candidate.id;
                let name = candidate_name(candidate);
                let name = if name.is_empty() { "<none>" } else { &name };

                // Log each peripheral once for debugging
                if first_sighting {
                    let adv = &candidate.advertisement;
                    let svc_summary: String = adv
                        .service_uuids
                        .iter()
                        .map(|u| u.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mfg_summary: String = adv
                        .manufacturer_data
                        .iter()
                        .map(|(k, v)| format!("0x{k:04X}[{}B]", v.len()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    debug!(
                        "scan: id={id}  name={name:?}  services=[{svc_summary}]  mfg=[{mfg_summary}]"
                    );
                }

                if let Some(how) = match_candidate(candidate, &upper_pattern) {
                    info!("Matched by {how}: name={name:?}  id={id}");
                    return Some(candidate.clone());
                }
            }
            None
        })
        .await;

        if !matches!(result, Ok(Some(_))) {
            warn!(
                "Scan saw {} peripheral(s) total but none matched '{pattern}' \
                 by name, service UUID, or manufacturer data",
                seen.len()
            );
        }

        result
            .ok()
            .flatten()
            .ok_or_else(|| anyhow!("Timed out scanning for an MW75 device after {timeout_secs} s"))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// The best name known for a scanned device, or an empty string.
fn candidate_name(candidate: &Candidate) -> String {
    candidate
        .name
        .clone()
        .or_else(|| candidate.advertisement.local_name.clone())
        .unwrap_or_default()
}

/// Fold a sighting into the list of devices seen so far.
///
/// Returns the merged entry and whether this was the device's first sighting.
fn merge_sighting(seen: &mut Vec<Candidate>, sighting: Candidate) -> (&Candidate, bool) {
    match seen.iter().position(|c| c.id == sighting.id) {
        Some(i) => {
            seen[i].merge(sighting);
            (&seen[i], false)
        }
        None => {
            seen.push(sighting);
            (&seen[seen.len() - 1], true)
        }
    }
}

/// Decide whether a scanned device is an MW75, and by which heuristic.
///
/// `upper_pattern` is the configured name pattern, already upper-cased.
fn match_candidate(candidate: &Candidate, upper_pattern: &str) -> Option<&'static str> {
    let adv = &candidate.advertisement;

    // Match 1: name contains pattern
    let name = candidate_name(candidate);
    if !name.is_empty() && name.to_uppercase().contains(upper_pattern) {
        return Some("name");
    }

    // Match 2: advertised services include MW75_SERVICE_UUID
    let service = BluetoothUuid::from(MW75_SERVICE_UUID);
    if adv.service_uuids.contains(&service) || adv.overflow_service_uuids.contains(&service) {
        return Some("service UUID");
    }

    // Match 3: manufacturer data contains pattern as ASCII
    let pattern_bytes = upper_pattern.as_bytes();
    let in_mfg_data = adv.manufacturer_data.values().any(|data| {
        let upper: Vec<u8> = data.iter().map(|b| b.to_ascii_uppercase()).collect();
        upper
            .windows(pattern_bytes.len())
            .any(|w| w == pattern_bytes)
    });
    in_mfg_data.then_some("manufacturer data")
}

/// Drop the BLE link and wait for the disconnect to complete.
async fn disconnect_and_wait(gatt: &RemoteGattServer) {
    let _ = tokio::time::timeout(Duration::from_secs(5), gatt.disconnect_and_wait()).await;
}

// ── Mw75Handle ────────────────────────────────────────────────────────────────

/// A handle to an active MW75 connection for sending commands and feeding data.
pub struct Mw75Handle {
    peripheral: BluetoothDevice,
    command_char: RemoteGattCharacteristic,
    processor: std::sync::Mutex<PacketProcessor>,
    tx: mpsc::Sender<Mw75Event>,
    device_name: String,
    /// Set to `true` before an intentional BLE disconnect (e.g. pre-RFCOMM)
    /// so the notification handler doesn't fire a spurious
    /// `Mw75Event::Disconnected`.
    ble_disconnect_expected: Arc<AtomicBool>,
    /// Configured EEG sample rate (determines whether raw mode is used).
    sample_rate: SampleRate,
}

impl Mw75Handle {
    /// Write a raw command to the MW75 command characteristic.
    ///
    /// If BLE is currently disconnected (e.g. for RFCOMM), this will
    /// temporarily reconnect, send the command, and disconnect again.
    pub async fn write_command(&self, cmd: &[u8]) -> Result<()> {
        if self.peripheral.gatt().connected() {
            self.command_char.write_value_with_response(cmd).await?;
        } else {
            self.ble_send_with_reconnect(&[cmd]).await?;
        }
        Ok(())
    }

    /// Temporarily reconnect BLE, send one or more commands, then disconnect.
    ///
    /// Used when RFCOMM is active and BLE was previously disconnected.
    /// The reconnect is brief — just long enough to write the commands.
    async fn ble_send_with_reconnect(&self, cmds: &[&[u8]]) -> Result<()> {
        info!("BLE not connected — reconnecting temporarily to send command(s)…");

        // Mark disconnect as expected so watchers don't fire
        self.ble_disconnect_expected.store(true, Ordering::SeqCst);

        let gatt = self.peripheral.gatt();

        // Reconnect with timeout
        tokio::time::timeout(Duration::from_secs(10), gatt.connect())
            .await
            .map_err(|_| anyhow!("BLE reconnect timed out after 10 s"))??;

        // Rediscover services (required after reconnect) and find the command
        // characteristic again: handles from the previous connection are stale
        let command_char =
            tokio::time::timeout(Duration::from_secs(10), gatt.get_characteristics())
                .await
                .map_err(|_| anyhow!("service discovery timed out after 10 s"))??
                .into_iter()
                .find(|c| c.uuid().as_uuid() == MW75_COMMAND_CHAR)
                .ok_or_else(|| anyhow!("Command characteristic not found after BLE reconnect"))?;

        info!("BLE reconnected, sending {} command(s)…", cmds.len());

        // Send all commands
        for cmd in cmds {
            command_char.write_value_with_response(cmd).await?;
            tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;
        }

        // Disconnect BLE again (RFCOMM stays active)
        disconnect_and_wait(&gatt).await;
        info!("BLE disconnected again after sending command(s)");
        Ok(())
    }

    /// Run the full BLE activation sequence: enable EEG → (optionally enable
    /// raw mode) → query battery.
    ///
    /// This mirrors the Python `BLEManager._send_activation_sequence()`.
    ///
    /// The raw mode command is only sent when the configured sample rate is
    /// [`SampleRate::Hz500`].  At [`SampleRate::Hz256`], EEG mode alone is
    /// enabled (raw mode is left disabled), and the device streams processed
    /// data at 256 Hz.
    ///
    /// After this returns, the MW75 will start streaming EEG packets over
    /// RFCOMM channel 25.
    ///
    /// If BLE is disconnected (e.g. during RFCOMM streaming), this will
    /// temporarily reconnect to send the commands, then disconnect again.
    pub async fn start(&self) -> Result<()> {
        let connected = self.peripheral.gatt().connected();
        let raw_mode = self.sample_rate.needs_raw_mode();

        info!(
            "Activation: sample_rate={}, raw_mode={}",
            self.sample_rate, raw_mode
        );

        if !connected {
            // Batch all commands via a single BLE reconnect cycle
            info!("Resuming EEG via temporary BLE reconnect…");
            let mut cmds: Vec<&[u8]> = vec![&ENABLE_EEG_CMD];
            if raw_mode {
                cmds.push(&ENABLE_RAW_MODE_CMD);
            }
            cmds.push(&BATTERY_CMD);
            self.ble_send_with_reconnect(&cmds).await?;
        } else {
            info!("Sending ENABLE_EEG…");
            self.write_command(&ENABLE_EEG_CMD).await?;
            tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;

            if raw_mode {
                info!("Sending ENABLE_RAW_MODE…");
                self.write_command(&ENABLE_RAW_MODE_CMD).await?;
                tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;
            } else {
                info!("Skipping ENABLE_RAW_MODE (256 Hz mode)");
            }

            info!("Getting battery level…");
            self.write_command(&BATTERY_CMD).await?;
            tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;
        }

        let _ = self
            .tx
            .send(Mw75Event::Activated(ActivationStatus {
                eeg_enabled: true,
                raw_mode_enabled: raw_mode,
            }))
            .await;

        info!("BLE activation sequence complete ({})", self.sample_rate);
        Ok(())
    }

    /// Send the disable sequence to stop EEG streaming.
    ///
    /// Mirrors `BLEManager._send_disable_sequence()`.
    ///
    /// If raw mode was enabled (500 Hz), both raw mode and EEG are disabled.
    /// If only EEG mode was enabled (256 Hz), only EEG is disabled.
    ///
    /// If BLE is disconnected (e.g. during RFCOMM streaming), this will
    /// temporarily reconnect to send the commands, then disconnect again.
    pub async fn stop(&self) -> Result<()> {
        let connected = self.peripheral.gatt().connected();
        let raw_mode = self.sample_rate.needs_raw_mode();

        if !connected {
            info!("Pausing EEG via temporary BLE reconnect…");
            let mut cmds: Vec<&[u8]> = Vec::new();
            if raw_mode {
                cmds.push(&DISABLE_RAW_MODE_CMD);
            }
            cmds.push(&DISABLE_EEG_CMD);
            self.ble_send_with_reconnect(&cmds).await?;
        } else {
            if raw_mode {
                info!("Sending DISABLE_RAW_MODE…");
                self.write_command(&DISABLE_RAW_MODE_CMD).await?;
                tokio::time::sleep(Duration::from_millis(BLE_ACTIVATION_DELAY_MS)).await;
            }

            info!("Sending DISABLE_EEG…");
            self.write_command(&DISABLE_EEG_CMD).await?;
            tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;
        }

        info!("EEG streaming disabled");
        Ok(())
    }

    /// Feed raw data from the transport (e.g. RFCOMM) into the packet processor.
    ///
    /// Any complete packets found are parsed and dispatched as events on the
    /// `mpsc::Receiver` returned by [`Mw75Client::connect`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(handle: &mw75::mw75_client::Mw75Handle) {
    /// // In your RFCOMM read loop:
    /// let data: Vec<u8> = vec![/* raw bytes from socket */];
    /// handle.feed_data(&data).await;
    /// # }
    /// ```
    pub async fn feed_data(&self, data: &[u8]) {
        let events = {
            let mut proc = self.processor.lock().unwrap();
            proc.process_data(data)
        };
        for event in events {
            let _ = self.tx.send(event).await;
        }
    }

    /// Get a snapshot of the current packet processing statistics.
    pub fn get_stats(&self) -> crate::types::ChecksumStats {
        self.processor.lock().unwrap().get_stats()
    }

    /// Check if the BLE peripheral is still connected.
    pub async fn is_connected(&self) -> bool {
        self.peripheral.gatt().connected()
    }

    /// Gracefully disconnect from the MW75.
    ///
    /// Sends the disable command sequence first, then disconnects BLE.
    pub async fn disconnect(&self) -> Result<()> {
        self.stop().await.ok();
        disconnect_and_wait(&self.peripheral.gatt()).await;
        Ok(())
    }

    /// Send a [`Mw75Event::Disconnected`] event on the channel.
    ///
    /// Used internally by the RFCOMM reader loop when the transport closes.
    pub async fn send_disconnected(&self) {
        let _ = self.tx.send(Mw75Event::Disconnected).await;
    }

    /// Get the Bluetooth address of the connected peripheral.
    ///
    /// Returns the platform-specific peripheral ID string.
    /// On Linux (BlueZ), this is the MAC address in `"AA:BB:CC:DD:EE:FF"` format.
    /// On macOS, this is a UUID string.
    pub fn peripheral_id(&self) -> String {
        self.peripheral
            .address()
            .unwrap_or_else(|| self.peripheral.id().to_owned())
    }

    /// Get the advertised device name (e.g. `"MW75 Neuro"`).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Get the configured EEG sample rate.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Disconnect the BLE link only (keeping the handle alive for RFCOMM).
    ///
    /// On macOS, the BLE connection must be dropped before RFCOMM can connect.
    /// This disconnects BLE without sending disable commands.
    ///
    /// The notification handler will **not** emit
    /// `Mw75Event::Disconnected` — the flag is set before the disconnect so
    /// downstream consumers know the session is still alive (transitioning to
    /// RFCOMM).
    pub async fn disconnect_ble(&self) -> Result<()> {
        info!("Disconnecting BLE (pre-RFCOMM)…");
        self.ble_disconnect_expected.store(true, Ordering::SeqCst);
        disconnect_and_wait(&self.peripheral.gatt()).await;
        info!("BLE disconnected");
        Ok(())
    }
}
