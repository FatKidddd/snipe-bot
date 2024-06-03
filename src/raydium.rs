use bincode;
use raydium_library::{
    amm::{
        calculate_pool_vault_amounts, get_keys_for_market, load_amm_keys, swap, swap_with_slippage,
        CalculateMethod, SwapDirection,
    },
    common::send_txn,
};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};

#[repr(C)]
#[derive(Debug, Serialize, Deserialize)]
pub struct LiquidityStateLayoutV4 {
    status: u64,
    nonce: u64,
    max_order: u64,
    depth: u64,
    base_decimal: u64,
    quote_decimal: u64,
    state: u64,
    reset_flag: u64,
    min_size: u64,
    vol_max_cut_ratio: u64,
    amount_wave_ratio: u64,
    base_lot_size: u64,
    quote_lot_size: u64,
    min_price_multiplier: u64,
    max_price_multiplier: u64,
    system_decimal_value: u64,
    min_separate_numerator: u64,
    min_separate_denominator: u64,
    trade_fee_numerator: u64,
    trade_fee_denominator: u64,
    pnl_numerator: u64,
    pnl_denominator: u64,
    swap_fee_numerator: u64,
    swap_fee_denominator: u64,
    base_need_take_pnl: u64,
    quote_need_take_pnl: u64,
    quote_total_pnl: u64,
    base_total_pnl: u64,
    pool_open_time: u64,
    punish_pc_amount: u64,
    punish_coin_amount: u64,
    orderbook_to_init_time: u64,
    // u128('pool_total_deposit_pc'),
    // u128('pool_total_deposit_coin'),
    swap_base_in_amount: u128,
    swap_quote_out_amount: u128,
    swap_base2_quote_fee: u64,
    swap_quote_in_amount: u128,
    swap_base_out_amount: u128,
    swap_quote2_base_fee: u64,
    // amm vault
    base_vault: Pubkey,
    quote_vault: Pubkey,
    // mint
    base_mint: Pubkey,
    quote_mint: Pubkey,
    lp_mint: Pubkey,
    // market
    open_orders: Pubkey,
    market_id: Pubkey,
    market_program_id: Pubkey,
    target_orders: Pubkey,
    withdraw_queue: Pubkey,
    lp_vault: Pubkey,
    owner: Pubkey,
    // true circulating supply without lock up
    lp_reserve: u64,
    padding: [u8; 3],
}

impl LiquidityStateLayoutV4 {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        bincode::deserialize(bytes).expect("liquidity state layout failed to deserialize")
    }
}

pub fn solve(
    amm_program: Pubkey,
    amm_pool_id: Pubkey,
    input_token_mint: Pubkey,
    output_token_mint: Pubkey,
    client: &RpcClient,
    payer_keypair: Keypair,
) -> Result<(), anyhow::Error> {
    let slippage_bps = 50u64; // 0.5%
    let amount_specified = 2000_000000u64;
    let swap_base_in = false;

    // load amm keys
    let amm_keys = load_amm_keys(&client, &amm_program, &amm_pool_id)?;
    // load market keys
    let market_keys = get_keys_for_market(&client, &amm_keys.market_program, &amm_keys.market)?;
    // calculate amm pool vault with load data at the same time or use simulate to calculate
    let result = calculate_pool_vault_amounts(
        &client,
        &amm_program,
        &amm_pool_id,
        &amm_keys,
        &market_keys,
        CalculateMethod::Simulate(payer_keypair.pubkey()),
    )?;
    let direction = if input_token_mint == amm_keys.amm_coin_mint
        && output_token_mint == amm_keys.amm_pc_mint
    {
        SwapDirection::Coin2PC
    } else {
        SwapDirection::PC2Coin
    };
    let other_amount_threshold = swap_with_slippage(
        result.pool_pc_vault_amount,
        result.pool_coin_vault_amount,
        result.swap_fee_numerator,
        result.swap_fee_denominator,
        direction,
        amount_specified,
        swap_base_in,
        slippage_bps,
    )?;
    println!(
        "amount_specified:{}, other_amount_threshold:{}",
        amount_specified, other_amount_threshold
    );

    let build_swap_instruction = swap(
        &amm_program,
        &amm_keys,
        &market_keys,
        &payer_keypair.pubkey(),
        &spl_associated_token_account::get_associated_token_address(
            &payer_keypair.pubkey(),
            &input_token_mint,
        ),
        &spl_associated_token_account::get_associated_token_address(
            &payer_keypair.pubkey(),
            &output_token_mint,
        ),
        amount_specified,
        other_amount_threshold,
        swap_base_in,
    )?;

    // send init tx
    let txn = Transaction::new_signed_with_payer(
        &vec![build_swap_instruction],
        Some(&payer_keypair.pubkey()),
        &vec![&payer_keypair],
        client.get_latest_blockhash()?,
    );
    let sig = send_txn(&client, &txn, true)?;
    println!("sig:{:#?}", sig);
    Ok(())
}
