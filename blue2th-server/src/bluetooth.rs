// SPDX-License-Identifier: MIT OR Apache-2.0

//! BlueZ access for the backend, via `bluer` (D-Bus). Requires `bluetoothd`
//! running on the host. Phase 1 only reads adapters and paired devices; scanning
//! and connect/disconnect land in later phases (see `docs/ROADMAP.md`).

use async_stream::try_stream;
use blue2th_proto::{AdapterInfo, DeviceInfo};
use bluer::{agent::Agent, AdapterEvent, Address, Device, Session};
use futures::{Stream, StreamExt};

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

/// Why a `connect` attempt failed: at the **pairing** step, or anywhere else.
///
/// The two are not the same story for the user. A failed pairing means the
/// speaker was not in pairing mode, is out of range, or refused the bond — the
/// app must let the user retry. Any other BlueZ failure on an already-paired
/// device means the hardware is the suspect, which is what the greyed-out row is
/// for. Kept typed rather than folded into a message: the HTTP status the app
/// reads is derived from it (`502` vs `500`).
#[derive(Debug)]
pub enum ConnectError {
    /// `device.pair()` failed, or the BlueZ agent could not be registered for it.
    Pairing(bluer::Error),
    /// Any other BlueZ failure: no adapter, connect refused, properties unreadable.
    Bluetooth(bluer::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `bluer::Error` already renders as "kind: message"; the pairing arm only
        // says at which step it happened, so the operator reads both.
        match self {
            ConnectError::Pairing(err) => write!(f, "pairing failed: {err}"),
            ConnectError::Bluetooth(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Pair (if needed), trust, and connect a device on the default adapter.
/// Trusting lets BlueZ reconnect its audio profiles without re-confirmation.
///
/// Pairing needs an agent, or BlueZ answers `Pair()` with "No agent available"
/// and the user has to go and confirm the bond on the PC — the very thing the
/// phone is here to avoid. The agent registered here has every handler `None`,
/// which publishes `NoInputNoOutput` (Just Works): while it is registered, the
/// host accepts a bond without confirmation. That is a security trade-off, and
/// it is only acceptable because the registration lasts exactly the span of one
/// user-initiated `pair()` — never lift it to startup.
pub async fn connect_device(addr: Address) -> Result<DeviceInfo, ConnectError> {
    let session = Session::new().await.map_err(ConnectError::Bluetooth)?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(ConnectError::Bluetooth)?;
    let device = adapter.device(addr).map_err(ConnectError::Bluetooth)?;

    if !device.is_paired().await.unwrap_or(false) {
        // Registered on the very session that issues `Pair()`: BlueZ resolves
        // the agent from the D-Bus sender, so this needs no default-agent
        // privilege. The handle unregisters on drop, hence the explicit block —
        // a `let _ = …` binding would drop it before `pair()` even runs.
        let handle = session
            .register_agent(Agent::default())
            .await
            // Failing to register is our side failing, not the speaker's, but it
            // is still the pairing step that could not happen: typed as such so
            // the app offers a retry instead of greying the row out.
            .map_err(ConnectError::Pairing)?;
        let paired = device.pair().await;
        drop(handle);
        paired.map_err(ConnectError::Pairing)?;
    }
    device
        .set_trusted(true)
        .await
        .map_err(ConnectError::Bluetooth)?;
    device.connect().await.map_err(ConnectError::Bluetooth)?;
    device_info(&device).await.map_err(ConnectError::Bluetooth)
}

/// Connect a device that is **already paired**, without ever pairing it.
///
/// The auto-reconnect pass (phase 6.5) dials remembered speakers unattended, so
/// it must not create a bond: an address whose bond disappeared (unpaired from
/// the desktop, adapter swapped) is refused rather than paired again behind the
/// user's back. `/connect` keeps [`connect_device`] — there the user is asking.
pub async fn connect_paired_device(addr: Address) -> bluer::Result<DeviceInfo> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    let device = adapter.device(addr)?;

    if !device.is_paired().await.unwrap_or(false) {
        return Err(bluer::Error {
            kind: bluer::ErrorKind::NotFound,
            message: format!("{addr} is not a paired device"),
        });
    }
    device.connect().await?;
    device_info(&device).await
}

/// Disconnect a device on the default adapter.
pub async fn disconnect_device(addr: Address) -> bluer::Result<DeviceInfo> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    let device = adapter.device(addr)?;

    device.disconnect().await?;
    device_info(&device).await
}

/// Stream devices discovered by an active scan on the default adapter.
///
/// Powers the adapter on, starts discovery, and yields a `DeviceInfo` for every
/// `DeviceAdded` event. Discovery stops when the returned stream is dropped (the
/// caller, e.g. the SSE handler, drops it on client disconnect or after a cap).
pub fn scan_events() -> impl Stream<Item = bluer::Result<DeviceInfo>> {
    try_stream! {
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

        let mut events = adapter.discover_devices().await?;
        while let Some(event) = events.next().await {
            if let AdapterEvent::DeviceAdded(addr) = event {
                let device = adapter.device(addr)?;
                yield device_info(&device).await?;
            }
        }
    }
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
