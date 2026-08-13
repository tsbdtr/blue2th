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

// Custom main activity, referenced by `[application] android_main_activity` in
// Dioxus.toml: the stock Dioxus/wry activity plus the overrides phase 5.2 needs.
//
// The presence service below lives in this file on purpose — dx copies this single
// file into the generated Gradle project, and Kotlin allows several top-level
// classes per file, so this is the only way to ship a second class without another
// dx configuration hook.

package dev.dioxus.main

import android.app.Service
import android.content.Intent
import android.os.IBinder

typealias BuildConfig = com.example.Blue2Th.BuildConfig

class MainActivity : WryActivity() {
    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        // Started while we are in the foreground (a background start would be
        // refused on Android 8+). It does nothing until the task is removed.
        startService(Intent(this, Blue2thPresenceService::class.java))
    }

    // Spotify's consent screen redirects to blue2th://spotify-callback. When the
    // app is already running, Android hands that redirect to onNewIntent instead
    // of relaunching us — and the base Activity does NOT update getIntent(), so
    // the Rust side (src/deep_link.rs) would keep reading the launcher intent and
    // never see the authorization code. Store it explicitly.
    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        if (intent != null) {
            setIntent(intent)
        }
    }

    // Presence reporting (see src/lifecycle.rs). Playback deliberately continues
    // while blue2th is backgrounded, but Android freezes the process within
    // seconds, which drops the SSE feed the backend uses as a heartbeat — a frozen
    // app then looks exactly like a dead one. These callbacks tell it apart.
    // libmain.so is already loaded by WryActivity's companion object.
    private external fun nativeOnForeground()
    private external fun nativeOnBackground()
    private external fun nativeOnGone()

    override fun onStart() {
        super.onStart()
        nativeOnForeground()
    }

    override fun onStop() {
        super.onStop()
        nativeOnBackground()
    }

    override fun onDestroy() {
        // isFinishing distinguishes a real exit from a configuration change (a
        // rotation destroys and recreates the activity without the user leaving).
        if (isFinishing) {
            nativeOnGone()
        }
        super.onDestroy()
    }
}

// Exists for a single callback: `onTaskRemoved`, which Android delivers when the
// user swipes blue2th out of the recents list. The activity's `onDestroy` is NOT
// called in that case, so without this the backend would only learn the app is
// gone through the watchdog's 30-minute backstop.
//
// This is a plain service: no notification, no foreground type. `stopWithTask`
// is false in the manifest so it outlives the task long enough to report.
class Blue2thPresenceService : Service() {
    private external fun nativeOnGone()

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Nothing to run while alive, and Android must not restart it by itself.
        return START_NOT_STICKY
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        // Blocks briefly (see src/lifecycle.rs): the process is about to be torn
        // down, so a detached request would never leave the device.
        nativeOnGone()
        super.onTaskRemoved(rootIntent)
        stopSelf()
    }
}
