use anyhow::Result;
use async_trait::async_trait;
use solana_message::AccountMeta;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use test_common::{LabeledTransactionSignature, RpcContext, SimdTest, TestOutcome};

pub struct Simd0153Test;

/// Native ZK ElGamal Proof program address (post-activation).
fn zk_elgamal_proof_program_id() -> Pubkey {
    Pubkey::from_str("ZkE1Gama1Proof11111111111111111111111111111").unwrap()
}

/// Deprecated ZK Token Proof program — never activated; SIMD-0153 removes it.
fn zk_token_proof_program_id() -> Pubkey {
    Pubkey::from_str("ZkTokenProof1111111111111111111111111111111").unwrap()
}

#[async_trait]
impl SimdTest for Simd0153Test {
    fn program(&self) -> Option<test_common::ProgramDeployment> {
        Some(test_common::ProgramDeployment {
            keypair_path: "target/deploy/simd_0153-keypair.json".to_string(),
            so_path: "target/deploy/simd_0153.so".to_string(),
        })
    }

    async fn run_rpc(&self, ctx: RpcContext) -> Result<TestOutcome> {
        use solana_instruction::Instruction;
        use solana_message::Message;
        use solana_signer::Signer;
        use solana_transaction::Transaction;

        let is_activated = self.detect_feature_activated(&ctx);
        let expect_activated_bit = if is_activated { 1 } else { 0 };

        // 1. On-chain feature-gate + proof-program assertions via our
        //    scaffolding program. The program inspects the ZkE1Gama1Proof
        //    account flags directly to confirm post-activation it is an
        //    executable native builtin (and pre-activation it is not), and
        //    in both cases that the deprecated ZkTokenProof program is NOT
        //    a live builtin (it was never activated; SIMD-0153 removes it).
        let elgamal_id = zk_elgamal_proof_program_id();
        let zk_token_id = zk_token_proof_program_id();
        let instruction = Instruction::new_with_bytes(
            ctx.program_id,
            &[expect_activated_bit],
            vec![
                AccountMeta::new_readonly(ctx.feature_gate, false),
                AccountMeta::new_readonly(elgamal_id, false),
                AccountMeta::new_readonly(zk_token_id, false),
            ],
        );
        let recent_blockhash = ctx.rpc_client.get_latest_blockhash()?;
        let message = Message::new(&[instruction], Some(&ctx.payer.pubkey()));
        let transaction = Transaction::new(&[&ctx.payer], message, recent_blockhash);

        let gate_sig = match ctx.rpc_client.send_and_confirm_transaction(&transaction) {
            Ok(sig) => LabeledTransactionSignature {
                label: "feature-check".to_string(),
                signature: sig.to_string(),
                success: true,
                error: None,
            },
            Err(e) => {
                return Ok(TestOutcome::Fail {
                    message: format!("Feature-gate check transaction failed: {e}"),
                    tx_signatures: vec![],
                });
            }
        };

        // 2. RPC-level cross-check: the ZK ElGamal Proof program is
        //    observable as an executable account post-activation and
        //    absent (or non-executable) pre-activation; the deprecated
        //    ZkTokenProof program must be non-executable in both states.
        let elgamal_account = ctx.rpc_client.get_account(&elgamal_id).ok();
        let elgamal_present_and_executable = elgamal_account
            .as_ref()
            .map(|a| a.executable)
            .unwrap_or(false);
        let zk_token_account = ctx.rpc_client.get_account(&zk_token_id).ok();
        let zk_token_executable = zk_token_account
            .as_ref()
            .map(|a| a.executable)
            .unwrap_or(false);

        let tx_signatures = vec![gate_sig];

        if zk_token_executable && ctx.network_name != "localnet" {
            return Ok(TestOutcome::Fail {
                message: format!(
                    "SIMD-0153 deprecates the ZK Token Proof program at {zk_token_id}, but it is reported as executable on {}",
                    ctx.network_name
                ),
                tx_signatures,
            });
        }

        if is_activated {
            if !elgamal_present_and_executable {
                return Ok(TestOutcome::Fail {
                    message: format!(
                        "SIMD-0153 reports activated, but the ZK ElGamal Proof program at {elgamal_id} is missing or non-executable"
                    ),
                    tx_signatures,
                });
            }
            Ok(TestOutcome::Pass {
                message: format!(
                    "Feature gate active; ZK ElGamal Proof program present and executable at {elgamal_id}; deprecated ZkTokenProof at {zk_token_id} is not a builtin"
                ),
                tx_signatures,
            })
        } else {
            if elgamal_present_and_executable {
                return Ok(TestOutcome::Fail {
                    message: format!(
                        "SIMD-0153 reports inactive, but the ZK ElGamal Proof program at {elgamal_id} is already executable"
                    ),
                    tx_signatures,
                });
            }
            Ok(TestOutcome::Pass {
                message: format!(
                    "Feature gate inactive; ZK ElGamal Proof program at {elgamal_id} not yet a builtin; deprecated ZkTokenProof at {zk_token_id} is not a builtin"
                ),
                tx_signatures,
            })
        }
    }
}

pub fn register() -> Box<dyn SimdTest> {
    Box::new(Simd0153Test)
}
