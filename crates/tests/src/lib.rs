use std::collections::HashMap;
use test_common::{E2eTest, SimdTest};

mod e2e;
mod simd_0153;
mod simd_0194;
mod simd_0266;

/// Construct the SIMD test registry. Each `register()` returns a boxed test
/// impl whose `id()` is used as the map key so the manifest entry's
/// `test.id` field maps deterministically to the right impl.
pub fn all_simd_tests() -> HashMap<String, Box<dyn SimdTest>> {
    let mut map: HashMap<String, Box<dyn SimdTest>> = HashMap::new();
    for register in [
        simd_0153::register,
        simd_0194::register,
        simd_0266::register,
    ] {
        let test = register();
        map.insert(test.id().to_string(), test);
    }
    map
}

pub fn all_e2e_tests() -> HashMap<String, Box<dyn E2eTest>> {
    let mut map = HashMap::new();
    map.insert(
        "e2e_confidential_transfers".to_string(),
        e2e::confidential_transfers::register(),
    );
    map
}
