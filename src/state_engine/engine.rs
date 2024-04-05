use solana_account_decoder::UiAccountEncoding;
use solana_account_decoder::UiDataSliceConfig;
use solana_sdk::bs58;
use std::sync::Arc;

use anchor_client::anchor_lang::AccountDeserialize;
use anchor_client::anchor_lang::Discriminator;
use anchor_client::Program;
use anyhow::anyhow;
use dashmap::{DashMap, DashSet};
use log::{debug, error, warn};
use marginfi::state::{
    marginfi_account::MarginfiAccount, marginfi_group::Bank, price::OraclePriceFeedAdapter,
};
use solana_client::{
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::{account_info::IntoAccountInfo, program_pack::Pack, pubkey::Pubkey};
use solana_sdk::{account::Account, signature::Keypair};
use tokio::sync::{Mutex, RwLock};

use crate::utils::{accessor, batch_get_multiple_accounts, BatchLoadingConfig};

const BANK_GROUP_PK_OFFSET: usize = 8 + 8 + 1;

pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub account: MarginfiAccount,
    pub banks: Vec<Arc<RwLock<BankWrapper>>>,
}

impl MarginfiAccountWrapper {
    pub fn new(
        address: Pubkey,
        account: MarginfiAccount,
        banks: Vec<Arc<RwLock<BankWrapper>>>,
    ) -> Self {
        Self {
            address,
            account,
            banks,
        }
    }
}

pub struct OracleWrapper {
    pub address: Pubkey,
    pub price_adapter: OraclePriceFeedAdapter,
}

impl OracleWrapper {
    pub fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self {
        Self {
            address,
            price_adapter,
        }
    }
}

pub struct BankWrapper {
    pub address: Pubkey,
    pub bank: Bank,
    pub oracle_adapter: OracleWrapper,
}

impl BankWrapper {
    pub fn new(address: Pubkey, bank: Bank, oracle_adapter_wrapper: OracleWrapper) -> Self {
        Self {
            address,
            bank,
            oracle_adapter: oracle_adapter_wrapper,
        }
    }
}

pub struct TokenAccountWrapper {
    pub address: Pubkey,
    pub mint: Pubkey,
    pub balance: u64,
    pub mint_decimals: u8,
}

#[derive(Debug)]
pub struct StateEngineConfig {
    pub rpc_url: String,
    pub yellowstone_endpoint: String,
    pub yellowstone_x_token: Option<String>,
    pub marginfi_program_id: Pubkey,
    pub marginfi_group_address: Pubkey,
    pub signer_pubkey: Pubkey,
}

#[allow(dead_code)]
pub struct StateEngineService {
    nb_rpc_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    anchor_client: anchor_client::Client<Arc<Keypair>>,
    marginfi_accounts: DashMap<Pubkey, Arc<RwLock<MarginfiAccountWrapper>>>,
    banks: DashMap<Pubkey, Arc<RwLock<BankWrapper>>>,
    token_accounts: DashMap<Pubkey, Arc<RwLock<TokenAccountWrapper>>>,
    config: StateEngineConfig,
    accounts_to_track: Arc<RwLock<Vec<Pubkey>>>,
    oracle_to_bank_map: DashMap<Pubkey, Vec<Arc<RwLock<BankWrapper>>>>,
    tracked_oracle_accounts: DashSet<Pubkey>,
    tracked_token_accounts: DashSet<Pubkey>,
    update_tasks: Arc<Mutex<DashMap<Pubkey, tokio::task::JoinHandle<anyhow::Result<()>>>>>,
}

#[allow(dead_code)]
impl StateEngineService {
    pub async fn start(config: StateEngineConfig) -> anyhow::Result<Arc<Self>> {
        debug!("StateEngineService::start");

        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(config.rpc_url.clone(), "".to_string()),
            Arc::new(Keypair::new()),
        );

        let nb_rpc_client = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new(
            config.rpc_url.clone(),
        ));
        let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new(
            config.rpc_url.clone(),
        ));

        let state_engine_service = Arc::new(Self {
            marginfi_accounts: DashMap::new(),
            banks: DashMap::new(),
            token_accounts: DashMap::new(),
            anchor_client,
            config,
            nb_rpc_client,
            rpc_client,
            accounts_to_track: Arc::new(RwLock::new(Vec::new())),
            oracle_to_bank_map: DashMap::new(),
            tracked_oracle_accounts: DashSet::new(),
            tracked_token_accounts: DashSet::new(),
            update_tasks: Arc::new(Mutex::new(DashMap::new())),
        });

        state_engine_service.load_oracles_and_banks().await?;
        state_engine_service.load_token_accounts().await?;

        Ok(state_engine_service)
    }

    pub fn get_accounts_to_track(&self) -> Vec<Pubkey> {
        self.tracked_oracle_accounts
            .iter()
            .chain(self.tracked_token_accounts.iter())
            .map(|e| *e)
            .collect::<Vec<_>>()
    }

    async fn load_oracles_and_banks(self: &Arc<Self>) -> anyhow::Result<()> {
        let program: Program<Arc<Keypair>> = self.anchor_client.program(marginfi::id())?;
        let banks = program
            .accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                self.config.marginfi_group_address.as_ref(),
            ))])
            .await?;

        let oracle_keys = banks
            .iter()
            .map(|(_, bank)| bank.config.oracle_keys[0])
            .collect::<Vec<_>>();

        let mut oracle_accounts = batch_get_multiple_accounts(
            self.nb_rpc_client.clone(),
            &oracle_keys,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        let mut oracles_with_addresses = oracle_keys
            .iter()
            .zip(oracle_accounts.iter_mut())
            .collect::<Vec<_>>();

        for ((bank_address, bank), (oracle_address, maybe_oracle_account)) in
            banks.iter().zip(oracles_with_addresses.iter_mut())
        {
            let oracle_ai =
                (*oracle_address, maybe_oracle_account.as_mut().unwrap()).into_account_info();
            let oracle_ai_c = oracle_ai.clone();

            let bank_ref = self
                .banks
                .entry(*bank_address)
                .and_modify(|bank_entry| match bank_entry.try_write() {
                    Ok(mut bank_wg) => {
                        bank_wg.bank = *bank;
                    }
                    Err(e) => {
                        error!("Failed to acquire write lock on bank: {}", e);
                    }
                })
                .or_insert_with(|| {
                    Arc::new(RwLock::new(BankWrapper::new(
                        *bank_address,
                        *bank,
                        OracleWrapper::new(
                            **oracle_address,
                            OraclePriceFeedAdapter::try_from_bank_config(
                                &bank.config,
                                &[oracle_ai_c],
                                i64::MAX,
                                u64::MAX,
                            )
                            .unwrap(),
                        ),
                    )))
                });

            self.oracle_to_bank_map
                .entry(**oracle_address)
                .and_modify(|vec| vec.push(bank_ref.clone()))
                .or_insert_with(|| vec![bank_ref.clone()]);

            self.tracked_oracle_accounts.insert(**oracle_address);
        }

        Ok(())
    }

    pub fn update_oracle(
        &self,
        oracle_address: &Pubkey,
        mut oracle_account: Account,
    ) -> anyhow::Result<()> {
        if let Some(banks_to_update) = self.oracle_to_bank_map.get(oracle_address) {
            let oracle_ai = (oracle_address, &mut oracle_account).into_account_info();
            for bank_to_update in banks_to_update.iter() {
                if let Ok(mut bank_to_update) = bank_to_update.try_write() {
                    bank_to_update.oracle_adapter.price_adapter =
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank_to_update.bank.config,
                            &[oracle_ai.clone()],
                            i64::MAX,
                            u64::MAX,
                        )?;
                } else {
                    warn!("Failed to acquire write lock on bank, oracle update skipped");
                }
            }
        } else {
            warn!("Received update for unknown oracle {}", oracle_address);
        }

        Ok(())
    }

    pub fn update_bank(&self, bank_address: &Pubkey, bank: Account) -> anyhow::Result<bool> {
        let bank = bytemuck::from_bytes::<Bank>(&bank.data.as_slice()[8..]);

        let new_bank = self.banks.contains_key(bank_address);

        self.banks
            .entry(*bank_address)
            .and_modify(|bank_entry| {
                if let Ok(mut bank_entry) = bank_entry.try_write() {
                    bank_entry.bank = *bank;
                } else {
                    warn!("Failed to acquire write lock on bank, bank update skipped");
                }
            })
            .or_insert_with(|| {
                debug!("Received update for a new bank {}", bank_address);

                let oracle_address = bank.config.oracle_keys[0];
                let mut oracle_account = self.rpc_client.get_account(&oracle_address).unwrap();
                let oracle_account_ai = (&oracle_address, &mut oracle_account).into_account_info();

                self.tracked_oracle_accounts.insert(oracle_address);

                Arc::new(RwLock::new(BankWrapper::new(
                    *bank_address,
                    *bank,
                    OracleWrapper::new(
                        oracle_address,
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &[oracle_account_ai],
                            i64::MAX,
                            u64::MAX,
                        )
                        .unwrap(),
                    ),
                )))
            });

        Ok(new_bank)
    }

    async fn load_token_accounts(self: &Arc<Self>) -> anyhow::Result<()> {
        let banks = self.banks.clone();
        let mut bank_mints = Vec::new();
        for (_, bank) in banks {
            let bank_guard = bank.read().await;
            bank_mints.push(bank_guard.bank.mint);
        }

        let mut token_account_addresses = vec![];

        for mint in bank_mints.iter() {
            let ata = spl_associated_token_account::get_associated_token_address(
                &self.config.signer_pubkey,
                mint,
            );
            token_account_addresses.push(ata);
        }

        let accounts = batch_get_multiple_accounts(
            self.nb_rpc_client.clone(),
            &token_account_addresses,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        let token_accounts_with_addresses_and_mints = token_account_addresses
            .iter()
            .zip(bank_mints.iter())
            .zip(accounts)
            .collect::<Vec<_>>();

        for ((token_account_address, mint), maybe_token_account) in
            token_accounts_with_addresses_and_mints.iter()
        {
            let balance = maybe_token_account
                .as_ref()
                .map(|a| accessor::amount(&a.data))
                .unwrap_or(0);

            let token_accounts = self.token_accounts.clone();

            token_accounts
                .entry(**mint)
                .and_modify(|token_account| {
                    let token_account = Arc::clone(token_account);
                    tokio::spawn(async move {
                        let mut token_account_guard = token_account.write().await;
                        token_account_guard.balance = balance;
                    });
                })
                .or_insert_with(|| {
                    Arc::new(RwLock::new(TokenAccountWrapper {
                        address: **token_account_address,
                        mint: **mint,
                        balance,
                        mint_decimals: 0,
                    }))
                });

            self.tracked_token_accounts.insert(**token_account_address);
        }

        Ok(())
    }

    pub fn update_token_account(
        &self,
        token_account_address: &Pubkey,
        token_account: Account,
    ) -> anyhow::Result<()> {
        let token_accounts = self.token_accounts.clone();
        let mint = accessor::mint(&token_account.data);
        let balance = accessor::amount(&token_account.data);

        token_accounts
            .entry(mint)
            .and_modify(|token_account| {
                let token_account = Arc::clone(token_account);
                tokio::spawn(async move {
                    let mut token_account_guard = token_account.write().await;
                    token_account_guard.balance = balance;
                });
            })
            .or_insert_with(|| {
                let mint_account = self.rpc_client.get_account(&mint).unwrap();
                let decimals = spl_token::state::Mint::unpack(&mint_account.data)
                    .map_err(|e| anyhow::anyhow!("Failed to unpack mint: {:?}", e))
                    .unwrap()
                    .decimals;

                Arc::new(RwLock::new(TokenAccountWrapper {
                    address: *token_account_address,
                    mint,
                    balance,
                    mint_decimals: decimals,
                }))
            });

        Ok(())
    }

    pub fn get_group_id(&self) -> Pubkey {
        self.config.marginfi_group_address
    }

    pub fn get_marginfi_program_id(&self) -> Pubkey {
        self.config.marginfi_program_id
    }

    pub fn is_tracked_oracle(&self, address: &Pubkey) -> bool {
        self.tracked_oracle_accounts.contains(address)
    }

    pub fn is_tracked_token_account(&self, address: &Pubkey) -> bool {
        self.tracked_token_accounts.contains(address)
    }

    async fn load_marginfi_accounts(self: &Arc<Self>) -> anyhow::Result<()> {
        let marginfi_account_addresses = self
            .nb_rpc_client
            .get_program_accounts_with_config(
                &self.config.marginfi_program_id,
                RpcProgramAccountsConfig {
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        data_slice: Some(UiDataSliceConfig {
                            offset: 0,
                            length: 0,
                        }),
                        ..Default::default()
                    },
                    filters: Some(vec![
                        #[allow(deprecated)]
                        RpcFilterType::Memcmp(Memcmp {
                            offset: 8,
                            #[allow(deprecated)]
                            bytes: MemcmpEncodedBytes::Base58(
                                self.config.marginfi_group_address.to_string(),
                            ),
                            #[allow(deprecated)]
                            encoding: None,
                        }),
                        #[allow(deprecated)]
                        RpcFilterType::Memcmp(Memcmp {
                            offset: 0,
                            #[allow(deprecated)]
                            bytes: MemcmpEncodedBytes::Base58(
                                bs58::encode(MarginfiAccount::DISCRIMINATOR).into_string(),
                            ),
                            #[allow(deprecated)]
                            encoding: None,
                        }),
                    ]),
                    with_context: Some(false),
                },
            )
            .await?;

        let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_addresses
            .iter()
            .map(|(pubkey, _)| *pubkey)
            .collect();

        let mut marginfi_accounts = batch_get_multiple_accounts(
            self.nb_rpc_client.clone(),
            &marginfi_account_pubkeys,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        for (address, account) in marginfi_account_addresses
            .iter()
            .zip(marginfi_accounts.iter_mut())
        {
            let account = account.as_mut().unwrap();
            let mut data_slice = account.data.as_slice();
            let marginfi_account = MarginfiAccount::try_deserialize(&mut data_slice).unwrap();
            self.update_marginfi_account(&address.0, &marginfi_account)?;
        }

        Ok(())
    }

    pub fn update_marginfi_account(
        &self,
        marginfi_account_address: &Pubkey,
        marginfi_account: &MarginfiAccount,
    ) -> anyhow::Result<()> {
        let marginfi_accounts = self.marginfi_accounts.clone();

        marginfi_accounts
            .entry(*marginfi_account_address)
            .and_modify(|marginfi_account_ref| {
                let marginfi_account_ref = Arc::clone(marginfi_account_ref);
                let marginfi_account_updated = *marginfi_account;
                tokio::spawn(async move {
                    let mut marginfi_account_guard = marginfi_account_ref.write().await;
                    marginfi_account_guard.account = marginfi_account_updated;
                });
            })
            .or_insert_with(|| {
                Arc::new(RwLock::new(MarginfiAccountWrapper::new(
                    *marginfi_account_address,
                    *marginfi_account,
                    Vec::new(),
                )))
            });

        Ok(())
    }

    async fn update_all_marginfi_accounts(self: Arc<Self>) -> anyhow::Result<()> {
        let marginfi_accounts = self.marginfi_accounts.clone();
        for account_ref in marginfi_accounts.iter() {
            let account = account_ref.value().read().await;
            let marginfi_account = account.account; // clone the underlying data
            let address = account.address; // get the address from the account

            let update_tasks = self.update_tasks.lock().await;
            let self_clone = Arc::clone(&self);
            let join_handle = tokio::spawn(async move {
                self_clone
                    .update_marginfi_account(&address, &marginfi_account)
                    .map_err(|e| anyhow!("error updating marginfi account {}", e))
            });
            update_tasks.insert(address, join_handle);
        }
        Ok(())
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        tokio::task::spawn(async move {
            loop {
                if let Err(e) = self.clone().update_all_marginfi_accounts().await {
                    error!("Failed to update all marginfi accounts: {}", e);
                }
            }
        });
        Ok(())
    }

    pub async fn start_and_run(config: StateEngineConfig) -> anyhow::Result<()> {
        debug!("start_and_run");
        let service = Self::start(config).await?;
        service.run().await
    }
}
