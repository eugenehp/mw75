//! Diagnostic: dump every BLE peripheral the adapter sees.
//!
//! Usage: cargo run --example scan_dump
//!
//! Scans for 15 s and prints name, id, RSSI, advertised service UUIDs, and
//! manufacturer-data for *every* peripheral — so we can see whether the MW75
//! shows up at all (and under what name / with which services), instead of
//! silently failing the name filter.

use std::time::Duration;

use webbluetooth::{Bluetooth, LeScanOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bluetooth = Bluetooth::shared();

    println!("Scanning for 15 s — listing ALL peripherals…\n");
    let mut scan = bluetooth
        .request_le_scan(
            LeScanOptions::accept_all_advertisements()
                .keep_repeated_devices(true)
                .accept_all_manufacturer_data(),
        )
        .await?;

    // Later packets from a device are folded into its first sighting.
    let peripherals = scan.collect_for(Duration::from_secs(15)).await;
    scan.stop();

    println!("Found {} peripheral(s):\n", peripherals.len());

    for p in peripherals {
        let adv = &p.advertisement;
        let name = p
            .name
            .clone()
            .or_else(|| adv.local_name.clone())
            .unwrap_or_else(|| "<no name>".into());
        let services: Vec<String> = adv.service_uuids.iter().map(|u| u.to_string()).collect();
        let mfg: Vec<String> = adv
            .manufacturer_data
            .iter()
            .map(|(id, data)| format!("{id:#06x}=[{}]", hex(data)))
            .collect();
        let mark = if name.to_uppercase().contains("MW75")
            || services.iter().any(|s| s.contains("00001100"))
        {
            "  <<< MW75?"
        } else {
            ""
        };
        println!("• {name}{mark}");
        println!("    id:       {}", p.id);
        println!("    rssi:     {:?}", p.rssi());
        println!("    services: {services:?}");
        println!("    mfg_data: {mfg:?}");
        println!();
    }
    Ok(())
}

fn hex(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
