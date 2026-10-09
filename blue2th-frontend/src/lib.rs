// SPDX-License-Identifier: MIT OR Apache-2.0

//! The app's modules outside the UI. `main.rs` uses them from here instead of
//! declaring them again: a second copy in the binary ran every unit test twice
//! and held its own copy of every static (#168).

pub mod backend;
pub mod deep_link;
pub mod discovery;
pub mod jni_util;
// Presence reports: the JNI hooks on Android (their tokio half is native-only,
// #159), the page lifecycle events in the browser (#160).
pub mod lifecycle;
pub mod settings;
pub mod timer;
