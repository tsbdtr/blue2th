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

use dioxus::prelude::*;

#[post("/api/bluetooth/scan")]
pub async fn scan_devices() -> Result<Vec<String>, ServerFnError> {
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
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

#[post("/api/bluetooth/connect")]
pub async fn connect_device(name: String) -> Result<bool, ServerFnError> {
    let _ = name;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    Ok(true)
}

#[post("/api/bluetooth/disconnect")]
pub async fn disconnect_device(name: String) -> Result<bool, ServerFnError> {
    let _ = name;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(true)
}

#[post("/api/bluetooth/enable")]
pub async fn enable_bluetooth() -> Result<bool, ServerFnError> {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    Ok(true)
}
