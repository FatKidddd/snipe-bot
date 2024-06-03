use crate::raydium::LiquidityStateLayoutV4;
use futures_util::StreamExt;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    nonblocking::pubsub_client::PubsubClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
    rpc_response::{Response, RpcKeyedAccount, SlotUpdate},
};
use solana_metrics::{datapoint_error, datapoint_info};
use solana_sdk::{
    account::Account,
    clock::Slot,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    pubkey::Pubkey,
    signature::Keypair,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::{sync::mpsc::Sender, time::sleep};
use tonic::Streaming;

// slot update subscription loop that attempts to maintain a connection to an RPC server
pub async fn slot_subscribe_loop(pubsub_addr: String, slot_sender: Sender<Slot>) {
    let mut connect_errors: u64 = 0;
    let mut slot_subscribe_errors: u64 = 0;
    let mut slot_subscribe_disconnect_errors: u64 = 0;

    loop {
        sleep(Duration::from_secs(1)).await;

        match PubsubClient::new(&pubsub_addr).await {
            Ok(pubsub_client) => match pubsub_client.slot_updates_subscribe().await {
                Ok((mut slot_update_subscription, _unsubscribe_fn)) => {
                    while let Some(slot_update) = slot_update_subscription.next().await {
                        if let SlotUpdate::FirstShredReceived { slot, timestamp: _ } = slot_update {
                            datapoint_info!("slot_subscribe_slot", ("slot", slot, i64));
                            if slot_sender.send(slot).await.is_err() {
                                datapoint_error!("slot_subscribe_send_error", ("errors", 1, i64));
                                return;
                            }
                        }
                    }
                    slot_subscribe_disconnect_errors += 1;
                    datapoint_error!(
                        "slot_subscribe_disconnect_error",
                        ("errors", slot_subscribe_disconnect_errors, i64)
                    );
                }
                Err(e) => {
                    slot_subscribe_errors += 1;
                    datapoint_error!(
                        "slot_subscribe_error",
                        ("errors", slot_subscribe_errors, i64),
                        ("error_str", e.to_string(), String),
                    );
                }
            },
            Err(e) => {
                connect_errors += 1;
                datapoint_error!(
                    "slot_subscribe_pubsub_connect_error",
                    ("errors", connect_errors, i64),
                    ("error_str", e.to_string(), String)
                );
            }
        }
    }
}

// NOTE: block subscription only for private rpc
// you must have --rpc-pubsub-enable-block-subscription and relevant flags started
// on your RPC servers for this to work.

pub async fn program_account_subscribe_loop(
    pubsub_addr: String,
    quote_token: String,
    program_account_receiver: Sender<Response<RpcKeyedAccount>>,
) {
    let mut connect_errors: u64 = 0;
    let mut program_account_subscribe_errors: u64 = 0;
    let mut program_account_subscribe_disconnect_errors: u64 = 0;

    const MAINNET_PROGRAM_ID_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
    const MAINNET_PROGRAM_ID_OPENBOOK_MARKET: &str = "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX";

    const LIQUIDITY_STATE_LAYOUT_V4_SPAN: u64 = 752;
    const LIQUIDITY_STATE_LAYOUT_V4_STATUS_OFFSET: usize = 0;
    const LIQUIDITY_STATE_LAYOUT_V4_QUOTE_MINT_OFFSET: usize = 432;
    const LIQUIDITY_STATE_LAYOUT_V4_MARKET_PROGRAM_ID_OFFSET: usize = 560;

    loop {
        sleep(Duration::from_secs(1)).await; // probably need to remove or adjust this

        // https://docs.solana.com/developing/clients/jsonrpc-api#programsubscribe
        // https://solana.com/docs/rpc/http/getaccountinfo

        // i think base mint can be wsol also? nvm i think its unlikely
        match PubsubClient::new(&pubsub_addr).await {
            Ok(pubsub_client) => match pubsub_client
                .program_subscribe(
                    &Pubkey::from_str(MAINNET_PROGRAM_ID_AMM_V4).expect("pubkey"),
                    Some(RpcProgramAccountsConfig {
                        filters: Some(vec![
                            RpcFilterType::DataSize(LIQUIDITY_STATE_LAYOUT_V4_SPAN),
                            RpcFilterType::Memcmp(Memcmp::new(
                                LIQUIDITY_STATE_LAYOUT_V4_QUOTE_MINT_OFFSET,
                                MemcmpEncodedBytes::Base58(quote_token.clone()),
                            )),
                            RpcFilterType::Memcmp(Memcmp::new(
                                LIQUIDITY_STATE_LAYOUT_V4_MARKET_PROGRAM_ID_OFFSET,
                                MemcmpEncodedBytes::Base58(
                                    MAINNET_PROGRAM_ID_OPENBOOK_MARKET.to_string(),
                                ),
                            )),
                            RpcFilterType::Memcmp(Memcmp::new(
                                LIQUIDITY_STATE_LAYOUT_V4_STATUS_OFFSET,
                                MemcmpEncodedBytes::Bytes(vec![6, 0, 0, 0, 0, 0, 0, 0]),
                            )),
                        ]),
                        account_config: RpcAccountInfoConfig {
                            encoding: Some(UiAccountEncoding::Base64Zstd), // maybe faster
                            commitment: Some(CommitmentConfig {
                                commitment: CommitmentLevel::Confirmed,
                            }),
                            data_slice: None,
                            min_context_slot: None,
                        },
                        with_context: None,
                    }),
                )
                .await
            {
                Ok((mut program_account_update_subscription, _unsubscribe_fn)) => {
                    while let Some(program_account_update) =
                        program_account_update_subscription.next().await
                    {
                        let account: Account = program_account_update
                            .value
                            .account
                            .decode()
                            .expect("failed to decode");
                        let bytes = account.data;
                        let pool_state = LiquidityStateLayoutV4::from_bytes(&bytes);
                        println!("pool_state: {:?}", pool_state);

                        datapoint_info!(
                            "program_account_subscribe_slot",
                            ("slot", program_account_update.context.slot, i64)
                        );
                        if program_account_receiver
                            .send(program_account_update)
                            .await
                            .is_err()
                        {
                            datapoint_error!(
                                "program_account_subscribe_send_error",
                                ("errors", 1, i64)
                            );
                            return;
                        }
                    }
                    program_account_subscribe_disconnect_errors += 1;
                    datapoint_error!(
                        "program_account_subscribe_disconnect_error",
                        ("errors", program_account_subscribe_disconnect_errors, i64)
                    );
                }
                Err(e) => {
                    program_account_subscribe_errors += 1;
                    datapoint_error!(
                        "program_account_subscribe_error",
                        ("errors", program_account_subscribe_errors, i64),
                        ("error_str", e.to_string(), String),
                    );
                }
            },
            Err(e) => {
                connect_errors += 1;
                datapoint_error!(
                    "program_account_subscribe_pubsub_connect_error",
                    ("errors", connect_errors, i64),
                    ("error_str", e.to_string(), String)
                );
            }
        }
    }
}
