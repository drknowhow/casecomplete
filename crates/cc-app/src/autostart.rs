//! Start with Windows: a value under HKCU\...\CurrentVersion\Run pointing at this exe.

use anyhow::{Context, Result};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "CaseComplete";

fn command() -> Result<String> {
    let exe = std::env::current_exe().context("current exe")?;
    Ok(format!("\"{}\" --background", exe.display()))
}

pub fn is_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(VALUE))
        .map(|v| command().map_or(false, |c| v == c))
        .unwrap_or(false)
}

pub fn set(enabled: bool) -> Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_WRITE | KEY_READ)
        .context("open Run key")?;
    if enabled {
        key.set_value(VALUE, &command()?).context("set Run value")?;
    } else if key.get_value::<String, _>(VALUE).is_ok() {
        key.delete_value(VALUE).context("delete Run value")?;
    }
    Ok(())
}
