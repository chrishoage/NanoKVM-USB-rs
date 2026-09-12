//! Structural checks that audio cannot control input, capture, or rendering.

use std::path::Path;

/// Read every file in `dir`, returning (path, contents) and asserting nothing was skipped.
fn sources(dir: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut entries = 0usize;
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{dir}: {e}")) {
        let path = entry.expect("dir entry").path();
        entries += 1;
        out.push((
            path.display().to_string(),
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
        ));
    }
    assert_eq!(out.len(), entries, "a file was not read");
    out
}

#[test]
fn no_file_under_src_audio_reaches_video_input_or_the_viewer() {
    let files = sources(concat!(env!("CARGO_MANIFEST_DIR"), "/src/audio"));
    assert!(
        files.len() >= 5,
        "only {} files were searched; src/audio has mod, ring, pcm, alsa and tone",
        files.len()
    );
    for name in ["mod.rs", "ring.rs", "pcm.rs", "alsa.rs", "tone.rs"] {
        assert!(
            files.iter().any(|(path, _)| path.ends_with(name)),
            "{name} was not among the files searched"
        );
    }

    // The module paths, not the bare words: `capture` and `input` are ordinary English and appear
    // in the prose of this module constantly ("the capture thread", "the audio input"). What must
    // not appear is a *use* of another subsystem. Split so this file's own text cannot satisfy the
    // search it performs.
    let forbidden = [
        concat!("crate::", "capture"),
        concat!("crate::", "input"),
        concat!("crate::", "viewer"),
        concat!("crate::", "serial"),
        concat!("crate::", "proto"),
        concat!("crate::", "link"),
        concat!("crate::", "script"),
        concat!("crate::", "cli"),
        concat!("nanokvm::", "capture"),
        concat!("nanokvm::", "input"),
        concat!("nanokvm::", "viewer"),
        "PipelineHandle",
        "Producer",
        "FrameSource",
    ];
    for (path, source) in &files {
        // Doc links are still references: `[crate::capture::Pipeline]` in a comment is a sign
        // somebody was thinking about reaching for it, and the rule is cheap enough to keep
        // absolute. The one exception is the *discovery* side, which audio does not import
        // either — the opener that knows about `discovery` lives in `discovery::reopen`, above
        // this module, which is what keeps the dependency pointing the right way.
        for needle in forbidden {
            assert_eq!(
                source.matches(needle).count(),
                0,
                "{needle} must not appear in {path}: audio is a side channel ( rev 5)"
            );
        }
    }
}

/// The other direction of the same rule, and the one that decides whether a stalled card
/// can hurt anything: `src/viewer/app.rs` may *read* the audio handle for the title and may do
/// nothing else with it. No `stop`, no waiting, no branching the loop on it.
#[test]
fn the_event_loop_only_reads_audio_for_the_title() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/viewer/app.rs");
    let source = std::fs::read_to_string(Path::new(path)).expect("app.rs must be readable");

    // The only audio API the event loop is allowed to call. `snapshot`, `muted` and `config` are
    // all reads; anything that changes state, waits, or tears down is not the loop's to call.
    for forbidden in [
        concat!("audio", ".stop"),
        concat!("audio", ".set_muted"),
        concat!("AudioHandle::", "spawn"),
    ] {
        assert_eq!(
            source.matches(forbidden).count(),
            0,
            "{forbidden} must not appear in app.rs: the event loop reads audio and drives nothing"
        );
    }
}

/// `--no-audio` must mean *nothing is opened*, and the only way that is true is if nothing is
/// spawned. `main.rs` has one `AudioHandle::spawn`, and it sits in an arm of the match on
/// the flag — so the flag cannot degrade into "spawned but muted", which would still open the
/// card. The other `None` arm is a video node discovery never enumerated: no USB device, so no
/// card can belong to it and there is nothing for a retry to find.
#[test]
fn no_audio_skips_the_spawn_rather_than_muting_it() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs");
    let source = std::fs::read_to_string(Path::new(path)).expect("main.rs must be readable");
    assert_eq!(
        source.matches(concat!("AudioHandle::", "spawn")).count(),
        1,
        "there is exactly one place audio is started"
    );
    let flag = source
        .find("args.no_audio,")
        .expect("main.rs must branch on --no-audio");
    let spawn = source
        .find(concat!("AudioHandle::", "spawn"))
        .expect("checked above");
    assert!(
        flag < spawn,
        "the spawn must sit inside the --no-audio branch, not before it"
    );
    // And the `None` arm must come first, so the flag's meaning is "do not start it" rather than
    // "start it and turn it off".
    let none_arm = source[flag..spawn].find("        None\n");
    assert!(
        none_arm.is_some(),
        "--no-audio must select a `None` handle, so no thread and no open happen at all"
    );
}
