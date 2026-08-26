// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guards the two pieces of Android packaging identity that `dx` derives from the
//! cargo package name, and that therefore follow a crate rename unless pinned.
//!
//! Both were broken by the same refactor. The launcher label became
//! "Blue2ThFrontend"; the identifier would have silently made the build a
//! *different application* — unupdatable installs, paired backends and their
//! tokens lost, with the build green throughout.
//!
//! These assert on the files rather than on a build, so they cost nothing and run
//! on any host. They cannot catch a `dx` upgrade that changes how the manifest is
//! consumed — that is what the re-diff checklist in `CLAUDE.md` is for.

use std::path::Path;

/// The name shown under the launcher icon.
const DISPLAY_NAME: &str = "Blue2th";

/// The string Android uses to tell one application from another. Changing it
/// orphans every existing install; see `CLAUDE.md`.
const APPLICATION_ID: &str = "io.github.tsbdtr.blue2th";

/// Reads a file next to the crate manifest.
///
/// An unreadable file yields an empty string and fails the assertion below, rather
/// than `unwrap`/`expect`/`panic!`: clippy forbids all three here, and
/// `allow-expect-in-tests` does not reach a free function outside a `#[test]`.
fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        !content.is_empty(),
        "{} must exist and be readable",
        path.display()
    );
    content
}

#[test]
fn test_launcher_label_is_the_display_name() {
    let manifest = read("android/AndroidManifest.xml");
    let expected = format!("android:label=\"{DISPLAY_NAME}\"");

    assert!(
        manifest.contains(&expected),
        "the frozen manifest must declare {expected}, or the launcher shows \
         whatever dx derives from the cargo package name"
    );
}

#[test]
fn test_launcher_label_does_not_come_from_a_generated_resource() {
    let manifest = read("android/AndroidManifest.xml");

    assert!(
        !manifest.contains("android:label=\"@string/app_name\""),
        "android:label must stay a literal: dx regenerates res/values/strings.xml \
         on every build and derives app_name from the cargo package name, so \
         @string/app_name follows a crate rename"
    );
}

#[test]
fn test_application_identifier_is_pinned() {
    let config = read("Dioxus.toml");
    let expected = format!("identifier = \"{APPLICATION_ID}\"");

    assert!(
        config.contains(&expected),
        "Dioxus.toml must pin [android] {expected}. Left implicit, dx derives the \
         identifier from the cargo package name, and a changed identifier is a \
         different app: existing installs can no longer be updated and their \
         SharedPreferences are lost"
    );
}
