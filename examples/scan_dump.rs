//! Diagnostic: dump every BLE peripheral CoreBluetooth/btleplug sees.
//!
//! Usage: cargo run --example scan_dump
//!
//! Scans for 15 s and prints name, id, RSSI, advertised service UUIDs, and
//! manufacturer-data for *every* peripheral — so we can see whether the MW75
//! shows up at all (and under what name / with which services), instead of
//! silently failing the name filter.

use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Bluetooth adapter"))?;

    println!("Scanning for 15 s — listing ALL peripherals…\n");
    adapter.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs(15)).await;
    adapter.stop_scan().await.ok();

    let peripherals = adapter.peripherals().await?;
    println!("Found {} peripheral(s):\n", peripherals.len());

    for p in peripherals {
        let props = match p.properties().await {
            Ok(Some(props)) => props,
            _ => continue,
        };
        let name = props.local_name.unwrap_or_else(|| "<no name>".into());
        let services: Vec<String> = props.services.iter().map(|u| u.to_string()).collect();
        let mfg: Vec<String> = props
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
        println!("    id:       {}", p.id());
        println!("    rssi:     {:?}", props.rssi);
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
