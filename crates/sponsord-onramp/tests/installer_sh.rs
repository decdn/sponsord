//! `assets/decdn.sh` as the onramp serves it, run for real against that
//! onramp over HTTP (fake daemon and gate), `file://` releases and a stub
//! `decdn`: the install skips binaries already at the pin, and `pull` passes
//! the gate once per download, resumes a failed one without it, replaces a
//! capability near expiry, and refuses a second run of the same download.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use sponsord_onramp::test_support::{
    FakeOptions, Fakes, STUB_DECDN, app_state_with_options, publish_release, test_config,
};

const HASH: &str = "9194000d7b356650e6924a7746ec4afb0b705838b65913c0e46cba6b15af69e5";

struct Fixture {
    home: PathBuf,
    releases: PathBuf,
    stub: PathBuf,
    script: PathBuf,
    onramp: String,
    fakes: Fakes,
}

impl Fixture {
    async fn new(name: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("installer_sh")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let releases = root.join("releases");
        let stub = root.join("stub");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&stub).unwrap();
        let sums = publish_release(&releases, "decdn", "decdn", "v0.1.0", "0.1.0", STUB_DECDN);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let onramp = format!("http://{}", listener.local_addr().unwrap());
        let mut config = test_config();
        config.public_url.clone_from(&onramp);
        config.releases_base = format!("file://{}", releases.display());
        config.decdn_release.sums_sha256 = sums;
        let (state, fakes) = app_state_with_options(FakeOptions {
            config,
            ..FakeOptions::default()
        })
        .await;
        let app = sponsord_onramp::http::router(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let body = reqwest::get(format!("{onramp}/decdn.sh"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let script = root.join("decdn.sh");
        fs::write(&script, body).unwrap();
        Self {
            home,
            releases,
            stub,
            script,
            onramp,
            fakes,
        }
    }

    async fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg(&self.script)
            .args(args)
            .env("HOME", &self.home)
            .env("STUB_DIR", &self.stub)
            .env_remove("STUB_EXIT")
            .stdin(std::process::Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        tokio::time::timeout(Duration::from_secs(60), cmd.output())
            .await
            .expect("decdn.sh finishes")
            .unwrap()
    }

    /// Run the script, and fail the test unless it succeeds. Returns stdout.
    async fn run_ok(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = self.run_with(args, env).await;
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            out.status.success(),
            "decdn.sh failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.home.join(".local/bin").join(name)
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.home.join(".decdn/installer").join(name)
    }

    fn sponsored(&self) -> PathBuf {
        self.home.join(".decdn/sponsored")
    }

    fn state(&self) -> PathBuf {
        self.sponsored().join("downloads").join(HASH)
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.stub.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn keys_made(&self) -> u32 {
        fs::read_to_string(self.stub.join("keys")).map_or(0, |s| s.trim().parse().unwrap())
    }

    /// Play the person in the browser: once the script has written a key
    /// address other than `previous`, pass the gate for it.
    fn pass_gate(&self, previous: Option<String>) -> tokio::task::JoinHandle<String> {
        let address_file = self.state().join("address");
        let onramp = self.onramp.clone();
        tokio::spawn(async move {
            let address = loop {
                if let Ok(a) = fs::read_to_string(&address_file)
                    && Some(&a) != previous.as_ref()
                {
                    break a;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            let resp = reqwest::Client::new()
                .post(format!("{onramp}/v1/fund"))
                .json(&serde_json::json!({ "client": address, "proof": "ok" }))
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success(), "{}", resp.status());
            address
        })
    }
}

fn pull_args() -> Vec<String> {
    vec!["pull".into(), format!("b3:{HASH}")]
}

#[tokio::test]
async fn a_second_run_with_the_same_pin_downloads_nothing() {
    let fx = Fixture::new("same_pin").await;
    let first = fx.run_ok(&[], &[]).await;
    assert!(first.contains("Installing decdn v0.1.0"), "{first}");
    assert!(first.contains("decdn is ready"), "{first}");
    assert!(fx.bin("decdn").exists() && fx.marker("decdn").exists());

    // With the releases gone, any download would fail the run.
    fs::remove_dir_all(&fx.releases).unwrap();
    let second = fx.run_ok(&[], &[]).await;
    assert!(!second.contains("Installing"), "{second}");
    assert!(
        second.contains("decdn v0.1.0 is already installed."),
        "{second}"
    );
}

#[tokio::test]
async fn a_missing_marker_or_binary_reinstalls() {
    let fx = Fixture::new("reinstall").await;
    fx.run_ok(&[], &[]).await;

    fs::remove_file(fx.marker("decdn")).unwrap();
    let out = fx.run_ok(&[], &[]).await;
    assert!(out.contains("Installing decdn v0.1.0"), "{out}");

    fs::remove_file(fx.bin("decdn")).unwrap();
    let out = fx.run_ok(&[], &[]).await;
    assert!(out.contains("Installing decdn v0.1.0"), "{out}");
    assert!(fx.bin("decdn").exists());
}

#[tokio::test]
async fn a_changed_pin_reinstalls() {
    let fx = Fixture::new("changed_pin").await;
    fx.run_ok(&[], &[]).await;
    // A marker from another release, as after the onramp moves its pin.
    fs::write(fx.marker("decdn"), "00 x86_64-unknown-linux-gnu\n").unwrap();
    let out = fx.run_ok(&[], &[]).await;
    assert!(out.contains("Installing decdn v0.1.0"), "{out}");
}

#[tokio::test]
async fn pull_passes_the_gate_and_runs_decdn_with_the_capability() {
    let fx = Fixture::new("pull").await;
    let gate = fx.pass_gate(None);
    let mut args = pull_args();
    args.extend([
        "-o".into(),
        "out dir".into(),
        "--namespace".into(),
        "1".into(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdout = fx.run_ok(&args, &[]).await;
    let address = gate.await.unwrap();

    assert!(
        stdout.contains("Open this link to start the download:"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("{}/fund?client={address}", fx.onramp)),
        "{stdout}"
    );
    let pool = alloy_primitives::Address::repeat_byte(0x22);
    let bond = alloy_primitives::Address::repeat_byte(0x33);
    let state = fx.state();
    let shared = fx.sponsored().join("decdn");
    assert_eq!(
        fx.calls(),
        [format!(
            "bundle pull --hash {HASH} -o out dir --capability dcap1:FAKE1 \
             --keystore {}/keystore.json --keystore-password-file {}/password \
             --data-dir {} --rpc-url https://rpc.example --payment-pool-address {pool} \
             --capacity-bond-address {bond} --chain-id 421614 --namespace 1",
            state.display(),
            state.display(),
            shared.display(),
        )]
    );
    assert_eq!(fx.fakes.source.issued_count(), 1);
    assert!(!state.exists(), "state is deleted after a good pull");
    assert!(!fx.sponsored().join("decdn.lock").exists());
    assert!(
        !fx.sponsored()
            .join(format!("downloads/{HASH}.lock"))
            .exists()
    );
}

#[tokio::test]
async fn a_failed_pull_resumes_with_the_same_key_and_capability() {
    let fx = Fixture::new("resume").await;
    let args = pull_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let gate = fx.pass_gate(None);
    let out = fx.run_with(&args, &[("STUB_EXIT", "3")]).await;
    gate.await.unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Re-run the same command to resume"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let state = fx.state();
    assert_eq!(
        fs::read_to_string(state.join("capability")).unwrap(),
        "dcap1:FAKE1"
    );
    assert!(
        fs::read_to_string(state.join("expiry"))
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    // decdn refuses a data dir that group or others can read.
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&fx.sponsored().join("decdn")), 0o700);
    assert_eq!(mode(&state.join("password")), 0o600);

    // No gate this time: the saved key and capability are reused.
    let stdout = fx.run_ok(&args, &[]).await;
    assert!(!stdout.contains("Open this link"), "{stdout}");
    assert_eq!(fx.keys_made(), 1);
    assert_eq!(fx.fakes.source.issued_count(), 1);
    let calls = fx.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|c| c.contains("--capability dcap1:FAKE1 "))
    );
    assert!(!state.exists());
}

#[tokio::test]
async fn a_capability_near_expiry_is_replaced_with_a_new_key() {
    let fx = Fixture::new("expiry").await;
    let args = pull_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let gate = fx.pass_gate(None);
    fx.run_with(&args, &[("STUB_EXIT", "1")]).await;
    let first = gate.await.unwrap();
    fs::write(fx.state().join("expiry"), "1").unwrap();

    let gate = fx.pass_gate(Some(first.clone()));
    fx.run_ok(&args, &[]).await;
    let second = gate.await.unwrap();
    assert_ne!(first, second);
    assert_eq!(fx.keys_made(), 2);
    assert!(fx.calls()[1].contains("--capability dcap1:FAKE2 "));
}

#[tokio::test]
async fn a_download_already_running_is_not_started_twice() {
    let fx = Fixture::new("locked").await;
    let args = pull_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let lock = fx.sponsored().join(format!("downloads/{HASH}.lock"));
    fs::create_dir_all(&lock).unwrap();
    fs::write(lock.join("pid"), std::process::id().to_string()).unwrap();

    let out = fx.run_with(&args, &[]).await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already running"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fx.keys_made(), 0);
    assert!(lock.exists(), "another run's lock is left alone");

    // A lock left by a process that has exited is taken over.
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    fs::write(lock.join("pid"), dead_pid.to_string()).unwrap();
    let gate = fx.pass_gate(None);
    fx.run_ok(&args, &[]).await;
    gate.await.unwrap();
    assert!(!lock.exists());
}

#[tokio::test]
async fn a_busy_shared_data_dir_runs_the_download_in_its_own() {
    let fx = Fixture::new("busy_data_dir").await;
    let args = pull_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let lock = fx.sponsored().join("decdn.lock");
    fs::create_dir_all(&lock).unwrap();
    fs::write(lock.join("pid"), std::process::id().to_string()).unwrap();

    let gate = fx.pass_gate(None);
    let out = fx.run_with(&args, &[("STUB_EXIT", "1")]).await;
    gate.await.unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("without the shared peer cache"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fx.calls()[0].contains(&format!("--data-dir {} ", fx.state().display())));
}

#[tokio::test]
async fn bad_arguments_are_refused_before_installing() {
    let fx = Fixture::new("bad_args").await;
    for args in [
        &["pull", "b3:nothex"][..],
        &["pull"],
        &["fetch", HASH],
        &["pull", HASH, "--namespace", "0"],
        &["pull", HASH, "--namespace", "x1"],
        &["pull", HASH, "--bogus"],
    ] {
        let out = fx.run_with(args, &[]).await;
        assert!(!out.status.success(), "{args:?}");
    }
    assert!(!fx.bin("decdn").exists());
}
