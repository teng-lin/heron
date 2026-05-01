//! Platform → bundle-id mapping used by `start_capture` to seed the
//! v1 pipeline's `target_bundle_id`. Kept separate so the table is the
//! single place the orchestrator declares the cross-platform mapping;
//! adding a `Platform` variant fails to compile here first.

use heron_session::Platform;

pub(crate) fn platform_target_bundle_id(platform: Platform) -> &'static str {
    match platform {
        Platform::Zoom => "us.zoom.xos",
        Platform::GoogleMeet => "com.google.Chrome",
        Platform::MicrosoftTeams => "com.microsoft.teams2",
        Platform::Webex => "Cisco-Systems.Spark",
    }
}
