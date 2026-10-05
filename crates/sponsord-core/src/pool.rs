//! `PoolChain`: the on-chain `PaymentPool` operations the sponsor needs. It
//! reads the pool's remaining balance, tops the pool up from the treasury
//! (the hot wallet that owns the pool), reads the pool's owner (a boot-time
//! sanity check), and reads a signer's registration. `ChainPool` backs it in
//! production; tests use `test_support::FakePool`.

use alloy::network::{EthereumWallet, TxSigner};
use alloy::primitives::{Address, B256, Signature, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use async_trait::async_trait;
use decdn_client::buyer_pool::{ensure_allowance, top_up};
use decdn_common::redact::strip_urls;
use decdn_incentive::payment_pool::PaymentPool;
use sponsord_api::MicroUsdc;

/// A signer's registration under a pool: the terms fixed by the first
/// capability redeemed for it (`PaymentPool.authorized[poolId][signer]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authorization {
    pub spending_cap: MicroUsdc,
    pub expiry: u64,
}

/// Render a chain-call error and its causes as one line, without the RPC URL
/// reqwest names in each of them (#38). The causes carry the failure class
/// (connection refused, timeout, DNS, TLS) that the outer error's Display
/// leaves out. Each is stripped on its own, so a removed URL cannot take the
/// separator with it, and a cause that repeats the error before it (alloy's
/// transport error forwards reqwest's message) is skipped.
fn redacted(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut next = Some(err);
    while let Some(e) = next {
        let part = strip_urls(&e.to_string()).into_owned();
        if parts.last() != Some(&part) {
            parts.push(part);
        }
        next = e.source();
    }
    parts.join(": ")
}

/// The contract's unregistered state is a zero cap and zero expiry
/// (`PaymentPool._registerCapability`); anything else is a registration.
fn registration(cap: u64, expiry: u64) -> Option<Authorization> {
    (cap != 0 || expiry != 0).then_some(Authorization {
        spending_cap: MicroUsdc(cap),
        expiry,
    })
}

#[async_trait]
pub trait PoolChain: Send + Sync {
    /// The treasury wallet address, which must be the pool's on-chain owner.
    fn owner_address(&self) -> Address;

    /// The pool's still-payable balance (`deposit - totalRedeemed`).
    async fn remaining(&self, pool_id: B256) -> anyhow::Result<MicroUsdc>;

    /// Add `additional` USDC to the pool from the treasury; returns the
    /// credited amount.
    async fn top_up(&self, pool_id: B256, additional: MicroUsdc) -> anyhow::Result<MicroUsdc>;

    /// The pool's on-chain `owner` (used once at boot to confirm this wallet
    /// owns the configured pool).
    async fn pool_owner(&self, pool_id: B256) -> anyhow::Result<Address>;

    /// `signer`'s registration under `pool_id`, or `None` if no capability
    /// has been redeemed for it yet.
    async fn authorization(
        &self,
        pool_id: B256,
        signer: Address,
    ) -> anyhow::Result<Option<Authorization>>;
}

/// The `PaymentPool` contract, read through and paid into by the treasury
/// wallet.
pub struct ChainPool {
    contract: PaymentPool::PaymentPoolInstance<DynProvider>,
    provider: DynProvider,
    owner: Address,
    token: Address,
    payment_pool: Address,
}

impl ChainPool {
    /// Connect to the `PaymentPool` at `payment_pool` over `rpc_url`, sending
    /// transactions as `signer`, and read its settlement token (the
    /// contract's immutable `usdc()`).
    ///
    /// # Errors
    ///
    /// A malformed `rpc_url`, or the `usdc()` read fails.
    pub async fn connect<S>(rpc_url: &str, payment_pool: Address, signer: S) -> anyhow::Result<Self>
    where
        S: TxSigner<Signature> + Send + Sync + 'static,
    {
        let owner = signer.address();
        // The URL stays out of every error `ChainPool` returns: RPC URLs often
        // carry an API key in their path or query. Here it is never formatted;
        // transport errors, whose reqwest Display ends in ` for url (<url>)`,
        // go through `redacted` (#38).
        let url = rpc_url.parse().map_err(|e| {
            anyhow::anyhow!(
                "RPC URL ({} characters) is not a valid URL: {e}",
                rpc_url.len()
            )
        })?;
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::new(signer))
            .connect_http(url)
            .erased();
        let contract = PaymentPool::new(payment_pool, provider.clone());
        let token = contract
            .usdc()
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("read PaymentPool.usdc(): {}", redacted(&e)))?;
        Ok(Self {
            contract,
            provider,
            owner,
            token,
            payment_pool,
        })
    }
}

#[async_trait]
impl PoolChain for ChainPool {
    fn owner_address(&self) -> Address {
        self.owner
    }

    async fn remaining(&self, pool_id: B256) -> anyhow::Result<MicroUsdc> {
        let pool = self
            .contract
            .getPool(pool_id)
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("getPool({pool_id}): {}", redacted(&e)))?;
        Ok(MicroUsdc(pool.deposit.saturating_sub(pool.totalRedeemed)))
    }

    async fn top_up(&self, pool_id: B256, additional: MicroUsdc) -> anyhow::Result<MicroUsdc> {
        let amount = U256::from(additional.0);
        ensure_allowance(
            &self.provider,
            self.token,
            self.owner,
            self.payment_pool,
            Some(amount),
        )
        .await
        .map_err(|e| anyhow::anyhow!("approve the top-up: {}", redacted(e.as_ref())))?;
        let topped = top_up(&self.contract, pool_id, amount)
            .await
            .map_err(|e| anyhow::anyhow!("top up {pool_id}: {}", redacted(e.as_ref())))?;
        Ok(MicroUsdc(
            u64::try_from(topped.credited).unwrap_or(u64::MAX),
        ))
    }

    async fn pool_owner(&self, pool_id: B256) -> anyhow::Result<Address> {
        let pool = self
            .contract
            .getPool(pool_id)
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("getPool({pool_id}): {}", redacted(&e)))?;
        Ok(pool.owner)
    }

    async fn authorization(
        &self,
        pool_id: B256,
        signer: Address,
    ) -> anyhow::Result<Option<Authorization>> {
        let a = self
            .contract
            .getAuthorization(pool_id, signer)
            .call()
            .await
            .map_err(|e| {
                anyhow::anyhow!("getAuthorization({pool_id}, {signer}): {}", redacted(&e))
            })?;
        Ok(registration(a.cap, a.expiry))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use alloy::signers::local::PrivateKeySigner;

    use super::*;

    #[tokio::test]
    async fn a_malformed_rpc_url_stays_out_of_the_error() {
        let err = ChainPool::connect(
            "htt p://rpc.example/v2/SECRET-API-KEY",
            Address::ZERO,
            PrivateKeySigner::random(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(!err.contains("SECRET-API-KEY"), "{err}");
        assert!(err.contains("not a valid URL"), "{err}");
    }

    /// reqwest names the URL in a transport error; the key in its path must
    /// not survive into `ChainPool`'s error (#38).
    #[tokio::test]
    async fn an_unreachable_rpc_url_stays_out_of_the_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = ChainPool::connect(
            &format!("http://127.0.0.1:{port}/v3/SECRET-API-KEY"),
            Address::ZERO,
            PrivateKeySigner::random(),
        )
        .await
        .err()
        .unwrap();
        let err = format!("{err:#}");
        assert!(err.contains("usdc()"), "{err}");
        assert!(
            err.contains("tcp connect error"),
            "the cause is kept: {err}"
        );
        assert!(!err.contains("SECRET-API-KEY"), "{err}");
        assert!(!err.contains(&format!(":{port}")), "{err}");
    }

    #[test]
    fn zero_cap_and_expiry_is_unregistered() {
        let auth = |cap, expiry| Authorization {
            spending_cap: MicroUsdc(cap),
            expiry,
        };
        assert_eq!(registration(0, 0), None);
        assert_eq!(registration(5, 10), Some(auth(5, 10)));
        assert_eq!(registration(0, 10), Some(auth(0, 10)));
    }
}
