use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

use serde::Deserialize;
use tokio::process::Command;
use windows_sys::Win32::System::{
    SystemInformation::GetSystemDirectoryW, Threading::CREATE_NO_WINDOW,
};

use crate::preflight::{Diagnostic, Verdict, command::read_probe, evidence::windows_verdict};

const QUERY: &str = r"
$ErrorActionPreference = 'Stop'
Import-Module ($PSHOME + '\Modules\CimCmdlets\CimCmdlets.psd1')
$rows = @(CimCmdlets\Get-CimInstance -Query 'SELECT Manufacturer, Model, HypervisorPresent FROM Win32_ComputerSystem' -OperationTimeoutSec 3)
if ($rows.Count -ne 1) { exit 1 }
$json = @{ Manufacturer = $rows[0].Manufacturer; Model = $rows[0].Model; HypervisorPresent = $rows[0].HypervisorPresent } | Microsoft.PowerShell.Utility\ConvertTo-Json -Compress
$bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
$stdout = [Console]::OpenStandardOutput()
$stdout.Write($bytes, 0, $bytes.Length)
$stdout.Flush()
";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ComputerSystem {
    manufacturer: Option<String>,
    model: Option<String>,
    hypervisor_present: Option<bool>,
}

pub(crate) async fn detect() -> Verdict {
    let Some(home) = powershell_home() else {
        return Verdict::Unknown(Diagnostic::ProbeUnavailable);
    };
    let mut command = Command::new(home.join("powershell.exe"));
    command
        .creation_flags(CREATE_NO_WINDOW)
        .current_dir(&home)
        .env("PSModulePath", home.join("Modules"))
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            QUERY,
        ]);
    let output = match read_probe(&mut command).await {
        Ok(output) => output,
        Err(error) => return Verdict::Unknown(error),
    };
    match serde_json::from_slice::<ComputerSystem>(&output) {
        Ok(system) => windows_verdict(
            system.manufacturer.as_deref(),
            system.model.as_deref(),
            system.hypervisor_present,
        ),
        Err(_) => Verdict::Unknown(Diagnostic::InvalidEvidence),
    }
}

fn powershell_home() -> Option<PathBuf> {
    const CAPACITY: u32 = 260;
    let mut system = [0_u16; CAPACITY as usize];
    // SAFETY: the array is writable for exactly CAPACITY UTF-16 code units.
    let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), CAPACITY) };
    if length == 0 || length >= CAPACITY {
        return None;
    }
    let directory = PathBuf::from(OsString::from_wide(&system[..length as usize]));
    Some(directory.join("WindowsPowerShell").join("v1.0"))
}
