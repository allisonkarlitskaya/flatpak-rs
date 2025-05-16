// TODO: add remote: support

use std::fmt;

use anyhow::ensure;
use serde::{Deserialize, Deserializer};

// don't store indexes: scanning for the correct parts is fast enough...
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ref(Box<str>);

impl TryFrom<String> for Ref {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        ensure!(valid_ref(&value), "Not a valid ref: {value}");
        Ok(Ref(value.into()))
    }
}

impl From<Ref> for String {
    fn from(value: Ref) -> Self {
        value.0.to_string()
    }
}

impl AsRef<str> for Ref {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for Ref {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.try_into().map_err(serde::de::Error::custom)
    }
}

impl Ref {
    fn part(&self, n: usize) -> &str {
        // SAFETY: we verified that we have 4 parts on construction
        self.0.split('/').nth(n).unwrap()
    }

    pub(crate) fn new_runtime(runtime: &str) -> anyhow::Result<Self> {
        format!("runtime/{runtime}").try_into()
    }

    pub(crate) fn get_parts(&self) -> (Option<&str>, &str, &str, &str, &str) {
        let mut iter = self.0.split('/');

        // SAFETY: we checked that there are 4 items in there
        (
            None,
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
        )
    }

    pub(crate) fn get_remote(&self) -> Option<&str> {
        None
    }

    pub(crate) fn is_runtime(&self) -> bool {
        self.part(0) == "runtime"
    }

    pub(crate) fn is_app(&self) -> bool {
        self.part(0) == "app"
    }

    pub(crate) fn get_id(&self) -> &str {
        self.part(1)
    }

    pub(crate) fn get_arch(&self) -> &str {
        self.part(2)
    }

    pub(crate) fn get_branch(&self) -> &str {
        self.part(3)
    }
}

impl std::str::FromStr for Ref {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ensure!(valid_ref(s), "Not a valid ref: {s}");
        Ok(Self(Box::from(s)))
    }
}

fn valid_ref(value: &str) -> bool {
    value.split('/').count() == 4 &&
    value.split('/').all(|s| !s.is_empty()) &&
    // SAFETY: we already verified that we have a first item
    ["runtime", "app"].contains(&value.split('/').next().unwrap())
}
