const RELEASE_VERSION: &str = "0.0.9";
const RELEASE_TAG: &str = "v0.0.9";

#[test]
fn package_version_matches_release() {
    assert_eq!(env!("CARGO_PKG_VERSION"), RELEASE_VERSION);
}

#[test]
fn agentsync_lockfile_entry_matches_release() {
    let lockfile = include_str!("../Cargo.lock");
    let agentsync_versions: Vec<_> = lockfile
        .split("[[package]]")
        .skip(1)
        .filter_map(|package| {
            let mut name = None;
            let mut version = None;

            for line in package.lines().map(str::trim) {
                if let Some(value) = line.strip_prefix("name = \"") {
                    name = value.strip_suffix('"');
                } else if let Some(value) = line.strip_prefix("version = \"") {
                    version = value.strip_suffix('"');
                }
            }

            (name == Some("agentsync")).then_some(version)
        })
        .collect();

    assert_eq!(
        agentsync_versions.len(),
        1,
        "Cargo.lock must contain exactly one agentsync package entry"
    );
    assert_eq!(
        agentsync_versions[0],
        Some(RELEASE_VERSION),
        "the agentsync package entry in Cargo.lock must match the release"
    );
}

#[test]
fn readme_release_examples_use_only_the_current_tag() {
    let readme = include_str!("../README.md");
    let release_tags: Vec<_> = readme
        .match_indices("v0.0.")
        .map(|(start, _)| {
            let suffix = &readme[start..];
            let end = suffix
                .char_indices()
                .skip("v0.0.".len())
                .find(|(_, character)| !character.is_ascii_digit())
                .map_or(suffix.len(), |(index, _)| index);
            &suffix[..end]
        })
        .collect();

    assert!(
        !release_tags.is_empty(),
        "README must contain at least one pinned v0.0.x release example"
    );
    assert!(
        release_tags.iter().all(|tag| *tag == RELEASE_TAG),
        "README contains release tags other than {RELEASE_TAG}: {release_tags:?}"
    );
}
