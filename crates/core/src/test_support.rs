//! Test doubles for code built on `sponsord-core`. Compiled for this
//! crate's own tests and, behind the `test-support` feature, for dependents'
//! tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, B256};
use async_trait::async_trait;

use crate::money::MicroUsdc;
use crate::treasury::{Authorization, Treasury};

/// In-memory pool treasury: fixed owner, a mutable remaining balance, and a
/// settable per-signer registration map.
pub struct FakeTreasury {
    owner: Address,
    remaining: Mutex<u64>,
    registrations: Mutex<HashMap<Address, Authorization>>,
    fail_authorization: AtomicBool,
}

impl FakeTreasury {
    #[must_use]
    pub fn new(owner: Address, remaining: u64) -> Self {
        Self {
            owner,
            remaining: Mutex::new(remaining),
            registrations: Mutex::new(HashMap::new()),
            fail_authorization: AtomicBool::new(false),
        }
    }

    /// Record `signer` as registered on-chain with `auth`.
    pub fn register(&self, signer: Address, auth: Authorization) {
        self.registrations.lock().unwrap().insert(signer, auth);
    }

    /// Make every `authorization` read fail (an RPC outage).
    pub fn fail_authorization_reads(&self, fail: bool) {
        self.fail_authorization.store(fail, Ordering::SeqCst);
    }
}

#[async_trait]
impl Treasury for FakeTreasury {
    fn owner_address(&self) -> Address {
        self.owner
    }
    async fn remaining(&self, _pool_id: B256) -> anyhow::Result<MicroUsdc> {
        let r = self
            .remaining
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        Ok(MicroUsdc(*r))
    }
    async fn top_up(&self, _pool_id: B256, additional: MicroUsdc) -> anyhow::Result<MicroUsdc> {
        let mut r = self
            .remaining
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        *r = r.saturating_add(additional.0);
        Ok(MicroUsdc(*r))
    }
    async fn pool_owner(&self, _pool_id: B256) -> anyhow::Result<Address> {
        Ok(self.owner)
    }
    async fn authorization(
        &self,
        _pool_id: B256,
        signer: Address,
    ) -> anyhow::Result<Option<Authorization>> {
        anyhow::ensure!(
            !self.fail_authorization.load(Ordering::SeqCst),
            "authorization read failed"
        );
        Ok(self.registrations.lock().unwrap().get(&signer).copied())
    }
}
