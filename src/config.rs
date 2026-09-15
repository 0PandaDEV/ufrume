use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub organization: Organization,
    pub rules: Rules,
    pub formatting: Formatting,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Organization {
    pub structure: String,
    pub compilation_structure: Option<String>,
    pub fallback_structure: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Rules {
    pub handle_missing_metadata: String,
    pub handle_duplicates: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Formatting {
    pub replace_chars: HashMap<String, String>,
    pub max_filename_length: u8,
    pub normalize_patterns: Vec<String>,
}

impl Default for Organization {
    fn default() -> Self {
        Organization {
            structure: "{artist}/{year} - {album}/{track:02} - {title}".to_string(),
            compilation_structure: Some(
                "Compilations/{album}/{track:02} - {artist} - {title}".to_string(),
            ),
            fallback_structure: "{filename}".to_string(),
        }
    }
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            handle_missing_metadata: "fallback".to_string(),
            handle_duplicates: "skip".to_string(),
        }
    }
}

impl Default for Formatting {
    fn default() -> Self {
        let mut replace_chars = HashMap::new();
        replace_chars.insert("/".to_string(), "-".to_string());
        replace_chars.insert(":".to_string(), "-".to_string());
        replace_chars.insert("?".to_string(), "".to_string());

        Formatting {
            replace_chars,
            max_filename_length: 255,
            normalize_patterns: vec![" - Single".to_string(), " - EP".to_string()],
        }
    }
}

fn get_config_path() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir().expect("Config directory could not be found");
    Ok(config_dir.join("ufrume").join("config.toml"))
}

pub fn load_or_create_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_path = get_config_path()?;

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if config_path.exists() {
        let config_str = fs::read_to_string(&config_path)?;
        let config: Config = toml::from_str(&config_str)?;
        Ok(config)
    } else {
        let default_config = Config::default();
        let config_str = toml::to_string_pretty(&default_config)?;
        fs::write(&config_path, config_str)?;

        Ok(default_config)
    }
}
