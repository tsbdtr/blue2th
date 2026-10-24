// SPDX-License-Identifier: MIT OR Apache-2.0

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use super::*;

/// The directory type bits `MetadataExt::mode` carries for a directory
/// (`S_IFDIR`): `judge_state_dir` receives the mode as `metadata` gives it, and
/// must still print `755`, not `40755`.
const S_IFDIR: u32 = 0o040_000;

/// A state directory path for the pure verdicts. Never touched on disk.
const DIR: &str = "/home/u/.local/state/blue2th";

/// The effective uid the pure verdicts run as.
const EUID: u32 = 1000;

/// A fresh, private scratch directory under the system temp dir, unique to
/// `name` and to this process. Never the real state directory.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "blue2th-test-state-store-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// Create `dir` and set its mode to `mode`, whatever the umask.
fn dir_with_mode(dir: &Path, mode: u32) {
    std::fs::create_dir_all(dir).expect("create the directory");
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The permission bits of `path`, `None` when it cannot be read.
fn mode_of(path: &Path) -> Option<u32> {
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

/// The refusal message for `found` at `dir`, `None` when the verdict passes.
fn refusal(dir: &str, found: Found, euid: u32) -> Option<String> {
    judge_state_dir(Path::new(dir), found, euid)
        .err()
        .map(|r| r.to_string())
}

/// Whether `message` holds `number` as a whole run of digits: `755` must be
/// printed as `755`, and a `40755` (the raw `st_mode`) does not count.
fn has_number(message: &str, number: &str) -> bool {
    message
        .split(|c: char| !c.is_ascii_digit())
        .any(|run| run == number)
}

/// `path` single-quoted for a shell, as the message must print it.
fn quoted(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

// ---- judge_state_dir: the pure verdict ----

// Criterion: the check passes a directory whose `mode & 0o077 == 0` and whose
// owner is the effective uid.
#[test]
fn test_judge_state_dir_passes_an_owner_only_700_directory() {
    let verdict = judge_state_dir(
        Path::new(DIR),
        Found::Directory {
            mode: S_IFDIR | 0o700,
            uid: EUID,
        },
        EUID,
    );
    assert!(
        verdict.is_ok(),
        "{:?}",
        verdict.err().map(|r| r.to_string())
    );
}

// Criterion (guard, "not exactly 700"): `500` and `600` grant nothing to group
// or others, so they pass. Near-miss: `mode & 0o777 == 0o700`, which refuses
// both.
#[test]
fn test_judge_state_dir_passes_owner_only_modes_other_than_700() {
    for mode in [0o500, 0o600] {
        let verdict = judge_state_dir(
            Path::new(DIR),
            Found::Directory {
                mode: S_IFDIR | mode,
                uid: EUID,
            },
            EUID,
        );
        assert!(
            verdict.is_ok(),
            "{mode:o}: {:?}",
            verdict.err().map(|r| r.to_string())
        );
    }
}

// Criterion: a `755` directory (every install so far) is refused, with the
// path single-quoted, the mode as `stat -c %a` prints it, the owner uid and
// `chmod 700 '<path>'` — and no `chown`, since the owner is right.
#[test]
fn test_judge_state_dir_refuses_755_with_the_chmod_command() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o755,
            uid: EUID,
        },
        EUID,
    );
    assert!(message.is_some(), "a 755 directory must be refused");
    let message = message.unwrap_or_default();
    assert!(message.contains(&quoted(DIR)), "the path: {message:?}");
    assert!(has_number(&message, "755"), "the mode as 755: {message:?}");
    assert!(has_number(&message, "1000"), "the owner uid: {message:?}");
    assert!(
        message.contains(&format!("chmod 700 {}", quoted(DIR))),
        "the fixing command: {message:?}"
    );
    assert!(
        !message.contains("chown"),
        "the owner is right, no chown: {message:?}"
    );
}

// Criterion (guard, "nothing for group"): `710` is refused. Near-miss: a check
// of the read and write bits only (`mode & 0o066`), which passes it.
#[test]
fn test_judge_state_dir_refuses_group_execute_710() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o710,
            uid: EUID,
        },
        EUID,
    )
    .unwrap_or_default();
    assert!(
        message.contains(&format!("chmod 700 {}", quoted(DIR))),
        "710 must be refused with chmod: {message:?}"
    );
    assert!(has_number(&message, "710"), "the mode as 710: {message:?}");
}

// Criterion (guard, "nothing for others"): `701` is refused. Near-miss: a check
// of the group bits only, which passes it.
#[test]
fn test_judge_state_dir_refuses_others_execute_701() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o701,
            uid: EUID,
        },
        EUID,
    )
    .unwrap_or_default();
    assert!(
        message.contains(&format!("chmod 700 {}", quoted(DIR))),
        "701 must be refused with chmod: {message:?}"
    );
    assert!(has_number(&message, "701"), "the mode as 701: {message:?}");
}

// Criterion (guard, "belongs to the effective user"): a `700` directory owned
// by `euid + 1` is refused with `sudo chown <euid> '<path>'`, naming the owner
// found, and no `chmod` since the mode is right. Near-miss: a check of the mode
// alone, which passes it.
#[test]
fn test_judge_state_dir_refuses_a_700_directory_of_another_user() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o700,
            uid: EUID + 1,
        },
        EUID,
    );
    assert!(
        message.is_some(),
        "another user's directory must be refused"
    );
    let message = message.unwrap_or_default();
    assert!(message.contains(&quoted(DIR)), "the path: {message:?}");
    assert!(
        has_number(&message, "1001"),
        "the owner uid found: {message:?}"
    );
    assert!(
        message.contains(&format!("sudo chown 1000 {}", quoted(DIR))),
        "the fixing command names the effective uid: {message:?}"
    );
    assert!(
        !message.contains("chmod"),
        "the mode is right, no chmod: {message:?}"
    );
}

// Criterion: a directory both too open and owned by another user gets both
// commands, `chown` first.
#[test]
fn test_judge_state_dir_gives_chown_then_chmod_when_both_are_wrong() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o750,
            uid: 2002,
        },
        EUID,
    )
    .unwrap_or_default();
    let chown = message.find(&format!("sudo chown 1000 {}", quoted(DIR)));
    let chmod = message.find(&format!("chmod 700 {}", quoted(DIR)));
    assert!(chown.is_some(), "the chown command: {message:?}");
    assert!(chmod.is_some(), "the chmod command: {message:?}");
    assert!(chown < chmod, "chown comes first: {message:?}");
    assert!(has_number(&message, "750"), "the mode: {message:?}");
    assert!(has_number(&message, "2002"), "the owner: {message:?}");
}

// Requested at manual verification (#194): the refusal states the rule the
// check applies — nothing for group or others — and not "must be mode 700",
// which is false: `500` passes.
#[test]
fn test_judge_state_dir_refusal_states_nothing_to_group_or_others_not_mode_700() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o755,
            uid: EUID,
        },
        EUID,
    )
    .unwrap_or_default();
    assert!(
        message.contains("nothing to group or others"),
        "the rule the check applies: {message:?}"
    );
    assert!(
        !message.contains("must be mode 700"),
        "500 passes too, so 700 is not the rule: {message:?}"
    );
}

// Criterion: the mode is printed as `stat -c %a` prints it, which keeps the
// setgid bit: `2755`, not `755`. And `chmod 700` leaves a setgid directory at
// `2700` (observed: GNU coreutils keeps a directory's setgid bit on a numeric
// mode), which grants nothing to group or others, so it passes.
#[test]
fn test_judge_state_dir_prints_a_setgid_mode_as_stat_does_and_passes_2700() {
    let message = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o2755,
            uid: EUID,
        },
        EUID,
    )
    .unwrap_or_default();
    assert!(
        has_number(&message, "2755"),
        "the mode as 2755: {message:?}"
    );
    assert!(
        message.contains(&format!("chmod 700 {}", quoted(DIR))),
        "the fixing command: {message:?}"
    );

    let fixed = refusal(
        DIR,
        Found::Directory {
            mode: S_IFDIR | 0o2700,
            uid: EUID,
        },
        EUID,
    );
    assert_eq!(fixed, None, "2700 grants nothing to group or others");
}

// Requested at manual verification (#194): `main` prints a returned error with
// `Debug`, so a refusal's `Debug` is its message — the same text as `Display`,
// not a `StateDirRefusal { message: … }` dump.
#[test]
fn test_state_dir_refusal_debug_is_its_message() {
    let refused = judge_state_dir(
        Path::new(DIR),
        Found::Directory {
            mode: S_IFDIR | 0o755,
            uid: EUID,
        },
        EUID,
    );
    let debug = refused.as_ref().err().map(|r| format!("{r:?}"));
    let display = refused.as_ref().err().map(|r| r.to_string());
    assert!(
        debug
            .as_deref()
            .is_some_and(|d| d.contains(&format!("chmod 700 {}", quoted(DIR)))),
        "the Debug carries the fixing command: {debug:?}"
    );
    assert_eq!(debug, display);
}

// Criterion: a missing path passes — a first start, which the first store
// write creates `0700`.
#[test]
fn test_judge_state_dir_passes_a_missing_path() {
    assert!(judge_state_dir(Path::new(DIR), Found::Missing, EUID).is_ok());
}

// Criterion: a path that is not a directory is refused, the message says so,
// and suggests no command.
#[test]
fn test_judge_state_dir_refuses_a_path_that_is_not_a_directory() {
    let message = refusal(DIR, Found::NotADirectory, EUID);
    assert!(message.is_some(), "a regular file must be refused");
    let message = message.unwrap_or_default();
    assert!(message.contains(&quoted(DIR)), "the path: {message:?}");
    assert!(
        message.contains("not a directory"),
        "says it is not a directory: {message:?}"
    );
    assert!(
        !message.contains("chmod") && !message.contains("chown"),
        "no command suggested: {message:?}"
    );
}

// Criterion (guard, "an inspection failure is not absence"): a `metadata`
// error other than `NotFound` is refused, and the message carries the I/O
// error.
#[test]
fn test_judge_state_dir_refuses_an_inspection_error_and_carries_it() {
    let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "probe-denied-7d1f");
    let message = refusal(DIR, Found::Uninspectable(error), EUID);
    assert!(message.is_some(), "a failure to look must refuse");
    let message = message.unwrap_or_default();
    assert!(message.contains(&quoted(DIR)), "the path: {message:?}");
    assert!(
        message.contains("probe-denied-7d1f"),
        "the I/O error: {message:?}"
    );
}

// Criterion (guard, "the quoted path"): a path with a space and one with a `'`
// give copy-pasteable commands — the `'` written `'\''`. Near-miss: the path
// interpolated bare.
#[test]
fn test_judge_state_dir_quotes_a_path_with_a_space_or_a_quote() {
    let cases = [
        (
            "/home/my user/.local/state/blue2th",
            "chmod 700 '/home/my user/.local/state/blue2th'",
            "sudo chown 1000 '/home/my user/.local/state/blue2th'",
        ),
        (
            "/home/o'brien/.local/state/blue2th",
            r"chmod 700 '/home/o'\''brien/.local/state/blue2th'",
            r"sudo chown 1000 '/home/o'\''brien/.local/state/blue2th'",
        ),
    ];
    for (dir, chmod, chown) in cases {
        let message = refusal(
            dir,
            Found::Directory {
                mode: S_IFDIR | 0o755,
                uid: EUID + 7,
            },
            EUID,
        )
        .unwrap_or_default();
        assert!(message.contains(chmod), "{dir}: {message:?}");
        assert!(message.contains(chown), "{dir}: {message:?}");
    }
}

// ---- ensure_state_dir: the one creating function ----

// Criteria: a missing state directory is created `700`, and its missing
// parents are `700` as well.
#[test]
fn test_ensure_state_dir_creates_the_directory_and_its_parents_700() {
    let root = scratch("ensure-missing");
    let a = root.join("a");
    let b = a.join("b");
    let dir = b.join("blue2th");

    let created = ensure_state_dir(&dir);
    assert!(created.is_ok(), "{:?}", created.err());
    for path in [&a, &b, &dir] {
        assert_eq!(mode_of(path), Some(0o700), "{}", path.display());
    }
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion (XDG Base Directory: "If the destination directory exists already
// the permissions should not be changed"): an existing `755` directory stays
// `755`, and that is not an error.
#[test]
fn test_ensure_state_dir_leaves_an_existing_directory_s_mode_alone() {
    let root = scratch("ensure-existing");
    let dir = root.join("blue2th");
    dir_with_mode(&dir, 0o755);

    let ensured = ensure_state_dir(&dir);
    assert!(ensured.is_ok(), "{:?}", ensured.err());
    assert_eq!(mode_of(&dir), Some(0o755));
    let _ = std::fs::remove_dir_all(&root);
}

// ---- check_state_dir: the file system, read for real ----

// Criterion: an owner-only `700` directory of the running user passes.
#[test]
fn test_check_state_dir_passes_a_real_700_directory() {
    let root = scratch("check-700");
    let dir = root.join("blue2th");
    dir_with_mode(&dir, 0o700);

    let verdict = check_state_dir(&dir);
    assert!(
        verdict.is_ok(),
        "{:?}",
        verdict.err().map(|r| r.to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion: a real `755` directory is refused, the message naming `755`, the
// owner read from the disk, and `chmod 700 '<path>'`.
#[test]
fn test_check_state_dir_refuses_a_real_755_directory() {
    let root = scratch("check-755");
    let dir = root.join("blue2th");
    dir_with_mode(&dir, 0o755);
    let owner = std::fs::metadata(&dir).map(|m| m.uid()).unwrap();
    let path = dir.to_string_lossy().into_owned();

    let message = check_state_dir(&dir)
        .err()
        .map(|r| r.to_string())
        .unwrap_or_default();
    assert!(
        message.contains(&format!("chmod 700 {}", quoted(&path))),
        "{message:?}"
    );
    // The scratch path carries `755` and the pid: the numbers are looked for
    // in the rest of the message.
    let rest = message.replace(&quoted(&path), "");
    assert!(has_number(&rest, "755"), "the mode: {message:?}");
    assert!(
        has_number(&rest, &owner.to_string()),
        "the owner: {message:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion (guard, "follows the link"): a symlink to a `700` directory passes.
// Near-miss: `symlink_metadata`, which reads the link's own mode, `777`.
#[test]
fn test_check_state_dir_follows_a_link_to_a_700_directory() {
    let root = scratch("check-link-700");
    let target = root.join("real");
    dir_with_mode(&target, 0o700);
    let link = root.join("blue2th");
    std::os::unix::fs::symlink(&target, &link).expect("create the link");

    let verdict = check_state_dir(&link);
    assert!(
        verdict.is_ok(),
        "{:?}",
        verdict.err().map(|r| r.to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion: the check judges a link's target — a link to a `755` directory is
// refused, the command naming the path that was checked (the link: `chmod`
// follows it).
#[test]
fn test_check_state_dir_refuses_a_link_to_a_755_directory() {
    let root = scratch("check-link-755");
    let target = root.join("real");
    dir_with_mode(&target, 0o755);
    let link = root.join("blue2th");
    std::os::unix::fs::symlink(&target, &link).expect("create the link");
    let path = link.to_string_lossy().into_owned();

    let message = check_state_dir(&link)
        .err()
        .map(|r| r.to_string())
        .unwrap_or_default();
    assert!(
        message.contains(&format!("chmod 700 {}", quoted(&path))),
        "{message:?}"
    );
    // The scratch path carries `755`: the mode is looked for in the rest.
    assert!(
        has_number(&message.replace(&quoted(&path), ""), "755"),
        "the target's mode: {message:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion: a missing path passes, and a dangling link reads as missing.
#[test]
fn test_check_state_dir_passes_a_missing_path_and_a_dangling_link() {
    let root = scratch("check-missing");
    let missing = root.join("blue2th");
    let dangling = root.join("dangling");
    std::os::unix::fs::symlink(root.join("nowhere"), &dangling).expect("create the link");

    for path in [&missing, &dangling] {
        let verdict = check_state_dir(path);
        assert!(
            verdict.is_ok(),
            "{}: {:?}",
            path.display(),
            verdict.err().map(|r| r.to_string())
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion: a regular file named `blue2th` is refused as not a directory.
#[test]
fn test_check_state_dir_refuses_a_regular_file() {
    let root = scratch("check-file");
    let file = root.join("blue2th");
    std::fs::write(&file, "").expect("create the file");

    let message = check_state_dir(&file)
        .err()
        .map(|r| r.to_string())
        .unwrap_or_default();
    assert!(message.contains("not a directory"), "{message:?}");
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion (guard, "an inspection failure is not absence"): `metadata` failing
// with anything but `NotFound` refuses, carrying the error. A link pointing at
// itself fails with `ELOOP` — even for root, unlike a mode-000 parent.
// Near-miss: `metadata(..).ok()`, which reads that failure as "missing" and
// passes.
#[test]
fn test_check_state_dir_refuses_a_path_metadata_cannot_inspect() {
    let root = scratch("check-loop");
    let looped = root.join("blue2th");
    std::os::unix::fs::symlink(&looped, &looped).expect("create the looping link");
    let error = std::fs::metadata(&looped).err();
    assert!(
        error
            .as_ref()
            .is_some_and(|e| e.kind() != std::io::ErrorKind::NotFound),
        "the fixture must fail with something else than NotFound: {error:?}"
    );
    let error = error.map(|e| e.to_string()).unwrap_or_default();

    let message = check_state_dir(&looped)
        .err()
        .map(|r| r.to_string())
        .unwrap_or_default();
    assert!(
        !error.is_empty() && message.contains(&error),
        "the message carries {error:?}: {message:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- the start-up decision ----

/// The flag the warning must name.
const FLAG: &str = "--skip-state-permissions-check";

/// The warning of a `Skip` decision, `None` for any other.
fn warning_of(decision: StartupCheck<'_>) -> Option<String> {
    match decision {
        StartupCheck::Skip { warning } => Some(warning),
        StartupCheck::Check(_) | StartupCheck::Nothing => None,
    }
}

// Criterion (guard, "warning even when the directory passes"): the flag with a
// `700` directory skips the check and still yields the warning naming the
// flag. Near-miss: warning only when the skipped check would have failed.
#[test]
fn test_startup_check_with_the_flag_warns_even_for_a_700_directory() {
    let root = scratch("startup-skip-700");
    let dir = root.join("blue2th");
    dir_with_mode(&dir, 0o700);

    let warning = warning_of(startup_check(true, Some(&dir)));
    assert!(
        warning.as_deref().is_some_and(|w| w.contains(FLAG)),
        "{warning:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion: the flag skips the check whatever the directory's state — a `755`
// one too — with the warning.
#[test]
fn test_startup_check_with_the_flag_skips_a_755_directory_with_the_warning() {
    let root = scratch("startup-skip-755");
    let dir = root.join("blue2th");
    dir_with_mode(&dir, 0o755);

    let warning = warning_of(startup_check(true, Some(&dir)));
    assert!(
        warning.as_deref().is_some_and(|w| w.contains(FLAG)),
        "{warning:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// Criterion: the warning is logged at every start the flag is set — with no
// state directory as well (pinned reading of "at every start it is set").
#[test]
fn test_startup_check_with_the_flag_and_no_state_dir_still_warns() {
    let warning = warning_of(startup_check(true, None));
    assert!(
        warning.as_deref().is_some_and(|w| w.contains(FLAG)),
        "{warning:?}"
    );
}

// Criterion: without the flag and with no state path, neither check nor
// warning.
#[test]
fn test_startup_check_without_a_state_dir_does_nothing() {
    assert_eq!(startup_check(false, None), StartupCheck::Nothing);
}

// Criterion: without the flag, a state directory is checked — that one.
#[test]
fn test_startup_check_without_the_flag_checks_the_given_directory() {
    let dir = Path::new("/srv/state-a/blue2th");
    assert_eq!(startup_check(false, Some(dir)), StartupCheck::Check(dir));
}
