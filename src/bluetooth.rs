// Copyright 2026 Blue2th
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[derive(Debug)]
pub struct BluetoothError(String);

impl std::fmt::Display for BluetoothError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BluetoothError {}

impl BluetoothError {
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

pub async fn scan_devices() -> Result<Vec<String>, BluetoothError> {
    Ok(vec![
        "Blue Speaker".to_string(),
        "HeadPhones Pro".to_string(),
        "Smart Watch X1".to_string(),
        "Galaxy Buds 2".to_string(),
        "AirPods Max".to_string(),
        "Sony WH-1000XM5".to_string(),
        "Bose QC45".to_string(),
        "JBL Flip 6".to_string(),
        "Logitech MX Keys".to_string(),
        "Apple Magic Mouse".to_string(),
        "Xbox Controller".to_string(),
        "PS5 DualSense".to_string(),
        "Fitbit Charge 6".to_string(),
        "Garmin Forerunner".to_string(),
        "Tile Mate".to_string(),
    ])
}

pub async fn connect_device(name: String) -> Result<bool, BluetoothError> {
    let _ = name;
    Ok(true)
}

pub async fn disconnect_device(name: String) -> Result<bool, BluetoothError> {
    let _ = name;
    Ok(true)
}

#[cfg(target_os = "android")]
pub async fn enable_bluetooth_inner() -> Result<bool, BluetoothError> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| BluetoothError::new(e.to_string()))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| BluetoothError::new(e.to_string()))?;

    let adapter = env
        .call_static_method(
            "android/bluetooth/BluetoothAdapter",
            "getDefaultAdapter",
            "()Landroid/bluetooth/BluetoothAdapter;",
            &[],
        )
        .map_err(|e| BluetoothError::new(e.to_string()))?
        .l()
        .map_err(|e| BluetoothError::new(e.to_string()))?;

    if adapter.is_null() {
        return Ok(false);
    }

    let enabled = env
        .call_method(&adapter, "isEnabled", "()Z", &[])
        .map_err(|e| BluetoothError::new(e.to_string()))?
        .z()
        .map_err(|e| BluetoothError::new(e.to_string()))?;

    Ok(enabled)
}

#[cfg(not(target_os = "android"))]
pub async fn enable_bluetooth_inner() -> Result<bool, BluetoothError> {
    Ok(true)
}

pub async fn enable_bluetooth() -> Result<bool, BluetoothError> {
    enable_bluetooth_inner().await
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "android"))]
    #[tokio::test]
    async fn test_enable_bluetooth_inner_returns_ok_on_non_android() {
        let result = super::enable_bluetooth_inner().await;
        assert!(result.is_ok(), "non-Android stub must return Ok");
    }
}
