//! Explicit macOS microphone consent.
//!
//! Capture spawns an external tool whose *implicit* mic access macOS may silently deny for
//! a background daemon — no dialog at all, notoriously on macOS 13. Asking through
//! AVCaptureDevice's request API is the sanctioned way to make the consent dialog appear,
//! attributed to this binary (with the usage description embedded by build.rs), and it
//! lets a running job WAIT for the user's click instead of failing while the dialog is
//! still on screen. Linux has no TCC gate — everything here is a no-op there.

#[cfg(target_os = "macos")]
pub use mac::{ensure_consent, preflight, status_label};

#[cfg(not(target_os = "macos"))]
pub fn ensure_consent() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn preflight() {}

/// Machine-readable mic-permission state, for `server.info` / `dialf --version`.
///
/// Linux has no TCC gate, so there is nothing to report and nothing to fix.
#[cfg(not(target_os = "macos"))]
pub fn status_label() -> &'static str {
    "not_applicable"
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::bail;
    use block2::StackBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};

    // AVAuthorizationStatus
    const NOT_DETERMINED: isize = 0;
    const AUTHORIZED: isize = 3;

    /// How long a capture waits for the user to answer the consent dialog.
    const DIALOG_WAIT: Duration = Duration::from_secs(120);

    const FIX_HINT: &str =
        "enable `dialf` in System Settings → Privacy & Security → Microphone";

    /// Why a dialog may never appear, given the multiplexer (if any) we were started from.
    ///
    /// Pure so the wording is testable: the multiplexer case is the one that costs hours,
    /// because tccd shows nothing at all and the daemon cannot tell that apart from a user
    /// who simply hasn't clicked yet.
    pub(super) fn no_dialog_hint(mux: Option<&str>) -> String {
        match mux {
            Some(mux) => format!(
                "no dialog will appear: this daemon was started from {mux}, and macOS attributes \
                 the request to {mux} rather than to dialf. Start it from a plain Terminal window, \
                 or install it as a service (`dialf service install --user`), and try again"
            ),
            None => format!(
                "if no dialog appeared, start the daemon from a plain Terminal window (not tmux/\
                 screen/ssh, where macOS shows nothing) or as a service; if one did appear, \
                 approve it ({FIX_HINT})"
            ),
        }
    }

    /// The live authorization state as a stable string.
    pub fn status_label() -> &'static str {
        let Some((cls, media)) = av() else {
            return "unknown";
        };
        match status(cls, media) {
            AUTHORIZED => "authorized",
            NOT_DETERMINED => "not_determined",
            _ => "denied",
        }
    }

    /// AVCaptureDevice class + the AVMediaTypeAudio string ("soun"). The binary doesn't
    /// link AVFoundation; load it on demand.
    fn av(
    ) -> Option<(&'static AnyClass, *mut AnyObject)> {
        unsafe {
            let h = libc::dlopen(
                b"/System/Library/Frameworks/AVFoundation.framework/AVFoundation\0".as_ptr().cast(),
                libc::RTLD_LAZY,
            );
            if h.is_null() {
                return None;
            }
            let cls = AnyClass::get("AVCaptureDevice")?;
            let ns = AnyClass::get("NSString")?;
            let media: *mut AnyObject = msg_send![ns, stringWithUTF8String: b"soun\0".as_ptr().cast::<std::os::raw::c_char>()];
            if media.is_null() {
                return None;
            }
            Some((cls, media))
        }
    }

    fn status(cls: &AnyClass, media: *mut AnyObject) -> isize {
        unsafe { msg_send![cls, authorizationStatusForMediaType: media] }
    }

    /// Ask tccd to show the consent dialog; the answer arrives on `tx`.
    fn request(cls: &AnyClass, media: *mut AnyObject, tx: mpsc::Sender<bool>) {
        let block = StackBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        })
        .copy();
        unsafe {
            let _: () = msg_send![cls, requestAccessForMediaType: media, completionHandler: &*block];
        }
        // The completion may fire long after this frame; never free the block early.
        std::mem::forget(block);
    }

    /// Daemon startup: log the mic state and, if consent was never asked, fire the dialog
    /// now (non-blocking) — so an install/upgrade prompts immediately, not mid-call.
    pub fn preflight() {
        let Some((cls, media)) = av() else {
            tracing::warn!("AVFoundation unavailable — cannot query Microphone permission");
            return;
        };
        match status(cls, media) {
            AUTHORIZED => tracing::info!("microphone: authorized"),
            NOT_DETERMINED => {
                let mux = crate::daemon::current_multiplexer();
                tracing::info!(
                    "microphone: never asked — requesting access now (a dialog should appear on \
                     this Mac's screen)"
                );
                if mux.is_some() {
                    tracing::warn!("microphone: {}", no_dialog_hint(mux));
                }
                let (tx, _) = mpsc::channel();
                request(cls, media, tx);
            }
            _ => tracing::warn!("microphone: DENIED for this daemon — {FIX_HINT}"),
        }
    }

    /// Gate a capture: authorized → Ok; never asked → show the dialog and wait for the
    /// click; denied → fail fast with the fix (instead of a vague empty-capture timeout).
    pub fn ensure_consent() -> anyhow::Result<()> {
        let Some((cls, media)) = av() else {
            return Ok(()); // can't query — fall through to the capture tool's own failure
        };
        match status(cls, media) {
            AUTHORIZED => Ok(()),
            NOT_DETERMINED => {
                // Only that the request was *made* is known here — whether macOS actually put a
                // dialog on screen is not observable, and it silently does not under a
                // multiplexer. Saying "dialog shown" here cost a long debugging session.
                // No warning here: `preflight` already logged the multiplexer caveat at
                // startup, and the timeout below carries it to whoever ran the job.
                let mux = crate::daemon::current_multiplexer();
                tracing::info!(
                    "microphone: requesting access — waiting up to {}s for approval",
                    DIALOG_WAIT.as_secs()
                );
                let (tx, rx) = mpsc::channel();
                request(cls, media, tx);
                match rx.recv_timeout(DIALOG_WAIT) {
                    Ok(true) => {
                        tracing::info!("microphone: granted");
                        Ok(())
                    }
                    Ok(false) => bail!("Microphone permission denied — {FIX_HINT}, then retry"),
                    Err(_) => bail!(
                        "No answer to the microphone request after {}s — {}",
                        DIALOG_WAIT.as_secs(),
                        no_dialog_hint(mux)
                    ),
                }
            }
            // macOS never re-prompts a denied binary, so "retry" alone is useless advice.
            _ => bail!(
                "Microphone permission denied for the daemon — macOS will not ask again for this \
                 binary, so {FIX_HINT} (the grant is per-binary: after an upgrade the entry you \
                 want is the newest one), then restart the daemon"
            ),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::mac::no_dialog_hint;

    /// The multiplexer case is the expensive one: macOS presents nothing, so the daemon must say
    /// so outright rather than leave the operator clicking at a dialog that was never there.
    #[test]
    fn the_multiplexer_hint_names_the_cause_and_the_fix() {
        let h = no_dialog_hint(Some("tmux"));
        assert!(h.contains("no dialog will appear"), "got: {h}");
        assert!(h.contains("tmux"), "got: {h}");
        assert!(h.contains("plain Terminal"), "got: {h}");
        assert!(h.contains("service install"), "got: {h}");
    }

    /// Without a multiplexer a dialog probably *is* on screen, so the hint must cover both
    /// branches instead of asserting one — that false certainty was the original bug.
    #[test]
    fn without_a_multiplexer_the_hint_covers_both_possibilities() {
        let h = no_dialog_hint(None);
        assert!(h.contains("if no dialog appeared"), "got: {h}");
        assert!(h.contains("approve it"), "got: {h}");
        assert!(!h.contains("no dialog will appear"), "must not claim it cannot appear: {h}");
    }

    /// Whatever the state, the label has to be one of the strings clients branch on.
    #[test]
    fn status_label_is_one_of_the_documented_values() {
        let l = super::status_label();
        assert!(
            matches!(l, "authorized" | "denied" | "not_determined" | "unknown"),
            "unexpected label: {l}"
        );
    }
}
