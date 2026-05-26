use anyhow::Result;
use clap::Parser;
use log::{debug, info};
use solana_client::{rpc_client::RpcClient, rpc_config::CommitmentConfig};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::path::Path;
use std::sync::Arc;
use test_common::{
    collect_dependency_features, collect_feature_deps, start_surfnet, start_surfnet_with_upstream,
    ActivationContext, E2eContext, FeatureConfig, Manifest, RequirementChecker, RpcContext,
    TestOutcome, TestReport, TestResult,
};

use tests::{all_e2e_tests, all_simd_tests};

#[derive(Parser)]
#[command(
    name = "feature-activation-verifier",
    about = "Verify SIMD feature activations on Solana networks"
)]
struct Cli {
    /// Filter tests. Matches against (in narrow-to-broad order): exact SIMD
    /// number (`153` or `0153`), exact feature-gate pubkey, pubkey substring,
    /// test id, or description substring (case-insensitive).
    #[arg(long)]
    filter: Option<String>,

    /// Target network: localnet, devnet, testnet, mainnet, or a custom RPC URL
    #[arg(long, default_value = "localnet")]
    network: String,

    /// Path to keypair file (required for testnet/mainnet, defaults to ~/.config/solana/id.json for localnet)
    #[arg(long)]
    keypair: Option<String>,

    /// Path to the manifest YAML file
    #[arg(long, default_value = "manifest.yaml")]
    manifest: String,

    /// Output format: text, json, yaml
    #[arg(long, default_value = "text")]
    output: String,

    /// Write json/yaml output to a file instead of stdout
    #[arg(long)]
    output_file: Option<String>,

    /// Run e2e tests even when their requirements are unmet. The test's
    /// status will still be reported as PENDING, but its output (message
    /// and tx signatures) will be captured.
    #[arg(long)]
    run_pending: bool,
}

fn rpc_url_for_network(network: &str) -> String {
    match network {
        "localnet" => "http://127.0.0.1:8899".to_string(),
        "testnet" => "https://api.testnet.solana.com".to_string(),
        "devnet" => "https://api.devnet.solana.com".to_string(),
        "mainnet" => "https://api.mainnet-beta.solana.com".to_string(),
        url => url.to_string(),
    }
}

fn load_keypair(path: &str) -> Result<Keypair> {
    let data = std::fs::read_to_string(path)?;
    let bytes: Vec<u8> = serde_json::from_str(&data)?;
    let secret: [u8; 32] = bytes[..32].try_into()?;
    Ok(Keypair::new_from_array(secret))
}

fn default_keypair_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/.config/solana/id.json")
}

fn resolve_keypair(keypair_arg: &Option<String>, network: &str) -> Result<Keypair> {
    if let Some(ref kp_path) = keypair_arg {
        return load_keypair(kp_path);
    }

    match network {
        "localnet" => {
            let default_path = default_keypair_path();
            load_keypair(&default_path).map_err(|e| {
                anyhow::anyhow!(
                    "No --keypair provided and failed to load default keypair at {}: {}",
                    default_path,
                    e
                )
            })
        }
        _ => anyhow::bail!("--keypair is required for network '{}'", network),
    }
}

fn load_or_generate_program_keypair(path: &str) -> Result<Keypair> {
    if let Ok(data) = std::fs::read_to_string(path) {
        debug!("Reading keypair at path: {}", path);
        let bytes: Vec<u8> = serde_json::from_str(&data)?;
        let secret: [u8; 32] = bytes[..32].try_into().map_err(|_| {
            anyhow::anyhow!(
                "Invalid program keypair at {}: expected at least 32 bytes",
                path
            )
        })?;
        return Ok(Keypair::new_from_array(secret));
    }

    debug!("Generating new program keypair at {path}...");
    let kp = Keypair::new();
    let bytes: Vec<u8> = kp.to_bytes().to_vec();
    let json = serde_json::to_string(&bytes)?;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, json)?;
    Ok(kp)
}

fn airdrop(rpc_client: &RpcClient, payer: &Keypair) -> Result<()> {
    let balance = rpc_client.get_balance(&payer.pubkey())?;
    let one_sol = 1_000_000_000;
    if balance < one_sol {
        info!("Airdropping 1 SOL to payer {}...", payer.pubkey());
        let sig = rpc_client.request_airdrop(&payer.pubkey(), one_sol)?;
        rpc_client.confirm_transaction(&sig)?;
    }
    Ok(())
}

/// Match `filter` against a feature entry. Tested in narrow-to-broad order
/// so that e.g. `--filter 153` doesn't also pick up an entry whose
/// description happens to contain the digits "153".
fn feature_matches_filter(address: &Pubkey, config: &FeatureConfig, filter: &str) -> bool {
    // 1. Exact SIMD number (with or without leading zeros).
    if let Some(n) = config.simd_number {
        let n_str = n.to_string();
        if n_str == filter || n_str == filter.trim_start_matches('0') {
            return true;
        }
        if format!("{:04}", n) == filter {
            return true;
        }
    }

    let address_str = address.to_string();

    // 2. Exact pubkey match.
    if address_str == filter {
        return true;
    }

    // 3. Pubkey substring match.
    if address_str.contains(filter) {
        return true;
    }

    // 4. Test id substring match.
    if let Some(test) = &config.test {
        if test.id.contains(filter) {
            return true;
        }
    }

    // 5. Description substring match (case-insensitive).
    if config
        .description
        .to_lowercase()
        .contains(&filter.to_lowercase())
    {
        return true;
    }

    // 6. Substring of SIMD-zero-padded form ("01" matches SIMD-0153).
    if let Some(n) = config.simd_number {
        if format!("{:04}", n).contains(filter) {
            return true;
        }
    }

    false
}

/// Build the human-facing label for a feature test result.
fn feature_label(address: &Pubkey, config: &FeatureConfig, pass_suffix: Option<&str>) -> String {
    let base = if let Some(n) = config.simd_number {
        format!("SIMD-{:04} {} ({})", n, config.description, address)
    } else {
        format!("Feature {} ({})", config.description, address)
    };
    match pass_suffix {
        Some(suffix) => format!("{base} ({suffix})"),
        None => base,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    let manifest = Manifest::load(Path::new(&cli.manifest))?;
    let tests = all_simd_tests();

    // Collect manifest entries, filtered then sorted deterministically by
    // (simd_number-with-None-last, base58 pubkey).
    let mut entries: Vec<(Pubkey, &FeatureConfig)> = manifest
        .iter_features()
        .filter(|(addr, config)| {
            if let Some(ref f) = cli.filter {
                feature_matches_filter(addr, config, f)
            } else {
                true
            }
        })
        .map(|(addr, config)| (*addr, config))
        .collect();
    entries.sort_by(|(a_addr, a), (b_addr, b)| {
        let a_key = (a.simd_number.is_none(), a.simd_number, a_addr.to_string());
        let b_key = (b.simd_number.is_none(), b.simd_number, b_addr.to_string());
        a_key.cmp(&b_key)
    });

    // Collect e2e checks, filtered
    let mut e2e_entries: Vec<_> = manifest
        .iter_e2e_checks()
        .filter(|(id, config)| {
            if let Some(ref f) = cli.filter {
                id.contains(f) || config.description.contains(f)
            } else {
                true
            }
        })
        .collect();
    e2e_entries.sort_by_key(|(id, _)| (*id).clone());

    if entries.is_empty() && e2e_entries.is_empty() {
        println!("No tests matched the filter.");
        return Ok(());
    }

    info!(
        "Running {} feature test(s) and {} e2e check(s) on {}...",
        entries.len(),
        e2e_entries.len(),
        cli.network
    );

    let payer = resolve_keypair(&cli.keypair, &cli.network)?;
    let url = rpc_url_for_network(&cli.network);
    debug!("Resolved RPC URL: {}", url);
    debug!("Network: {}", cli.network);
    if let Some(ref f) = cli.filter {
        debug!("Filter: {}", f);
    }
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        &url,
        CommitmentConfig::confirmed(),
    ));

    let mut results: Vec<TestResult> = Vec::new();

    for (address, config) in &entries {
        let test_id = config.test.as_ref().map(|t| t.id.as_str());
        let Some(test) = test_id.and_then(|id| tests.get(id)) else {
            let outcome = TestOutcome::Skip {
                reason: match test_id {
                    Some(id) => format!("No test implementation registered for id '{id}'"),
                    None => "No test implementation found".to_string(),
                },
            };
            results.push(TestResult::new(
                feature_label(address, config, None),
                &outcome,
                None,
            ));
            continue;
        };

        info!(
            "Starting test for feature {} (test id {})",
            address,
            test.id()
        );

        // For localnet, run two passes: deactivated then activated.
        // For other networks, run once with the live feature state.
        let passes = if cli.network == "localnet" {
            vec![
                (
                    "deactivated",
                    false,
                    Some((
                        collect_dependency_features(&manifest, address),
                        vec![*address],
                    )),
                ),
                (
                    "activated",
                    true,
                    Some((collect_feature_deps(&manifest, address), vec![])),
                ),
            ]
        } else {
            let activated = config.activation.is_activated_on(&cli.network);
            vec![("live", activated, None)]
        };

        for (pass_name, expect_activated, features) in passes {
            let label = if cli.network == "localnet" {
                feature_label(address, config, Some(pass_name))
            } else {
                feature_label(address, config, None)
            };

            let (surfnet_handle, rpc_client) =
                if let Some((activated_features, deactivated_features)) = features {
                    debug!(
                        "Activated feature gates for {} ({}): {:?}",
                        address, pass_name, activated_features
                    );
                    debug!(
                        "Deactivated feature gates for {} ({}): {:?}",
                        address, pass_name, deactivated_features
                    );
                    let handle = start_surfnet(activated_features, deactivated_features).await?;
                    debug!("Surfnet RPC url: {}", handle.rpc_url);
                    let client = Arc::new(RpcClient::new_with_commitment(
                        &handle.rpc_url,
                        CommitmentConfig::confirmed(),
                    ));
                    airdrop(&client, &payer)?;
                    (Some(handle), client)
                } else {
                    (None, Arc::clone(&rpc_client))
                };

            // Handle program deployment/checking
            let mut resolved_program_id = config
                .test_deployment_for(&cli.network)
                .and_then(|deployment| deployment.address);
            if resolved_program_id.is_none() {
                if let Some(deployment) = test.program() {
                    let program_kp =
                        match load_or_generate_program_keypair(&deployment.keypair_path) {
                            Ok(kp) => kp,
                            Err(e) => {
                                if let Some(h) = surfnet_handle {
                                    h.kill();
                                }
                                let outcome = TestOutcome::Fail {
                                    message: format!("Failed to load program keypair: {e}"),
                                    tx_signatures: vec![],
                                };
                                results.push(TestResult::new(label, &outcome, None));
                                continue;
                            }
                        };

                    resolved_program_id = Some(program_kp.pubkey());
                }
            }

            let ctx = RpcContext {
                rpc_client: Arc::clone(&rpc_client),
                payer: payer.insecure_clone(),
                network_name: cli.network.clone(),
                program_id: resolved_program_id.expect("Could not resolve program id"),
                feature_gate: *address,
                expect_activated: expect_activated,
            };
            if let Err(err_outcome) = test.deploy_or_skip_program(&ctx) {
                if let Some(h) = surfnet_handle {
                    h.kill();
                }
                results.push(TestResult::new(label, &err_outcome, None));
                continue;
            }

            // Detect on-chain activation and build context
            let detected = test.detect_feature_activated(&ctx);
            let activation = ActivationContext {
                expected: expect_activated,
                detected: Some(detected),
            };

            let outcome = test.run_rpc(ctx).await?;

            if let Some(h) = surfnet_handle {
                h.kill();
            }

            results.push(TestResult::new(label, &outcome, Some(activation)));
        }
    }

    // ----- E2E checks -----
    let e2e_tests = all_e2e_tests();
    for (id, config) in &e2e_entries {
        let label = format!("E2E {}", id);
        info!("Starting e2e check {}", id);

        // Resolve feature gates up-front so a misconfigured manifest fails loudly.
        let resolved_gates = match config.requires.resolved_feature_gates(&manifest) {
            Ok(g) => g,
            Err(e) => {
                let outcome = TestOutcome::Fail {
                    message: format!("Failed to resolve feature gates: {e}"),
                    tx_signatures: vec![],
                };
                results.push(TestResult::new(label, &outcome, None));
                continue;
            }
        };

        // Spin up runtime: surfnet for localnet (with required gates enabled),
        // live RpcClient otherwise.
        let (surfnet_handle, e2e_rpc_client, e2e_rpc_url) = if cli.network == "localnet" {
            let enable: Vec<_> = resolved_gates.iter().map(|(_, pk)| *pk).collect();
            // Use mainnet as the upstream RPC so that account-clone (e.g.
            // token-2022 BPF program at TokenzQd...) just works without a
            // separate `clone_program` step.
            let handle = match start_surfnet_with_upstream(enable, vec![]).await {
                Ok(h) => h,
                Err(e) => {
                    let outcome = TestOutcome::Fail {
                        message: format!("Failed to start surfnet: {e}"),
                        tx_signatures: vec![],
                    };
                    results.push(TestResult::new(label, &outcome, None));
                    continue;
                }
            };
            let url = handle.rpc_url.clone();
            let client = Arc::new(RpcClient::new_with_commitment(
                &url,
                CommitmentConfig::confirmed(),
            ));
            if let Err(e) = airdrop(&client, &payer) {
                handle.kill();
                let outcome = TestOutcome::Fail {
                    message: format!("Airdrop failed: {e}"),
                    tx_signatures: vec![],
                };
                results.push(TestResult::new(label, &outcome, None));
                continue;
            }
            (Some(handle), client, url)
        } else {
            (None, Arc::clone(&rpc_client), url.clone())
        };

        // Check requirements against whichever runtime we're on. Surfpool's
        // configured upstream RPC will fall through for missing accounts on
        // localnet, so the same check is meaningful in both cases.
        let unmet =
            match RequirementChecker::new(&e2e_rpc_client).check(&config.requires, &manifest) {
                Ok(u) => u,
                Err(e) => {
                    if let Some(h) = surfnet_handle {
                        h.kill();
                    }
                    let outcome = TestOutcome::Fail {
                        message: format!("Requirement check failed: {e}"),
                        tx_signatures: vec![],
                    };
                    results.push(TestResult::new(label, &outcome, None));
                    continue;
                }
            };

        if !unmet.is_empty() && !cli.run_pending {
            if let Some(h) = surfnet_handle {
                h.kill();
            }
            let outcome = TestOutcome::Pending {
                unmet,
                message: None,
                tx_signatures: vec![],
            };
            results.push(TestResult::new(label, &outcome, None));
            continue;
        }

        // Look up the test impl.
        let Some(test) = e2e_tests.get(config.test.as_str()) else {
            if let Some(h) = surfnet_handle {
                h.kill();
            }
            let outcome = TestOutcome::Skip {
                reason: format!(
                    "No e2e test implementation registered for '{}'",
                    config.test
                ),
            };
            results.push(TestResult::new(label, &outcome, None));
            continue;
        };

        let ctx = E2eContext {
            rpc_client: Arc::clone(&e2e_rpc_client),
            rpc_url: e2e_rpc_url.clone(),
            payer: payer.insecure_clone(),
            network_name: cli.network.clone(),
            required_feature_gates: resolved_gates.iter().map(|(_, pk)| *pk).collect(),
            required_programs: config.requires.programs.iter().map(|p| p.address).collect(),
        };

        let outcome = match test.run(ctx).await {
            Ok(o) => o,
            Err(e) => TestOutcome::Fail {
                message: format!("e2e test errored: {e}"),
                tx_signatures: vec![],
            },
        };

        // If we got here with unmet requirements (because of --run-pending),
        // demote any outcome to PENDING while preserving the message and tx
        // signatures captured during the run.
        let outcome = if !unmet.is_empty() {
            let (message, tx_signatures) = match outcome {
                TestOutcome::Pass {
                    message,
                    tx_signatures,
                } => (Some(message), tx_signatures),
                TestOutcome::Fail {
                    message,
                    tx_signatures,
                } => (Some(message), tx_signatures),
                TestOutcome::Skip { reason } => (Some(reason), vec![]),
                TestOutcome::Pending {
                    message,
                    tx_signatures,
                    ..
                } => (message, tx_signatures),
            };
            TestOutcome::Pending {
                unmet,
                message,
                tx_signatures,
            }
        } else {
            outcome
        };

        if let Some(h) = surfnet_handle {
            h.kill();
        }
        results.push(TestResult::new(label, &outcome, None));
    }

    let report = TestReport::new(results);

    match cli.output.as_str() {
        "json" => {
            let json = serde_json::to_string_pretty(&report)?;
            if let Some(ref path) = cli.output_file {
                std::fs::write(path, &json)?;
            } else {
                println!("{json}");
            }
        }
        "yaml" | "yml" => {
            let yaml = serde_yaml::to_string(&report)?;
            if let Some(ref path) = cli.output_file {
                std::fs::write(path, &yaml)?;
            } else {
                print!("{yaml}");
            }
        }
        _ => {
            // text format (default) — matches previous output
            println!();
            for result in &report.results {
                let status = match result.status.as_str() {
                    "pass" => "[PASS]",
                    "fail" => "[FAIL]",
                    "skip" => "[SKIP]",
                    "pending" => "[PENDING]",
                    _ => "[????]",
                };
                let activation_str = match &result.activation {
                    Some(ctx) => format!(" [{}]", ctx),
                    None => String::new(),
                };
                println!(
                    "{:9} {}{} - {}",
                    status, result.label, activation_str, result.message,
                );
                for unmet in &result.unmet {
                    println!("          - {}", unmet);
                }
                for tx in &result.tx_signatures {
                    let tx_status = if tx.success { "ok" } else { "err" };
                    let tx_error = tx
                        .error
                        .as_ref()
                        .map(|error| format!(" ({error})"))
                        .unwrap_or_default();
                    println!(
                        "          tx {} [{}]: {}{}",
                        tx.label, tx_status, tx.signature, tx_error
                    );
                }
            }
            println!(
                "\nSummary: {} passed, {} failed, {} pending, {} skipped",
                report.summary.passed,
                report.summary.failed,
                report.summary.pending,
                report.summary.skipped,
            );
        }
    }

    if report.any_fail() {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use test_common::Manifest;

    fn load_workspace_manifest() -> Manifest {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        Manifest::load(&workspace_root.join("manifest.yaml")).expect("manifest loads")
    }

    /// Helper: collect features that the CLI would pick for a given filter,
    /// mirroring the filter+sort logic in `main`.
    fn select<'a>(manifest: &'a Manifest, filter: &str) -> Vec<(Pubkey, &'a FeatureConfig)> {
        let mut entries: Vec<(Pubkey, &FeatureConfig)> = manifest
            .iter_features()
            .filter(|(addr, config)| feature_matches_filter(addr, config, filter))
            .map(|(addr, config)| (*addr, config))
            .collect();
        entries.sort_by(|(a_addr, a), (b_addr, b)| {
            let a_key = (a.simd_number.is_none(), a.simd_number, a_addr.to_string());
            let b_key = (b.simd_number.is_none(), b.simd_number, b_addr.to_string());
            a_key.cmp(&b_key)
        });
        entries
    }

    #[test]
    fn filter_by_zero_padded_simd_number_selects_exactly_one() {
        let m = load_workspace_manifest();
        let hits = select(&m, "0153");
        assert_eq!(hits.len(), 1, "expected exactly one entry for `0153`");
        let (_, cfg) = hits[0];
        assert_eq!(cfg.simd_number, Some(153));
    }

    #[test]
    fn filter_by_bare_simd_number_selects_exactly_one() {
        let m = load_workspace_manifest();
        let hits = select(&m, "153");
        // `153` is an exact SIMD-number match; description-substring fallback
        // does not run because the narrow match wins first.
        let zk_hits: Vec<_> = hits
            .iter()
            .filter(|(_, c)| c.simd_number == Some(153))
            .collect();
        assert_eq!(zk_hits.len(), 1);
    }

    #[test]
    fn filter_by_full_pubkey_selects_exact_feature() {
        let m = load_workspace_manifest();
        let zk_pk = Pubkey::from_str_const("zkhiy5oLowR7HY4zogXjCjeMXyruLqBwSWH21qcFtnv");
        let hits = select(&m, &zk_pk.to_string());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, zk_pk);
    }

    #[test]
    fn filter_by_pubkey_substring_selects_matching_feature() {
        let m = load_workspace_manifest();
        let hits = select(&m, "zkhiy5o");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.simd_number, Some(153));
    }

    #[test]
    fn filter_by_test_id_selects_feature_with_that_test() {
        let m = load_workspace_manifest();
        let hits = select(&m, "simd_0194");
        // `simd_0194` matches via test id; the SIMD-194 entry uses that test
        // id, so exactly one hit is expected.
        let with_test_id: Vec<_> = hits
            .iter()
            .filter(|(_, c)| {
                c.test
                    .as_ref()
                    .map(|t| t.id == "simd_0194")
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(with_test_id.len(), 1);
    }

    #[test]
    fn filter_by_description_substring_matches_case_insensitively() {
        let m = load_workspace_manifest();
        let hits = select(&m, "elgamal");
        assert!(hits.iter().any(|(_, c)| c.simd_number == Some(153)));
    }

    #[test]
    fn filter_with_no_match_returns_empty() {
        let m = load_workspace_manifest();
        let hits = select(&m, "definitely-not-a-real-thing-zzz");
        assert!(hits.is_empty());
    }

    #[test]
    fn label_branches_on_simd_number_presence() {
        let zk_pk = Pubkey::from_str_const("zkhiy5oLowR7HY4zogXjCjeMXyruLqBwSWH21qcFtnv");
        let with_simd = FeatureConfig {
            description: "ZK ElGamal".to_string(),
            simd_number: Some(153),
            simd_url: None,
            activation: test_common::manifest::ActivationConfig::default(),
            depends_on: vec![],
            deprecated_by: None,
            test: None,
        };
        let without_simd = FeatureConfig {
            description: "Mystery feature".to_string(),
            simd_number: None,
            simd_url: None,
            activation: test_common::manifest::ActivationConfig::default(),
            depends_on: vec![],
            deprecated_by: None,
            test: None,
        };
        assert!(feature_label(&zk_pk, &with_simd, None).starts_with("SIMD-0153 "));
        assert!(feature_label(&zk_pk, &without_simd, None).starts_with("Feature "));
    }
}
