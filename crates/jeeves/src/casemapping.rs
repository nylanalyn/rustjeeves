//! IRC nickname case-folding negotiated through `RPL_ISUPPORT CASEMAPPING`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaseMapping {
    Ascii,
    StrictRfc1459,
    #[default]
    Rfc1459,
}

impl CaseMapping {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ascii" => Some(Self::Ascii),
            "strict-rfc1459" | "rfc1459-strict" => Some(Self::StrictRfc1459),
            "rfc1459" => Some(Self::Rfc1459),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::StrictRfc1459 => "strict-rfc1459",
            Self::Rfc1459 => "rfc1459",
        }
    }

    pub fn fold(self, value: &str) -> String {
        value
            .chars()
            .map(|character| match character {
                'A'..='Z' => character.to_ascii_lowercase(),
                '[' if self != Self::Ascii => '{',
                ']' if self != Self::Ascii => '}',
                '\\' if self != Self::Ascii => '|',
                '^' if self == Self::Rfc1459 => '~',
                other => other,
            })
            .collect()
    }

    pub fn equivalent(self, left: &str, right: &str) -> bool {
        self.fold(left) == self.fold(right)
    }
}

#[derive(Clone, Default)]
pub struct CaseMappingRegistry {
    mappings: Arc<RwLock<HashMap<String, CaseMapping>>>,
}

impl CaseMappingRegistry {
    pub fn get(&self, server: &str) -> CaseMapping {
        self.mappings
            .read()
            .unwrap()
            .get(server)
            .copied()
            .unwrap_or_default()
    }

    pub fn set(&self, server: &str, mapping: CaseMapping) -> bool {
        self.mappings
            .write()
            .unwrap()
            .insert(server.to_string(), mapping)
            != Some(mapping)
    }

    pub fn fold(&self, server: &str, value: &str) -> String {
        self.get(server).fold(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_all_standard_irc_casemappings() {
        assert_eq!(CaseMapping::Ascii.fold("Nick[\\]^~"), "nick[\\]^~");
        assert_eq!(CaseMapping::StrictRfc1459.fold("Nick[\\]^~"), "nick{|}^~");
        assert_eq!(CaseMapping::Rfc1459.fold("Nick[\\]^~"), "nick{|}~~");
    }

    #[test]
    fn defaults_to_rfc1459_and_is_partitioned_by_network() {
        let registry = CaseMappingRegistry::default();
        assert!(registry.get("one").equivalent("User[", "user{"));
        registry.set("one", CaseMapping::Ascii);
        assert!(!registry.get("one").equivalent("User[", "user{"));
        assert!(registry.get("two").equivalent("User[", "user{"));
    }
}
