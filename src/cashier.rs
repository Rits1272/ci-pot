//! Taking and holding Cashu ecash.
//!
//! A funder pays by handing over a Cashu token. The cashier first checks the
//! token offline (in sats, from a mint this operator accepts) and then
//! redeems it at the mint. Redeeming swaps the token's proofs for new ones
//! only this wallet holds, so the same token cannot be credited twice.
//!
//! Everything received sits in one wallet in the data directory. The ledger,
//! not the wallet, knows how much of it belongs to which pot.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use cdk::nuts::{CurrencyUnit, Token};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::WalletSqliteDatabase;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub struct Cashier {
    mints: Vec<String>,
    database: Arc<WalletSqliteDatabase>,
    seed: [u8; 64],
}

impl Cashier {
    /// Opens the wallet in `data_dir`, creating it on first use.
    pub async fn open(data_dir: &Path, mints: &[String]) -> Result<Self> {
        make_private_dir(data_dir)?;
        let seed = load_or_create_seed(&data_dir.join("wallet.seed"))?;
        let path = data_dir.join("wallet.sqlite");
        let database = WalletSqliteDatabase::new(path.to_string_lossy().as_ref())
            .await
            .map_err(|e| anyhow!("cannot open {}: {e}", path.display()))?;
        Ok(Cashier {
            mints: mints.iter().map(|m| normalize(m)).collect(),
            database: Arc::new(database),
            seed,
        })
    }

    pub fn mints(&self) -> &[String] {
        &self.mints
    }

    /// Checks a token without contacting the mint. Returns its mint and value in sats.
    pub fn inspect(&self, token: &str) -> Result<(String, u64)> {
        let parsed =
            Token::from_str(token.trim()).map_err(|e| anyhow!("not a Cashu token: {e}"))?;
        if parsed.unit() != Some(CurrencyUnit::Sat) {
            bail!("token must be in sats");
        }
        let mint = parsed
            .mint_url()
            .map_err(|e| anyhow!("token names no mint: {e}"))?;
        let mint = normalize(&mint.to_string());
        if !self.mints.contains(&mint) {
            bail!(
                "token is from {mint}; accepted mints are {}",
                self.mints.join(", ")
            );
        }
        let value = parsed
            .value()
            .map_err(|e| anyhow!("token has no value: {e}"))?;
        Ok((mint, u64::from(value)))
    }

    /// Redeems a token at its mint and returns the sats received.
    pub async fn redeem(&self, token: &str) -> Result<u64> {
        let (mint, _) = self.inspect(token)?;
        let received = self
            .wallet(&mint)?
            .receive(token.trim(), ReceiveOptions::default())
            .await
            .map_err(|e| anyhow!("the mint did not accept the token: {e}"))?;
        Ok(u64::from(received))
    }

    /// Sats held at each accepted mint.
    pub async fn balances(&self) -> Result<Vec<(String, u64)>> {
        let mut balances = Vec::new();
        for mint in &self.mints {
            let balance = self.wallet(mint)?.total_balance().await?;
            balances.push((mint.clone(), u64::from(balance)));
        }
        Ok(balances)
    }

    /// Turns `sat` held at `mint` into a token the operator can paste into any Cashu wallet.
    pub async fn withdraw(&self, mint: &str, sat: u64) -> Result<String> {
        let mint = normalize(mint);
        if !self.mints.contains(&mint) {
            bail!("{mint} is not one of this wallet's mints");
        }
        let wallet = self.wallet(&mint)?;
        let prepared = wallet
            .prepare_send(Amount::from(sat), SendOptions::default())
            .await?;
        Ok(prepared.confirm(None).await?.to_string())
    }

    fn wallet(&self, mint: &str) -> Result<Wallet> {
        Wallet::new(
            mint,
            CurrencyUnit::Sat,
            self.database.clone(),
            self.seed,
            None,
        )
        .map_err(|e| anyhow!("cannot open a wallet for {mint}: {e}"))
    }
}

/// The ledger's name for a token: its SHA-256. The token itself is never stored.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.trim().as_bytes()))
}

/// Mint URLs are compared as text, so strip what does not change their meaning.
fn normalize(mint: &str) -> String {
    mint.trim().trim_end_matches('/').to_string()
}

/// Creates `dir` if needed and closes it to other users: it holds the wallet.
pub fn make_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", dir.display()))?;
    }
    Ok(())
}

/// The wallet seed is 64 random bytes, kept as hex in a file only the owner can read.
fn load_or_create_seed(path: &Path) -> Result<[u8; 64]> {
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let bytes =
            hex::decode(text.trim()).with_context(|| format!("{} is not hex", path.display()))?;
        return bytes
            .try_into()
            .map_err(|_| anyhow!("{} must hold 64 bytes", path.display()));
    }
    let mut seed = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut seed);

    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?
        .write_all(hex::encode(seed).as_bytes())?;
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn something_that_is_not_a_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cashier = Cashier::open(dir.path(), &["https://mint.example".into()])
            .await
            .unwrap();
        assert!(cashier.inspect("hello").is_err());
        assert!(cashier.redeem("cashuBnotreal").await.is_err());
    }

    #[tokio::test]
    async fn the_seed_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = Cashier::open(dir.path(), &[]).await.unwrap().seed;
        let second = Cashier::open(dir.path(), &[]).await.unwrap().seed;
        assert_eq!(first, second);
    }

    #[test]
    fn a_token_hash_ignores_surrounding_whitespace() {
        assert_eq!(token_hash(" cashuBabc\n"), token_hash("cashuBabc"));
        assert_ne!(token_hash("cashuBabc"), token_hash("cashuBabd"));
    }
}
