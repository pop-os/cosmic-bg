use cosmic_config::{cosmic_config_derive::CosmicConfigEntry, Config, CosmicConfigEntry};
use derive_setters::Setters;
use serde::{Deserialize, Serialize};

use crate::{Source, NAME};

#[derive(Default, Debug, Deserialize, Serialize, Clone, PartialEq, Setters, CosmicConfigEntry)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct State {
    /// The active wallpaper for each output
    /// (output_name, source of wallpaper)
    pub wallpapers: Vec<(String, Source)>,
}

impl State {
    pub fn version() -> u64 {
        1
    }

    pub fn state() -> Result<Config, cosmic_config::Error> {
        Config::new_state(NAME, Self::version())
    }
}
