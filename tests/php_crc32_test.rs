//! Compares [`php_crc32`] against reference vectors.
//!
//! The spawn ring and team assignment derive from this hash; these values were
//! produced by the former server and pin its exact
//! semantics (unsigned, no terminating NUL). A single differing bit would move
//! every player.

use lanverse_game::sim::crc::php_crc32;

/// (id, reference `crc32(id)` as signed 32-bit).
fn vectors() -> Vec<(&'static str, i64)> {
    vec![
        ("p-0", -1388061951),
        ("p-1", -633017449),
        ("p-2", 1129167405),
        ("p-3", 877300411),
        ("p-4", -1439817960),
        ("p-5", -584503410),
        ("p-6", 1142980148),
        ("p-7", 858214050),
        ("p-8", -1550298317),
        ("p-9", -727751771),
        ("p-10", -1704288979),
        ("p-11", -311579205),
        ("p-12", 1952775169),
        ("p-13", 56872087),
        ("p-14", -1660462796),
        ("p-15", -369071710),
        ("p-16", 1929984024),
        ("p-17", 68028558),
        ("p-18", -1800334049),
        ("p-19", -474602103),
        ("p-20", -1320694034),
        ("p-21", -968819080),
        ("p-22", 1598664642),
        ("p-23", 676241236),
        ("p-24", -1238758665),
        ("p-25", -1054000543),
        ("p-26", 1478781915),
        ("p-27", 790846285),
        ("p-28", -1080277284),
        ("p-29", -929335734),
        ("p-30", -1470300241),
        ("p-31", -547631303),
        ("p-32", 1179819651),
        ("p-33", 827698709),
        ("p-34", -1355727946),
        ("p-35", -667546848),
        ("p-36", 1094671002),
        ("p-37", 909666828),
        ("p-38", -1501071459),
        ("p-39", -780122357),
        ("p-ä", 1266829647),
        ("p-😀", -1481101937),
        ("p-local", -2017302968),
        ("", 0),
    ]
}

#[test]
fn crc32_matches_php_exactly() {
    for (id, expected) in vectors() {
        assert_eq!(i64::from(php_crc32(id.as_bytes())), expected, "crc32({id})");
    }
}

#[test]
fn multibyte_ids_hash_like_php() {
    // The hash covers UTF-8 bytes, not codepoints.
    let id = "p-ä";
    assert_eq!(
        php_crc32(id.as_bytes()),
        php_crc32(&[b'p', b'-', 0xC3, 0xA4])
    );
}
