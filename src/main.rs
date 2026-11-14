mod adaption;
mod agent;
mod approval;
mod settings;
mod terminal;
mod tools;
mod workspace;

use std::io::{self, IsTerminal, StdinLock, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use adaption::Client;
use agent::Agent;
use settings::{Config, Connection, Settings};
use terminal::{line, visible};
use workspace::Workspace;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Adaption model ID or stack:<slug>.
    #[arg(long, env = "ADAPTION_MODEL", global = true)]
    model: Option<String>,
    /// Project directory. Defaults to the Git root, or the current directory.
    #[arg(short = 'C', long, global = true)]
    dir: Option<PathBuf>,
    /// Enable agentic memory for an Adaption inference stack.
    #[arg(long, conflicts_with = "no_memory", global = true)]
    memory: bool,
    /// Disable agentic memory for this session, including on a memory-enabled stack.
    #[arg(long, global = true)]
    no_memory: bool,
    #[command(subcommand)]
    command: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// Submit one coding task. Approvals use the controlling terminal when available.
    Ask {
        /// Prompt for the agent. Pass a single "-" to read a prompt from piped stdin.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// List the model IDs available to this API key.
    Models,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {}", visible(&format!("{error:#}")));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let memory = if cli.memory {
        Some(true)
    } else {
        cli.no_memory.then_some(false)
    };
    let config = Config::locate()?;
    let mut settings = config.load()?;
    let model = cli.model.as_deref();

    if matches!(cli.command, Some(Mode::Models)) {
        let key = settings.key().context(SETUP_REQUIRED)?;
        for model in Client::new(&key)?.models()? {
            println!("{}", line(&model));
        }
        return Ok(());
    }

    let prompt = match cli.command {
        Some(Mode::Ask { prompt }) if prompt == ["-"] => {
            if io::stdin().is_terminal() {
                bail!("pipe a prompt to adaption-code ask -");
            }
            let mut input = io::stdin().lock();
            let mut prompt = String::new();
            while let Some(next) = terminal::read_line(&mut input)? {
                prompt.push_str(&next);
                prompt.push('\n');
                agent::check_prompt_size(&prompt)?;
            }
            Some(prompt)
        }
        Some(Mode::Ask { prompt }) => Some(prompt.join(" ")),
        None => None,
        Some(Mode::Models) => unreachable!(),
    };
    if prompt.is_none() && !io::stdin().is_terminal() {
        bail!("use adaption-code ask \"your prompt\" or pipe a prompt to adaption-code ask -");
    }

    let workspace = Workspace::open(cli.dir)?;
    tools::install_interrupt_handler()?;
    if let Some(prompt) = prompt {
        let connection = settings
            .connection(model, memory)?
            .context(SETUP_REQUIRED)?;
        return connect(&connection, &workspace)?.run(&prompt, &mut io::stdout().lock());
    }

    let mut input = io::stdin().lock();
    let mut output = io::stderr().lock();
    eprintln!("Adaption Code");
    eprintln!(
        "Workspace: {}",
        line(&workspace.root().display().to_string())
    );
    eprintln!("Commands: /settings, /help, /clear, /permissions, /revoke, /quit");
    if settings.key().is_none() && settings::setup_popup(&mut input, &mut output)? {
        edit_settings(
            &mut settings,
            &config,
            &mut input,
            &mut output,
            model,
            memory,
        );
    }
    drop(output);

    let mut session: Option<(Connection, Agent<'_>)> = None;
    loop {
        eprint!("\nadaption> ");
        io::stderr().flush()?;
        let Some(prompt) = terminal::read_line(&mut input)? else {
            break;
        };
        match prompt.trim() {
            "" => {}
            "/quit" | "/exit" => break,
            "/help" => eprintln!(
                "Enter a coding task or ask a follow-up.\n\
                 /settings configures Adaption\n\
                 /clear clears the conversation\n\
                 /permissions shows remembered approvals\n\
                 /revoke clears remembered approvals\n\
                 /quit exits"
            ),
            "/settings" => {
                edit_settings(
                    &mut settings,
                    &config,
                    &mut input,
                    &mut io::stderr().lock(),
                    model,
                    memory,
                );
                if let Some((previous, _)) = &session
                    && settings.connection(model, memory).ok().flatten().as_ref() != Some(previous)
                {
                    session = None;
                    eprintln!("A new conversation will use the updated connection.");
                }
            }
            "/clear" => {
                if let Some((_, agent)) = &mut session {
                    agent.clear_history();
                }
                eprintln!("Conversation cleared.");
            }
            "/permissions" => {
                if let Some((_, agent)) = &session {
                    agent.show_permissions();
                } else {
                    eprintln!("No remembered approvals.");
                }
            }
            "/revoke" => {
                if let Some((_, agent)) = &mut session {
                    agent.revoke_permissions();
                }
                eprintln!("Remembered approvals cleared.");
            }
            _ => {
                let result = (|| -> Result<()> {
                    let connection = settings
                        .connection(model, memory)?
                        .context(SETUP_REQUIRED)?;
                    if session.is_none() {
                        let agent = connect(&connection, &workspace)?;
                        eprintln!("Model: {}", line(agent.model()));
                        session = Some((connection, agent));
                    }
                    if let Some((_, agent)) = &mut session {
                        agent.run(&prompt, &mut io::stdout().lock())?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("Error: {}", visible(&format!("{error:#}")));
                }
            }
        }
    }
    Ok(())
}

const SETUP_REQUIRED: &str =
    "Adaption is unconfigured. Run adaption-code and open /settings to add an API key.";

fn connect<'a>(connection: &Connection, workspace: &'a Workspace) -> Result<Agent<'a>> {
    Ok(Agent::new(
        Client::new(&connection.key)?,
        connection.model.clone(),
        connection.memory,
        workspace,
    ))
}

fn edit_settings(
    settings: &mut Settings,
    config: &Config,
    input: &mut StdinLock<'_>,
    output: &mut impl Write,
    model: Option<&str>,
    memory: Option<bool>,
) {
    if let Err(error) = settings::edit(settings, config, input, output, model, memory) {
        eprintln!("Error: {}", visible(&format!("{error:#}")));
    }
}
