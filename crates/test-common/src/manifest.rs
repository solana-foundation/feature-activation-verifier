use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use log::warn;
use serde::Deserialize;
use solana_pubkey::Pubkey;

/// Alias for "the base58 string representation of a feature-gate pubkey" used
/// purely for documentation; the in-memory key type is `solana_pubkey::Pubkey`.
pub type FeatureKey = Pubkey;

#[derive(Debug, Clone, Default)]
pub struct Manifest {
    features: HashMap<Pubkey, FeatureConfig>,
    e2e_checks: HashMap<String, E2eCheckConfig>,
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_yaml(&contents)
    }

    pub fn from_yaml(contents: &str) -> anyhow::Result<Self> {
        let raw: ManifestRaw = serde_yaml::from_str(contents)?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: ManifestRaw) -> anyhow::Result<Self> {
        let mut features = HashMap::with_capacity(raw.features.len());
        for (key, value) in raw.features {
            let pubkey = Pubkey::from_str(&key).map_err(|e| {
                anyhow::anyhow!(
                    "manifest feature key '{}' is not a valid base58 pubkey: {}",
                    key,
                    e
                )
            })?;
            let config = value.try_into_config(&pubkey)?;
            if features.insert(pubkey, config).is_some() {
                anyhow::bail!("duplicate feature key in manifest: {}", pubkey);
            }
        }

        let manifest = Manifest {
            features,
            e2e_checks: raw.e2e_checks,
        };

        // Soft-validate cross-references. `depends_on` and `deprecated_by`
        // entries that don't resolve to a known feature are not a hard error
        // because real-world feature lists sometimes reference upstream
        // SIMDs that this manifest does not (yet) track. Log so the operator
        // notices.
        for (feature, config) in &manifest.features {
            for dep in &config.depends_on {
                if !manifest.features.contains_key(dep) {
                    warn!("feature {feature} declares a dependency on unknown feature {dep}");
                }
            }
            if let Some(deprecated_by) = &config.deprecated_by {
                if !manifest.features.contains_key(deprecated_by) {
                    warn!(
                        "feature {feature} declares `deprecated_by` for unknown feature {deprecated_by}"
                    );
                }
            }
        }

        Ok(manifest)
    }

    pub fn get(&self, address: &Pubkey) -> Option<&FeatureConfig> {
        self.features.get(address)
    }

    pub fn iter_features(&self) -> impl Iterator<Item = (&Pubkey, &FeatureConfig)> {
        self.features.iter()
    }

    pub fn iter_e2e_checks(&self) -> impl Iterator<Item = (&String, &E2eCheckConfig)> {
        self.e2e_checks.iter()
    }

    /// Look up a feature by SIMD number. Returns the first match (the YAML
    /// loader does not enforce uniqueness on `simd_number`).
    pub fn find_by_simd_number(&self, n: u32) -> Option<(&Pubkey, &FeatureConfig)> {
        self.features.iter().find(|(_, c)| c.simd_number == Some(n))
    }

    /// Resolve a feature-gate label that is either a base58 pubkey or a
    /// `simd_<N>` / `simd-<N>` alias (case-insensitive) to a `Pubkey`.
    /// Pubkey is attempted first because that is the canonical reference
    /// form under the feature-centric schema; the alias fallback exists for
    /// e2e check ergonomics.
    pub fn resolve_feature_gate(&self, key: &str) -> Option<Pubkey> {
        if let Ok(pk) = Pubkey::from_str(key) {
            return Some(pk);
        }
        let trimmed = key.trim().to_lowercase();
        if let Some(num_str) = trimmed
            .strip_prefix("simd_")
            .or_else(|| trimmed.strip_prefix("simd-"))
        {
            if let Ok(n) = num_str.parse::<u32>() {
                return self.find_by_simd_number(n).map(|(pk, _)| *pk);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Raw (deserialization-only) types
// ---------------------------------------------------------------------------
//
// We can't directly deserialize `HashMap<Pubkey, FeatureConfig>` because
// `Pubkey`'s deserialize impl expects either a base58 string in a value
// position or a `[u8; 32]` — neither cleanly composes with serde_yaml's map-key
// handling. We deserialize into a `HashMap<String, FeatureConfigRaw>` first
// and post-process into the final type, which gives us a precise error
// message naming the offending key when parsing fails.

#[derive(Debug, Deserialize)]
struct ManifestRaw {
    #[serde(default)]
    features: HashMap<String, FeatureConfigRaw>,
    #[serde(default)]
    e2e_checks: HashMap<String, E2eCheckConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct FeatureConfigRaw {
    description: String,
    #[serde(default)]
    simd_number: Option<u32>,
    #[serde(default)]
    simd_url: Option<String>,
    #[serde(default)]
    activation: ActivationConfigRaw,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    deprecated_by: Option<String>,
    #[serde(default)]
    test: Option<TestConfig>,
}

impl FeatureConfigRaw {
    fn try_into_config(self, owner: &Pubkey) -> anyhow::Result<FeatureConfig> {
        let depends_on = self
            .depends_on
            .iter()
            .map(|s| {
                Pubkey::from_str(s).map_err(|e| {
                    anyhow::anyhow!(
                        "feature {} has an invalid `depends_on` entry '{}': {}",
                        owner,
                        s,
                        e
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let deprecated_by = self
            .deprecated_by
            .as_deref()
            .map(|s| {
                Pubkey::from_str(s).map_err(|e| {
                    anyhow::anyhow!(
                        "feature {} has an invalid `deprecated_by` entry '{}': {}",
                        owner,
                        s,
                        e
                    )
                })
            })
            .transpose()?;
        Ok(FeatureConfig {
            description: self.description,
            simd_number: self.simd_number,
            simd_url: self.simd_url,
            activation: self.activation.into(),
            depends_on,
            deprecated_by,
            test: self.test,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ActivationConfigRaw {
    #[serde(default)]
    devnet: Option<NetworkActivationConfig>,
    #[serde(default)]
    testnet: Option<NetworkActivationConfig>,
    #[serde(default)]
    mainnet: Option<NetworkActivationConfig>,
}

impl From<ActivationConfigRaw> for ActivationConfig {
    fn from(raw: ActivationConfigRaw) -> Self {
        Self {
            devnet: raw.devnet,
            testnet: raw.testnet,
            mainnet: raw.mainnet,
        }
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FeatureConfig {
    /// Human-readable description.
    pub description: String,
    /// SIMD number, when this feature is associated with a published SIMD.
    pub simd_number: Option<u32>,
    /// Link to the SIMD specification (or other reference) when available.
    pub simd_url: Option<String>,
    /// Per-network activation epochs.
    pub activation: ActivationConfig,
    /// Other feature gates this feature depends on.
    pub depends_on: Vec<Pubkey>,
    /// Feature gate that supersedes this one, when applicable.
    pub deprecated_by: Option<Pubkey>,
    /// Optional test configuration. Absent means "no test runs for this
    /// feature" and the runner will report it as a skip.
    pub test: Option<TestConfig>,
}

impl FeatureConfig {
    pub fn test_deployment_for(&self, network: &str) -> Option<&NetworkDeploymentConfig> {
        self.test
            .as_ref()
            .and_then(|test| test.deployment_for(network))
    }

    pub fn is_activated_on(&self, network: &str) -> bool {
        self.activation.is_activated_on(network)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActivationConfig {
    pub devnet: Option<NetworkActivationConfig>,
    pub testnet: Option<NetworkActivationConfig>,
    pub mainnet: Option<NetworkActivationConfig>,
}

impl ActivationConfig {
    /// Returns whether the feature is expected to be activated on the given
    /// network, based on the presence of an `epoch` value in the manifest.
    pub fn is_activated_on(&self, network: &str) -> bool {
        let config = match network {
            "devnet" => self.devnet.as_ref(),
            "testnet" => self.testnet.as_ref(),
            "mainnet" => self.mainnet.as_ref(),
            _ => None,
        };
        config.and_then(|c| c.epoch).is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkActivationConfig {
    pub epoch: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestConfig {
    /// Logical id used to look up the test impl in the registry.
    pub id: String,
    /// Source-tree path to the test implementation (documentation only).
    pub location: String,
    #[serde(default)]
    pub devnet: Option<NetworkDeploymentConfig>,
    #[serde(default)]
    pub testnet: Option<NetworkDeploymentConfig>,
    #[serde(default)]
    pub mainnet: Option<NetworkDeploymentConfig>,
}

impl TestConfig {
    pub fn deployment_for(&self, network: &str) -> Option<&NetworkDeploymentConfig> {
        match network {
            "devnet" => self.devnet.as_ref(),
            "testnet" => self.testnet.as_ref(),
            "mainnet" => self.mainnet.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkDeploymentConfig {
    #[serde(deserialize_with = "deserialize_option_pubkey", default)]
    pub address: Option<Pubkey>,
    #[serde(deserialize_with = "deserialize_option_pubkey", default)]
    pub authority: Option<Pubkey>,
}

// ---------------------------------------------------------------------------
// E2E check types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct E2eCheckConfig {
    pub description: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub requires: E2eCheckRequirements,
    /// Logical test id, registered in `crates/tests/src/lib.rs::all_e2e_tests()`.
    pub test: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct E2eCheckRequirements {
    /// Feature gates that must be active. Each entry is either a raw base58
    /// pubkey (primary form) or a `simd_<N>` alias (resolved via the
    /// `features:` map's `simd_number` field).
    #[serde(default)]
    pub feature_gates: Vec<String>,

    /// On-chain programs that must be deployed (and optionally pinned to a
    /// specific binary hash).
    #[serde(default)]
    pub programs: Vec<ProgramDeploymentRequirement>,
}

impl E2eCheckRequirements {
    /// Resolve every feature-gate label to a `(label, Pubkey)` pair. Errors
    /// if a label is neither a known feature reference (raw pubkey or
    /// `simd_<N>` alias) nor a parseable pubkey.
    pub fn resolved_feature_gates(
        &self,
        manifest: &Manifest,
    ) -> anyhow::Result<Vec<(String, Pubkey)>> {
        self.feature_gates
            .iter()
            .map(|label| {
                manifest
                    .resolve_feature_gate(label)
                    .map(|pk| (label.clone(), pk))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "feature gate '{}' is neither a valid base58 pubkey nor a known simd_<N> alias",
                            label
                        )
                    })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProgramDeploymentRequirement {
    #[serde(deserialize_with = "deserialize_pubkey")]
    pub address: Pubkey,
    /// Hex hash from `solana-verify get-executable-hash` of the deployed ELF.
    /// `None` means "must exist and be executable" with no binary pinning.
    #[serde(default)]
    pub expected_hash: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper deserializers
// ---------------------------------------------------------------------------

fn deserialize_pubkey<'de, D>(deserializer: D) -> Result<Pubkey, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Pubkey::from_str(&s).map_err(serde::de::Error::custom)
}

fn deserialize_option_pubkey<'de, D>(deserializer: D) -> Result<Option<Pubkey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    s.map(|s| Pubkey::from_str(&s).map_err(serde::de::Error::custom))
        .transpose()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
// (Replaced in Task 11 with full coverage of the new schema.)

#[cfg(test)]
mod tests {
    use super::*;

    const ZK_ELGAMAL_FEATURE: &str = "zkhiy5oLowR7HY4zogXjCjeMXyruLqBwSWH21qcFtnv";
    const RENT_FEATURE: &str = "rent6iVy6PDoViPBeJ6k5EJQrkj62h7DPyLbWGHwjrC";
    const ZERO_PUBKEY: &str = "11111111111111111111111111111111";

    fn load(yaml: &str) -> Manifest {
        Manifest::from_yaml(yaml).expect("manifest should parse")
    }

    #[test]
    fn parses_feature_with_and_without_simd_metadata() {
        let m = load(
            r#"
features:
  zkhiy5oLowR7HY4zogXjCjeMXyruLqBwSWH21qcFtnv:
    description: "ZK ElGamal Proof Program"
    simd_number: 153
    simd_url: "https://example/0153.md"
    activation:
      devnet: { epoch: 801 }
  rent6iVy6PDoViPBeJ6k5EJQrkj62h7DPyLbWGHwjrC:
    description: "Deprecate Rent Exemption Threshold"
"#,
        );
        let zk = m
            .get(&Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap())
            .expect("zk feature present");
        assert_eq!(zk.simd_number, Some(153));
        assert_eq!(zk.simd_url.as_deref(), Some("https://example/0153.md"));
        assert_eq!(
            zk.activation.devnet.as_ref().and_then(|c| c.epoch),
            Some(801)
        );
        let rent = m
            .get(&Pubkey::from_str(RENT_FEATURE).unwrap())
            .expect("rent feature present");
        assert!(rent.simd_number.is_none());
        assert!(rent.simd_url.is_none());
    }

    #[test]
    fn rejects_invalid_pubkey_as_feature_key() {
        let err = Manifest::from_yaml(
            r#"
features:
  not-a-pubkey:
    description: "bogus"
"#,
        )
        .expect_err("invalid key should error");
        let msg = format!("{err}");
        assert!(
            msg.contains("not-a-pubkey"),
            "error names offending key: {msg}"
        );
    }

    #[test]
    fn rejects_invalid_depends_on_entry() {
        let err = Manifest::from_yaml(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "ZK ElGamal"
    depends_on:
      - not-a-pubkey
"#
        ))
        .expect_err("invalid depends_on should error");
        assert!(format!("{err}").contains("not-a-pubkey"));
    }

    #[test]
    fn find_by_simd_number_returns_matching_feature() {
        let m = load(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "ZK ElGamal"
    simd_number: 153
  {RENT_FEATURE}:
    description: "Rent"
"#
        ));
        let (pk, _) = m.find_by_simd_number(153).expect("found");
        assert_eq!(pk, &Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap());
        assert!(m.find_by_simd_number(999).is_none());
    }

    #[test]
    fn resolve_feature_gate_accepts_pubkey_and_simd_alias() {
        let m = load(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "ZK ElGamal"
    simd_number: 153
"#
        ));
        let zk_pk = Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap();
        assert_eq!(m.resolve_feature_gate(ZK_ELGAMAL_FEATURE), Some(zk_pk));
        assert_eq!(m.resolve_feature_gate("simd_153"), Some(zk_pk));
        assert_eq!(m.resolve_feature_gate("SIMD_0153"), Some(zk_pk));
        assert_eq!(m.resolve_feature_gate("simd-153"), Some(zk_pk));
        // Unknown SIMD alias falls back to None.
        assert!(m.resolve_feature_gate("simd_9999").is_none());
        // Raw pubkey that isn't in the manifest still resolves (the resolver
        // does not require the pubkey to be registered).
        assert_eq!(
            m.resolve_feature_gate(ZERO_PUBKEY),
            Some(Pubkey::from_str(ZERO_PUBKEY).unwrap())
        );
        assert!(m.resolve_feature_gate("garbage").is_none());
    }

    #[test]
    fn prefers_network_specific_test_deployment_address() {
        let m = load(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "ZK ElGamal"
    simd_number: 153
    test:
      id: "simd_0153"
      location: "crates/tests/src/simd_0153.rs"
      testnet:
        address: GQfx3D8zDArQVtaRXqiJiSVe8mG2dKNGeiKBWr9YKPS5
"#
        ));
        let config = m
            .get(&Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap())
            .unwrap();
        assert_eq!(
            config
                .test_deployment_for("testnet")
                .and_then(|d| d.address),
            Some(Pubkey::from_str_const(
                "GQfx3D8zDArQVtaRXqiJiSVe8mG2dKNGeiKBWr9YKPS5"
            ))
        );
        assert!(config.test_deployment_for("localnet").is_none());
        assert!(config.test_deployment_for("devnet").is_none());
    }

    #[test]
    fn e2e_check_resolves_feature_gate_by_simd_alias_and_raw_pubkey() {
        let m = load(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "ZK ElGamal"
    simd_number: 153
e2e_checks:
  example:
    description: "Example"
    test: e2e_example
    requires:
      feature_gates:
        - {ZK_ELGAMAL_FEATURE}
        - simd_153
        - {ZERO_PUBKEY}
"#
        ));
        let check = m.iter_e2e_checks().next().unwrap().1;
        let resolved = check.requires.resolved_feature_gates(&m).unwrap();
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].1, Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap());
        assert_eq!(resolved[1].0, "simd_153");
        assert_eq!(resolved[1].1, Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap());
        assert_eq!(resolved[2].1, Pubkey::from_str(ZERO_PUBKEY).unwrap());
    }

    #[test]
    fn e2e_check_unknown_feature_gate_label_errors() {
        let m = load(
            r#"
e2e_checks:
  example:
    description: "Example"
    test: e2e_example
    requires:
      feature_gates:
        - not_a_simd_or_pubkey
"#,
        );
        let check = m.iter_e2e_checks().next().unwrap().1;
        assert!(check.requires.resolved_feature_gates(&m).is_err());
    }

    #[test]
    fn parses_features_only_document() {
        let m = load(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "ZK ElGamal"
"#
        ));
        assert_eq!(m.iter_features().count(), 1);
        assert_eq!(m.iter_e2e_checks().count(), 0);
    }

    #[test]
    fn parses_e2e_checks_only_document() {
        let m = load(&format!(
            r#"
e2e_checks:
  example:
    description: "Example"
    test: e2e_example
    requires:
      feature_gates:
        - {ZERO_PUBKEY}
"#
        ));
        assert_eq!(m.iter_features().count(), 0);
        assert_eq!(m.iter_e2e_checks().count(), 1);
    }

    #[test]
    fn depends_on_resolves_to_pubkey_list() {
        let m = load(&format!(
            r#"
features:
  {ZK_ELGAMAL_FEATURE}:
    description: "depends on rent"
    depends_on:
      - {RENT_FEATURE}
  {RENT_FEATURE}:
    description: "rent"
"#
        ));
        let zk = m
            .get(&Pubkey::from_str(ZK_ELGAMAL_FEATURE).unwrap())
            .unwrap();
        assert_eq!(zk.depends_on, vec![Pubkey::from_str(RENT_FEATURE).unwrap()]);
    }

    /// Load the actual on-disk `manifest.yaml` (relative to the workspace
    /// root, which is `CARGO_MANIFEST_DIR/../..`) and assert that every
    /// `depends_on` and `deprecated_by` reference resolves to a known
    /// feature key. This is the migration safety net: if anyone re-introduces
    /// a dangling string-style id, this test fails loudly.
    #[test]
    fn on_disk_manifest_parses_and_cross_references_resolve() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        let manifest_path = workspace_root.join("manifest.yaml");
        let m = Manifest::load(&manifest_path).expect("on-disk manifest should parse");

        let known: std::collections::HashSet<Pubkey> =
            m.iter_features().map(|(pk, _)| *pk).collect();

        let mut errors: Vec<String> = Vec::new();
        for (pk, cfg) in m.iter_features() {
            for dep in &cfg.depends_on {
                if !known.contains(dep) {
                    errors.push(format!(
                        "feature {pk} ({}) depends on unknown feature {dep}",
                        cfg.description
                    ));
                }
            }
            if let Some(dep) = &cfg.deprecated_by {
                if !known.contains(dep) {
                    errors.push(format!(
                        "feature {pk} ({}) is deprecated_by unknown feature {dep}",
                        cfg.description
                    ));
                }
            }
        }

        assert!(
            errors.is_empty(),
            "manifest cross-reference errors:\n  {}",
            errors.join("\n  ")
        );

        // Sanity: at least the three SIMDs we already test on are present.
        for simd_n in [153u32, 194, 266] {
            assert!(
                m.find_by_simd_number(simd_n).is_some(),
                "SIMD-{simd_n:04} missing from on-disk manifest"
            );
        }

        // E2E confidential_transfers check is present and its feature
        // gates resolve cleanly.
        let (_, e2e) = m
            .iter_e2e_checks()
            .find(|(id, _)| *id == "confidential_transfers")
            .expect("confidential_transfers e2e present");
        let resolved = e2e
            .requires
            .resolved_feature_gates(&m)
            .expect("e2e feature gates resolve");
        assert!(!resolved.is_empty());
    }
}
