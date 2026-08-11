//! FORK: minimal interactive account picker for `codex account switch`.
//!
//! Arrow keys/`j`/`k` move, digits jump, Enter confirms, Esc/`q` cancels.
//! When stdin or stderr is not a terminal it falls back to a numbered prompt,
//! and with no way to ask at all it errors, pointing at the named form.

use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;

use anyhow::Context;
use anyhow::Result;
use crossterm::cursor::MoveToColumn;
use crossterm::cursor::MoveUp;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::read;
use crossterm::execute;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;

use super::render::AccountRow;
use super::render::usage_summary;

/// Returns the selected row index, or `None` when the user cancels.
pub(crate) fn pick_account(rows: &[AccountRow]) -> Result<Option<usize>> {
    if rows.is_empty() {
        return Ok(None);
    }
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        pick_interactive(rows)
    } else if std::io::stdin().is_terminal() {
        anyhow::bail!(
            "cannot show the account picker without a terminal; pass the account name: `codex account switch <name>`"
        );
    } else {
        pick_numbered(rows)
    }
}

fn row_line(row: &AccountRow) -> String {
    let marker = if row.is_active { "●" } else { " " };
    let email = row
        .email
        .as_deref()
        .map(|email| format!(" <{email}>"))
        .unwrap_or_default();
    let plan = row
        .plan
        .as_deref()
        .map(|plan| format!(" [{plan}]"))
        .unwrap_or_default();
    format!(
        "{marker} {}{email}{plan} — {}",
        row.label,
        usage_summary(&row.usage)
    )
}

fn pick_numbered(rows: &[AccountRow]) -> Result<Option<usize>> {
    let mut stderr = std::io::stderr();
    for (index, row) in rows.iter().enumerate() {
        writeln!(stderr, "{:2}. {}", index + 1, row_line(row))?;
    }
    write!(stderr, "Account number (empty cancels): ")?;
    stderr.flush()?;

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read the selection")?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let number: usize = line
        .parse()
        .with_context(|| format!("\"{line}\" is not a number"))?;
    if number == 0 || number > rows.len() {
        anyhow::bail!("account number out of range (1-{})", rows.len());
    }
    Ok(Some(number - 1))
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn pick_interactive(rows: &[AccountRow]) -> Result<Option<usize>> {
    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "Select an account (Enter confirms, Esc cancels):"
    )?;

    // First paint; subsequent paints move the cursor back up and redraw.
    let mut selected = rows.iter().position(|row| row.is_active).unwrap_or(0);
    paint(&mut stderr, rows, selected, /* first */ true)?;

    let _guard = RawModeGuard::new()?;
    loop {
        let Event::Key(key) = read().context("failed to read terminal input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.checked_sub(1).unwrap_or(rows.len() - 1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1) % rows.len();
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let index = (digit as usize) - ('1' as usize);
                if index < rows.len() {
                    selected = index;
                }
            }
            KeyCode::Enter => {
                writeln!(stderr)?;
                return Ok(Some(selected));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                writeln!(stderr)?;
                return Ok(None);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                writeln!(stderr)?;
                return Ok(None);
            }
            _ => continue,
        }
        paint(&mut stderr, rows, selected, /* first */ false)?;
    }
}

fn paint(
    stderr: &mut std::io::Stderr,
    rows: &[AccountRow],
    selected: usize,
    first: bool,
) -> Result<()> {
    if !first {
        execute!(stderr, MoveUp(rows.len() as u16))?;
    }
    for (index, row) in rows.iter().enumerate() {
        execute!(stderr, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        let cursor = if index == selected { ">" } else { " " };
        // Raw mode needs explicit carriage returns.
        write!(stderr, "{cursor} {}\r\n", row_line(row))?;
    }
    stderr.flush()?;
    Ok(())
}
