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

const LOGO: [&str; 3] = [
    r#"                ..+####.
+#+   +#+  #############
                ''+####'"#,
    r#"          .+##+.   .+++.
+##############   +#####
+##############   ######
          '+##+'   '+++'"#,
    r#".+##+.   .+##+.   .+##+.
######   ######   ######
'+##+'   '+##+'   '+##+'"#,
];

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
                bail!("pipe a prompt to adapt ask -");
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
        bail!("use adapt ask \"your prompt\" or pipe a prompt to adapt ask -");
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
    let width = rustix::termios::tcgetwinsize(&input)
        .ok()
        .map(|size| usize::from(size.ws_col))
        .filter(|&width| width > 0)
        .unwrap_or(80);
    write_banner(
        &mut output,
        width,
        banner_colors(io::stderr().is_terminal()),
    )?;
    writeln!(
        output,
        "Workspace: {}",
        line(&workspace.root().display().to_string())
    )?;
    writeln!(
        output,
        "Commands: /settings, /help, /clear, /permissions, /revoke, /quit"
    )?;
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
        eprint!("\nadapt> ");
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
    "Adaption is unconfigured. Run adapt and open /settings to add an API key.";

fn write_banner(
    output: &mut impl Write,
    width: usize,
    colors: Option<[&str; 3]>,
) -> io::Result<()> {
    writeln!(output)?;
    if LOGO
        .iter()
        .flat_map(|row| row.lines())
        .all(|line| line.len() <= width)
    {
        for (index, row) in LOGO.iter().enumerate() {
            if index > 0 {
                writeln!(output)?;
            }
            if let Some(colors) = colors {
                writeln!(output, "{}{row}\x1b[39m", colors[index])?;
            } else {
                writeln!(output, "{row}")?;
            }
        }
        writeln!(output)?;
    }
    writeln!(output, "Adapt")
}

fn banner_colors(is_terminal: bool) -> Option<[&'static str; 3]> {
    let term = std::env::var("TERM").unwrap_or_default();
    if !is_terminal
        || term == "dumb"
        || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
    {
        return None;
    }

    // The blue, lavender, and rose accents are from Adaption's homepage stylesheet.
    let color_term = std::env::var("COLORTERM").unwrap_or_default();
    Some(
        if color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct") {
            [
                "\x1b[38;2;73;134;180m",
                "\x1b[38;2;169;186;209m",
                "\x1b[38;2;218;167;210m",
            ]
        } else if term.contains("256color") {
            ["\x1b[38;5;67m", "\x1b[38;5;146m", "\x1b[38;5;182m"]
        } else {
            ["\x1b[34m", "\x1b[36m", "\x1b[35m"]
        },
    )
}

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
