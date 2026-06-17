//! BlueZ access for the backend, via `bluer` (D-Bus). Requires `bluetoothd`
//! running on the host. Phase 1 only reads adapters and paired devices; scanning
//! and connect/disconnect land in later phases (see `docs/ROADMAP.md`).

use blue2th_proto::{AdapterInfo, DeviceInfo};
use bluer::{Device, Session};

/// List every Bluetooth adapter present on the host.
pub async fn list_adapters() -> bluer::Result<Vec<AdapterInfo>> {
    let session = Session::new().await?;
    let mut adapters = Vec::new();
    for name in session.adapter_names().await? {
        let adapter = session.adapter(&name)?;
        adapters.push(AdapterInfo {
            address: adapter.address().await?.to_string(),
            powered: adapter.is_powered().await?,
            discovering: adapter.is_discovering().await?,
            name,
        });
    }
    Ok(adapters)
}

/// List the paired devices on the host's default adapter.
pub async fn list_paired_devices() -> bluer::Result<Vec<DeviceInfo>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;

    let mut devices = Vec::new();
    for addr in adapter.device_addresses().await? {
        let device = adapter.device(addr)?;
        // /devices only reports bonded devices; skip transient scan results.
        if !device.is_paired().await.unwrap_or(false) {
            continue;
        }
        devices.push(device_info(&device).await?);
    }
    Ok(devices)
}

/// Map a `bluer::Device` to the wire `DeviceInfo`. Optional properties (name,
/// rssi) are treated leniently: a missing value yields `None` rather than failing
/// the whole listing.
async fn device_info(device: &Device) -> bluer::Result<DeviceInfo> {
    Ok(DeviceInfo {
        address: device.address().to_string(),
        name: device.alias().await.ok(),
        paired: device.is_paired().await?,
        connected: device.is_connected().await?,
        rssi: device.rssi().await.unwrap_or(None),
    })
}
