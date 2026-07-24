//! Memorable adjective+noun name minting for agent identities (ADR-009 D2).
//!
//! Names are `AdjectiveNoun` (e.g. `BlueLake`, `GreenCastle`) — memorable
//! handles, *not* role descriptions. We avoid adding a `rand` dependency: a
//! seed (nanosecond clock, salted by the caller) drives a tiny LCG to pick one
//! adjective and one noun. The caller retries with a fresh seed on collision,
//! so determinism-per-seed is fine and keeps the function pure and testable.

const ADJECTIVES: &[&str] = &[
    "Amber", "Azure", "Blue", "Bold", "Brave", "Bright", "Calm", "Clever", "Coral", "Crimson",
    "Dawn", "Deep", "Eager", "Ember", "Fair", "Fleet", "Gold", "Green", "Grey", "Hardy", "Ivory",
    "Jade", "Keen", "Lark", "Lucky", "Mellow", "Merry", "Noble", "Onyx", "Pale", "Quiet", "Rapid",
    "Royal", "Ruby", "Sage", "Scarlet", "Silent", "Silver", "Slate", "Snowy", "Solar", "Steady",
    "Storm", "Swift", "Teal", "Umber", "Vivid", "Warm", "Wild", "Zesty",
];

const NOUNS: &[&str] = &[
    "Alder", "Anchor", "Arbor", "Badger", "Basin", "Beacon", "Bear", "Birch", "Bison", "Bluff",
    "Brook", "Canyon", "Castle", "Cedar", "Cliff", "Comet", "Crane", "Delta", "Dune", "Eagle",
    "Falcon", "Fern", "Forge", "Fox", "Grove", "Harbor", "Hawk", "Heron", "Lake", "Lynx", "Maple",
    "Meadow", "Mesa", "Otter", "Peak", "Pine", "Quarry", "Raven", "Reef", "Ridge", "River",
    "Stone", "Summit", "Thicket", "Vale", "Willow", "Wolf", "Aspen", "Cove", "Harrier",
];

/// Total distinct names this generator can produce. Used by callers to bound
/// collision-retry attempts (no point retrying more than the space size).
pub fn name_space() -> usize {
    ADJECTIVES.len() * NOUNS.len()
}

/// Mint one `AdjectiveNoun` name from a seed. Pure and total: every `u64`
/// yields a valid name. Callers salt the seed (e.g. nanosecond clock +
/// attempt counter) and retry on DB-uniqueness collision.
pub fn mint(seed: u64) -> String {
    // Split the seed so the adjective and noun indices are independent. A tiny
    // LCG step (Numerical Recipes constants) decorrelates the low bits from a
    // clock-derived seed.
    let mixed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let adj = (mixed % ADJECTIVES.len() as u64) as usize;
    let noun = ((mixed / ADJECTIVES.len() as u64) % NOUNS.len() as u64) as usize;
    format!("{}{}", ADJECTIVES[adj], NOUNS[noun])
}

/// True if `name` is a well-formed caller-supplied handle: non-empty, ASCII
/// alphanumeric, bounded length. This is a *format* guard (mirrors
/// `session_store::file_name`'s safe-id check) — it does not require the name
/// be from our word lists, so agents may bring their own memorable name.
pub fn is_valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn mint_is_deterministic_per_seed() {
        assert_eq!(mint(42), mint(42));
    }

    #[test]
    fn mint_always_valid_format() {
        // Every seed yields a name that passes the format guard.
        for seed in 0u64..1000 {
            let n = mint(seed);
            assert!(is_valid(&n), "minted name {n:?} failed format guard");
        }
    }

    #[test]
    fn mint_covers_many_distinct_names() {
        // Sanity: minting over many seeds explores a large slice of the space,
        // so collision-retry is cheap in practice.
        let mut seen = HashSet::new();
        for seed in 0u64..2000 {
            seen.insert(mint(seed));
        }
        assert!(
            seen.len() > 500,
            "expected wide coverage, only saw {} distinct names",
            seen.len()
        );
    }

    #[test]
    fn is_valid_rejects_unsafe_and_empty() {
        assert!(!is_valid(""));
        assert!(!is_valid("has space"));
        assert!(!is_valid("../etc"));
        assert!(!is_valid("name-with-dash"));
        assert!(!is_valid(&"x".repeat(65)));
        assert!(is_valid("BlueLake"));
        assert!(is_valid("agent7"));
    }
}
