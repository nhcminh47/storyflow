use slug_app::slugify;

#[test]
fn keeps_ascii_letters_and_digits() {
    assert_eq!(slugify("Release 42"), "release-42");
}

#[test]
fn collapses_internal_separator_runs() {
    assert_eq!(slugify("Alpha /_été\t-BETA"), "alpha-t-beta");
    assert_eq!(slugify("One--   two  THREE"), "one-two-three");
}

#[test]
fn drops_edge_separators_and_handles_empty_inputs() {
    assert_eq!(slugify("  !!Hello  "), "hello");
    assert_eq!(slugify("🤖 -- "), "");
    assert_eq!(slugify(""), "");
}
