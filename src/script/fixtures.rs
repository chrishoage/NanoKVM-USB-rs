//! Packet-fixture loading for dry runs and compiler tests.
//!
//! The committed TOML file supplies expected bytes independently of the encoders.

use serde::Deserialize;

use crate::proto::cmd;

/// Every `[[packet]]` in the authority file. Other tables (`[meta]`, `[[response]]`,
/// `[[disagreement]]`) and every descriptive field are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct Fixtures {
    #[serde(default)]
    packet: Vec<Fixture>,
}

/// One `[[packet]]`: the name to print, the command and payload it describes, and the frame bytes
/// as hex.
#[derive(Debug, Clone, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub cmd: u8,
    pub data: Vec<u8>,
    /// Hex bytes, possibly followed by prose (the file's format allows it).
    pub frame: String,
}

/// Why the authority file could not be read. Ordinary at run time; fatal in a test.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("read {path}: {source}")]
    Read {
        path: &'static str,
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: &'static str,
        source: toml::de::Error,
    },
}

impl Fixtures {
    /// Where the authority lives, resolved against the source tree at compile time.
    pub const PATH: &'static str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/packets/ch9329.toml");

    pub fn load() -> Result<Fixtures, FixtureError> {
        let text = std::fs::read_to_string(Self::PATH).map_err(|source| FixtureError::Read {
            path: Self::PATH,
            source,
        })?;
        Self::parse(&text)
    }

    /// [`Fixtures::load`] without the file, so a test can check what a *doctored* authority does
    /// without retyping any bytes of the real one.
    pub fn parse(text: &str) -> Result<Fixtures, FixtureError> {
        toml::from_str(text).map_err(|source| FixtureError::Parse {
            path: Self::PATH,
            source,
        })
    }

    /// Every packet in the file, in file order.
    pub fn packets(&self) -> &[Fixture] {
        &self.packet
    }

    /// The fixture whose bytes are this keyboard payload, if the file carries one.
    pub fn keyboard(&self, payload: &[u8]) -> Option<&Fixture> {
        self.packet
            .iter()
            .find(|p| p.cmd == cmd::SEND_KB_GENERAL_DATA && p.data == payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::report::KeyboardReport;

    #[test]
    fn the_authority_file_loads_and_carries_keyboard_packets() {
        let fixtures = Fixtures::load().expect("fixtures/packets/ch9329.toml");
        let keyboard: Vec<&str> = fixtures
            .packets()
            .iter()
            .filter(|p| p.cmd == cmd::SEND_KB_GENERAL_DATA)
            .map(|p| &*p.name)
            .collect();
        assert!(
            keyboard.contains(&"kb_release_all"),
            "the release-all packet is the one every script ends with: {keyboard:?}"
        );
        assert!(keyboard.len() >= 3, "{keyboard:?}");
    }

    #[test]
    fn a_payload_is_looked_up_by_its_bytes() {
        let fixtures = Fixtures::load().expect("fixtures/packets/ch9329.toml");
        let found = fixtures
            .keyboard(&KeyboardReport::RELEASE_ALL.payload())
            .expect("the release-all payload is in the authority");
        assert_eq!(found.name, "kb_release_all");
        // A payload no fixture describes is simply unnamed, not an error.
        assert!(fixtures.keyboard(&[0xAB; 8]).is_none());
    }

    #[test]
    fn a_missing_file_is_an_ordinary_error_that_names_the_path() {
        let err = Fixtures::parse("packet = 3").unwrap_err();
        assert!(err.to_string().contains("ch9329.toml"), "{err}");
    }
}
