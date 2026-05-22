use anyhow::Result;
use async_trait::async_trait;
use solana_message::AccountMeta;
use test_common::{LabeledTransactionSignature, RpcContext, SimdTest, TestOutcome};

pub struct Simd0194Test;

#[async_trait]
impl SimdTest for Simd0194Test {
    fn id(&self) -> &'static str {
        "simd_0194"
    }

    fn program(&self) -> Option<test_common::ProgramDeployment> {
        Some(test_common::ProgramDeployment {
            keypair_path: "programs/simd_0194/program-keypair.json".to_string(),
            so_path: "target/deploy/simd_0194.so".to_string(),
        })
    }

    async fn run_rpc(&self, ctx: RpcContext) -> Result<TestOutcome> {
        use solana_instruction::Instruction;
        use solana_message::Message;
        use solana_signer::Signer;
        use solana_transaction::Transaction;

        let program_id = ctx.program_id;
        let feature_id = ctx.feature_gate;

        let is_activated = self.detect_feature_activated(&ctx);
        let expect_activated_bit = if is_activated { 1 } else { 0 };

        let instruction = Instruction::new_with_bytes(
            program_id,
            &[expect_activated_bit],
            vec![AccountMeta::new_readonly(feature_id, false)],
        );
        let recent_blockhash = ctx.rpc_client.get_latest_blockhash()?;
        let message = Message::new(&[instruction], Some(&ctx.payer.pubkey()));
        let transaction = Transaction::new(&[&ctx.payer], message, recent_blockhash);

        match ctx.rpc_client.send_and_confirm_transaction(&transaction) {
            Ok(sig) => Ok(TestOutcome::Pass {
                message: format!("Transaction confirmed: {sig}"),
                tx_signatures: vec![LabeledTransactionSignature {
                    label: "feature-check".to_string(),
                    signature: sig.to_string(),
                    success: true,
                    error: None,
                }],
            }),
            Err(e) => Ok(TestOutcome::Fail {
                message: format!("Transaction failed: {e}"),
                tx_signatures: vec![],
            }),
        }
    }
}

pub fn register() -> Box<dyn SimdTest> {
    Box::new(Simd0194Test)
}
