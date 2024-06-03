mod event_loops;
mod raydium;

use crate::event_loops::program_account_subscribe_loop;
use env_logger::TimestampPrecision;
use log::*;
use raydium::LiquidityStateLayoutV4;
use solana_client::{
    client_error::ClientError,
    nonblocking::{pubsub_client::PubsubClientError, rpc_client::RpcClient},
    rpc_response::{Response, RpcKeyedAccount},
};
use solana_sdk::{
    account::Account,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    hash::Hash,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::{Transaction, VersionedTransaction},
};
use std::{result, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{sync::mpsc::channel, time::interval};

#[derive(Debug, Error)]
enum BackrunError {
    #[error("RpcError {0}")]
    RpcError(#[from] ClientError),
    #[error("PubSubError {0}")]
    PubSubError(#[from] PubsubClientError),
    #[error("Shutdown")]
    Shutdown,
}

type Result<T> = result::Result<T, BackrunError>;

async fn get_blockhash(rpc_client: &RpcClient) -> Result<Hash> {
    Ok(rpc_client
        .get_latest_blockhash_with_commitment(CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
        .await?
        .0)
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::builder()
        .format_timestamp(Some(TimestampPrecision::Micros))
        .init();

    let payer_keypair = Arc::new(read_keypair_file(env!("PAYER_KEYPAIR")).expect("parse kp file"));
    let rpc_url = env!("RPC_URL").to_string();
    let rpc_ws_url = env!("RPC_WS_URL").to_string();
    let quote_token = match env!("QUOTE_TOKEN_SYMBOL") {
        "WSOL" => "So11111111111111111111111111111111111111112",
        "USDC" => "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        _ => panic!("Invalid token symbol"),
    }
    .to_string();

    let (program_account_sender, mut program_account_receiver) = channel(100);
    let rpc_client = RpcClient::new(rpc_url);
    let mut tick = interval(Duration::from_secs(5));
    let mut blockhash = get_blockhash(&rpc_client).await?;

    tokio::spawn(program_account_subscribe_loop(
        rpc_ws_url.clone(),
        quote_token.clone(),
        program_account_sender,
    ));

    loop {
        tokio::select! {
            _ = tick.tick() => {
                blockhash = get_blockhash(&rpc_client).await?;
            }
            maybe_program_account = program_account_receiver.recv() => {
                let program_account: Response<RpcKeyedAccount> = maybe_program_account.ok_or(BackrunError::Shutdown)?;
                info!("received program_account: [pubkey={:?}]", program_account.value.pubkey);

                let account: Account = program_account
                    .value
                    .account
                    .decode()
                    .expect("failed to decode");
                let bytes = account.data;
                let pool_state = LiquidityStateLayoutV4::from_bytes(&bytes);
                info!("{:?}", pool_state);
            }
        }
    }
}
