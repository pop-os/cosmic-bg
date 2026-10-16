// SPDX-License-Identifier: MPL-2.0

pub mod state;

use cosmic_config::{Config as CosmicConfig, ConfigGet, ConfigSet};
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;

pub const NAME: &str = "com.system76.CosmicBackground";
pub const BACKGROUNDS: &str = "backgrounds";
pub const DEFAULT_BACKGROUND: &str = "all";
pub const SAME_ON_ALL: &str = "same-on-all";
pub const VERSION: u64 = 1;

/// XXX must be kept in step with `cosmic_theme::ThemeMode`
pub const MODE_NAME: &str = "com.system76.CosmicTheme.Mode";
pub const MODE_VERSION: u64 = 1;
pub const MODE_KEY: &str = "is_dark";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];

    #[must_use]
    pub const fn from_is_dark(is_dark: bool) -> Self {
        if is_dark { Self::Dark } else { Self::Light }
    }

    #[must_use]
    pub const fn config_name(self) -> &'static str {
        match self {
            Self::Dark => "com.system76.CosmicBackground.Dark",
            Self::Light => "com.system76.CosmicBackground.Light",
        }
    }

    /// Create a context to this mode's backgrounds.
    pub fn context(self) -> Result<Context, cosmic_config::Error> {
        CosmicConfig::new(self.config_name(), VERSION).map(Context)
    }

    /// The config that says whether dark or light is active.
    pub fn config() -> Result<CosmicConfig, cosmic_config::Error> {
        CosmicConfig::new(MODE_NAME, MODE_VERSION)
    }

    #[must_use]
    pub fn current() -> Self {
        Self::config()
            .and_then(|config| config.get::<bool>(MODE_KEY))
            .map_or(Self::Dark, Self::from_is_dark)
    }
}

/// Create a context to the backgrounds of the current mode.
///
/// # Errors
///
/// Fails if cosmic-config paths are missing or cannot be created.
pub fn context() -> Result<Context, cosmic_config::Error> {
    Mode::current().context()
}

/// Create a context to the backgrounds of a given mode.
pub fn context_for(mode: Mode) -> Result<Context, cosmic_config::Error> {
    mode.context()
}

/// The old background config.
pub fn shared() -> Result<CosmicConfig, cosmic_config::Error> {
    CosmicConfig::new(NAME, VERSION)
}

#[derive(Clone, Debug)]
pub struct Context(pub CosmicConfig);

impl Context {
    /// Get all stored backgrounds from cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if the config is missing or fails to parse.
    pub fn backgrounds(&self) -> Vec<String> {
        match self.0.get::<Vec<String>>(BACKGROUNDS) {
            Ok(value) => value,
            Err(why) => {
                tracing::error!(?why, "error reading background config");
                Vec::new()
            }
        }
    }

    pub fn default_background(&self) -> Entry {
        self.entry("all").unwrap_or_else(|_| Entry::fallback())
    }

    /// Get the entry for an output from cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if the config is missing or fails to parse.
    pub fn entry(&self, output: &str) -> Result<Entry, cosmic_config::Error> {
        self.0.get::<Entry>(output)
    }

    #[must_use]
    pub fn same_on_all(&self) -> bool {
        if let Ok(value) = self.0.get::<bool>(SAME_ON_ALL) {
            return value;
        }

        let _res = self.0.set(SAME_ON_ALL, true);

        true
    }

    pub fn set_same_on_all(&self, value: bool) -> Result<(), cosmic_config::Error> {
        if self.same_on_all() != value {
            return self.0.set(SAME_ON_ALL, value);
        }

        Ok(())
    }

    /// Seed this mode's backgrounds from the config both modes took over from,
    /// when it holds none of its own. Returns whether anything was written.
    pub fn seed_from(&self, shared: &CosmicConfig) -> Result<bool, cosmic_config::Error> {
        if self.0.get::<Entry>(DEFAULT_BACKGROUND).is_ok() {
            return Ok(false);
        }

        let mut seeded = false;

        if let Ok(entry) = shared.get::<Entry>(DEFAULT_BACKGROUND) {
            self.0.set(DEFAULT_BACKGROUND, entry)?;
            seeded = true;
        }

        let backgrounds = shared.get::<Vec<String>>(BACKGROUNDS).unwrap_or_default();
        for output in &backgrounds {
            let key = ["output.", output].concat();
            if let Ok(entry) = shared.get::<Entry>(&key) {
                self.0.set(&key, entry)?;
                seeded = true;
            }
        }
        if !backgrounds.is_empty() {
            self.0.set(BACKGROUNDS, backgrounds)?;
            seeded = true;
        }

        if let Ok(same_on_all) = shared.get::<bool>(SAME_ON_ALL) {
            self.0.set(SAME_ON_ALL, same_on_all)?;
            seeded = true;
        }

        Ok(seeded)
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Setters)]
#[must_use]
pub struct Entry {
    /// the configured output
    #[setters(skip)]
    pub output: String,
    /// the configured image source
    #[setters(skip)]
    pub source: Source,
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

/// A background image which is colored.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, PartialOrd)]
pub enum Color {
    Single([f32; 3]),
    Gradient(Gradient),
}

/// A background image which is colored by a gradient.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, PartialOrd)]
pub struct Gradient {
    pub colors: Cow<'static, [[f32; 3]]>,
    pub radius: f32,
}

/// The source of a background image.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub enum Source {
    /// Background image(s) from a path.
    Path(PathBuf),
    /// A background color or gradient.
    Color(Color),
}

impl Entry {
    /// Define a preferred background for a given output device.
    pub fn new(output: String, source: Source) -> Self {
        Self {
            output,
            source,
            rotation_frequency: 900,
            filter_method: FilterMethod::default(),
            scaling_mode: ScalingMode::default(),
            sampling_method: SamplingMethod::default(),
        }
    }

    /// Fallback in case config and default schema can't be loaded
    pub fn fallback() -> Self {
        Self {
            output: String::from("all"),
            source: Source::Path(PathBuf::from(
                "/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg",
            )),
            rotation_frequency: 3600,
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

impl Entry {
    #[must_use]
    pub fn key(&self) -> String {
        self.output.to_string()
    }
}

#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub same_on_all: bool,
    pub outputs: HashSet<String>,
    pub backgrounds: Vec<Entry>,
    pub default_background: Entry,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            same_on_all: true,
            outputs: HashSet::new(),
            backgrounds: Vec::new(),
            default_background: Entry::fallback(),
        }
    }
}

impl Config {
    /// Load config with the provided name from cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if invalid iter are stored within cosmic-config at time of parsing them.
    pub fn load(context: &Context) -> Result<Self, cosmic_config::Error> {
        let mut config = Self {
            same_on_all: context.same_on_all(),
            ..Default::default()
        };

        config.default_background = context.default_background();

        if !config.same_on_all {
            config.load_backgrounds(context);
        }

        Ok(config)
    }

    pub fn load_backgrounds(&mut self, context: &Context) {
        self.backgrounds.clear();
        self.outputs.clear();

        let entries = context
            .backgrounds()
            .into_iter()
            .filter_map(|output| context.entry(&["output.", &output].concat()).ok());

        for entry in entries {
            self.outputs.insert(entry.output.clone());
            self.backgrounds.push(entry);
        }

        self.default_background = context.default_background();
    }

    /// Get the entry for a given output.
    #[must_use]
    pub fn entry(&self, output: &str) -> Option<&Entry> {
        self.backgrounds.iter().find(|entry| entry.output == output)
    }

    /// get a mutable entry for a given output.
    #[must_use]
    pub fn entry_mut(&mut self, output: &str) -> Option<&mut Entry> {
        self.backgrounds
            .iter_mut()
            .find(|entry| entry.output == output)
    }

    /// Applies the entry for the given output to cosmic-config.
    ///
    /// # Errors
    ///
    /// Fails if the config could not be set in cosmic-config.
    pub fn set_entry(
        &mut self,
        context: &Context,
        entry: Entry,
    ) -> Result<(), cosmic_config::Error> {
        let output_key = if entry.output == "all" {
            entry.output.clone()
        } else {
            self.outputs.insert(entry.output.clone());
            ["output.", &entry.output].concat()
        };

        if context.0.get(&output_key).ok().as_ref() != Some(&entry) {
            context.0.set(&output_key, entry.clone())?;
        }

        if let Some(old) = self.entry_mut(&output_key) {
            *old = entry;
        } else if entry.output != "all" {
            self.backgrounds.push(entry);
        }

        let new_value = self.outputs.iter().cloned().collect::<Vec<_>>();

        if context.backgrounds() != new_value
            && let Err(why) = context.0.set::<Vec<String>>(BACKGROUNDS, new_value)
        {
            tracing::error!(?why, "failed to update outputs");
        }

        Ok(())
    }
}
