mod adaption;
mod agent;
mod approval;
mod terminal;
mod tools;
mod workspace;

use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use adaption::Client;
use agent::Agent;
use terminal::{line, visible};
use workspace::Workspace;

const DEFAULT_MODEL: &str = "qwen3.5-397b";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Adaption model ID or stack:<slug>.
    #[arg(long, env = "ADAPTION_MODEL", default_value = DEFAULT_MODEL, global = true)]
    model: String,
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
        if !cli.model.starts_with("stack:") || cli.model == "stack:" {
            bail!("--memory requires --model stack:<slug>");
        }
        Some(true)
    } else {
        cli.no_memory.then_some(false)
    };

    let key = env::var("ADAPTION_API_KEY").context("set ADAPTION_API_KEY in your environment")?;
    let client = Client::new(&key)?;

    if matches!(cli.command, Some(Mode::Models)) {
        for model in client.models()? {
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
    let mut agent = Agent::new(client, cli.model, memory, workspace);
    if let Some(prompt) = prompt {
        return agent.run(&prompt, &mut io::stdout().lock());
    }

    eprintln!("Adaption Code · {}", line(agent.model()));
    eprintln!("Workspace: {}", line(&agent.root().display().to_string()));
    eprintln!("Commands: /help, /clear, /permissions, /revoke, /quit");
    let stdin = io::stdin();
    loop {
        eprint!("\nadaption> ");
        io::stderr().flush()?;
        let Some(prompt) = terminal::read_line(&mut stdin.lock())? else {
            break;
        };
        match prompt.trim() {
            "" => {}
            "/quit" | "/exit" => break,
            "/help" => eprintln!(
                "Enter a coding task or ask a follow-up.\n\
                 /clear clears the conversation\n\
                 /permissions shows remembered approvals\n\
                 /revoke clears remembered approvals\n\
                 /quit exits"
            ),
            "/clear" => {
                agent.clear_history();
                eprintln!("Conversation cleared.");
            }
            "/permissions" => agent.show_permissions(),
            "/revoke" => {
                agent.revoke_permissions();
                eprintln!("Remembered approvals cleared.");
            }
            _ => {
                if let Err(error) = agent.run(&prompt, &mut io::stdout().lock()) {
                    eprintln!("Error: {}", visible(&format!("{error:#}")));
                }
            }
        }
    }
    Ok(())
}
