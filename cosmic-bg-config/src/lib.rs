// SPDX-License-Identifier: MPL-2.0-only

use cosmic_config::{Config, ConfigGet, ConfigSet};
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::PathBuf,
};

pub const NAME: &str = "com.system76.CosmicBackground";
pub const BG_KEY: &str = "backgrounds";

/// Fallback in case config and default schema can't be loaded
pub fn fallback() -> CosmicBgEntry {
    CosmicBgEntry {
        output: CosmicBgOutput::All,
        source: PathBuf::from("/usr/share/backgrounds/pop/"),
        filter_by_theme: true,
        rotation_frequency: 3600,
        filter_method: FilterMethod::default(),
        scaling_mode: ScalingMode::default(),
        sampling_method: SamplingMethod::default(),
    }
}

/// Configuration for the panel's ouput
#[derive(Debug, Deserialize, Serialize, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
#[must_use]
pub enum CosmicBgOutput {
    /// show panel on a specific output
    Name(String),
    /// show panel on all outputs
    All,
}

impl ToString for CosmicBgOutput {
    fn to_string(&self) -> String {
        match self {
            CosmicBgOutput::All => "all".into(),
            CosmicBgOutput::Name(name) => format!("output.{}", name.clone()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Setters)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CosmicBgEntry {
    /// the configured output
    #[setters(skip)]
    pub output: CosmicBgOutput,
    /// the configured image source
    #[setters(skip)]
    pub source: PathBuf,
    /// whether the images should be filtered by the active theme
    pub filter_by_theme: bool,
    /// frequency at which the wallpaper is rotated in seconds
    pub rotation_frequency: u64,
    /// filter used to scale images
    #[serde(default)]
    pub filter_method: FilterMethod,
    /// mode used to scale images,
    #[serde(default)]
    pub scaling_mode: ScalingMode,
    #[serde(default)]
    pub sampling_method: SamplingMethod,
}

impl CosmicBgEntry {
    /// Define a preferred background for a given output device.
    pub fn new(output: CosmicBgOutput, source: PathBuf) -> Self {
        CosmicBgEntry {
            output,
            source,
            filter_by_theme: false,
            rotation_frequency: 900,
            filter_method: FilterMethod::default(),
            scaling_mode: ScalingMode::default(),
            sampling_method: SamplingMethod::default(),
        }
    }
}

/// Image filtering method
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
pub enum FilterMethod {
    // nearest neighbor filtering
    Nearest,
    // linear filtering
    Linear,
    // lanczos filtering with window 3
    #[default]
    Lanczos,
}

impl From<FilterMethod> for image::imageops::FilterType {
    fn from(method: FilterMethod) -> Self {
        match method {
            FilterMethod::Nearest => image::imageops::FilterType::Nearest,
            FilterMethod::Linear => image::imageops::FilterType::Triangle,
            FilterMethod::Lanczos => image::imageops::FilterType::Lanczos3,
        }
    }
}

/// Image filtering method
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
pub enum SamplingMethod {
    // Rotate through images in Aplhanumeeric order
    #[default]
    Alphanumeric,
    // Rotate through images in Random order
    Random,
    // TODO GnomeWallpapers
}

/// Image scaling mode
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
pub enum ScalingMode {
    // Fit the image and fill the rest of the area with the given RGB color
    Fit([f32; 3]),
    /// Stretch the image ignoring any aspect ratio to fit the area
    Stretch,
    /// Zoom the image so that it fill the whole area
    #[default]
    Zoom,
}

impl CosmicBgEntry {
    #[must_use]
    pub fn key(&self) -> String {
        self.output.to_string()
    }
}

#[must_use]
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CosmicBgConfig {
    pub outputs: HashSet<CosmicBgOutput>,
    pub backgrounds: Vec<CosmicBgEntry>,
}

impl CosmicBgConfig {
    /// Creates a config with fallback defaults.
    pub fn fallback() -> Self {
        Self {
            outputs: HashSet::new(),
            backgrounds: vec![fallback()],
        }
    }

    /// Convenience function for cosmic-config
    ///
    /// # Errors
    ///
    /// Fails if cosmic-config paths are missing or cannot be created.
    pub fn helper() -> Result<Config, cosmic_config::Error> {
        Config::new(NAME, 1)
    }

    /// Load config with the provided name from cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if invalid iter are stored within cosmic-config at time of parsing them.
    pub fn load(context: &Config) -> Result<Self, cosmic_config::Error> {
        let mut config = Self::default();

        let entries = Self::load_outputs(context)?
            .into_iter()
            .filter_map(|output| Self::load_entry(context, &output.to_string()).ok());

        for entry in entries {
            config.outputs.insert(entry.output.clone());
            config.backgrounds.push(entry);
        }

        // add the default all wallpaper if all is not already present
        if config.backgrounds.is_empty()
            || config
                .backgrounds
                .iter()
                .all(|e| e.output != CosmicBgOutput::All)
        {
            eprintln!("No wallpapers configured. Using defaults.");
            config.backgrounds.push(fallback());
        }

        Ok(config)
    }

    /// Get the entry for a given output.
    #[must_use]
    pub fn entry(&self, output: &CosmicBgOutput) -> Option<&CosmicBgEntry> {
        self.backgrounds
            .iter()
            .find(|entry| &entry.output == output)
    }

    /// get a mutable entry for a given output.
    #[must_use]
    pub fn entry_mut(&mut self, output: &CosmicBgOutput) -> Option<&mut CosmicBgEntry> {
        self.backgrounds
            .iter_mut()
            .find(|entry| &entry.output == output)
    }

    /// Get the entry for an output from cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if the config is missing or fails to parse.
    pub fn load_entry(
        config: &Config,
        output: &str,
    ) -> Result<CosmicBgEntry, cosmic_config::Error> {
        config.get::<CosmicBgEntry>(output)
    }

    /// Applies the entry for the given output to cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if the config could not be set in cosmic-config.
    pub fn set_entry(
        &mut self,
        config: &Config,
        entry: CosmicBgEntry,
    ) -> Result<(), cosmic_config::Error> {
        config.set(&entry.output.to_string(), entry.clone())?;

        if let Some(old) = self.entry_mut(&entry.output) {
            *old = entry;
        } else {
            self.outputs.insert(entry.output.clone());
            self.backgrounds.push(entry);
        }

        if let Err(why) = config.set(BG_KEY, self.outputs.iter().collect::<Vec<_>>()) {
            eprintln!("failed to update outputs: {why:?}");
        }

        Ok(())
    }

    /// Get all stored outputs from cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if the config is missing or fails to parse.
    pub fn load_outputs(config: &Config) -> Result<Vec<CosmicBgOutput>, cosmic_config::Error> {
        config.get::<Vec<CosmicBgOutput>>(BG_KEY)
    }
}
