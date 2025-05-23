use anyhow::{Context, Result};
use ini::{Ini, Properties};

use crate::r#ref::Ref;

// don't store indexes: scanning for the correct parts is fast enough...
#[derive(Debug)]
pub(crate) struct Manifest {
    ini: Ini,
}

impl Manifest {
    pub fn new(s: impl AsRef<str>) -> Result<Self> {
        let ini = Ini::load_from_str(s.as_ref()).context("Failed to parse flatpak manifest")?;
        Ok(Self { ini })
    }

    fn section(&self, name: &str) -> Result<&Properties> {
        self.ini
            .section(Some(name))
            .with_context(|| format!("Manifest is missing section [{name}]"))
    }

    pub(crate) fn get(&self, section: &str, key: &str) -> Result<&str> {
        self.section(section)?
            .get(key)
            .with_context(|| format!("Section [{section}] is missing {key}="))
    }

    #[allow(dead_code)]
    pub(crate) fn get_opt(&self, section: &str, key: &str) -> Option<&str> {
        self.ini.section(Some(section))?.get(key)
    }

    pub(crate) fn get_runtime(&self) -> Result<Ref> {
        Ref::new_runtime(self.get("Application", "runtime")?)
    }

    pub(crate) fn get_environment(&self) -> Result<impl IntoIterator<Item = (&str, &str)>> {
        self.section("Environment")
    }
}
