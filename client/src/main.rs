mod event_loops;
mod raydium;

use crate::event_loops::{program_account_subscribe_loop, slot_subscribe_loop};
use env_logger::TimestampPrecision;
use log::*;
use rand::{rngs::ThreadRng, thread_rng, Rng};
use solana_client::{
    client_error::{reqwest::Version, ClientError},
    nonblocking::{pubsub_client::PubsubClientError, rpc_client::RpcClient},
    rpc_response::{Response as RpcResponse, RpcKeyedAccount},
};
use solana_metrics::{datapoint_info, set_host_id};
use solana_sdk::{
    clock::Slot,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    hash::Hash,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature, Signer},
    system_instruction::transfer,
    transaction::{Transaction, VersionedTransaction},
};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    path::PathBuf,
    result,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    runtime::Builder,
    sync::mpsc::{channel, Receiver},
    time::interval,
};
use tonic::{codegen::InterceptedService, transport::Channel, Response, Status};

#[derive(Debug, Error)]
enum BackrunError {
    #[error("TonicError {0}")]
    TonicError(#[from] tonic::transport::Error),
    #[error("GrpcError {0}")]
    GrpcError(#[from] Status),
    #[error("RpcError {0}")]
    RpcError(#[from] ClientError),
    #[error("PubSubError {0}")]
    PubSubError(#[from] PubsubClientError),
    #[error("BlockEngineConnectionError {0}")]
    BlockEngineConnectionError(#[from] BlockEngineConnectionError),
    #[error("Shutdown")]
    Shutdown,
}

type Result<T> = result::Result<T, BackrunError>;

async fn maintenance_tick(
    searcher_client: &mut SearcherServiceClient<InterceptedService<Channel, ClientInterceptor>>,
    rpc_client: &RpcClient,
    leader_schedule: &mut HashMap<Pubkey, HashSet<Slot>>,
    blockhash: &mut Hash,
    regions: Vec<String>,
) -> Result<()> {
    *blockhash = rpc_client
        .get_latest_blockhash_with_commitment(CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
        .await?
        .0;
    let new_leader_schedule = searcher_client
        .get_connected_leaders(ConnectedLeadersRequest {})
        .await?
        .into_inner()
        .connected_validators
        .iter()
        .fold(HashMap::new(), |mut hmap, (pubkey, slot_list)| {
            hmap.insert(
                Pubkey::from_str(pubkey).unwrap(),
                slot_list.slots.iter().cloned().collect(),
            );
            hmap
        });
    if new_leader_schedule != *leader_schedule {
        info!("connected_validators: {:?}", new_leader_schedule.keys());
        *leader_schedule = new_leader_schedule;
    }

    let next_scheduled_leader = searcher_client
        .get_next_scheduled_leader(NextScheduledLeaderRequest { regions })
        .await?
        .into_inner();
    info!(
        "next_scheduled_leader: {} in {} slots from {}",
        next_scheduled_leader.next_leader_identity,
        next_scheduled_leader.next_leader_slot - next_scheduled_leader.current_slot,
        next_scheduled_leader.next_leader_region
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_searcher_loop(
    block_engine_url: String,
    auth_keypair: Arc<Keypair>,
    payer_keypair: &Keypair,
    rpc_url: String,
    regions: Vec<String>,
    message: String,
    mut slot_receiver: Receiver<Slot>,
    mut program_account_receiver: Receiver<RpcResponse<RpcKeyedAccount>>,
    mut bundle_results_receiver: Receiver<BundleResult>,
) -> Result<()> {
    let mut leader_schedule: HashMap<Pubkey, HashSet<Slot>> = HashMap::new();
    let mut block_signatures: HashMap<Slot, HashSet<Signature>> = HashMap::new();

    let mut searcher_client = get_searcher_client(&block_engine_url, &auth_keypair).await?;

    let mut rng = thread_rng();
    let tip_accounts: Vec<Pubkey> = vec![
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
        "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
        "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
        "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
        "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
        "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
        "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
    ]
    .into_iter()
    .map(|s| Pubkey::from_str(s).expect("tip account"))
    .collect();

    let rpc_client = RpcClient::new(rpc_url);
    let mut blockhash = rpc_client
        .get_latest_blockhash_with_commitment(CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
        .await?
        .0;

    let mut highest_slot = 0;
    let mut is_leader_slot = false;

    let mut tick = interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                maintenance_tick(&mut searcher_client, &rpc_client, &mut leader_schedule, &mut blockhash, regions.clone()).await?;
            }
            // maybe bundles won't be accepted if not leader slot
            maybe_slot = slot_receiver.recv() => {
                highest_slot = maybe_slot.ok_or(BackrunError::Shutdown)?;
                is_leader_slot = leader_schedule.iter().any(|(_, slots)| slots.contains(&highest_slot));
            }
            maybe_program_account = program_account_receiver.recv() => {
                let program_account = maybe_program_account.ok_or(BackrunError::Shutdown)?;
                info!("received program_account: [pubkey={:?}]", program_account.value.pubkey);
                // let bundles = build_bundles();
                // let results = send_bundles(&mut searcher_client, &bundles).await?;
            }
            maybe_bundle_result = bundle_results_receiver.recv() => {
                let bundle_result = maybe_bundle_result.ok_or(BackrunError::Shutdown)?;
                info!("received bundle_result: [bundle_id={:?}, result={:?}]", bundle_result.bundle_id, bundle_result.result);
            }
        }
    }
}

fn main() -> Result<()> {
    env_logger::builder()
        .format_timestamp(Some(TimestampPrecision::Micros))
        .init();

    let payer_keypair = Arc::new(read_keypair_file(env!("PAYER_KEYPAIR")).expect("parse kp file"));
    let auth_keypair = Arc::new(read_keypair_file(env!("AUTH_KEYPAIR")).expect("parse kp file"));
    let block_engine_url = env!("BLOCK_ENGINE_URL").to_string();
    let rpc_url = env!("RPC_URL").to_string();
    let rpc_ws_url = env!("RPC_WS_URL").to_string();
    let quote_token = match env!("QUOTE_TOKEN_SYMBOL") {
        "WSOL" => "So11111111111111111111111111111111111111112",
        "USDC" => "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        _ => panic!("Invalid token symbol"),
    }
    .to_string();

    let regions = vec!["amsterdam", "frankfurt", "ny", "tokyo"]
        .into_iter()
        .map(String::from)
        .collect();
    let message = "henlo".to_string();

    set_host_id(auth_keypair.pubkey().to_string());

    let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(async move {
        let (slot_sender, slot_receiver) = channel(100);
        let (program_account_sender, program_account_receiver) = channel(100);
        let (bundle_results_sender, bundle_results_receiver) = channel(100);

        // tokio::spawn(slot_subscribe_loop(rpc_ws_url.clone(), slot_sender));
        tokio::spawn(program_account_subscribe_loop(
            rpc_ws_url.clone(),
            quote_token.clone(),
            program_account_sender,
        ));
        // tokio::spawn(bundle_results_loop(
        //     block_engine_url.clone(),
        //     auth_keypair.clone(),
        //     bundle_results_sender,
        // ));

        let result = run_searcher_loop(
            block_engine_url,
            auth_keypair,
            &payer_keypair,
            rpc_url,
            regions,
            message,
            slot_receiver,
            program_account_receiver,
            bundle_results_receiver,
        )
        .await;
        error!("searcher loop exited result: {result:?}");

        Ok(())
    })
}
