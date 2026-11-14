use std::env;
use std::fs::{self, DirBuilder, File};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};

use crate::terminal::{Interrupted, Palette, Tone};
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
    input: &mut (impl BufRead + AsFd),
    output: &mut impl Write,
    palette: Palette,
) -> Result<bool> {
    writeln!(
        output,
        "\n{} {}",
        palette.paint(Tone::Info, "╭─"),
        palette.paint(Tone::Text, "Adapt setup")
    )?;
    writeln!(
        output,
        "{} Add your Adaption API key in {}.",
        palette.paint(Tone::Info, "│"),
        palette.paint(Tone::Action, "/settings")
    )?;
    writeln!(
        output,
        "{} {}     {}",
        palette.paint(Tone::Info, "│"),
        palette.paint(Tone::Action, "1. Open /settings"),
        palette.paint(Tone::Text, "2. Later")
    )?;
    writeln!(output, "{}", palette.paint(Tone::Info, "╰─"))?;
    loop {
        write!(output, "{}", palette.paint(Tone::Action, "Choose [1]: "))?;
        output.flush()?;
        if let Some(choice) = setup_choice(terminal::read_interactive_line(input)?.as_deref()) {
            return Ok(choice);
        }
        writeln!(output, "{}", palette.paint(Tone::Action, "Enter 1 or 2."))?;
    }
}

fn setup_choice(input: Option<&str>) -> Option<bool> {
    match input.map(str::trim) {
        Some("" | "1" | "/settings") => Some(true),
        Some("2") | None => Some(false),
        _ => None,
    }
}

pub fn edit(
    settings: &mut Settings,
    config: &Config,
    input: &mut (impl BufRead + AsFd),
    output: &mut impl Write,
    model_override: Option<&str>,
    memory_override: Option<bool>,
) -> Result<()> {
    let palette = Palette::stderr();
    let mut draft = settings.clone();
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
        let border = palette.paint(Tone::Info, "│");
        writeln!(
            output,
            "\n{} {}",
            palette.paint(Tone::Info, "╭─"),
            palette.paint(Tone::Text, "/settings")
        )?;
        let key_tone = if key_status == "required" {
            Tone::Action
        } else {
            Tone::Text
        };
        writeln!(
            output,
            "{border} {}          {}",
            palette.paint(key_tone, "1. API key"),
            palette.paint(key_tone, key_status)
        )?;
        writeln!(
            output,
            "{border} {}   {}",
            palette.paint(Tone::Text, "2. Model or stack"),
            terminal::line(model)
        )?;
        writeln!(
            output,
            "{border} {}   {}",
            palette.paint(Tone::Text, "3. Agentic memory"),
            memory_label(draft.agentic_memory)
        )?;
        writeln!(
            output,
            "{border} {}",
            palette.paint(Tone::Text, "4. List available models")
        )?;
        writeln!(
            output,
            "{border} {}",
            palette.paint(Tone::Action, "5. Save and return")
        )?;
        writeln!(output, "{border} 0. Cancel")?;
        if let Some(model) = model_override {
            writeln!(
                output,
                "{border} Session model override: {}",
                palette.paint(Tone::Info, terminal::line(model))
            )?;
        }
        if let Some(memory) = memory_override {
            writeln!(
                output,
                "{border} Session memory override: {}",
                palette.paint(Tone::Info, memory_label(Some(memory)))
            )?;
        }
        writeln!(output, "{}", palette.paint(Tone::Info, "╰─"))?;

        write!(output, "{}", palette.paint(Tone::Action, "Choose: "))?;
        output.flush()?;
        match terminal::read_interactive_line(input)?
            .as_deref()
            .map(str::trim)
        {
            Some("1") => {
                writeln!(output, "{KEY_HELP}")?;
                match terminal::read_hidden_line(
                    input,
                    output,
                    &palette.paint(Tone::Action, "API key (input hidden, Enter to keep): "),
                )? {
                    Some(key) if !key.trim().is_empty() => {
                        let key = key.trim();
                        if valid_key(key) {
                            draft.api_key = Some(key.to_owned());
                        } else {
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
                    _ => {}
                }
            }
            Some("2") => {
                write!(
                    output,
                    "{}",
                    palette.paint(
                        Tone::Action,
                        "Adaption model ID or stack:<slug> (Enter to keep): "
                    )
                )?;
                output.flush()?;
                if let Some(model) = terminal::read_interactive_line(input)? {
                    let model = model.trim();
                    if model.is_empty() {
                        continue;
                    }
                    if let Err(error) = validate_model(model) {
                        writeln!(
                            output,
                            "{}",
                            palette.paint(Tone::Action, terminal::line(&error.to_string()))
                        )?;
                        continue;
                    }
                    draft.model = Some(model.to_owned());
                }
            }
            Some("3") => {
                writeln!(
                    output,
                    "For an inference stack, enable memory, disable it, or use the stack's setting."
                )?;
                write!(
                    output,
                    "{}",
                    palette.paint(
                        Tone::Action,
                        "[1] On  [2] Off  [3] Stack default  [Enter] Keep: "
                    )
                )?;
                output.flush()?;
                match terminal::read_interactive_line(input)?
                    .as_deref()
                    .map(str::trim)
                {
                    Some("1") => draft.agentic_memory = Some(true),
                    Some("2") => draft.agentic_memory = Some(false),
                    Some("3") => draft.agentic_memory = None,
                    Some("") | None => {}
                    _ => writeln!(
                        output,
                        "{}",
                        palette.paint(Tone::Action, "Enter 1, 2, or 3.")
                    )?,
                }
            }
            Some("4") => show_models(&draft, output, palette)?,
            Some("5") => {
                if draft.key().is_none() {
                    writeln!(
                        output,
                        "{}",
                        palette.paint(Tone::Action, "Add an API key with option 1 before saving.")
                    )?;
                    continue;
                }
                if &draft != settings || !config.path().exists() {
                    config.save(&draft)?;
                    *settings = draft;
                }
                writeln!(output, "{}", palette.paint(Tone::Info, "Settings saved."))?;
                return Ok(());
            }
            Some("0") | None => return Ok(()),
            _ => writeln!(output, "{}", palette.paint(Tone::Action, "Enter 0–5."))?,
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

fn show_models(settings: &Settings, output: &mut impl Write, palette: Palette) -> Result<()> {
    let Some(key) = settings.key() else {
        writeln!(
            output,
            "{}",
            palette.paint(Tone::Action, "Add an API key with option 1 first.")
        )?;
        return Ok(());
    };
    match Client::new(&key).and_then(|client| client.models()) {
        Ok(models) if models.is_empty() => writeln!(
            output,
            "{}",
            palette.paint(Tone::Action, "No models available to this key.")
        )?,
        Ok(models) => {
            for model in models {
                writeln!(output, "  {}", terminal::line(&model))?;
            }
        }
        Err(error) if error.is::<Interrupted>() => return Err(error),
        Err(_) => writeln!(
            output,
            "{}",
            palette.paint(
                Tone::Action,
                "Could not list models. Check your API key and connection."
            )
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn popup_accepts_default_and_later() {
        assert_eq!(setup_choice(Some("")), Some(true));
        assert_eq!(setup_choice(Some("2")), Some(false));
        assert_eq!(setup_choice(Some("9")), None);
    }
}
