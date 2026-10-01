#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, B256};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;

use sponsord::captcha::CaptchaVerifier;
use sponsord::config::{ReleasePin, ServerConfig};
use sponsord::issuer::Issuer;
use sponsord::money::MicroUsdc;
use sponsord::state::AppState;
use sponsord::store::Store;
use sponsord::treasury::Treasury;

pub use sponsord_core::test_support::FakeTreasury;

pub struct FakeCaptcha {
    pass: AtomicBool,
}
impl FakeCaptcha {
    #[must_use]
    pub fn new(pass: bool) -> Self {
        Self {
            pass: AtomicBool::new(pass),
        }
    }
}
#[async_trait]
impl CaptchaVerifier for FakeCaptcha {
    async fn verify(&self, _token: &str, _remote_ip: Option<&str>) -> anyhow::Result<bool> {
        Ok(self.pass.load(Ordering::SeqCst))
    }
}

pub struct FakeOptions {
    pub captcha_passes: bool,
    pub capability_cap: MicroUsdc,
}
impl Default for FakeOptions {
    fn default() -> Self {
        Self {
            captcha_passes: true,
            capability_cap: MicroUsdc(10_000_000),
        }
    }
}

/// Build an `AppState` with a real temp-dir `Store` and real (offline)
/// `Issuer`, but fake `Treasury` and captcha. Bypasses `state::build`'s
/// on-chain owner check (no chain in these tests). The temp dir is leaked so
/// it outlives this call; test binaries are short-lived.
#[must_use]
pub fn app_state_with_options(opts: FakeOptions) -> AppState {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let data_dir = dir.keep();
    let store = Arc::new(Store::open(&data_dir).unwrap_or_else(|e| panic!("open store: {e}")));

    let signer = PrivateKeySigner::random();
    let owner = signer.address();
    let pool_id = B256::repeat_byte(0x11);
    let domain = decdn_incentive::voucher_domain(421_614, Address::repeat_byte(0x22));
    let issuer = Arc::new(Issuer::new(
        signer,
        domain,
        pool_id,
        opts.capability_cap.0,
        2_592_000,
    ));

    let treasury: Arc<dyn Treasury> = Arc::new(FakeTreasury::new(owner, 100_000_000));
    let turnstile: Arc<dyn CaptchaVerifier> = Arc::new(FakeCaptcha::new(opts.captcha_passes));

    let cfg = ServerConfig {
        bind: "127.0.0.1:0"
            .parse()
            .unwrap_or_else(|e| panic!("bind: {e}")),
        public_url: "https://up.decdn.org".into(),
        rpc_url: "http://localhost:8545".into(),
        chain_id: 421_614,
        payment_pool: Address::repeat_byte(0x22),
        pool_id,
        capacity_bond: Address::ZERO,
        treasury_keystore: PathBuf::from("/dev/null"),
        capability_cap: opts.capability_cap,
        capability_ttl_secs: 2_592_000,
        pool_low_water: MicroUsdc(20_000_000),
        pool_refill: MicroUsdc(100_000_000),
        pool_watch_interval_secs: 3600,
        turnstile_secret: "secret".into(),
        turnstile_sitekey: "TEST_SITEKEY".into(),
        data_dir,
        decdn_release: ReleasePin {
            tag: "v0.1.0".into(),
            sums_sha256: "ab".repeat(32),
        },
        wrapper_release: ReleasePin {
            tag: "v0.2.0".into(),
            sums_sha256: "cd".repeat(32),
        },
    };
    AppState {
        store,
        treasury,
        issuer,
        turnstile,
        cfg: Arc::new(cfg),
    }
}

#[must_use]
pub fn app_state_with_fakes() -> AppState {
    app_state_with_options(FakeOptions::default())
}
