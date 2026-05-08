//! Source of the currently-active mint keyset id.
//!
//! Indirection so the CLI can pass a fixed id today while the future cdk
//! integration drops in a `MintKeysetSource` without touching the manifest
//! module's API.

pub trait KeysetSource {
    fn current_active(&self) -> anyhow::Result<String>;
}

#[derive(Clone, Debug)]
pub struct FixedKeysetSource(pub String);

impl FixedKeysetSource {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl KeysetSource for FixedKeysetSource {
    fn current_active(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_source_returns_value() {
        let s = FixedKeysetSource::new("00deadbeef");
        assert_eq!(s.current_active().unwrap(), "00deadbeef");
    }
}
