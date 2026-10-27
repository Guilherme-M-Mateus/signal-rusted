//! 1:1 voice calls as the UI sees them. The media side (RingRTC's CallManager over Signal's
//! prebuilt WebRTC) is `calls::media`, built only with the `calls` feature; without it the app
//! shows no call controls and `Command::Call` does nothing.

use presage::libsignal_service::prelude::Uuid;

/// Whether this build can place and answer calls.
pub const ENABLED: bool = cfg!(feature = "calls");

#[cfg(feature = "calls")]
pub mod media;
#[cfg(feature = "calls")]
pub use media::Out;
/// What RingRTC reports to the backend; nothing without the `calls` feature.
#[cfg(not(feature = "calls"))]
#[derive(Debug)]
pub enum Out {}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Setting up: we're calling, or an offer is being checked before it rings.
    Calling,
    Ringing,
    Connected,
    Reconnecting,
    /// Over, with why ("Call ended", "No answer", …); the UI shows it until dismissed.
    Ended(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub peer: Uuid,
    pub incoming: bool,
    pub video: bool,
    pub phase: Phase,
    pub muted: bool,
    /// When it first connected (ms), for the call timer.
    pub connected_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CallCommand {
    /// Calls this contact (audio only).
    Start(Uuid),
    Accept,
    /// Hangs up, cancels or declines the current call.
    Hangup,
    Mute(bool),
}

/// The call timer: "0:07", "12:30", "1:02:03".
pub fn elapsed(since_ms: u64, now_ms: u64) -> String {
    let s = now_ms.saturating_sub(since_ms) / 1000;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timer() {
        assert_eq!(super::elapsed(1_000, 8_999), "0:07");
        assert_eq!(super::elapsed(0, 750_000), "12:30");
        assert_eq!(super::elapsed(0, 3_723_000), "1:02:03");
        assert_eq!(super::elapsed(5_000, 1_000), "0:00", "clock skew never goes negative");
    }
}
