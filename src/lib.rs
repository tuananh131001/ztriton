//! ztriton core: Jellyfin client, libmpv player, and saved login/preferences.
#![expect(
    unused_crate_dependencies,
    reason = "Cargo shares one dependency list between the library, the app binary and the tests"
)]

pub mod jellyfin;
pub mod player;
pub mod keychain;
pub mod preferences;
