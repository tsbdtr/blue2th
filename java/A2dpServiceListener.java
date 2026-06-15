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

package dev.dioxus.main;

/**
 * A2DP {@link android.bluetooth.BluetoothProfile.ServiceListener} whose callbacks
 * delegate to native (Rust) methods. The class is compiled to a standalone
 * {@code .dex} embedded in the Rust binary and loaded at runtime via
 * {@code DexClassLoader}; an instance is then passed to
 * {@code BluetoothAdapter.getProfileProxy(...)}.
 *
 * <p>The JNI mangled names of the native methods are
 * {@code Java_dev_dioxus_main_A2dpServiceListener_nativeOnServiceConnected} and
 * {@code Java_dev_dioxus_main_A2dpServiceListener_nativeOnServiceDisconnected},
 * exported from {@code src/bluetooth.rs}.
 */
public final class A2dpServiceListener
        implements android.bluetooth.BluetoothProfile.ServiceListener {

    @Override
    public void onServiceConnected(int profile, android.bluetooth.BluetoothProfile proxy) {
        nativeOnServiceConnected(proxy);
    }

    @Override
    public void onServiceDisconnected(int profile) {
        nativeOnServiceDisconnected(profile);
    }

    private static native void nativeOnServiceConnected(android.bluetooth.BluetoothProfile proxy);

    private static native void nativeOnServiceDisconnected(int profile);
}
