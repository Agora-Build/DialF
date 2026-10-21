//! Turning an audio tool's failure into its cause and fix.
//!
//! Shared by the startup capture probe, playback failures and the sox check (`dialf import`
//! and the daemon), so the same failure gets the same explanation wherever it surfaces. Pure functions — every branch is
//! covered by the verbatim tool messages that motivated it.

/// A problem line with its fix under it — how every audio diagnosis is shown.
pub fn with_fix(problem: &str, fix: Option<&str>) -> String {
    match fix {
        Some(fix) => format!("{problem}\n  ↳ {fix}"),
        None => problem.to_string(),
    }
}

/// How several devices sharing one name relate to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameClash {
    /// One device by that name (or none): nothing to say.
    None,
    /// macOS lists one card as an input-only and an output-only device under one name (the TI
    /// "USB AUDIO  CODEC"). With original sox, whichever direction draws the wrong half fails
    /// outright; sox_ng picks the right one.
    Halves,
    /// Separate devices share a name (e.g. two identical cards). sox opens the first one it
    /// finds, which may simply be the other card — no error, just the wrong device.
    Duplicates,
}

/// Classify the devices called `name`, given each one's (has input, has output).
pub fn name_clash(name: &str, devices: &[(&str, bool, bool)]) -> NameClash {
    let same: Vec<_> = devices.iter().filter(|(n, ..)| *n == name).collect();
    if same.len() < 2 {
        return NameClash::None;
    }
    let input_only = same.iter().any(|(_, i, o)| *i && !*o);
    let output_only = same.iter().any(|(_, i, o)| *o && !*i);
    if input_only && output_only {
        NameClash::Halves
    } else {
        NameClash::Duplicates
    }
}

/// Original sox's CoreAudio driver compares only the name, as a *prefix*, takes the first
/// match and never checks direction (coreaudio.c). So a new name must not begin with the old
/// one, or the old one still wins.
const PREFIX_NOTE: &str = "sox matches names by prefix, so the original would still win";

/// The fix for a card split into same-named input and output halves. Original sox (14.4.2,
/// the last release, 2015) takes the first name match in either direction; sox_ng — the
/// maintained fork, Homebrew's `sox_ng`, already what nixpkgs ships as `sox` — skips devices
/// with no streams in the direction being opened (`DeviceHasBuffersInScope`, verified in the
/// sox_ng-14.8.1 tag), so the plain name works. Failing that, an aggregate device of both
/// halves has a name of its own.
pub fn split_device_fix(name: &str) -> String {
    format!(
        "macOS lists \"{name}\" as two devices with the same name (an input-only and an \
         output-only half), and original sox opens whichever comes first. Easiest fix: replace it \
         with sox_ng, which skips the half that can't go this direction (`brew uninstall sox && \
         brew install sox_ng`; same `sox` command, same config). Or combine both halves into one \
         aggregate device and use its name instead: `audictl list` shows the two IDs, then \
         `audictl aggregate create --name \"DialF Codec\" --devices \"<input id>,<output id>\"` \
         (or Audio MIDI Setup → + → Create Aggregate Device). Don't begin the new name with \
         \"{name}\" — {PREFIX_NOTE}"
    )
}

/// The fix for separate devices that share a name: an aggregate holding only the intended one.
pub fn duplicate_device_fix(name: &str) -> String {
    format!(
        "more than one device is named \"{name}\", and sox opens the first one it finds — \
         possibly not the one you mean. Make an aggregate device containing only the one you \
         want and use its name: `audictl list` shows each one's ID, then `audictl aggregate \
         create --name \"DialF Card\" --devices \"<id>\"` (or Audio MIDI Setup). Don't begin \
         the new name with \"{name}\" — {PREFIX_NOTE}"
    )
}

/// The device name in a sox "can't open input|output `<name>': …" message.
pub fn sox_device_name(error: &str) -> Option<&str> {
    let start = error.find('`')? + 1;
    let len = error[start..].find('\'')?;
    Some(&error[start..start + len])
}

/// The fix for an audio tool failure whose own message pins down the cause; `None` when it
/// doesn't, rather than a guess.
pub fn device_fix(error: &str) -> Option<String> {
    let e = error.to_ascii_lowercase();
    // Before the "not found" check: the device DOES exist — sox opened the half of it that
    // can't go this direction.
    if e.contains("can not get audio device properties") {
        return Some(split_device_fix(sox_device_name(error).unwrap_or("<device>")));
    }
    if e.contains("too few input channels") {
        return Some(
            "capture_cmd selects a channel (`remix N`) that sox did not open — sox opens a card \
             with 2 channels unless told otherwise, so add `-c <channels>` before the device, \
             matching what the card offers (Linux: `grep Channels /proc/asound/card*/stream0`)"
                .into(),
        );
    }
    if e.contains("busy") {
        return Some(
            "another program holds the sound card — on Linux usually PipeWire; release the card \
             for direct access, e.g. `audictl card hide <card>`"
                .into(),
        );
    }
    if e.contains("can not open audio device")
        || e.contains("no such file or directory")
        || e.contains("cannot find card")
        || e.contains("no such device")
    {
        return Some(
            "the device named in capture_cmd/playback_cmd does not exist on this machine — check \
             its name (Linux: `cat /proc/asound/cards`; `dialf import` offers the devices it \
             finds)"
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_clashes_are_told_apart() {
        let codec = "USB AUDIO  CODEC";
        let halves = [(codec, false, true), (codec, true, false), ("MiniFuse 2", true, true)];
        assert_eq!(name_clash(codec, &halves), NameClash::Halves);
        assert_eq!(name_clash("MiniFuse 2", &halves), NameClash::None);
        assert_eq!(name_clash("absent", &halves), NameClash::None);
        // Two identical full-duplex cards: the "combine the halves" advice would be wrong.
        let twins = [("MiniFuse 2", true, true), ("MiniFuse 2", true, true)];
        assert_eq!(name_clash("MiniFuse 2", &twins), NameClash::Duplicates);
        // Two identical output-only devices: same story.
        let outs = [("Speaker", false, true), ("Speaker", false, true)];
        assert_eq!(name_clash("Speaker", &outs), NameClash::Duplicates);
    }

    #[test]
    fn the_two_fixes_say_different_things() {
        let split = split_device_fix("X");
        assert!(split.contains("input-only and an output-only half") && split.contains("<input id>,<output id>"));
        assert!(split.contains("brew install sox_ng"), "the simplest fix comes first: {split}");
        assert!(split.find("sox_ng").unwrap() < split.find("aggregate").unwrap(), "{split}");
        let dup = duplicate_device_fix("X");
        assert!(dup.contains("only the one you want") && dup.contains("\"<id>\""));
        assert!(!dup.contains("half"), "twins have no halves: {dup}");
        for fix in [split, dup] {
            assert!(fix.contains("Don't begin the new name with \"X\""), "{fix}");
        }
    }

    /// Verbatim from Homebrew sox on macOS for a device that doesn't exist — note "output
    /// file" and the double space after "input". Both must read as "does not exist".
    #[test]
    fn real_sox_not_found_messages() {
        for err in [
            "/opt/homebrew/bin/sox FAIL formats: can't open output file `NoSuchDevice-xyz': can not open audio device",
            "/opt/homebrew/bin/sox FAIL formats: can't open input  `NoSuchDevice-xyz': can not open audio device",
        ] {
            assert!(device_fix(err).unwrap().contains("does not exist"), "{err}");
            assert_eq!(sox_device_name(err), Some("NoSuchDevice-xyz"));
        }
    }

    /// sox says "can't open output" for playback; the device name must come out of it too.
    #[test]
    fn device_name_comes_out_of_input_and_output_messages() {
        assert_eq!(sox_device_name("can't open input `USB AUDIO  CODEC': x"), Some("USB AUDIO  CODEC"));
        assert_eq!(sox_device_name("can't open output `Speakers': x"), Some("Speakers"));
        assert_eq!(sox_device_name("no quotes here"), None);
    }
#[test]
    fn a_split_card_is_not_reported_as_missing() {
        let err = "/usr/local/bin/sox FAIL formats: can't open input `USB AUDIO  CODEC': can not get audio device properties";
        let fix = device_fix(err).unwrap();
        assert!(fix.contains("aggregate"), "{fix}");
        assert!(fix.contains("\"USB AUDIO  CODEC\""), "names the device, double space intact: {fix}");
        assert!(!fix.contains("does not exist"), "{fix}");
        assert_eq!(sox_device_name(err), Some("USB AUDIO  CODEC"));
    }

#[test]
    fn a_missing_device_is_still_reported_as_missing() {
        // sox coreaudio, no device by that name at all
        let mac = "sox FAIL formats: can't open input `Nope': can not open audio device";
        assert!(device_fix(mac).unwrap().contains("does not exist"));
        // sox alsa, no such card
        let linux = "sox FAIL formats: can't open input `plughw:9,0': audio open error: No such file or directory";
        assert!(device_fix(linux).unwrap().contains("does not exist"));
    }

#[test]
    fn known_tool_errors_get_their_fix_and_unknown_ones_none() {
        assert!(device_fix("arecord: Device or resource busy").unwrap().contains("audictl card hide"));
        // sox always says why after the device; a bare "can't open input" is not a diagnosis.
        assert_eq!(device_fix("sox FAIL formats: can't open input `plughw:9,0'"), None);
        assert_eq!(device_fix("something new"), None);
    }
}
