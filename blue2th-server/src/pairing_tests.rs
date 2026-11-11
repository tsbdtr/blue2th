// SPDX-License-Identifier: MIT OR Apache-2.0

//! #197: what the terminal says about pairing — the active code's banner
//! block, the success and refusal lines with the peer's address, the
//! cancellation and queue-end lines — and `--exit-if-unpaired`, its parse
//! and its decision. All pure.

use std::net::SocketAddr;

use super::*;

const URL: &str = "https://192.168.1.107:4000";
const NAME: &str = "blue2th-PC";

/// A peer on the LAN, with the port its connection came from.
fn peer(text: &str) -> Option<SocketAddr> {
    let parsed = text.parse::<SocketAddr>();
    assert!(parsed.is_ok(), "{text} is a socket address");
    parsed.ok()
}

fn parse(list: &[&str]) -> Result<Cli, clap::Error> {
    <Cli as clap::Parser>::try_parse_from(
        std::iter::once("blue2th-server").chain(list.iter().copied()),
    )
}

// ---- The banner, on activation ----

// Criterion: with a single code, the active code's block is today's single
// block, unnumbered.
#[test]
fn test_active_pairing_banner_of_a_single_code_is_the_single_code_banner() {
    assert_eq!(
        active_pairing_banner(URL, NAME, "K7M2QX", (1, 1)),
        pairing_banner(URL, NAME, "K7M2QX")
    );
}

// Criterion: with n > 1, the active code's block is headed `Pairing code
// k/n` — k and n distinct values, so a swap fails.
#[test]
fn test_active_pairing_banner_numbers_the_active_code_out_of_n() {
    assert_eq!(
        active_pairing_banner(URL, NAME, "ABCDEF", (2, 3)),
        format!("\nPairing code 2/3:{}", pairing_banner(URL, NAME, "ABCDEF"))
    );
}

// ---- The log lines ----

// Criterion: a success logs `client paired from <ip>`, the peer's IP
// without its port, followed by ` (code k/n)` when n > 1.
#[test]
fn test_pair_success_line_names_the_peer_ip_and_the_code_position() {
    assert_eq!(
        pair_success_line(peer("192.168.1.42:51234"), (1, 2)),
        "client paired from 192.168.1.42 (code 1/2)"
    );
    assert_eq!(
        pair_success_line(peer("192.168.1.42:51234"), (3, 4)),
        "client paired from 192.168.1.42 (code 3/4)"
    );
    assert_eq!(
        pair_success_line(peer("192.168.1.42:51234"), (1, 1)),
        "client paired from 192.168.1.42",
        "a single code is not numbered"
    );
}

// Criterion: an IPv6 peer is logged as its bare address — no brackets, no
// port.
#[test]
fn test_pair_success_line_logs_an_ipv6_peer_without_its_port() {
    assert_eq!(
        pair_success_line(peer("[fe80::1]:51234"), (1, 1)),
        "client paired from fe80::1"
    );
}

// Criterion: a request with no known peer address logs `from an unknown
// address`.
#[test]
fn test_pair_lines_without_a_peer_say_an_unknown_address() {
    assert_eq!(
        pair_success_line(None, (2, 2)),
        "client paired from an unknown address (code 2/2)"
    );
    assert_eq!(
        pair_refusal_line(None, 1),
        "pairing attempt refused (1/3) from an unknown address"
    );
}

// Criterion: a counted failure logs `pairing attempt refused (k/3) from
// <ip>`, without the port.
#[test]
fn test_pair_refusal_line_counts_out_of_three_and_names_the_peer_ip() {
    assert_eq!(
        pair_refusal_line(peer("192.168.1.42:51234"), 1),
        "pairing attempt refused (1/3) from 192.168.1.42"
    );
    assert_eq!(
        pair_refusal_line(peer("10.0.0.9:40000"), 2),
        "pairing attempt refused (2/3) from 10.0.0.9"
    );
}

// Criterion: the third failure warns that every remaining code is cancelled,
// and says how to get new ones.
#[test]
fn test_pairing_cancelled_line_names_the_cap_and_the_remedy() {
    assert_eq!(
        pairing_cancelled_line(),
        "pairing codes cancelled after 3 failed attempts — run with --pair for new ones"
    );
}

// Criterion: after the last code is consumed or expires, `no pairing code
// left` is logged.
#[test]
fn test_no_code_left_line() {
    assert_eq!(no_code_left_line(), "no pairing code left");
}

// Criterion: `--exit-if-unpaired` logs `no client paired: exiting
// (--exit-if-unpaired)` before it exits.
#[test]
fn test_exit_unpaired_line() {
    assert_eq!(
        exit_unpaired_line(),
        "no client paired: exiting (--exit-if-unpaired)"
    );
}

// ---- --exit-if-unpaired ----

// Criterion: `--exit-if-unpaired` requires `--pair` — clap refuses it alone.
#[test]
fn test_cli_refuses_exit_if_unpaired_without_pair() {
    assert!(
        parse(&["--exit-if-unpaired"]).is_err(),
        "--exit-if-unpaired alone must be refused"
    );
}

// Criterion: with `--pair`, bare or with a count, `--exit-if-unpaired` sets
// its flag and nothing else.
#[test]
fn test_cli_reads_exit_if_unpaired_with_pair() {
    assert_eq!(
        parse(&["--pair", "--exit-if-unpaired"]).map_err(|e| e.to_string()),
        Ok(Cli {
            pair: Some(1),
            bind: None,
            skip_state_permissions_check: false,
            exit_if_unpaired: true,
        })
    );
    assert_eq!(
        parse(&["--exit-if-unpaired", "--pair", "2"]).map_err(|e| e.to_string()),
        Ok(Cli {
            pair: Some(2),
            bind: None,
            skip_state_permissions_check: false,
            exit_if_unpaired: true,
        })
    );
}

// Criterion (guard, `--exit-if-unpaired`): when the queue ends with zero
// clients paired, the backend exits; one client paired keeps it running;
// the queue still running, or the flag absent, never exits.
#[test]
fn test_should_exit_unpaired_only_when_the_queue_ends_with_nobody_paired() {
    assert!(should_exit_unpaired(true, true, 0));
    assert!(
        !should_exit_unpaired(true, true, 1),
        "one client paired is enough to keep running"
    );
    assert!(
        !should_exit_unpaired(true, false, 0),
        "the queue has not ended"
    );
    assert!(
        !should_exit_unpaired(false, true, 0),
        "the flag was not given"
    );
}

// Criterion: `--exit-if-unpaired` exits with status 1, and its message is
// printed once — by the log line, not a second time by `main`'s `Error: …`,
// which a returned error would trigger. A stop for any other reason (a
// signal) is a success.
#[test]
fn test_unpaired_exit_is_status_1_without_an_error_to_print() {
    assert_eq!(
        unpaired_exit(&AtomicBool::new(true)).map_err(|e| e.to_string()),
        Ok(std::process::ExitCode::FAILURE)
    );
    assert_eq!(
        unpaired_exit(&AtomicBool::new(false)).map_err(|e| e.to_string()),
        Ok(std::process::ExitCode::SUCCESS)
    );
}

// ---- The pairing queue task ----

/// Run the pairing queue task over `auth` until it fires or drops its exit
/// signal: `Some(true)` fired, `Some(false)` dropped, `None` neither in time.
async fn queue_outcome(auth: auth::AuthStore, exit_if_unpaired: bool) -> Option<bool> {
    let (_router, state) = app_with_auth_store_and_state(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        auth,
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn_pairing_queue(
        state,
        URL.to_string(),
        NAME.to_string(),
        exit_if_unpaired,
        tx,
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .ok()
        .map(|sent| sent.is_ok())
}

/// A store whose `count` codes were armed `ago` before now.
fn armed_ago(count: u32, ago: std::time::Duration) -> (auth::AuthStore, Vec<String>) {
    let mut store = auth::AuthStore::with_token("queue-token");
    let armed_at = std::time::SystemTime::now() - ago;
    let codes = store.arm_pairing(count, armed_at);
    (store, codes)
}

// Criterion (guard, `--exit-if-unpaired`): the queue ends — its only code
// expired — with zero clients paired, and the task fires the exit.
#[tokio::test]
async fn test_pairing_queue_ending_with_nobody_paired_fires_the_exit() {
    let (store, _) = armed_ago(1, auth::PAIRING_TTL);
    assert_eq!(queue_outcome(store, true).await, Some(true));
}

// Criterion: without `--exit-if-unpaired`, the same queue end only ends the
// task: the exit is dropped, never fired.
#[tokio::test]
async fn test_pairing_queue_ending_without_the_flag_never_fires_the_exit() {
    let (store, _) = armed_ago(1, auth::PAIRING_TTL);
    assert_eq!(queue_outcome(store, false).await, Some(false));
}

// Criterion (guard, `--exit-if-unpaired`): one client paired keeps the backend
// running when the remaining code then expires.
#[tokio::test]
async fn test_pairing_queue_ending_after_one_pairing_never_fires_the_exit() {
    let armed_for = auth::PAIRING_TTL * 2;
    let (mut store, codes) = armed_ago(2, armed_for);
    let armed_at = std::time::SystemTime::now() - armed_for;
    let first = codes.first().cloned().unwrap_or_default();
    // Redeemed at the arming instant, so the second code's five minutes,
    // counted from there, are over by now.
    assert_eq!(
        store.redeem(&first, armed_at),
        Ok("queue-token".to_string())
    );
    assert_eq!(store.paired(), 1);
    assert_eq!(queue_outcome(store, true).await, Some(false));
}
