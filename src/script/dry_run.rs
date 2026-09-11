//! Rendering a compiled script as the frames it *would* send.
//!
//! A dry run opens nothing and sends nothing, so this returns a `String` and prints none of it —
//! the caller decides where it goes, and the renderer stays pure like the rest of `script`.
//!
//! Where the authority file has a packet with these exact bytes the row is named after it, and
//! where the encoder and the authority disagree the row says `DISAGREES` and prints both (§9.1).
//! A name that is not the truth would be worse than no name at all.

use std::fmt::Write as _;

use crate::proto::frame::EncodeError;
use crate::proto::{cmd, encode};
use crate::script::fixtures::Fixtures;
use crate::script::{Script, Step};

/// The frames `script` would send, one line per step, ending in a newline.
///
/// `fixtures` is `Option` because a shipped binary has no source tree to read the authority from
/// (`Fixtures::load` failing is ordinary); without it every row's name column is `-`.
pub fn render(script: &Script, fixtures: Option<&Fixtures>) -> Result<String, EncodeError> {
    let mut out = String::new();
    let reports = script.report_count();
    let _ = writeln!(
        out,
        "dry run: {reports} reports, nothing sent, no device opened"
    );
    for (i, step) in script.steps.iter().enumerate() {
        let n = i + 1;
        match step {
            Step::Wait(d) => {
                let _ = writeln!(
                    out,
                    "{:>4}  {:<22}  --",
                    n,
                    format!("wait {}ms", d.as_millis())
                );
            }
            Step::Send(send) => {
                let payload = send.report.payload();
                let frame = hex(&encode(cmd::SEND_KB_GENERAL_DATA, &payload)?);
                let name = match fixtures.and_then(|f| f.keyboard(&payload)) {
                    None => "-".to_string(),
                    Some(f) if f.frame.eq_ignore_ascii_case(&frame) => f.name.clone(),
                    Some(f) => format!("{} DISAGREES: fixture says {}", f.name, f.frame),
                };
                let _ = writeln!(out, "{:>4}  {:<22}  {frame}   {name}", n, send.label);
            }
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::compile::{compile_key, compile_macro, CapsLock};
    use crate::script::layout::Layout;

    fn script() -> Script {
        compile_macro("key shift+a\nwait 250\n", None, CapsLock::Off).expect("compiles")
    }

    #[test]
    fn every_step_gets_a_line_and_a_wait_sends_nothing() {
        let text = render(&script(), None).expect("renders");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1 + 3, "a header and one line per step: {text}");
        assert!(lines[0].contains("2 reports"), "{text}");
        assert!(
            lines[0].contains("nothing sent, no device opened"),
            "{text}"
        );
        assert!(lines[1].contains("key shift+a"), "{text}");
        assert!(lines[2].contains("release"), "{text}");
        assert!(lines[3].contains("wait 250ms"), "{text}");
        assert!(lines[3].ends_with("--"), "a wait has no frame: {text}");
        assert!(text.ends_with('\n'));
    }

    /// Without the authority file every frame is still printed; only the name is unknown.
    #[test]
    fn without_fixtures_the_name_column_is_a_dash() {
        let text = render(&script(), None).expect("renders");
        for line in text.lines().skip(1).take(2) {
            assert!(line.ends_with(" -"), "{line}");
        }
    }

    #[test]
    fn a_frame_the_authority_describes_is_named_after_it() {
        let fixtures = Fixtures::load().expect("fixtures/packets/ch9329.toml");
        let text = render(&script(), Some(&fixtures)).expect("renders");
        assert!(text.contains("kb_shift_a_press"), "{text}");
        assert!(text.contains("kb_release_all"), "{text}");
        assert!(!text.contains("DISAGREES"), "{text}");
    }

    /// §9.1: the authority wins. If the encoder and the file disagree, the row says so and prints
    /// what the file has, instead of a name that is not the truth.
    ///
    /// The doctored file is derived from the real one — its own frame string, with its last byte
    /// changed — so no bytes are retyped here.
    #[test]
    fn a_doctored_fixture_is_reported_as_a_disagreement() {
        let real = std::fs::read_to_string(Fixtures::PATH).expect("the authority file");
        let genuine = Fixtures::load().expect("loads");
        let release = genuine
            .keyboard(&crate::proto::report::KeyboardReport::RELEASE_ALL.payload())
            .expect("the release-all fixture");
        let (head, last) = release.frame.rsplit_once(' ').expect("more than one byte");
        let flipped = format!(
            "{head} {:02X}",
            u8::from_str_radix(last, 16).expect("hex") ^ 0xFF
        );
        let doctored = Fixtures::parse(&real.replace(&release.frame, &flipped)).expect("parses");

        let text = render(&script(), Some(&doctored)).expect("renders");
        assert!(text.contains("kb_release_all DISAGREES"), "{text}");
        assert!(
            text.contains(&flipped),
            "the file's bytes are shown: {text}"
        );
    }

    #[test]
    fn a_modifier_only_chord_renders_its_press_and_its_release() {
        let script = compile_key(&["super".to_string()], Layout::Us).expect("compiles");
        let text = render(&script, None).expect("renders");
        assert!(text.contains("key super"), "{text}");
        assert_eq!(text.lines().count(), 3, "{text}");
    }
}
