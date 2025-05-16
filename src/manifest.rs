use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer};

use crate::r#ref::Ref;

// don't store indexes: scanning for the correct parts is fast enough...
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Manifest(Box<str>);

impl std::str::FromStr for Manifest {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Box::from(s)))
    }
}

impl TryFrom<String> for Manifest {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(Manifest(value.into()))
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.try_into().map_err(serde::de::Error::custom)
    }
}

impl Manifest {
    pub(crate) fn get_runtime(&self) -> Result<Ref> {
        // lol hax
        let Some(runtime) = self
            .0
            .lines()
            .find_map(|line| line.strip_prefix("runtime="))
        else {
            bail!("Manifest is missing runtime= line?");
        };

        Ref::new_runtime(runtime)
    }
}
