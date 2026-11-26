//! # block 報酬の分配スクリプト
//!
//! validator の identity に貯まった block 報酬を、epoch の終わりに次の順で振り分ける。
//!
//! ```text
//!                       ┌─ (1) vote account を目標残高まで補充
//!   identity の余剰 ────┼─ (2) staker へ分配 (Jito Priority Fee Distribution)
//!                       └─ (3) 残りを余剰アカウントへ
//! ```
//!
//! 使い方:
//!
//! ```text
//! cargo run --release               # 本番 (cron から 15 分おきに起動する想定)
//! cargo run --release -- --dry-run  # 送信せずに、配分の確認と simulate だけ行う
//! ```

use std::{env, fs};

use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::read_keypair_file;
use solana_message::{
    v1::{self, TransactionConfig},
    VersionedMessage,
};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::versioned::VersionedTransaction;

// ═══════════════════════════════════════════════════════════════════
//  設定 — ここを書き換えて使う
// ═══════════════════════════════════════════════════════════════════

const SOL: u64 = 1_000_000_000; // lamports

/// 残高の取得と送信に使う RPC
const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// validator の identity の keypair。block 報酬が貯まる場所で、すべての送金元になる
const IDENTITY_KEYPAIR: &str = "/home/sol/validator-keypair.json";

/// vote account
const VOTE_ACCOUNT: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// 余剰をためておくアカウント。staker に分配しなかった残りをここへ送る
const SAVINGS_ACCOUNT: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// identity に常に残しておく額。これを超えた分を振り分ける
const IDENTITY_RESERVE: u64 = 10 * SOL;

/// vote account をこの残高まで補充する (Alpenglow の VAT 用。0 にすると補充しない)
const VOTE_ACCOUNT_TARGET: u64 = 10 * SOL;

/// staker に分配する割合 (bps: 5000 = 50%)。残りは余剰アカウントへ
const SHARE_BPS: u64 = 5000;

/// epoch 内でこの slot を過ぎたら実行する
/// (1 epoch = 432,000 slot。1 slot ≈ 200ms なので、420,000 なら epoch 終了の約 40 分前)
const RUN_AFTER_SLOT_INDEX: u64 = 420_000;

/// 処理した epoch を記録するファイル。cron が何度起動しても 1 epoch に 1 回だけ実行するため
const LAST_EPOCH_FILE: &str = "last-epoch";

// ═══════════════════════════════════════════════════════════════════
//  Jito Priority Fee Distribution (PFD) program
// ═══════════════════════════════════════════════════════════════════
//
// validator は epoch ごとに自分用の PFD account を作り、staker に配りたい額を入金する。
// epoch が終わると Jito の TipRouter NCN が stake の比率で merkle root を作り、
// 各 staker の stake account へ配ってくれる。こちらがやるのは「作る」と「入金する」だけ。

const PFD_PROGRAM: Pubkey = Pubkey::from_str_const("Priority6weCZ5HwDn29NxLFpb7TDp2iLZ6XKc5e8d3");
const PFD_CONFIG: Pubkey = Pubkey::from_str_const("AyLtimvPr3aaRXRNaeg4T1Cbgx3mrnhJEVaaKkQX6vyW");

/// merkle root の生成と claim を TipRouter NCN に任せるための、Jito 標準の authority
const MERKLE_ROOT_UPLOAD_AUTHORITY: Pubkey = Pubkey::from_str_const("8F4jGUmxF36vQ6yabnsxX6AQVXdKBhs8kGSUuRKSg8Xt");

/// PFD 上の validator commission。0 なら入金額がそのまま staker に配られる
const VALIDATOR_COMMISSION_BPS: u16 = 0;

/// PFD account のデータサイズ (作成時の rent の計算に使う)
const PFD_ACCOUNT_SIZE: usize = 176;

// ═══════════════════════════════════════════════════════════════════
//  トランザクション
// ═══════════════════════════════════════════════════════════════════

/// 手数料 (署名 1 つ分)。identity が払うので、配分の前に差し引いておく
const TX_FEE: u64 = 5_000;

/// v1 トランザクションは上限を指定しないと 0 扱いになるので、
/// 実測 (約 16,000 CU / 0.6 MB) に余裕を持たせた値を入れておく
const COMPUTE_UNIT_LIMIT: u32 = 100_000;
const LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 2 * 1024 * 1024;

fn main() {
    let dry_run = env::args().any(|arg| arg == "--dry-run");
    if dry_run {
        println!("[dry-run] 配分の確認と simulate だけ行い、送信はしません");
    }

    assert_ne!(VOTE_ACCOUNT, Pubkey::default(), "VOTE_ACCOUNT を設定してください");
    assert_ne!(SAVINGS_ACCOUNT, Pubkey::default(), "SAVINGS_ACCOUNT を設定してください");

    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let identity =
        read_keypair_file(IDENTITY_KEYPAIR).unwrap_or_else(|e| panic!("{IDENTITY_KEYPAIR} を読めません: {e}"));

    // ─── 1. 実行するタイミングか確認する ──────────────────────────────

    let epoch_info = rpc.get_epoch_info().expect("epoch の取得に失敗");
    let epoch = epoch_info.epoch;
    println!("epoch {epoch}  (slot {} / {})", epoch_info.slot_index, epoch_info.slots_in_epoch);

    let last_epoch: Option<u64> = fs::read_to_string(LAST_EPOCH_FILE).ok().and_then(|text| text.trim().parse().ok());

    if epoch_info.slot_index < RUN_AFTER_SLOT_INDEX {
        println!("→ slot {RUN_AFTER_SLOT_INDEX} になるまで待ちます");
        if !dry_run {
            return;
        }
    } else if last_epoch.is_some_and(|last| last >= epoch) {
        println!("→ この epoch は処理済みです");
        if !dry_run {
            return;
        }
    }

    // ─── 2. 残高を読む ────────────────────────────────────────────

    let (pfd_account, pfd_bump) = pfd_account_address(&VOTE_ACCOUNT, epoch);

    let accounts = rpc
        .get_multiple_accounts(&[identity.pubkey(), VOTE_ACCOUNT, SAVINGS_ACCOUNT, pfd_account])
        .expect("残高の取得に失敗");
    let balance = |i: usize| accounts[i].as_ref().map_or(0, |account| account.lamports);

    let identity_balance = balance(0);
    let vote_balance = balance(1);
    let savings_balance = balance(2);
    let pfd_account_exists = accounts[3].is_some();
    let pfd_rent = rpc.get_minimum_balance_for_rent_exemption(PFD_ACCOUNT_SIZE).expect("rent の取得に失敗");

    println!();
    println!("残高");
    println!("  {:>15} SOL  {}  identity", sol(identity_balance), identity.pubkey());
    println!("  {:>15} SOL  {}  vote account", sol(vote_balance), VOTE_ACCOUNT);
    println!("  {:>15} SOL  {}  余剰アカウント", sol(savings_balance), SAVINGS_ACCOUNT);
    println!(
        "  {:>15} SOL  {}  PFD account (epoch {epoch}{})",
        sol(balance(3)),
        pfd_account,
        if pfd_account_exists { "" } else { ", 未作成" }
    );

    assert!(
        savings_balance > 0,
        "余剰アカウント {SAVINGS_ACCOUNT} がまだありません。先に少額の SOL を送って作っておいてください"
    );

    // ─── 3. 配分を計算する ─────────────────────────────────────────
    //
    // identity の残高のうち IDENTITY_RESERVE を超えた分 (= 前回からの block 報酬) を、
    // 上から順に使っていく。(1)(2) を折半の前に引くので、そのコストは staker と同じ比率で負担する。

    let surplus = identity_balance.saturating_sub(IDENTITY_RESERVE + TX_FEE);
    let mut rest = surplus;

    // (1) vote account を目標残高まで補充する。足りなければ出せる分だけ
    let vote_top_up = VOTE_ACCOUNT_TARGET.saturating_sub(vote_balance).min(rest);
    rest -= vote_top_up;

    // (2) この epoch の PFD account がまだなければ作る。rent は identity が払う
    let create_pfd_account = SHARE_BPS > 0 && !pfd_account_exists && rest > pfd_rent;
    if create_pfd_account {
        rest -= pfd_rent;
    }

    // (3) 残りを staker への分配と余剰アカウントに分ける (PFD account を作れないほど少額なら全部余剰へ)
    let can_share = pfd_account_exists || create_pfd_account;
    let share = if can_share { (rest as u128 * SHARE_BPS as u128 / 10_000) as u64 } else { 0 };
    let savings = rest - share;

    println!();
    println!("配分  (identity の余剰 {} SOL)", sol(surplus));
    println!("  {:>15} SOL  → vote account に補充", sol(vote_top_up));
    if create_pfd_account {
        println!("  {:>15} SOL  → PFD account の作成 (rent)", sol(pfd_rent));
    }
    println!("  {:>15} SOL  → staker へ分配 ({}%)", sol(share), SHARE_BPS as f64 / 100.0);
    println!("  {:>15} SOL  → 余剰アカウントへ", sol(savings));
    println!();

    // ─── 4. 命令を組み立てる ───────────────────────────────────────

    let mut instructions = vec![];
    if vote_top_up > 0 {
        instructions.push(transfer(&identity.pubkey(), &VOTE_ACCOUNT, vote_top_up));
    }
    if create_pfd_account {
        instructions.push(create_pfd_account_ix(&identity.pubkey(), &pfd_account, pfd_bump));
    }
    if share > 0 {
        instructions.push(deposit_to_pfd_account_ix(&identity.pubkey(), &pfd_account, share));
    }
    if savings > 0 {
        instructions.push(transfer(&identity.pubkey(), &SAVINGS_ACCOUNT, savings));
    }

    if instructions.is_empty() {
        println!("→ 送るものがありません");
        if !dry_run {
            fs::write(LAST_EPOCH_FILE, epoch.to_string()).expect("last-epoch の書き込みに失敗");
        }
        return;
    }

    // ─── 5. 送る ──────────────────────────────────────────────────
    //
    // すべての命令を 1 つのトランザクションに入れるので、どれかが失敗すれば全体が取り消される。
    // 金額は毎回残高から計算するので、失敗しても次の実行でそのままやり直せる。

    let blockhash = rpc.get_latest_blockhash().expect("blockhash の取得に失敗");
    let message = VersionedMessage::V1(
        v1::Message::try_compile_with_config(
            &identity.pubkey(),
            &instructions,
            blockhash,
            TransactionConfig::empty()
                .with_compute_unit_limit(COMPUTE_UNIT_LIMIT)
                .with_loaded_accounts_data_size_limit(LOADED_ACCOUNTS_DATA_SIZE_LIMIT),
        )
        .expect("トランザクションの組み立てに失敗"),
    );

    if dry_run {
        // 署名せずに simulate だけする (そのまま送信できる tx を RPC に渡さないため)
        let unsigned = VersionedTransaction {
            signatures: vec![Signature::default()], // 署名者は identity だけ
            message,
        };
        let config =
            RpcSimulateTransactionConfig { sig_verify: false, replace_recent_blockhash: true, ..Default::default() };
        let result = rpc.simulate_transaction_with_config(&unsigned, config).expect("simulate に失敗").value;
        match result.err {
            None => println!(
                "→ simulate 成功 ({} CU, {} bytes)",
                result.units_consumed.unwrap_or_default(),
                result.loaded_accounts_data_size.unwrap_or_default(),
            ),
            Some(err) => {
                println!("→ simulate 失敗: {err:?}");
                for log in result.logs.unwrap_or_default() {
                    println!("    {log}");
                }
            }
        }
        return;
    }

    let transaction = VersionedTransaction::try_new(message, &[&identity]).expect("署名に失敗");

    // 送信前に simulate (preflight) し、confirmed になるまで待つ
    let signature =
        rpc.send_and_confirm_transaction(&transaction).expect("送信に失敗 (次の実行で残高から計算し直します)");
    println!("→ 送信しました: {signature}");

    fs::write(LAST_EPOCH_FILE, epoch.to_string()).expect("last-epoch の書き込みに失敗");
}

// ═══════════════════════════════════════════════════════════════════
//  PFD program の命令
// ═══════════════════════════════════════════════════════════════════
//
// PFD program は Anchor 製。使う命令は 2 つだけなので、IDL を使わずに手で組み立てる。
// 命令データ = 命令識別子 (sha256("global:<命令名>") の先頭 8 byte) + 引数

/// vote account と epoch から決まる PFD account のアドレス (PDA) と bump
fn pfd_account_address(vote_account: &Pubkey, epoch: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"PF_DISTRIBUTION_ACCOUNT", vote_account.as_ref(), &epoch.to_le_bytes()],
        &PFD_PROGRAM,
    )
}

/// `initialize_priority_fee_distribution_account`: この epoch の PFD account を作る。
/// 署名者は identity でなければならない (program が vote account の node_pubkey と照合する)。
fn create_pfd_account_ix(identity: &Pubkey, pfd_account: &Pubkey, bump: u8) -> Instruction {
    let mut data = vec![49, 128, 247, 162, 140, 2, 193, 87];
    data.extend_from_slice(MERKLE_ROOT_UPLOAD_AUTHORITY.as_ref());
    data.extend_from_slice(&VALIDATOR_COMMISSION_BPS.to_le_bytes());
    data.push(bump);

    Instruction {
        program_id: PFD_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(PFD_CONFIG, false),
            AccountMeta::new(*pfd_account, false),
            AccountMeta::new_readonly(VOTE_ACCOUNT, false),
            AccountMeta::new(*identity, true),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

/// `transfer_priority_fee_tips`: PFD account に入金する。
/// 単純な送金ではなくこの命令を使うのは、program 側で入金額の累計を記録しているため。
fn deposit_to_pfd_account_ix(from: &Pubkey, pfd_account: &Pubkey, lamports: u64) -> Instruction {
    let mut data = vec![195, 208, 218, 42, 198, 181, 69, 74];
    data.extend_from_slice(&lamports.to_le_bytes());

    Instruction {
        program_id: PFD_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(PFD_CONFIG, false),
            AccountMeta::new(*pfd_account, false),
            AccountMeta::new(*from, true),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

/// lamports を `12.345678900` の形で表示する
fn sol(lamports: u64) -> String {
    format!("{}.{:09}", lamports / SOL, lamports % SOL)
}

// ═══════════════════════════════════════════════════════════════════
//  テスト: PFD の定数と命令が mainnet / Anchor と一致するか
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn anchor_discriminator(name: &str) -> Vec<u8> {
        Sha256::digest(format!("global:{name}"))[..8].to_vec()
    }

    #[test]
    fn instruction_discriminators() {
        let create = create_pfd_account_ix(&Pubkey::new_unique(), &Pubkey::new_unique(), 255);
        let deposit = deposit_to_pfd_account_ix(&Pubkey::new_unique(), &Pubkey::new_unique(), 1);
        assert_eq!(create.data[..8], anchor_discriminator("initialize_priority_fee_distribution_account"));
        assert_eq!(deposit.data[..8], anchor_discriminator("transfer_priority_fee_tips"));
    }

    #[test]
    fn pfd_config_address() {
        let (config, _) = Pubkey::find_program_address(&[b"CONFIG_ACCOUNT"], &PFD_PROGRAM);
        assert_eq!(config, PFD_CONFIG);
    }

    #[test]
    fn pfd_account_address_on_mainnet() {
        // mainnet 上で実在を確認した PFD account
        let vote_account = Pubkey::from_str_const("DMSuZcavta8L1w1tSiH8bALWjz6Q6KSryGG6m6Az4Qt5");
        assert_eq!(
            pfd_account_address(&vote_account, 1011).0,
            Pubkey::from_str_const("43mTuGpST1kUcWHaFWHDY3VMNmCT8aUtkKXE1h7bpKma")
        );
    }

    #[test]
    fn formats_sol() {
        assert_eq!(sol(1_280_949_909), "1.280949909");
        assert_eq!(sol(10 * SOL), "10.000000000");
    }
}
