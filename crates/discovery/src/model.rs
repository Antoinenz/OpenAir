//! Turning mDNS `model` identifiers into names people recognise.
//!
//! Receivers advertise an internal identifier — `AppleTV6,2`, `AudioAccessory5,1`
//! — which is precise and unreadable. The picker shows this beside every device,
//! so it is worth translating.
//!
//! **The fallback is the important part.** This table is assembled from public
//! sources and cannot be exhaustive; hardware ships that it will never know
//! about. An unknown identifier is therefore shown exactly as advertised. A
//! wrong marketing name is worse than a raw one: the raw string is at least
//! searchable, and honest about being an identifier.

/// Exact identifier → marketing name.
const EXACT: &[(&str, &str)] = &[
    // Apple TV
    ("AppleTV2,1", "Apple TV (2nd gen)"),
    ("AppleTV3,1", "Apple TV (3rd gen)"),
    ("AppleTV3,2", "Apple TV (3rd gen)"),
    ("AppleTV5,3", "Apple TV HD"),
    ("AppleTV6,2", "Apple TV 4K"),
    ("AppleTV11,1", "Apple TV 4K (2nd gen)"),
    ("AppleTV14,1", "Apple TV 4K (3rd gen)"),
    // HomePod
    ("AudioAccessory1,1", "HomePod"),
    ("AudioAccessory1,2", "HomePod"),
    ("AudioAccessory5,1", "HomePod mini"),
    ("AudioAccessory6,1", "HomePod (2nd gen)"),
    // AirPort
    ("AirPort4,107", "AirPort Express"),
    ("AirPort10,115", "AirPort Express (2nd gen)"),
];

/// Identifier prefix → family name, for lines with too many members to table.
///
/// Longest prefix first: `MacBookPro` must be tried before `MacBook`, or every
/// Pro would come out as a MacBook. The generation suffix is dropped — nobody
/// can decode "18,3" and it would only cost row width.
const PREFIXES: &[(&str, &str)] = &[
    ("MacBookPro", "MacBook Pro"),
    ("MacBookAir", "MacBook Air"),
    ("MacBook", "MacBook"),
    ("MacPro", "Mac Pro"),
    ("MacStudio", "Mac Studio"),
    ("Macmini", "Mac mini"),
    ("iMacPro", "iMac Pro"),
    ("iMac", "iMac"),
    ("Macintosh", "Mac"),
];

/// A readable name for an mDNS `model` identifier.
///
/// Returns the input unchanged when the identifier is not recognised.
pub fn pretty_model(model: &str) -> &str {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return "unknown";
    }
    if let Some((_, name)) = EXACT.iter().find(|(id, _)| *id == trimmed) {
        return name;
    }
    if let Some((_, name)) = PREFIXES.iter().find(|(p, _)| trimmed.starts_with(p)) {
        return name;
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_apple_tvs_we_have_tested_against() {
        assert_eq!(pretty_model("AppleTV5,3"), "Apple TV HD");
        assert_eq!(pretty_model("AppleTV6,2"), "Apple TV 4K");
    }

    #[test]
    fn names_homepods() {
        assert_eq!(pretty_model("AudioAccessory5,1"), "HomePod mini");
        assert_eq!(pretty_model("AudioAccessory1,1"), "HomePod");
    }

    #[test]
    fn matches_mac_families_by_prefix() {
        assert_eq!(pretty_model("MacBookPro18,3"), "MacBook Pro");
        assert_eq!(pretty_model("MacBookAir10,1"), "MacBook Air");
        assert_eq!(pretty_model("Macmini9,1"), "Mac mini");
        assert_eq!(pretty_model("iMac21,1"), "iMac");
    }

    #[test]
    fn longer_prefixes_win() {
        // Ordered wrongly, every Pro and Air would come out as "MacBook".
        assert_eq!(pretty_model("MacBookPro18,3"), "MacBook Pro");
        assert_ne!(pretty_model("MacBookAir10,1"), "MacBook");
        assert_eq!(pretty_model("iMacPro1,1"), "iMac Pro");
    }

    #[test]
    fn an_unknown_identifier_is_shown_as_advertised() {
        // The rule that matters: a wrong marketing name is worse than a raw
        // identifier, so anything unrecognised passes straight through.
        assert_eq!(pretty_model("AppleTV99,9"), "AppleTV99,9");
        assert_eq!(pretty_model("ShairportSync"), "ShairportSync");
        assert_eq!(pretty_model("Some Random Speaker"), "Some Random Speaker");
    }

    #[test]
    fn an_empty_model_reads_as_unknown() {
        assert_eq!(pretty_model(""), "unknown");
        assert_eq!(pretty_model("   "), "unknown");
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(pretty_model("  AppleTV6,2  "), "Apple TV 4K");
    }

    #[test]
    fn the_table_has_no_duplicate_identifiers() {
        // A duplicate would mean the second entry is unreachable, which is the
        // kind of thing that only shows up as "why is my device named wrong".
        let mut ids: Vec<&str> = EXACT.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate identifier in EXACT");
    }

    #[test]
    fn prefixes_are_ordered_longest_first_within_a_family() {
        // Guards the ordering invariant directly: no prefix may be preceded by
        // one it starts with.
        for (i, (prefix, _)) in PREFIXES.iter().enumerate() {
            for (earlier, _) in &PREFIXES[..i] {
                assert!(
                    !prefix.starts_with(earlier),
                    "{prefix} is shadowed by the earlier {earlier}"
                );
            }
        }
    }
}
