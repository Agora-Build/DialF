//! Explicit macOS microphone consent.
//!
//! Capture spawns an external tool whose *implicit* mic access macOS may silently deny for
//! a background daemon — no dialog at all, notoriously on macOS 13. Asking through
//! AVCaptureDevice's request API is the sanctioned way to make the consent dialog appear,
//! attributed to this binary (with the usage description embedded by build.rs), and it
//! lets a running job WAIT for the user's click instead of failing while the dialog is
//! still on screen. Linux has no TCC gate — everything here is a no-op there.

#[cfg(target_os = "macos")]
pub use mac::{ensure_consent, preflight};

#[cfg(not(target_os = "macos"))]
pub fn ensure_consent() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn preflight() {}

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
                tracing::info!(
                    "microphone: never asked — requesting now (approve the dialog on this Mac's screen)"
                );
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
                tracing::info!(
                    "microphone consent dialog shown — waiting for the user to approve"
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
                        "Microphone consent dialog not answered within {}s — approve it on \
                         the Mac's screen ({FIX_HINT}), then retry",
                        DIALOG_WAIT.as_secs()
                    ),
                }
            }
            _ => bail!("Microphone permission denied for the daemon — {FIX_HINT}, then retry"),
        }
    }
}
