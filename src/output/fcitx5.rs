//! fcitx5 text output
//!
//! Commits the transcription to the focused input context through the
//! fcitx5-commit addon's D-Bus method. This is the one X11 path that delivers
//! text instead of keycodes: fcitx5 puts the string into the focused text field
//! by the same route it uses to insert a pinyin candidate, so nothing depends on
//! the keyboard mapping, on the active layout, or on a client having re-read the
//! mapping before the next key event arrives. There is no keymap change, so
//! there is no settle wait, and a character the layout cannot produce is no
//! harder than any other.
//!
//! Requirements: fcitx5 running on the session bus with the fcitx5-commit addon
//! loaded (AUR `fcitx5-commit-git`). The addon exports
//! `CommitString(s) -> b` on `/commit` of fcitx5's well-known name because
//! fcitx5's own D-Bus API has no way to insert text: `Controller1` switches
//! input methods, and `InputContext1.CommitString` is a signal fcitx5 sends to
//! its clients.
//!
//! The target application must be an fcitx5 client, which means its toolkit has
//! an fcitx5 input-method module, or it uses fcitx5's XIM server, or it speaks
//! the Wayland input-method protocol. Terminals, TTYs and applications with no
//! input-method support still need a typing driver, so this driver belongs at
//! the front of a chain that keeps one.
//!
//! `auto_submit` is not honored: committing a string cannot press Enter. A
//! newline can be committed, but chat applications treat that as a line break
//! rather than a submit, so pressing nothing is better than pretending.

use super::TextOutput;
use crate::error::OutputError;
use std::time::Duration;
use tokio::sync::OnceCell;
use zbus::Connection;

/// fcitx5's well-known session-bus name. The addon lives on fcitx5's own bus
/// connection, so its object is reached through that name.
const SERVICE: &str = "org.fcitx.Fcitx5";

/// Object path the fcitx5-commit addon exports.
const OBJECT_PATH: &str = "/commit";

/// Interface the fcitx5-commit addon exports.
const INTERFACE: &str = "io.github.vendetta1871.Commit1";

/// Method that commits a string to the most recent input context.
const METHOD: &str = "CommitString";

/// How long to wait for fcitx5 to answer. The call is a round trip to another
/// process and the chain has drivers behind this one, so a busy or wedged
/// fcitx5 must not hold up the dictation.
const CALL_TIMEOUT: Duration = Duration::from_millis(500);

/// Set the first time the driver finds fcitx5 running without the addon, so the
/// install instructions are logged once per process instead of once per
/// dictation.
static WARNED_MISSING_ADDON: std::sync::Once = std::sync::Once::new();

/// fcitx5 text output.
pub struct Fcitx5Output {
    /// Session-bus connection, opened on first use and then reused: an output
    /// driver runs once per transcription and `is_available` runs before it, so
    /// connecting per call would add a round trip to every dictation.
    connection: OnceCell<Connection>,
    /// Text appended after the transcription
    append_text: Option<String>,
}

impl Fcitx5Output {
    /// Create a new fcitx5 output
    pub fn new(append_text: Option<String>) -> Self {
        Self {
            connection: OnceCell::new(),
            append_text,
        }
    }

    /// Commit `text` to the focused input context.
    ///
    /// `false` means fcitx5 has no focused input context - nothing was
    /// committed, and the caller should try the next driver rather than leave
    /// the transcription unsent.
    async fn commit(&self, text: &str) -> Result<bool, OutputError> {
        let connection = self
            .connection
            .get_or_try_init(|| async { Connection::session().await })
            .await
            .map_err(|e| OutputError::InjectionFailed(format!("no session bus for fcitx5: {e}")))?;

        let body = (text,);
        let call =
            connection.call_method(Some(SERVICE), OBJECT_PATH, Some(INTERFACE), METHOD, &body);
        let reply = tokio::time::timeout(CALL_TIMEOUT, call)
            .await
            .map_err(|_| {
                OutputError::InjectionFailed(format!(
                    "fcitx5 did not answer {METHOD} within {} ms",
                    CALL_TIMEOUT.as_millis()
                ))
            })?
            .map_err(|e| {
                if addon_is_missing(&e) {
                    OutputError::Fcitx5AddonMissing
                } else {
                    OutputError::InjectionFailed(format!("fcitx5 {METHOD} failed: {e}"))
                }
            })?;

        reply.body().deserialize::<bool>().map_err(|e| {
            OutputError::InjectionFailed(format!(
                "fcitx5 {METHOD} returned {e} instead of a boolean"
            ))
        })
    }
}

/// Whether a D-Bus error means the addon (or fcitx5 itself) is absent, as
/// opposed to a call that failed for some other reason.
///
/// The distinction decides what the user is told: an absent addon is a setup
/// step, anything else is a fault.
fn addon_is_missing(error: &zbus::Error) -> bool {
    match error {
        // Raised by the bus daemon when the name, path or interface is not there.
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.ServiceUnknown"
        ),
        zbus::Error::InterfaceNotFound => true,
        zbus::Error::FDO(error) => matches!(
            &**error,
            zbus::fdo::Error::ServiceUnknown(_)
                | zbus::fdo::Error::NameHasNoOwner(_)
                | zbus::fdo::Error::UnknownObject(_)
                | zbus::fdo::Error::UnknownInterface(_)
                | zbus::fdo::Error::UnknownMethod(_)
        ),
        _ => false,
    }
}

#[async_trait::async_trait]
impl TextOutput for Fcitx5Output {
    async fn output(&self, text: &str) -> Result<(), OutputError> {
        let mut payload = text.to_string();
        if let Some(append) = &self.append_text {
            payload.push_str(append);
        }
        if payload.is_empty() {
            return Ok(());
        }

        if self.commit(&payload).await? {
            tracing::info!(
                "Text committed via fcitx5 ({} chars)",
                payload.chars().count()
            );
            Ok(())
        } else {
            Err(OutputError::Fcitx5NoInputContext)
        }
    }

    async fn is_available(&self) -> bool {
        // The addon documents an empty string as a probe: nothing is committed,
        // and the answer still reports whether an input context exists. Either
        // answer means the addon is loaded and answering.
        match self.commit("").await {
            Ok(_) => true,
            Err(e) => {
                // A driver that is simply not installed stays quiet, but a
                // fcitx5 the user asked for and cannot use is a setup step they
                // need to hear about - once, not once per dictation.
                if matches!(e, OutputError::Fcitx5AddonMissing) {
                    WARNED_MISSING_ADDON.call_once(|| tracing::warn!("{e}"));
                }
                tracing::debug!("fcitx5 not available: {e}");
                false
            }
        }
    }

    fn name(&self) -> &'static str {
        "fcitx5"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_without_the_service_counts_as_a_missing_addon() {
        // What the bus daemon reports when fcitx5 is not running, or when the
        // addon is not loaded. Deciding this correctly is what turns an opaque
        // failure into "install the addon".
        for error in [
            zbus::fdo::Error::ServiceUnknown(SERVICE.to_string()),
            zbus::fdo::Error::UnknownObject(OBJECT_PATH.to_string()),
            zbus::fdo::Error::UnknownInterface(INTERFACE.to_string()),
            zbus::fdo::Error::UnknownMethod(METHOD.to_string()),
        ] {
            assert!(addon_is_missing(&zbus::Error::FDO(Box::new(error))));
        }
    }

    #[test]
    fn other_failures_are_not_reported_as_a_missing_addon() {
        // A timeout or a serialization fault must not tell the user to install
        // something that is already there.
        assert!(!addon_is_missing(&zbus::Error::Failure(
            "connection dropped".to_string()
        )));
        assert!(!addon_is_missing(&zbus::Error::FDO(Box::new(
            zbus::fdo::Error::Timeout("no reply".to_string())
        ))));
    }

    #[tokio::test]
    async fn empty_text_never_reaches_the_bus() {
        // Committing nothing would answer "no input context" and send the
        // transcription down the fallback chain for no reason.
        let output = Fcitx5Output::new(None);
        assert!(output.output("").await.is_ok());
    }
}
