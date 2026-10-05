//! A `sponsord` that cannot reach its RPC endpoint exits with an error that
//! names the failed read but not the RPC URL, whose path carries the
//! provider's API key (#38).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use decdn_incentive::eth_identity;

#[test]
fn an_unreachable_rpc_url_stays_out_of_the_exit_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    eth_identity::generate_and_persist(dir.path(), "pw", false).unwrap();
    let keystore = eth_identity::keystore_path(dir.path());

    // Bound and dropped: nothing listens on the port, so the boot read is
    // refused.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sponsord"))
        .env_clear()
        .env("SPONSORD_BIND", "127.0.0.1:0")
        .env("SPONSORD_API_TOKEN", "0123456789abcdef0123456789abcdef")
        .env(
            "SPONSORD_RPC_URL",
            format!("http://127.0.0.1:{port}/v3/SECRET-API-KEY"),
        )
        .env(
            "SPONSORD_PAYMENT_POOL_ADDR",
            "0x2222222222222222222222222222222222222222",
        )
        .env("SPONSORD_POOL_ID", format!("0x{}", "11".repeat(32)))
        .env("SPONSORD_TREASURY_KEYSTORE", &keystore)
        .env("SPONSORD_TREASURY_PASSWORD", "pw")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("sponsord did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.starts_with("Error: "), "{stderr}");
    assert!(stderr.contains("usdc()"), "{stderr}");
    assert!(
        stderr.contains("tcp connect error"),
        "the cause is kept: {stderr}"
    );
    for text in [&stdout, &stderr] {
        assert!(!text.contains("SECRET-API-KEY"), "{text}");
        assert!(!text.contains(&format!(":{port}")), "{text}");
    }
}
