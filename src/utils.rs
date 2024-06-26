use std::{
    str::FromStr,
    sync::{atomic::AtomicUsize, Arc, RwLock},
};

use anyhow::{anyhow, Result};
use backoff::ExponentialBackoff;
use dashmap::DashMap;
use fixed::types::I80F48;
use marginfi::{
    bank_authority_seed, bank_seed,
    prelude::MarginfiResult,
    state::{
        marginfi_account::{calc_value, Balance, BalanceSide, LendingAccount, RequirementType},
        marginfi_group::{Bank, BankVaultType, RiskTier},
        price::{PriceAdapter, PriceBias},
    },
};
use rayon::{iter::ParallelIterator, slice::ParallelSlice};
use serde::{Deserialize, Deserializer};
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;
use yellowstone_grpc_proto::geyser::SubscribeUpdateAccountInfo;

use crate::state_engine::engine::BankWrapper;

pub struct BatchLoadingConfig {
    pub max_batch_size: usize,
    pub max_concurrent_calls: usize,
}

impl BatchLoadingConfig {
    pub const DEFAULT: Self = Self {
        max_batch_size: 100,
        max_concurrent_calls: 64,
    };
}

/// Batch load accounts from the RPC client using the getMultipleAccounts RPC call.
///
/// - `max_batch_size`: The maximum number of accounts to load in a single RPC call.
/// - `max_concurrent_calls`: The maximum number of concurrent RPC calls.
///
/// This function will perform multiple RPC calls concurrently, up to `max_concurrent_calls`.
/// If the number of pending RPC calls exceeds `max_concurrent_calls`, the function will
/// await until some calls complete before initiating more, to respect the concurrency limit.
/// Additionally, logs progress information including the number of accounts being fetched,
/// the size of each chunk, and the current progress using trace and debug logs.
pub fn batch_get_multiple_accounts(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    addresses: &[Pubkey],
    BatchLoadingConfig {
        max_batch_size,
        max_concurrent_calls,
    }: BatchLoadingConfig,
) -> anyhow::Result<Vec<Option<Account>>> {
    let batched_addresses = addresses.chunks(max_batch_size * max_concurrent_calls);
    let total_addresses = addresses.len();
    let total_batches = batched_addresses.len();

    let mut accounts = Vec::new();
    let fetched_accounts = Arc::new(AtomicUsize::new(0));

    for (batch_index, batch) in batched_addresses.enumerate() {
        let batch_size = batch.len();

        log::trace!(
            "Fetching batch {} / {} with {} addresses.",
            batch_index + 1,
            total_batches,
            batch_size
        );

        let mut batched_accounts = batch
            .par_chunks(max_batch_size)
            .map(|chunk| -> anyhow::Result<Vec<_>> {
                let rpc_client = rpc_client.clone();
                let chunk = chunk.to_vec();
                let chunk_size = chunk.len();

                log::trace!(" - Fetching chunk of size {}", chunk_size);

                let chunk_res = backoff::retry(ExponentialBackoff::default(), move || {
                    let rpc_client = rpc_client.clone();
                    let chunk = chunk.clone();

                    rpc_client
                        .get_multiple_accounts_with_config(
                            &chunk,
                            RpcAccountInfoConfig {
                                encoding: Some(UiAccountEncoding::Base64Zstd),
                                ..Default::default()
                            },
                        )
                        .map_err(backoff::Error::transient)
                })?
                .value;

                let fetched_chunk_size = chunk_res.len();

                fetched_accounts
                    .fetch_add(fetched_chunk_size, std::sync::atomic::Ordering::Relaxed);

                log::trace!(
                    " - Fetched chunk with {} accounts. Progress: {} / {}",
                    fetched_chunk_size,
                    fetched_accounts.load(std::sync::atomic::Ordering::Relaxed),
                    total_addresses
                );

                Ok(chunk_res)
            })
            .collect::<Result<Vec<_>>>()?
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();

        accounts.append(&mut batched_accounts);
    }

    log::debug!(
        "Finished fetching all accounts. Total accounts fetched: {}",
        fetched_accounts.load(std::sync::atomic::Ordering::Relaxed)
    );

    Ok(accounts)
}

// Field parsers to save compute. All account validation is assumed to be done
// outside of these methods.
pub mod accessor {
    use super::*;

    pub fn amount(bytes: &[u8]) -> u64 {
        let mut amount_bytes = [0u8; 8];
        amount_bytes.copy_from_slice(&bytes[64..72]);
        u64::from_le_bytes(amount_bytes)
    }

    pub fn mint(bytes: &[u8]) -> Pubkey {
        let mut mint_bytes = [0u8; 32];
        mint_bytes.copy_from_slice(&bytes[..32]);
        Pubkey::new_from_array(mint_bytes)
    }

    pub fn authority(bytes: &[u8]) -> Pubkey {
        let mut owner_bytes = [0u8; 32];
        owner_bytes.copy_from_slice(&bytes[32..64]);
        Pubkey::new_from_array(owner_bytes)
    }
}

pub fn account_update_to_account(account_update: &SubscribeUpdateAccountInfo) -> Result<Account> {
    let SubscribeUpdateAccountInfo {
        lamports,
        owner,
        executable,
        rent_epoch,
        data,
        ..
    } = account_update;

    let owner = Pubkey::try_from(owner.clone()).expect("Invalid pubkey");

    let account = Account {
        lamports: *lamports,
        data: data.clone(),
        owner,
        executable: *executable,
        rent_epoch: *rent_epoch,
    };

    Ok(account)
}

pub(crate) fn from_pubkey_string<'de, D>(deserializer: D) -> Result<Pubkey, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Pubkey::from_str(&s).map_err(serde::de::Error::custom)
}

pub(crate) fn from_option_vec_pubkey_string<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<Pubkey>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<Vec<String>> = Deserialize::deserialize(deserializer)?;

    match s {
        Some(a) => Ok(Some(
            a.into_iter()
                .map(|s| Pubkey::from_str(&s).map_err(serde::de::Error::custom))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        None => Ok(None),
    }
}

pub(crate) fn fixed_from_float<'de, D>(deserializer: D) -> Result<I80F48, D::Error>
where
    D: Deserializer<'de>,
{
    let s: f64 = Deserialize::deserialize(deserializer)?;

    Ok(I80F48::from_num(s))
}

pub(crate) fn from_vec_str_to_pubkey<'de, D>(deserializer: D) -> Result<Vec<Pubkey>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Vec<String> = Deserialize::deserialize(deserializer)?;
    s.into_iter()
        .map(|s| Pubkey::from_str(&s).map_err(serde::de::Error::custom))
        .collect()
}

pub struct BankAccountWithPriceFeedEva<'a> {
    bank: Arc<RwLock<BankWrapper>>,
    balance: &'a Balance,
}

impl<'a> BankAccountWithPriceFeedEva<'a> {
    pub fn load(
        lending_account: &'a LendingAccount,
        banks: Arc<DashMap<Pubkey, Arc<RwLock<BankWrapper>>>>,
    ) -> anyhow::Result<Vec<BankAccountWithPriceFeedEva<'a>>> {
        let active_balances = lending_account
            .balances
            .iter()
            .filter(|balance| balance.active);

        active_balances
            .enumerate()
            .map(|(_i, balance)| {
                let bank = banks
                    .get(&balance.bank_pk)
                    .ok_or_else(|| anyhow::anyhow!("Bank not found"))?
                    .clone();

                Ok(BankAccountWithPriceFeedEva { bank, balance })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn load_single(
        lending_account: &'a LendingAccount,
        banks: Arc<DashMap<Pubkey, Arc<RwLock<BankWrapper>>>>,
        bank_pk: &Pubkey,
    ) -> anyhow::Result<Option<BankAccountWithPriceFeedEva<'a>>> {
        let balance = lending_account
            .balances
            .iter()
            .find(|balance| balance.active && balance.bank_pk == *bank_pk);

        if balance.is_none() {
            return Ok(None);
        }

        let balance = balance.unwrap();

        let bank = banks
            .get(&balance.bank_pk)
            .ok_or_else(|| anyhow::anyhow!("Bank not found"))?
            .clone();

        Ok(Some(BankAccountWithPriceFeedEva { bank, balance }))
    }

    #[inline(always)]
    /// Calculate the value of the assets and liabilities of the account in the form of (assets, liabilities)
    ///
    /// Nuances:
    /// 1. Maintenance requirement is calculated using the real time price feed.
    /// 2. Initial requirement is calculated using the time weighted price feed, if available.
    /// 3. Initial requirement is discounted by the initial discount, if enabled and the usd limit is exceeded.
    /// 4. Assets are only calculated for collateral risk tier.
    /// 5. Oracle errors are ignored for deposits in isolated risk tier.
    pub fn calc_weighted_assets_and_liabilities_values(
        &self,
        requirement_type: RequirementType,
    ) -> anyhow::Result<(I80F48, I80F48)> {
        match self.balance.get_side() {
            Some(side) => {
                let bank = &self.bank.read().unwrap().bank;
                match side {
                    BalanceSide::Assets => Ok((
                        self.calc_weighted_assets(requirement_type, &bank)?,
                        I80F48::ZERO,
                    )),
                    BalanceSide::Liabilities => Ok((
                        I80F48::ZERO,
                        self.calc_weighted_liabs(requirement_type, &bank)?,
                    )),
                }
            }
            None => Ok((I80F48::ZERO, I80F48::ZERO)),
        }
    }

    #[inline(always)]
    fn calc_weighted_assets(
        &self,
        requirement_type: RequirementType,
        bank: &Bank,
    ) -> anyhow::Result<I80F48> {
        match bank.config.risk_tier {
            RiskTier::Collateral => {
                let price_feed = &self.bank.read().unwrap().oracle_adapter.price_adapter;
                let mut asset_weight = bank
                    .config
                    .get_weight(requirement_type, BalanceSide::Assets);

                let lower_price = price_feed.get_price_of_type(
                    requirement_type.get_oracle_price_type(),
                    Some(PriceBias::Low),
                )?;

                if matches!(requirement_type, RequirementType::Initial) {
                    if let Some(discount) =
                        bank.maybe_get_asset_weight_init_discount(lower_price)?
                    {
                        asset_weight = asset_weight
                            .checked_mul(discount)
                            .ok_or_else(|| anyhow!("math error"))?;
                    }
                }

                Ok(calc_value(
                    bank.get_asset_amount(self.balance.asset_shares.into())?,
                    lower_price,
                    bank.mint_decimals,
                    Some(asset_weight),
                )?)
            }
            RiskTier::Isolated => Ok(I80F48::ZERO),
        }
    }

    #[inline(always)]
    fn calc_weighted_liabs(
        &self,
        requirement_type: RequirementType,
        bank: &Bank,
    ) -> MarginfiResult<I80F48> {
        let price_feed = &self.bank.read().unwrap().oracle_adapter.price_adapter;
        let liability_weight = bank
            .config
            .get_weight(requirement_type, BalanceSide::Liabilities);

        let higher_price = price_feed.get_price_of_type(
            requirement_type.get_oracle_price_type(),
            Some(PriceBias::High),
        )?;

        calc_value(
            bank.get_liability_amount(self.balance.liability_shares.into())?,
            higher_price,
            bank.mint_decimals,
            Some(liability_weight),
        )
    }

    #[inline]
    pub fn is_empty(&self, side: BalanceSide) -> bool {
        self.balance.is_empty(side)
    }
}

pub fn find_bank_vault_pda(
    bank_pk: &Pubkey,
    vault_type: BankVaultType,
    program_id: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(bank_seed!(vault_type, bank_pk), program_id)
}

pub fn find_bank_vault_authority_pda(
    bank_pk: &Pubkey,
    vault_type: BankVaultType,
    program_id: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(bank_authority_seed!(vault_type, bank_pk), program_id)
}

pub fn calc_weighted_assets(
    bank_rw_lock: Arc<RwLock<BankWrapper>>,
    amount: I80F48,
    requirement_type: RequirementType,
) -> anyhow::Result<I80F48> {
    let bank_wrapper_ref = bank_rw_lock.read().unwrap();
    let price_feed = &bank_wrapper_ref.oracle_adapter.price_adapter;
    let mut asset_weight = bank_wrapper_ref
        .bank
        .config
        .get_weight(requirement_type, BalanceSide::Assets);

    let price_bias = if matches!(requirement_type, RequirementType::Equity) {
        None
    } else {
        Some(PriceBias::Low)
    };

    let lower_price =
        price_feed.get_price_of_type(requirement_type.get_oracle_price_type(), price_bias)?;

    if matches!(requirement_type, RequirementType::Initial) {
        if let Some(discount) = bank_wrapper_ref
            .bank
            .maybe_get_asset_weight_init_discount(lower_price)?
        {
            asset_weight = asset_weight
                .checked_mul(discount)
                .ok_or_else(|| anyhow!("math error"))?;
        }
    }

    Ok(calc_value(
        amount,
        lower_price,
        bank_wrapper_ref.bank.mint_decimals,
        Some(asset_weight),
    )?)
}

#[inline(always)]
pub fn calc_weighted_liabs(
    bank_rw_lock: Arc<RwLock<BankWrapper>>,
    amount: I80F48,
    requirement_type: RequirementType,
) -> anyhow::Result<I80F48> {
    let bank_wrapper_ref = bank_rw_lock.read().unwrap();
    let bank = &bank_wrapper_ref.bank;
    let price_feed = &bank_wrapper_ref.oracle_adapter.price_adapter;
    let liability_weight = bank
        .config
        .get_weight(requirement_type, BalanceSide::Liabilities);

    let price_bias = if matches!(requirement_type, RequirementType::Equity) {
        None
    } else {
        Some(PriceBias::High)
    };

    let higher_price =
        price_feed.get_price_of_type(requirement_type.get_oracle_price_type(), price_bias)?;

    Ok(calc_value(
        amount,
        higher_price,
        bank.mint_decimals,
        Some(liability_weight),
    )?)
}
