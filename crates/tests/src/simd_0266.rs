use anyhow::Result;
use async_trait::async_trait;
use solana_client::rpc_config::{CommitmentConfig, RpcSendTransactionConfig};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::{AccountMeta, Message};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::instruction::{create_account, transfer as system_transfer};
use solana_transaction::Transaction;
use spl_associated_token_account_interface::{
    address::get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token_interface::{
    instruction::{
        approve, approve_checked, burn, burn_checked, close_account, freeze_account,
        initialize_account, initialize_account2, initialize_account3, initialize_immutable_owner,
        initialize_mint, initialize_mint2, initialize_multisig, initialize_multisig2, mint_to,
        mint_to_checked, revoke, set_authority, sync_native, thaw_account, transfer,
        transfer_checked, AuthorityType,
    },
    native_mint,
};
use std::{ops::Mul, str::FromStr};
use test_common::{LabeledTransactionSignature, RpcContext, SimdTest, TestOutcome};

const MINT_SIZE: u64 = 82;
const TOKEN_ACCOUNT_SIZE: u64 = 165;
const MULTISIG_SIZE: u64 = 355;
const DECIMALS: u8 = 9;

const EXPECTED_APPROVE_CUS: u64 = 126;
const EXPECTED_APPROVE_CHECKED_CUS: u64 = 164;
const EXPECTED_BURN_CUS: u64 = 125;
const EXPECTED_BURN_CHECKED_CUS: u64 = 129;
const EXPECTED_CLOSE_ACCOUNT_CUS: u64 = 120;
const EXPECTED_FREEZE_ACCOUNT_CUS: u64 = 145;
const EXPECTED_INITIALIZE_ACCOUNT_CUS: u64 = 150;
const EXPECTED_INITIALIZE_ACCOUNT2_CUS: u64 = 171;
const EXPECTED_INITIALIZE_ACCOUNT3_CUS: u64 = 248;
const EXPECTED_INITIALIZE_IMMUTABLE_OWNER_CUS: u64 = 38;
const EXPECTED_INITIALIZE_MINT_CUS: u64 = 101;
const EXPECTED_INITIALIZE_MINT2_CUS: u64 = 228;
const EXPECTED_INITIALIZE_MULTISIG_CUS: u64 = 174;
const EXPECTED_INITIALIZE_MULTISIG2_CUS: u64 = 285;
const EXPECTED_MINT_TO_CUS: u64 = 119;
const EXPECTED_MINT_TO_CHECKED_CUS: u64 = 169;
const EXPECTED_REVOKE_CUS: u64 = 108;
const EXPECTED_SET_AUTHORITY_CUS: u64 = 138;
const EXPECTED_SYNC_NATIVE_CUS: u64 = 201;
const EXPECTED_THAW_ACCOUNT_CUS: u64 = 141;
const EXPECTED_TRANSFER_CUS: u64 = 81;
const EXPECTED_TRANSFER_CHECKED_CUS: u64 = 105;

const TOKEN_AMOUNT: u64 = 1_000_000_000;
const EXERCISE_AMOUNT: u64 = 100;
const WRAPPED_SOL_AMOUNT: u64 = 100_000_000;

pub struct Simd0266Test;

struct CuMeasurement {
    name: &'static str,
    actual: u64,
    expected_when_activated: Option<u64>,
}

fn token_program_id() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}

fn batch_transfer_ix(
    source: &Pubkey,
    dest: &Pubkey,
    authority: &Pubkey,
    amount: u64,
) -> Instruction {
    let amount_bytes = amount.to_le_bytes();
    let mut data = vec![0u8; 23]; // 1 + 2 * (2 + 1 + 8)
    data[0] = 255; // Batch

    // Transfer 1
    data[1] = 3; // num_accounts
    data[2] = 9; // data_len
    data[3] = 3; // Transfer discriminator
    data[4..12].copy_from_slice(&amount_bytes);

    // Transfer 2
    data[12] = 3; // num_accounts
    data[13] = 9; // data_len
    data[14] = 3; // Transfer discriminator
    data[15..23].copy_from_slice(&amount_bytes);

    Instruction::new_with_bytes(
        token_program_id(),
        &data,
        vec![
            AccountMeta::new(*source, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*source, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

fn verify_feature_ix(
    program_id: Pubkey,
    feature_gate: Pubkey,
    token_program: Pubkey,
    source: Pubkey,
    dest: Pubkey,
    payer: Pubkey,
    expect_activated: bool,
    amount: u64,
) -> Instruction {
    Instruction::new_with_bytes(
        program_id,
        &{
            let mut d = [0u8; 9];
            d[0] = u8::from(expect_activated);
            d[1..9].copy_from_slice(&amount.to_le_bytes());
            d
        },
        vec![
            AccountMeta::new_readonly(feature_gate, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new(source, false),
            AccountMeta::new(dest, false),
            AccountMeta::new_readonly(payer, true),
        ],
    )
}

fn withdraw_excess_lamports_ix(
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        token_program_id(),
        &[38],
        vec![
            AccountMeta::new(*source, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

fn unwrap_lamports_ix(
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: Option<u64>,
) -> Instruction {
    let mut data = vec![45];
    if let Some(amount) = amount {
        data.push(1);
        data.extend_from_slice(&amount.to_le_bytes());
    } else {
        data.push(0);
    }

    Instruction::new_with_bytes(
        token_program_id(),
        &data,
        vec![
            AccountMeta::new(*source, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

fn simulate_cu(
    ctx: &RpcContext,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> Result<(Option<String>, u64)> {
    let blockhash = ctx.rpc_client.get_latest_blockhash()?;
    let message = Message::new(instructions, Some(&ctx.payer.pubkey()));
    let mut all_signers: Vec<&Keypair> = vec![&ctx.payer];
    all_signers.extend_from_slice(signers);
    let tx = Transaction::new(&all_signers[..], message, blockhash);
    let result = ctx.rpc_client.simulate_transaction(&tx)?;
    let cus = result.value.units_consumed.unwrap_or(0);
    let err = result.value.err.map(|e| format!("{e}"));
    Ok((err, cus))
}

fn simulate_named(
    ctx: &RpcContext,
    name: &'static str,
    expected_when_activated: Option<u64>,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> Result<CuMeasurement> {
    let (err, actual) = simulate_cu(ctx, instructions, signers)?;
    if let Some(err) = err {
        anyhow::bail!("{name} simulation failed: {err}");
    }
    Ok(CuMeasurement {
        name,
        actual,
        expected_when_activated,
    })
}

fn try_simulate_named(
    ctx: &RpcContext,
    name: &'static str,
    expected_when_activated: Option<u64>,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> Result<Result<CuMeasurement, String>> {
    let (err, actual) = simulate_cu(ctx, instructions, signers)?;
    Ok(match err {
        Some(err) => Err(format!("{name} simulation failed: {err}")),
        None => Ok(CuMeasurement {
            name,
            actual,
            expected_when_activated,
        }),
    })
}

fn send_instructions(
    ctx: &RpcContext,
    label: impl Into<String>,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> Result<LabeledTransactionSignature> {
    let blockhash = ctx.rpc_client.get_latest_blockhash()?;
    let message = Message::new(instructions, Some(&ctx.payer.pubkey()));
    let mut all_signers: Vec<&Keypair> = vec![&ctx.payer];
    all_signers.extend_from_slice(signers);
    let tx = Transaction::new(&all_signers, message, blockhash);
    let signature = ctx.rpc_client.send_transaction_with_config(
        &tx,
        RpcSendTransactionConfig {
            skip_preflight: true,
            ..Default::default()
        },
    )?;
    ctx.rpc_client
        .poll_for_signature_with_commitment(&signature, CommitmentConfig::confirmed())?;
    let status = ctx
        .rpc_client
        .get_signature_statuses(&[signature])?
        .value
        .into_iter()
        .next()
        .flatten();

    let error = status.and_then(|s| s.err.map(|e| e.to_string()));

    Ok(LabeledTransactionSignature {
        label: label.into(),
        signature: signature.to_string(),
        success: error.is_none(),
        error,
    })
}

fn create_program_owned_account(
    ctx: &RpcContext,
    label: impl Into<String>,
    new_account: &Keypair,
    size: u64,
    owner: &Pubkey,
) -> Result<LabeledTransactionSignature> {
    let rent = ctx
        .rpc_client
        .get_minimum_balance_for_rent_exemption(size as usize)?;
    send_instructions(
        ctx,
        label,
        &[create_account(
            &ctx.payer.pubkey(),
            &new_account.pubkey(),
            rent,
            size,
            owner,
        )],
        &[new_account],
    )
}

fn create_native_token_account(
    ctx: &RpcContext,
    label: &str,
    account: &Keypair,
    owner: &Pubkey,
    extra_lamports: u64,
    sync_after_funding: bool,
) -> Result<Vec<LabeledTransactionSignature>> {
    let rent = ctx
        .rpc_client
        .get_minimum_balance_for_rent_exemption(TOKEN_ACCOUNT_SIZE as usize)?;

    let mut tx_signatures = vec![send_instructions(
        ctx,
        format!("{label}:create-account"),
        &[
            create_account(
                &ctx.payer.pubkey(),
                &account.pubkey(),
                rent,
                TOKEN_ACCOUNT_SIZE,
                &token_program_id(),
            ),
            initialize_account3(
                &token_program_id(),
                &account.pubkey(),
                &native_mint::id(),
                owner,
            )?,
        ],
        &[account],
    )?];

    tx_signatures.push(send_instructions(
        ctx,
        format!("{label}:fund"),
        &[system_transfer(
            &ctx.payer.pubkey(),
            &account.pubkey(),
            extra_lamports,
        )],
        &[],
    )?);

    if sync_after_funding {
        tx_signatures.push(send_instructions(
            ctx,
            format!("{label}:sync-native"),
            &[sync_native(&token_program_id(), &account.pubkey())?],
            &[],
        )?);
    }

    Ok(tx_signatures)
}

fn assert_expected_cus(measurements: &[CuMeasurement], is_activated: bool) -> Result<()> {
    for measurement in measurements {
        if is_activated {
            if let Some(expected) = measurement.expected_when_activated {
                anyhow::ensure!(
                    measurement.actual.mul(10) <= expected.mul(11)
                        && measurement.actual.mul(10) >= expected.mul(9),
                    "{} consumed {} CUs, expected within 10% of {} when SIMD-0266 is activated",
                    measurement.name,
                    measurement.actual,
                    expected
                );
            }
        } else if matches!(
            measurement.name,
            "Transfer" | "TransferChecked" | "MintTo" | "Approve" | "Burn"
        ) {
            anyhow::ensure!(
                measurement.actual > 1000,
                "{} consumed {} CUs, expected pre-activation SPL Token behavior to stay well above p-token levels",
                measurement.name,
                measurement.actual
            );
        }
    }

    Ok(())
}

fn measured_cus(measurements: &[CuMeasurement], name: &str) -> Option<u64> {
    measurements
        .iter()
        .find(|measurement| measurement.name == name)
        .map(|measurement| measurement.actual)
}

#[async_trait]
impl SimdTest for Simd0266Test {
    fn id(&self) -> &'static str {
        "simd_0266"
    }

    fn program(&self) -> Option<test_common::ProgramDeployment> {
        Some(test_common::ProgramDeployment {
            keypair_path: "programs/simd_0266/program-keypair.json".to_string(),
            so_path: "target/deploy/simd_0266.so".to_string(),
        })
    }

    async fn run_rpc(&self, ctx: RpcContext) -> Result<TestOutcome> {
        let payer_pubkey = ctx.payer.pubkey();
        let token_program = token_program_id();
        let is_activated = self.detect_feature_activated(&ctx);
        let mut tx_signatures = Vec::new();

        let mint = Keypair::new();
        let recipient = Keypair::new();
        let delegate = Keypair::new();
        let new_authority = Keypair::new();

        let source_ata = get_associated_token_address(&payer_pubkey, &mint.pubkey());
        let dest_ata = get_associated_token_address(&recipient.pubkey(), &mint.pubkey());

        tx_signatures.push(send_instructions(
            &ctx,
            "setup:initialize-mint",
            &[
                create_account(
                    &payer_pubkey,
                    &mint.pubkey(),
                    ctx.rpc_client
                        .get_minimum_balance_for_rent_exemption(MINT_SIZE as usize)?,
                    MINT_SIZE,
                    &token_program,
                ),
                initialize_mint2(
                    &token_program,
                    &mint.pubkey(),
                    &payer_pubkey,
                    Some(&payer_pubkey),
                    DECIMALS,
                )?,
            ],
            &[&mint],
        )?);

        tx_signatures.push(send_instructions(
            &ctx,
            "setup:create-atas-and-mint",
            &[
                create_associated_token_account(
                    &payer_pubkey,
                    &payer_pubkey,
                    &mint.pubkey(),
                    &token_program,
                ),
                create_associated_token_account(
                    &payer_pubkey,
                    &recipient.pubkey(),
                    &mint.pubkey(),
                    &token_program,
                ),
                mint_to(
                    &token_program,
                    &mint.pubkey(),
                    &source_ata,
                    &payer_pubkey,
                    &[],
                    TOKEN_AMOUNT,
                )?,
            ],
            &[],
        )?);

        let close_account_target = Keypair::new();
        let freeze_target = Keypair::new();
        let thaw_target = Keypair::new();
        let init_account_target = Keypair::new();
        let init_account2_target = Keypair::new();
        let init_account3_target = Keypair::new();
        let immutable_owner_target = Keypair::new();
        let init_mint_target = Keypair::new();
        let init_mint2_target = Keypair::new();
        let init_multisig_target = Keypair::new();
        let init_multisig2_target = Keypair::new();
        let sync_native_target = Keypair::new();
        let unwrap_native_target = Keypair::new();

        for account in [
            &close_account_target,
            &freeze_target,
            &thaw_target,
            &init_account_target,
            &init_account2_target,
            &init_account3_target,
            &immutable_owner_target,
        ] {
            tx_signatures.push(create_program_owned_account(
                &ctx,
                format!("setup:create-token-account:{}", account.pubkey()),
                account,
                TOKEN_ACCOUNT_SIZE,
                &token_program,
            )?);
        }

        for account in [&init_mint_target, &init_mint2_target] {
            tx_signatures.push(create_program_owned_account(
                &ctx,
                format!("setup:create-mint-account:{}", account.pubkey()),
                account,
                MINT_SIZE,
                &token_program,
            )?);
        }

        for account in [&init_multisig_target, &init_multisig2_target] {
            tx_signatures.push(create_program_owned_account(
                &ctx,
                format!("setup:create-multisig-account:{}", account.pubkey()),
                account,
                MULTISIG_SIZE,
                &token_program,
            )?);
        }

        for account in [&close_account_target, &freeze_target, &thaw_target] {
            tx_signatures.push(send_instructions(
                &ctx,
                format!("setup:initialize-token-account:{}", account.pubkey()),
                &[initialize_account3(
                    &token_program,
                    &account.pubkey(),
                    &mint.pubkey(),
                    &payer_pubkey,
                )?],
                &[],
            )?);
        }

        tx_signatures.push(send_instructions(
            &ctx,
            "setup:freeze-thaw-target",
            &[freeze_account(
                &token_program,
                &thaw_target.pubkey(),
                &mint.pubkey(),
                &payer_pubkey,
                &[],
            )?],
            &[],
        )?);

        tx_signatures.push(send_instructions(
            &ctx,
            "setup:approve-delegate",
            &[approve(
                &token_program,
                &source_ata,
                &delegate.pubkey(),
                &payer_pubkey,
                &[],
                EXERCISE_AMOUNT,
            )?],
            &[],
        )?);

        tx_signatures.push(send_instructions(
            &ctx,
            "setup:fund-mint-with-sol",
            &[system_transfer(
                &payer_pubkey,
                &mint.pubkey(),
                WRAPPED_SOL_AMOUNT,
            )],
            &[],
        )?);

        tx_signatures.extend(create_native_token_account(
            &ctx,
            "setup:sync-native-target",
            &sync_native_target,
            &payer_pubkey,
            WRAPPED_SOL_AMOUNT,
            false,
        )?);
        tx_signatures.extend(create_native_token_account(
            &ctx,
            "setup:unwrap-native-target",
            &unwrap_native_target,
            &payer_pubkey,
            WRAPPED_SOL_AMOUNT,
            true,
        )?);

        let multisig_signer_1 = Keypair::new();
        let multisig_signer_2 = Keypair::new();
        let multisig_signers = [&multisig_signer_1.pubkey(), &multisig_signer_2.pubkey()];

        let mut measurements = vec![
            simulate_named(
                &ctx,
                "InitializeMint",
                Some(EXPECTED_INITIALIZE_MINT_CUS),
                &[initialize_mint(
                    &token_program,
                    &init_mint_target.pubkey(),
                    &payer_pubkey,
                    Some(&payer_pubkey),
                    DECIMALS,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeAccount",
                Some(EXPECTED_INITIALIZE_ACCOUNT_CUS),
                &[initialize_account(
                    &token_program,
                    &init_account_target.pubkey(),
                    &mint.pubkey(),
                    &payer_pubkey,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeMultisig",
                Some(EXPECTED_INITIALIZE_MULTISIG_CUS),
                &[initialize_multisig(
                    &token_program,
                    &init_multisig_target.pubkey(),
                    &multisig_signers,
                    2,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "Transfer",
                Some(EXPECTED_TRANSFER_CUS),
                &[transfer(
                    &token_program,
                    &source_ata,
                    &dest_ata,
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "Approve",
                Some(EXPECTED_APPROVE_CUS),
                &[approve(
                    &token_program,
                    &source_ata,
                    &delegate.pubkey(),
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "Revoke",
                Some(EXPECTED_REVOKE_CUS),
                &[revoke(&token_program, &source_ata, &payer_pubkey, &[])?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "SetAuthority",
                Some(EXPECTED_SET_AUTHORITY_CUS),
                &[set_authority(
                    &token_program,
                    &source_ata,
                    Some(&new_authority.pubkey()),
                    AuthorityType::CloseAccount,
                    &payer_pubkey,
                    &[],
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "MintTo",
                Some(EXPECTED_MINT_TO_CUS),
                &[mint_to(
                    &token_program,
                    &mint.pubkey(),
                    &source_ata,
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "Burn",
                Some(EXPECTED_BURN_CUS),
                &[burn(
                    &token_program,
                    &source_ata,
                    &mint.pubkey(),
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "CloseAccount",
                Some(EXPECTED_CLOSE_ACCOUNT_CUS),
                &[close_account(
                    &token_program,
                    &close_account_target.pubkey(),
                    &payer_pubkey,
                    &payer_pubkey,
                    &[],
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "FreezeAccount",
                Some(EXPECTED_FREEZE_ACCOUNT_CUS),
                &[freeze_account(
                    &token_program,
                    &freeze_target.pubkey(),
                    &mint.pubkey(),
                    &payer_pubkey,
                    &[],
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "ThawAccount",
                Some(EXPECTED_THAW_ACCOUNT_CUS),
                &[thaw_account(
                    &token_program,
                    &thaw_target.pubkey(),
                    &mint.pubkey(),
                    &payer_pubkey,
                    &[],
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "TransferChecked",
                Some(EXPECTED_TRANSFER_CHECKED_CUS),
                &[transfer_checked(
                    &token_program,
                    &source_ata,
                    &mint.pubkey(),
                    &dest_ata,
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                    DECIMALS,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "ApproveChecked",
                Some(EXPECTED_APPROVE_CHECKED_CUS),
                &[approve_checked(
                    &token_program,
                    &source_ata,
                    &mint.pubkey(),
                    &delegate.pubkey(),
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                    DECIMALS,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "MintToChecked",
                Some(EXPECTED_MINT_TO_CHECKED_CUS),
                &[mint_to_checked(
                    &token_program,
                    &mint.pubkey(),
                    &source_ata,
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                    DECIMALS,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "BurnChecked",
                Some(EXPECTED_BURN_CHECKED_CUS),
                &[burn_checked(
                    &token_program,
                    &source_ata,
                    &mint.pubkey(),
                    &payer_pubkey,
                    &[],
                    EXERCISE_AMOUNT,
                    DECIMALS,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeAccount2",
                Some(EXPECTED_INITIALIZE_ACCOUNT2_CUS),
                &[initialize_account2(
                    &token_program,
                    &init_account2_target.pubkey(),
                    &mint.pubkey(),
                    &payer_pubkey,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "SyncNative",
                Some(EXPECTED_SYNC_NATIVE_CUS),
                &[sync_native(&token_program, &sync_native_target.pubkey())?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeAccount3",
                Some(EXPECTED_INITIALIZE_ACCOUNT3_CUS),
                &[initialize_account3(
                    &token_program,
                    &init_account3_target.pubkey(),
                    &mint.pubkey(),
                    &payer_pubkey,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeMultisig2",
                Some(EXPECTED_INITIALIZE_MULTISIG2_CUS),
                &[initialize_multisig2(
                    &token_program,
                    &init_multisig2_target.pubkey(),
                    &multisig_signers,
                    2,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeMint2",
                Some(EXPECTED_INITIALIZE_MINT2_CUS),
                &[initialize_mint2(
                    &token_program,
                    &init_mint2_target.pubkey(),
                    &payer_pubkey,
                    Some(&payer_pubkey),
                    DECIMALS,
                )?],
                &[],
            )?,
            simulate_named(
                &ctx,
                "InitializeImmutableOwner",
                Some(EXPECTED_INITIALIZE_IMMUTABLE_OWNER_CUS),
                &[initialize_immutable_owner(
                    &token_program,
                    &immutable_owner_target.pubkey(),
                )?],
                &[],
            )?,
        ];
        let mut notes = Vec::new();

        if is_activated {
            match try_simulate_named(
                &ctx,
                "WithdrawExcessLamports",
                None,
                &[withdraw_excess_lamports_ix(
                    &mint.pubkey(),
                    &payer_pubkey,
                    &payer_pubkey,
                )],
                &[],
            )? {
                Ok(measurement) => measurements.push(measurement),
                Err(note) => notes.push(note),
            }
            match try_simulate_named(
                &ctx,
                "UnwrapLamports",
                None,
                &[unwrap_lamports_ix(
                    &unwrap_native_target.pubkey(),
                    &payer_pubkey,
                    &payer_pubkey,
                    Some(EXERCISE_AMOUNT),
                )],
                &[],
            )? {
                Ok(measurement) => measurements.push(measurement),
                Err(note) => notes.push(note),
            }
            match try_simulate_named(
                &ctx,
                "Batch",
                None,
                &[batch_transfer_ix(
                    &source_ata,
                    &dest_ata,
                    &payer_pubkey,
                    EXERCISE_AMOUNT,
                )],
                &[],
            )? {
                Ok(measurement) => measurements.push(measurement),
                Err(note) => notes.push(note),
            }
            match try_simulate_named(
                &ctx,
                "Batch CPI",
                None,
                &[verify_feature_ix(
                    ctx.program_id,
                    ctx.feature_gate,
                    token_program,
                    source_ata,
                    dest_ata,
                    payer_pubkey,
                    true,
                    EXERCISE_AMOUNT,
                )],
                &[],
            )? {
                Ok(measurement) => measurements.push(measurement),
                Err(note) => notes.push(note),
            }

            let batch_cpi_result = send_instructions(
                &ctx,
                "batch-cpi",
                &[verify_feature_ix(
                    ctx.program_id,
                    ctx.feature_gate,
                    token_program,
                    source_ata,
                    dest_ata,
                    payer_pubkey,
                    true,
                    EXERCISE_AMOUNT,
                )],
                &[],
            )?;
            if !batch_cpi_result.success {
                let error = batch_cpi_result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_string());
                tx_signatures.push(batch_cpi_result);
                return Ok(TestOutcome::Fail {
                    message: format!("Batch CPI transaction failed while activated: {error}"),
                    tx_signatures,
                });
            }
            tx_signatures.push(batch_cpi_result);
        } else {
            match try_simulate_named(
                &ctx,
                "Batch CPI (deactivated)",
                None,
                &[verify_feature_ix(
                    ctx.program_id,
                    ctx.feature_gate,
                    token_program,
                    source_ata,
                    dest_ata,
                    payer_pubkey,
                    false,
                    EXERCISE_AMOUNT,
                )],
                &[],
            )? {
                Ok(measurement) => {
                    return Ok(TestOutcome::Fail {
                        message: format!(
                            "{} unexpectedly succeeded with {} CUs while SIMD-0266 was deactivated",
                            measurement.name, measurement.actual
                        ),
                        tx_signatures,
                    });
                }
                Err(note) => notes.push(format!(
                    "Batch CPI rejected while deactivated as expected: {note}"
                )),
            }

            let batch_cpi_result = send_instructions(
                &ctx,
                "batch-cpi-deactivated",
                &[verify_feature_ix(
                    ctx.program_id,
                    ctx.feature_gate,
                    token_program,
                    source_ata,
                    dest_ata,
                    payer_pubkey,
                    false,
                    EXERCISE_AMOUNT,
                )],
                &[],
            )?;
            tx_signatures.push(batch_cpi_result.clone());
            if batch_cpi_result.success {
                return Ok(TestOutcome::Fail {
                    message: "Batch CPI unexpectedly succeeded while SIMD-0266 was deactivated"
                        .to_string(),
                    tx_signatures,
                });
            }
            let error = batch_cpi_result
                .error
                .unwrap_or_else(|| "transaction confirmed without an explicit error".to_string());
            notes.push(format!(
                "Batch CPI transaction rejected while deactivated as expected: {error}"
            ));
        }

        let transfer_cus = measured_cus(&measurements, "Transfer").unwrap_or_default();
        let initialize_mint_cus = measured_cus(&measurements, "InitializeMint").unwrap_or_default();
        let ptoken_runtime_active = transfer_cus <= 1000;

        if is_activated && !ptoken_runtime_active {
            let mut reasons = vec![format!(
                "Feature gate is active, but this runtime is still executing classic SPL Token paths (Transfer: {transfer_cus} CUs, InitializeMint: {initialize_mint_cus} CUs). Exact p-token CU assertions need the actual token-program swap, which this local environment does not appear to emulate yet."
            )];
            reasons.extend(notes);
            return Ok(TestOutcome::Fail {
                message: reasons.join(" | "),
                tx_signatures,
            });
        }

        if let Err(err) = assert_expected_cus(&measurements, ptoken_runtime_active) {
            return Ok(TestOutcome::Fail {
                message: err.to_string(),
                tx_signatures,
            });
        }

        Ok(TestOutcome::Pass {
            message: measurements
                .iter()
                .map(|measurement| format!("{}: {} CUs", measurement.name, measurement.actual))
                .collect::<Vec<_>>()
                .into_iter()
                .chain(notes)
                .collect::<Vec<_>>()
                .join(" | "),
            tx_signatures,
        })
    }
}

pub fn register() -> Box<dyn SimdTest> {
    Box::new(Simd0266Test)
}
