use std::{
    io::{self, Write},
    thread,
    time::Duration,
};

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "terminal-fixtures")]
struct Cli {
    #[command(subcommand)]
    scenario: Scenario,
}

#[derive(Debug, Subcommand)]
enum Scenario {
    Text(HoldArgs),
    Styled(HoldArgs),
    Cursor(HoldArgs),
    AltScreen(HoldArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Hold {
    None,
    Short,
    Medium,
    Long,
}

#[derive(Debug, Parser)]
struct HoldArgs {
    #[arg(long, value_enum, default_value_t = Hold::Medium)]
    hold: Hold,
}

fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let mut stdout = io::stdout().lock();

    match cli.scenario {
        Scenario::Text(args) => emit_text(&mut stdout, args.hold)?,
        Scenario::Styled(args) => emit_styled(&mut stdout, args.hold)?,
        Scenario::Cursor(args) => emit_cursor(&mut stdout, args.hold)?,
        Scenario::AltScreen(args) => emit_alt_screen(&mut stdout, args.hold)?,
    }

    Ok(())
}

fn emit_text(stdout: &mut impl Write, hold: Hold) -> Result<(), String> {
    writeln!(stdout, "fixture:text:start").map_err(|err| format!("write fixture text start: {err}"))?;
    writeln!(stdout, "plain ascii line").map_err(|err| format!("write plain line: {err}"))?;
    writeln!(stdout, "numbers: 0123456789").map_err(|err| format!("write numbers line: {err}"))?;
    writeln!(stdout, "fixture:text:end").map_err(|err| format!("write fixture text end: {err}"))?;
    stdout.flush().map_err(|err| format!("flush fixture text: {err}"))?;
    hold_for(hold);
    Ok(())
}

fn emit_styled(stdout: &mut impl Write, hold: Hold) -> Result<(), String> {
    writeln!(stdout, "fixture:styled:start").map_err(|err| format!("write fixture styled start: {err}"))?;
    writeln!(stdout, "\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m")
        .map_err(|err| format!("write styled line: {err}"))?;
    writeln!(stdout, "\x1b[38;2;255;90;0mtruecolor orange\x1b[0m").map_err(|err| format!("write truecolor line: {err}"))?;
    writeln!(stdout, "\x1b[48;5;25m\x1b[38;5;231m256-color background\x1b[0m").map_err(|err| format!("write 256 color line: {err}"))?;
    writeln!(stdout, "fixture:styled:end").map_err(|err| format!("write fixture styled end: {err}"))?;
    stdout.flush().map_err(|err| format!("flush fixture styled: {err}"))?;
    hold_for(hold);
    Ok(())
}

fn emit_cursor(stdout: &mut impl Write, hold: Hold) -> Result<(), String> {
    write!(stdout, "\x1b[2J\x1b[H").map_err(|err| format!("clear screen: {err}"))?;
    writeln!(stdout, "fixture:cursor:start").map_err(|err| format!("write fixture cursor start: {err}"))?;
    write!(stdout, "\x1b[?25l").map_err(|err| format!("hide cursor: {err}"))?;
    write!(stdout, "\x1b[4;10Hcursor-target").map_err(|err| format!("move cursor target: {err}"))?;
    write!(stdout, "\x1b[6;1Hafter-target").map_err(|err| format!("move after target: {err}"))?;
    write!(stdout, "\x1b[?25h").map_err(|err| format!("show cursor: {err}"))?;
    stdout.flush().map_err(|err| format!("flush fixture cursor: {err}"))?;
    hold_for(hold);
    writeln!(stdout, "\nfixture:cursor:end").map_err(|err| format!("write fixture cursor end: {err}"))?;
    stdout.flush().map_err(|err| format!("flush fixture cursor end: {err}"))?;
    Ok(())
}

fn emit_alt_screen(stdout: &mut impl Write, hold: Hold) -> Result<(), String> {
    writeln!(stdout, "fixture:altscreen:before").map_err(|err| format!("write alt-screen preface: {err}"))?;
    write!(stdout, "\x1b[?1049h\x1b[2J\x1b[H").map_err(|err| format!("enter alt screen: {err}"))?;
    writeln!(stdout, "fixture:altscreen:inside").map_err(|err| format!("write alt-screen body: {err}"))?;
    stdout.flush().map_err(|err| format!("flush alt-screen body: {err}"))?;
    hold_for(hold);
    write!(stdout, "\x1b[?1049l").map_err(|err| format!("leave alt screen: {err}"))?;
    writeln!(stdout, "fixture:altscreen:after").map_err(|err| format!("write alt-screen epilogue: {err}"))?;
    stdout.flush().map_err(|err| format!("flush alt-screen epilogue: {err}"))?;
    Ok(())
}

fn hold_for(hold: Hold) {
    let duration = match hold {
        Hold::None => return,
        Hold::Short => Duration::from_millis(150),
        Hold::Medium => Duration::from_millis(700),
        Hold::Long => Duration::from_secs(2),
    };
    thread::sleep(duration);
}
