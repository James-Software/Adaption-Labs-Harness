use std::env;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};

use crate::terminal::{Interrupted, Palette, TerminalInput, Tone};
use crate::{adaption::Client, terminal};

pub const DEFAULT_MODEL: &str = "qwen3.5-397b";
const MAX_SETTINGS: u64 = 16 * 1024;
const KEY_HELP: &str = "Create an API key in Adaption Settings → API keys.";

#[derive(Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct Settings {
    api_key: Option<String>,
    model: Option<String>,
    agentic_memory: Option<bool>,
}

#[derive(PartialEq, Eq)]
pub struct Connection {
    pub key: String,
    pub model: String,
    pub memory: Option<bool>,
}

pub struct Config {
    directory: PathBuf,
}

impl Config {
    pub fn locate() -> Result<Self> {
        let home = env::home_dir().context("could not locate your home directory")?;
        ensure!(
            home.is_absolute(),
            "home directory must be an absolute path"
        );
        Ok(Self {
            directory: home.join(".adaption-code"),
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.join("settings.json")
    }

    pub fn load(&self) -> Result<Settings> {
        let result = (|| {
            if !private_directory(&self.directory, false)? {
                return Ok(Settings::default());
            }
            let file = match open(
                self.path(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => File::from(fd),
                Err(rustix::io::Errno::NOENT) => return Ok(Settings::default()),
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.nlink() == 1 && private(&metadata),
                "settings file must be a private, single-link regular file owned by you"
            );
            let mut bytes = Vec::new();
            file.take(MAX_SETTINGS + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_SETTINGS,
                "settings file is too large"
            );
            // Avoid including any settings values in a parse error.
            let mut settings: Settings = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid settings JSON"))?;
            settings.api_key = settings.api_key.filter(|key| valid_key(key));
            Ok(settings)
        })();
        result.with_context(|| format!("loading {}", self.path().display()))
    }

    fn save(&self, settings: &Settings) -> Result<()> {
        let result = (|| {
            private_directory(&self.directory, true)?;
            let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
            serde_json::to_writer_pretty(&mut temp, settings)?;
            writeln!(temp)?;
            temp.as_file().sync_all()?;
            temp.persist(self.path()).map_err(|error| error.error)?;
            Ok::<_, anyhow::Error>(())
        })();
        result.with_context(|| format!("saving {}", self.path().display()))
    }
}

fn private_directory(path: &Path, create: bool) -> Result<bool> {
    if create {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && private(&metadata),
        "settings directory must be private and owned by you"
    );
    Ok(true)
}

fn private(metadata: &fs::Metadata) -> bool {
    metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o077 == 0
}

impl Settings {
    pub fn key(&self) -> Option<String> {
        env::var("ADAPTION_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .or_else(|| self.api_key.clone())
    }

    pub fn connection(
        &self,
        model: Option<&str>,
        memory: Option<bool>,
    ) -> Result<Option<Connection>> {
        let model = model.or(self.model.as_deref()).unwrap_or(DEFAULT_MODEL);
        validate_model(model)?;
        if memory == Some(true) {
            ensure!(
                model.starts_with("stack:"),
                "--memory requires --model stack:<slug>"
            );
        }
        let memory = memory.or(self.agentic_memory);
        let memory = if model.starts_with("stack:") {
            memory
        } else {
            None
        };
        Ok(self.key().map(|key| Connection {
            key,
            model: model.to_owned(),
            memory,
        }))
    }
}

fn validate_model(model: &str) -> Result<()> {
    ensure!(
        !model.is_empty()
            && model.len() <= 256
            && terminal::line(model) == model
            && !model.chars().any(char::is_whitespace)
            && model != "stack:",
        "enter a model ID or stack:<slug>"
    );
    Ok(())
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 2048 && key.bytes().all(|byte| byte.is_ascii_graphic())
}

pub fn setup_popup(
    input: &mut TerminalInput,
    output: &mut impl Write,
    palette: Palette,
) -> Result<bool> {
    writeln!(output, "\n{}", palette.paint(Tone::Info, "╭─ Adapt setup"))?;
    writeln!(
        output,
        "{} Add your Adaption API key in /settings.",
        palette.paint(Tone::Info, "│")
    )?;
    Ok(terminal::select(input, output, &["Open /settings", "Later"], 0, palette)? == Some(0))
}

pub fn edit(
    settings: &mut Settings,
    config: &Config,
    input: &mut TerminalInput,
    output: &mut impl Write,
    model_override: Option<&str>,
    memory_override: Option<bool>,
) -> Result<()> {
    let palette = Palette::stderr();
    let mut draft = settings.clone();
    let mut selected = 0;
    loop {
        let model = draft.model.as_deref().unwrap_or(DEFAULT_MODEL);
        let key_status = if env::var("ADAPTION_API_KEY").is_ok_and(|value| !value.trim().is_empty())
        {
            "using ADAPTION_API_KEY override"
        } else if draft.api_key.is_some() {
            "set (hidden)"
        } else {
            "required"
        };
        writeln!(output, "\n{}", palette.paint(Tone::Info, "╭─ /settings"))?;
        if let Some(model) = model_override {
            writeln!(output, "Session model override: {}", terminal::line(model))?;
        }
        if let Some(memory) = memory_override {
            writeln!(
                output,
                "Session memory override: {}",
                memory_label(Some(memory))
            )?;
        }
        let options = [
            format!("API key ({key_status})"),
            format!("Model or stack ({model})"),
            format!("Agentic memory ({})", memory_label(draft.agentic_memory)),
            "Save and return".to_owned(),
            "Cancel".to_owned(),
        ];
        let Some(choice) = terminal::select(input, output, &options, selected, palette)? else {
            return Ok(());
        };
        selected = choice;
        match choice {
            0 => {
                writeln!(output, "{KEY_HELP}")?;
                if let Some(key) = terminal::read_hidden_line(
                    input,
                    output,
                    &palette.paint(Tone::Action, "API key (input hidden, Enter to keep): "),
                )? {
                    let key = key.trim();
                    if valid_key(key) {
                        draft.api_key = Some(key.to_owned());
                    } else if !key.is_empty() {
                        writeln!(
                            output,
                            "{}",
                            palette.paint(
                                Tone::Action,
                                "Invalid key format. Paste the key from Adaption Settings."
                            )
                        )?;
                    }
                }
            }
            1 => {
                if let Some(model) = select_model(&draft, input, output, palette)? {
                    draft.model = Some(model);
                }
            }
            2 => {
                writeln!(
                    output,
                    "\n{}",
                    palette.paint(Tone::Info, "╭─ Agentic memory for inference stacks")
                )?;
                let values = [Some(true), Some(false), None];
                let initial = values
                    .iter()
                    .position(|&value| value == draft.agentic_memory)
                    .unwrap();
                if let Some(choice) =
                    terminal::select(input, output, &values.map(memory_label), initial, palette)?
                {
                    draft.agentic_memory = values[choice];
                }
            }
            3 => {
                if draft.key().is_none() {
                    writeln!(
                        output,
                        "{}",
                        palette.paint(Tone::Action, "Add an API key before saving.")
                    )?;
                    selected = 0;
                    continue;
                }
                if &draft != settings || !config.path().exists() {
                    config.save(&draft)?;
                    *settings = draft;
                }
                writeln!(output, "{}", palette.paint(Tone::Info, "Settings saved."))?;
                return Ok(());
            }
            4 => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn memory_label(memory: Option<bool>) -> &'static str {
    match memory {
        Some(true) => "On",
        Some(false) => "Off",
        None => "Stack default",
    }
}

fn select_model(
    settings: &Settings,
    input: &mut TerminalInput,
    output: &mut impl Write,
    palette: Palette,
) -> Result<Option<String>> {
    writeln!(
        output,
        "\n{}",
        palette.paint(Tone::Info, "╭─ Model or stack")
    )?;
    let models = if let Some(key) = settings.key() {
        writeln!(output, "Loading available models…")?;
        output.flush()?;
        match Client::new(&key).and_then(|client| client.models()) {
            Ok(models) => models,
            Err(error) if error.is::<Interrupted>() => return Err(error),
            Err(_) => {
                writeln!(
                    output,
                    "Could not load available models. Check your key or enter a model ID manually."
                )?;
                Vec::new()
            }
        }
    } else {
        writeln!(output, "Add an API key to see available models.")?;
        Vec::new()
    };
    let current = settings.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let models = available_models(models, current);
    let mut options = vec![
        format!("{current} (current)"),
        "Enter a model ID or stack manually".into(),
    ];
    options.extend(models.iter().cloned());
    match terminal::select(input, output, &options, 0, palette)? {
        None | Some(0) => Ok(None),
        Some(1) => {
            write!(
                output,
                "Adaption model ID or stack:<slug> (Enter to keep): "
            )?;
            output.flush()?;
            let value = terminal::read_interactive_line(input)?;
            let Some(model) = value
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
            else {
                return Ok(None);
            };
            if let Err(error) = validate_model(model) {
                writeln!(output, "{}", palette.paint(Tone::Action, error))?;
                return Ok(None);
            }
            Ok(Some(model.to_owned()))
        }
        Some(choice) => Ok(Some(models[choice - 2].clone())),
    }
}

fn available_models(mut models: Vec<String>, current: &str) -> Vec<String> {
    models.retain(|model| model != current && validate_model(model).is_ok());
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn saves_private_json_and_reloads_settings() {
        let home = tempfile::tempdir().unwrap();
        let config = Config {
            directory: home.path().join(".adaption-code"),
        };
        assert!(config.load().unwrap() == Settings::default());
        let first = Settings {
            api_key: Some("test-key".into()),
            model: Some("stack:coder".into()),
            agentic_memory: Some(true),
        };
        config.save(&first).unwrap();
        assert!(config.load().unwrap() == first);
        assert_eq!(
            fs::metadata(&config.directory).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::metadata(config.path()).unwrap().mode() & 0o777, 0o600);
        let updated = Settings {
            agentic_memory: Some(false),
            ..first
        };
        config.save(&updated).unwrap();
        assert!(config.load().unwrap() == updated);
    }

    #[test]
    fn model_options_exclude_the_current_model_and_invalid_ids() {
        let models = ["alpha", DEFAULT_MODEL, "invalid id", "stack:", "zeta"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(available_models(models, DEFAULT_MODEL), ["alpha", "zeta"]);
    }

    #[test]
    fn rejects_settings_with_public_modes_or_links() {
        let home = tempfile::tempdir().unwrap();
        let config = Config {
            directory: home.path().join(".adaption-code"),
        };
        config.save(&Settings::default()).unwrap();
        let path = config.path();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(config.load().is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = home.path().join("other.json");
        fs::hard_link(&path, &link).unwrap();
        assert!(config.load().is_err());
        fs::remove_file(&path).unwrap();
        symlink(&link, &path).unwrap();
        assert!(config.load().is_err());

        let old = home.path().join("old-config");
        fs::rename(&config.directory, &old).unwrap();
        symlink(&old, &config.directory).unwrap();
        assert!(config.load().is_err());
        assert!(config.save(&Settings::default()).is_err());
        fs::remove_file(&config.directory).unwrap();
        fs::rename(old, &config.directory).unwrap();
        fs::set_permissions(&config.directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(config.load().is_err());
        assert!(config.save(&Settings::default()).is_err());
    }

    #[test]
    fn malformed_settings_do_not_echo_values() {
        let home = tempfile::tempdir().unwrap();
        let config = Config {
            directory: home.path().join(".adaption-code"),
        };
        config.save(&Settings::default()).unwrap();
        fs::write(config.path(), r#"{"api_key":"fixture-key","model":false}"#).unwrap();
        let error = config.load().err().expect("invalid configuration");
        assert!(format!("{error:#}").contains("invalid settings JSON"));
        assert!(!format!("{error:#}").contains("fixture-key"));
    }

    #[test]
    fn validates_model_and_memory_overrides() {
        let settings = Settings {
            api_key: Some("fixture-key".into()),
            model: Some("stack:coder".into()),
            agentic_memory: Some(true),
        };
        let connection = settings.connection(None, Some(false)).unwrap().unwrap();
        assert_eq!(connection.model, "stack:coder");
        assert_eq!(connection.memory, Some(false));
        let connection = settings.connection(Some("hosted"), None).unwrap().unwrap();
        assert_eq!(connection.model, "hosted");
        assert_eq!(connection.memory, None);
        assert!(settings.connection(Some("hosted"), Some(true)).is_err());
        for model in ["", "has space", "stack:", "escape\x1b"] {
            assert!(settings.connection(Some(model), None).is_err());
        }
    }
}
