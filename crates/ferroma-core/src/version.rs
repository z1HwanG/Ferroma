//! Build and protocol version constants.

/// The Ferroma release version (`Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Ferroma Client Protocol version implemented by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// Build timestamp, injected by the release pipeline as `FERROMA_BUILD_TIMESTAMP`.
/// Falls back to `"unknown"` for local builds.
pub const BUILD_TIMESTAMP: &str = match option_env!("FERROMA_BUILD_TIMESTAMP") {
    Some(value) => value,
    None => "unknown",
};

/// Git revision, injected by the release pipeline as `FERROMA_GIT_SHA`.
pub const GIT_SHA: &str = match option_env!("FERROMA_GIT_SHA") {
    Some(value) => value,
    None => "unknown",
};

/// A single-line version banner, used in `--version`, SMTP `EHLO` comments and
/// the `X-Ferroma-Version` header.
pub fn banner() -> String {
    format!("Ferroma/{VERSION} (FCP/{PROTOCOL_VERSION})")
}

/// The full build identity, used by `ferroma version` and the Admin dashboard.
pub fn build_info() -> String {
    format!(
        "Ferroma {VERSION}\nprotocol: FCP/{PROTOCOL_VERSION}\nbuilt: {BUILD_TIMESTAMP}\nrevision: {GIT_SHA}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wiring the Dockerfile's build arguments depend on: whatever the constants hold has
    /// to reach the banner. Locally they are `"unknown"`; in a published image they are the
    /// release's commit and date. This is the assertion that would have failed when the image
    /// carried the identity only in its OCI labels.
    #[test]
    fn build_info_reports_the_identity_it_was_built_with() {
        assert!(build_info().contains(BUILD_TIMESTAMP));
        assert!(build_info().contains(GIT_SHA));
    }

    #[test]
    fn version_banner_is_well_formed() {
        assert!(banner().starts_with("Ferroma/"));
        assert!(banner().contains("FCP/"));
        assert!(build_info().contains(VERSION));
    }

    #[test]
    fn protocol_version_agrees_with_the_configuration_default() {
        // The constant and the config default must not drift: a client reading the
        // `X-Ferroma-Protocol` header and a server using `client.protocol_version`
        // have to be talking about the same number. The comparison is also what
        // keeps this a runtime assertion rather than a compile-time tautology.
        assert_eq!(
            PROTOCOL_VERSION,
            crate::config::ClientConfig::default().protocol_version
        );
    }
}
