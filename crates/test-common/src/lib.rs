use anyhow::Result;
use async_trait::async_trait;
use log::debug;
use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use std::sync::Arc;
use std::{collections::HashSet, env};
use surfpool_types::{
    RpcConfig, SimnetConfig, SimnetEvent, StudioConfig, SurfpoolConfig, SvmFeatureConfig,
};

use crate::surfpool::deploy_program_surfpool;

pub mod manifest;
pub mod requirements;
mod surfpool;
mod util;

pub use manifest::{
    E2eCheckConfig, E2eCheckRequirements, FeatureConfig, Manifest, ProgramDeploymentRequirement,
};
pub use requirements::{RequirementChecker, UnmetRequirement};

/// Collect the feature pubkey for `feature` **and** all its transitive dependencies.
pub fn collect_feature_deps(manifest: &Manifest, feature: &Pubkey) -> Vec<Pubkey> {
    let mut pubkeys = Vec::new();
    let mut visited = HashSet::new();
    collect_feature_deps_inner(manifest, feature, &mut pubkeys, &mut visited);
    pubkeys
}

/// Collect **only** the transitive dependency features for `feature`,
/// excluding `feature`'s own pubkey.
pub fn collect_dependency_features(manifest: &Manifest, feature: &Pubkey) -> Vec<Pubkey> {
    let mut pubkeys = Vec::new();
    let mut visited = HashSet::new();
    // Mark the requested feature as visited so its own pubkey is skipped,
    // then walk deps.
    visited.insert(*feature);
    if let Some(config) = manifest.get(feature) {
        for dep in &config.depends_on {
            collect_feature_deps_inner(manifest, dep, &mut pubkeys, &mut visited);
        }
    }
    pubkeys
}

fn collect_feature_deps_inner(
    manifest: &Manifest,
    feature: &Pubkey,
    pubkeys: &mut Vec<Pubkey>,
    visited: &mut HashSet<Pubkey>,
) {
    if !visited.insert(*feature) {
        return;
    }
    if let Some(config) = manifest.get(feature) {
        pubkeys.push(*feature);
        for dep in &config.depends_on {
            collect_feature_deps_inner(manifest, dep, pubkeys, visited);
        }
    }
}

pub struct SurfnetHandle {
    simnet_commands_tx: crossbeam::channel::Sender<surfpool_types::SimnetCommand>,
    _thread_handle: std::thread::JoinHandle<()>,
    pub rpc_url: String,
}

impl SurfnetHandle {
    pub fn kill(self) {
        let _ = self
            .simnet_commands_tx
            .send(surfpool_types::SimnetCommand::Terminate(None));
        let _ = self._thread_handle.join();
    }
}

/// Find an available TCP port by binding to port 0.
fn find_available_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

pub async fn start_surfnet(
    features_to_enable: Vec<Pubkey>,
    features_to_disable: Vec<Pubkey>,
) -> Result<SurfnetHandle> {
    start_surfnet_inner(features_to_enable, features_to_disable, true).await
}

/// Like [`start_surfnet`] but with an upstream RPC URL set so that the
/// surfnet can clone accounts (programs, mints, etc.) on demand. Required
/// for e2e tests that depend on BPF programs (e.g. token-2022) which are
/// not bundled into the surfnet's builtin set.
pub async fn start_surfnet_with_upstream(
    features_to_enable: Vec<Pubkey>,
    features_to_disable: Vec<Pubkey>,
) -> Result<SurfnetHandle> {
    start_surfnet_inner(features_to_enable, features_to_disable, false).await
}

async fn start_surfnet_inner(
    features_to_enable: Vec<Pubkey>,
    features_to_disable: Vec<Pubkey>,
    offline: bool,
) -> Result<SurfnetHandle> {
    let (mut surfnet_svm, simnet_events_rx, geyser_events_rx) =
        surfpool_core::surfnet::svm::SurfnetSvm::default();

    let mut feature_config = SvmFeatureConfig::new();
    feature_config.enable = features_to_enable;
    feature_config.disable = features_to_disable;

    surfnet_svm.apply_feature_config(&feature_config);

    debug!("Surfnet feature config: {:?}", feature_config);

    let rpc_port = find_available_port()?;
    let ws_port = find_available_port()?;
    let studio_port = find_available_port()?;
    let gossip_port = find_available_port()?;
    let tpu_port = find_available_port()?;
    let tpu_quic_port = find_available_port()?;
    debug!(
        "Surfnet ports: rpc={}, ws={}, studio={}, gossip={}, tpu={}, tpu_quic={}",
        rpc_port, ws_port, studio_port, gossip_port, tpu_port, tpu_quic_port
    );

    let rpc_url = format!("http://127.0.0.1:{}", rpc_port);

    let config = SurfpoolConfig {
        rpc: RpcConfig {
            bind_host: "127.0.0.1".to_string(),
            bind_port: rpc_port,
            ws_port,
            gossip_port,
            tpu_port,
            tpu_quic_port,
        },
        studio: StudioConfig {
            bind_host: "127.0.0.1".to_string(),
            bind_port: studio_port,
        },
        simnets: vec![SimnetConfig {
            offline_mode: offline,
            remote_rpc_url: if offline {
                None
            } else {
                env::var("UPSTREAM_RPC_URL").ok()
            },
            instruction_profiling_enabled: false,
            max_profiles: 1,
            ..Default::default()
        }],
        ..Default::default()
    };

    let (simnet_commands_tx, simnet_commands_rx) = crossbeam::channel::unbounded();

    let handle_tx = simnet_commands_tx.clone();

    // start_local_surfnet's future is !Send (crossbeam internals), so we
    // run it on a dedicated thread with its own single-threaded tokio runtime.
    let thread_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for surfnet");
        rt.block_on(async move {
            let _ = surfpool_core::start_local_surfnet(
                surfnet_svm,
                config,
                simnet_commands_tx,
                simnet_commands_rx,
                geyser_events_rx,
            )
            .await;
        });
    });

    // Wait for the surfnet to signal readiness on the events channel.
    loop {
        match simnet_events_rx.recv() {
            Ok(SimnetEvent::Ready(_)) => {
                debug!("Surfnet ready");
                break;
            }
            Ok(SimnetEvent::Aborted(msg)) => anyhow::bail!("Surfnet aborted: {msg}"),
            Ok(evt) => {
                debug!("Surfnet event: {:?}", evt);
                continue;
            }
            Err(_) => anyhow::bail!("Surfnet event channel closed unexpectedly"),
        }
    }

    Ok(SurfnetHandle {
        simnet_commands_tx: handle_tx,
        _thread_handle: thread_handle,
        rpc_url,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivationContext {
    /// Whether the feature was expected to be activated for this test run.
    pub expected: bool,
    /// Whether the feature was detected as activated on-chain (if checked).
    pub detected: Option<bool>,
}

impl std::fmt::Display for ActivationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detected_str = match self.detected {
            Some(true) => "activated",
            Some(false) => "not activated",
            None => "unknown",
        };
        write!(
            f,
            "expected={}, detected={}",
            if self.expected {
                "activated"
            } else {
                "not activated"
            },
            detected_str,
        )
    }
}

#[derive(Debug)]
pub enum TestOutcome {
    Pass {
        message: String,
        tx_signatures: Vec<LabeledTransactionSignature>,
    },
    Fail {
        message: String,
        tx_signatures: Vec<LabeledTransactionSignature>,
    },
    Skip {
        reason: String,
    },
    Pending {
        unmet: Vec<UnmetRequirement>,
        /// Optional message captured when the test was force-run despite
        /// unmet requirements (via `--run-pending`). Empty otherwise.
        message: Option<String>,
        /// Transaction signatures captured when the test was force-run
        /// despite unmet requirements. Empty otherwise.
        tx_signatures: Vec<LabeledTransactionSignature>,
    },
}

impl TestOutcome {
    pub fn is_fail(&self) -> bool {
        matches!(self, TestOutcome::Fail { .. })
    }

    pub fn label(&self) -> &str {
        match self {
            TestOutcome::Pass { .. } => "PASS",
            TestOutcome::Fail { .. } => "FAIL",
            TestOutcome::Skip { .. } => "SKIP",
            TestOutcome::Pending { .. } => "PENDING",
        }
    }

    pub fn message(&self) -> String {
        match self {
            TestOutcome::Pass { message, .. } => message.clone(),
            TestOutcome::Fail { message, .. } => message.clone(),
            TestOutcome::Skip { reason } => reason.clone(),
            TestOutcome::Pending { unmet, message, .. } => match message {
                Some(m) => format!("{} requirement(s) unmet; ran anyway: {m}", unmet.len()),
                None => format!("{} requirement(s) unmet", unmet.len()),
            },
        }
    }

    pub fn tx_signatures(&self) -> &[LabeledTransactionSignature] {
        match self {
            TestOutcome::Pass { tx_signatures, .. } => tx_signatures,
            TestOutcome::Fail { tx_signatures, .. } => tx_signatures,
            TestOutcome::Skip { .. } => &[],
            TestOutcome::Pending { tx_signatures, .. } => tx_signatures,
        }
    }

    pub fn unmet(&self) -> &[UnmetRequirement] {
        match self {
            TestOutcome::Pending { unmet, .. } => unmet,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LabeledTransactionSignature {
    pub label: String,
    pub signature: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub label: String,
    pub status: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tx_signatures: Vec<LabeledTransactionSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<ActivationContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet: Vec<UnmetRequirement>,
}

impl TestResult {
    pub fn new(
        label: String,
        outcome: &TestOutcome,
        activation: Option<ActivationContext>,
    ) -> Self {
        let status = match outcome {
            TestOutcome::Pass { .. } => "pass",
            TestOutcome::Fail { .. } => "fail",
            TestOutcome::Skip { .. } => "skip",
            TestOutcome::Pending { .. } => "pending",
        };
        Self {
            label,
            status: status.to_string(),
            message: outcome.message(),
            tx_signatures: outcome.tx_signatures().to_vec(),
            activation,
            unmet: outcome.unmet().to_vec(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TestSummary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub pending: usize,
}

#[derive(Debug, Serialize)]
pub struct TestReport {
    pub results: Vec<TestResult>,
    pub summary: TestSummary,
}

impl TestReport {
    pub fn new(results: Vec<TestResult>) -> Self {
        let passed = results.iter().filter(|r| r.status == "pass").count();
        let failed = results.iter().filter(|r| r.status == "fail").count();
        let skipped = results.iter().filter(|r| r.status == "skip").count();
        let pending = results.iter().filter(|r| r.status == "pending").count();
        Self {
            results,
            summary: TestSummary {
                passed,
                failed,
                skipped,
                pending,
            },
        }
    }

    pub fn any_fail(&self) -> bool {
        self.summary.failed > 0
    }

    pub fn any_pending(&self) -> bool {
        self.summary.pending > 0
    }
}

pub struct ProgramDeployment {
    pub keypair_path: String,
    pub so_path: String,
}

pub struct RpcContext {
    pub rpc_client: Arc<RpcClient>,
    pub payer: Keypair,
    pub network_name: String,
    pub program_id: Pubkey,
    pub feature_gate: Pubkey,
    /// Whether the feature is expected to be activated for this test run.
    pub expect_activated: bool,
}

impl RpcContext {
    fn is_program_deployed(&self) -> bool {
        match self.rpc_client.get_account(&self.program_id) {
            Ok(account) => account.executable,
            Err(_) => false,
        }
    }
}

#[async_trait]
pub trait SimdTest: Send + Sync {
    /// Stable registry id for this test impl. Matches
    /// `manifest.features.<addr>.test.id` and the key used by
    /// `tests::all_simd_tests()`.
    fn id(&self) -> &'static str;

    fn program(&self) -> Option<ProgramDeployment> {
        None
    }

    fn deploy_or_skip_program(&self, ctx: &RpcContext) -> Result<(), TestOutcome> {
        let Some(ProgramDeployment { so_path, .. }) = self.program() else {
            return Err(TestOutcome::Fail {
                message: "No program binary found".to_string(),
                tx_signatures: vec![],
            });
        };
        if ctx.network_name == "localnet" {
            if let Err(e) = deploy_program_surfpool(&ctx, &so_path) {
                Err(TestOutcome::Fail {
                    message: format!("Program deployment failed: {e}"),
                    tx_signatures: vec![],
                })
            } else {
                Ok(())
            }
        } else if !ctx.is_program_deployed() {
            Err(TestOutcome::Skip {
                reason: format!(
                    "Program {} not deployed on {}",
                    ctx.program_id, ctx.network_name
                ),
            })
        } else {
            Ok(())
        }
    }

    /// Detect whether the feature gate is activated on-chain.
    fn detect_feature_activated(&self, ctx: &RpcContext) -> bool {
        match ctx.rpc_client.get_account(&ctx.feature_gate) {
            Ok(account) => account.data.len() >= 9 && account.data[0] != 0,
            Err(_) => false,
        }
    }

    async fn run_rpc(&self, ctx: RpcContext) -> Result<TestOutcome>;
}

// --------------------------------------------------------------------------
// E2E feature-set tests
// --------------------------------------------------------------------------

/// Context handed to an [`E2eTest`] at runtime. The runner has already
/// verified that all `requires` are met before invoking `run`.
pub struct E2eContext {
    pub rpc_client: Arc<RpcClient>,
    /// RPC URL string corresponding to `rpc_client`. Needed by tests that
    /// build a non-blocking RPC client (e.g. for `spl-token-client::Token`).
    pub rpc_url: String,
    pub payer: Keypair,
    pub network_name: String,
    /// Resolved feature-gate pubkeys (in declaration order).
    pub required_feature_gates: Vec<Pubkey>,
    /// Resolved program ids (in declaration order).
    pub required_programs: Vec<Pubkey>,
}

#[async_trait(?Send)]
pub trait E2eTest: Send + Sync {
    /// Logical id this test is registered under (matches `e2e_checks.<name>.test`).
    fn id(&self) -> &'static str;

    async fn run(&self, ctx: E2eContext) -> Result<TestOutcome>;
}
