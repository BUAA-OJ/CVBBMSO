"""Probe hosted runner capabilities without installing network policy."""

import ctypes
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from pathlib import Path


PROBE_REF = "refs/heads/test/hosted-egress-capabilities"


def command(*args, timeout=15):
    return subprocess.run(
        args,
        check=True,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
    ).stdout.strip()


def macos_capabilities():
    # Open the device with write access to confirm permissions, then close it
    # without sending a mutating ioctl or applying a ruleset.
    device = command(
        "/usr/bin/sudo",
        "-n",
        sys.executable,
        "-I",
        "-B",
        "-c",
        "import os; fd = os.open('/dev/pf', os.O_RDWR); os.close(fd); print(os.geteuid())",
    )
    info = command("/usr/bin/sudo", "-n", "/sbin/pfctl", "-s", "info")
    cloners = command("/sbin/ifconfig", "-C").split()
    return {
        "pf_device_euid": int(device),
        "pf_status": next(line for line in info.splitlines() if line.startswith("Status:")),
        "interface_cloners": cloners,
        "feth_available_for_follow_up": "feth" in cloners,
    }


def wfp_permissions():
    from ctypes import wintypes

    class Guid(ctypes.Structure):
        _fields_ = [
            ("data1", wintypes.DWORD),
            ("data2", wintypes.WORD),
            ("data3", wintypes.WORD),
            ("data4", wintypes.BYTE * 8),
        ]

    class DisplayData(ctypes.Structure):
        _fields_ = [("name", wintypes.LPWSTR), ("description", wintypes.LPWSTR)]

    class Session(ctypes.Structure):
        _fields_ = [
            ("key", Guid),
            ("display", DisplayData),
            ("flags", wintypes.UINT),
            ("wait_ms", wintypes.UINT),
            ("pid", wintypes.DWORD),
            ("sid", ctypes.c_void_p),
            ("username", wintypes.LPWSTR),
            ("kernel_mode", wintypes.BOOL),
        ]

    wfp = ctypes.WinDLL("fwpuclnt.dll", winmode=0x800)
    signatures = {
        "FwpmEngineOpen0": (
            wintypes.LPCWSTR,
            wintypes.UINT,
            ctypes.c_void_p,
            ctypes.POINTER(Session),
            ctypes.POINTER(wintypes.HANDLE),
        ),
        "FwpmTransactionBegin0": (wintypes.HANDLE, wintypes.UINT),
        "FwpmTransactionAbort0": (wintypes.HANDLE,),
        "FwpmEngineClose0": (wintypes.HANDLE,),
    }
    for name, signature in signatures.items():
        function = getattr(wfp, name)
        function.argtypes = signature
        function.restype = wintypes.DWORD

    def checked(name, *args):
        status = getattr(wfp, name)(*args)
        if status:
            raise RuntimeError(f"{name} returned 0x{status:08x}")

    session = Session()
    session.display.name = "CVBBMSO hosted capabilities"
    session.flags = 1  # FWPM_SESSION_FLAG_DYNAMIC
    session.wait_ms = 2_000
    engine = wintypes.HANDLE()
    checked("FwpmEngineOpen0", None, 10, None, ctypes.byref(session), ctypes.byref(engine))
    try:
        # Opening a read/write transaction checks its authorization. Abort
        # immediately with no objects staged, and never commit.
        checked("FwpmTransactionBegin0", engine, 0)
        checked("FwpmTransactionAbort0", engine)
    finally:
        checked("FwpmEngineClose0", engine)
    return {"dynamic_session_closed": True, "empty_write_transaction_aborted": True}


def windows_capabilities():
    results = {
        "administrator": bool(ctypes.windll.shell32.IsUserAnAdmin()),
        "wfp": wfp_permissions(),
    }
    psh = r"""
    $ErrorActionPreference = 'Stop'
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $loopbackDriver = Join-Path $env:SystemRoot 'INF\netloop.inf'
    $signedLoopbackPackage = Test-Path -LiteralPath $loopbackDriver
    $adapters = @(Get-NetAdapter -IncludeHidden |
        Select-Object -Property InterfaceDescription, Status)
    [ordered]@{
        loopback_driver_manifest_present = $signedLoopbackPackage
        available_adapters = $adapters
    } | ConvertTo-Json -Depth 3 -Compress
    """
    results["interface_capabilities"] = json.loads(
        command("powershell.exe", "-NoProfile", "-NonInteractive", "-Command", psh)
    )
    return results


def supervised_case(directory, mode):
    marker = directory / mode
    child = """
import sys, time
from pathlib import Path
Path(sys.argv[1]).write_text("synthetic state", encoding="utf-8")
if sys.argv[2] == "failure":
    sys.exit(17)
time.sleep(60)
"""
    with subprocess.Popen(
        [sys.executable, "-I", "-B", "-c", child, str(marker), mode]
    ) as process:
        try:
            deadline = time.monotonic() + 8
            while not marker.is_file() and process.poll() is None:
                if time.monotonic() >= deadline:
                    raise RuntimeError("child did not report readiness")
                time.sleep(0.025)
            if not marker.is_file():
                raise RuntimeError("child exited before recording its state")
            try:
                outcome = f"exit {process.wait(timeout=1)}"
            except subprocess.TimeoutExpired:
                outcome = "timeout"
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
            marker.unlink(missing_ok=True)

    expected = "exit 17" if mode == "failure" else "timeout"
    if outcome != expected or process.returncode is None or marker.exists():
        raise RuntimeError(f"{mode} recovery did not meet its assertions")
    return {
        "outcome": outcome,
        "child_exit_code": process.returncode,
        "child_reaped": True,
        "synthetic_state_removed": True,
    }


def main():
    expected = {
        "GITHUB_ACTIONS": "true",
        "GITHUB_REPOSITORY": "BUAA-OJ/CVBBMSO",
        "GITHUB_EVENT_NAME": "workflow_dispatch",
        "GITHUB_REF": PROBE_REF,
        "RUNNER_ENVIRONMENT": "github-hosted",
    }
    if any(os.environ.get(key) != value for key, value in expected.items()):
        raise SystemExit("This probe only runs on its dedicated GitHub-hosted workflow")

    runner_temp = Path(os.environ["RUNNER_TEMP"])
    evidence = {
        "commit": os.environ["GITHUB_SHA"],
        "runner_os": os.environ["RUNNER_OS"],
        "platform": platform.platform(),
        "network_policy_applied": False,
        "supervision": {},
    }
    try:
        with tempfile.TemporaryDirectory(dir=runner_temp) as temp:
            for mode in ("failure", "timeout"):
                evidence["supervision"][mode] = supervised_case(Path(temp), mode)

        if sys.platform == "darwin":
            evidence["capabilities"] = macos_capabilities()
        elif sys.platform == "win32":
            evidence["capabilities"] = windows_capabilities()
        else:
            raise RuntimeError(f"Unsupported platform {sys.platform}")
    except Exception as error:
        evidence["error"] = str(error)
        raise
    finally:
        report = json.dumps(evidence, indent=2)
        (runner_temp / "cvbbmso-hosted-egress.json").write_text(report + "\n", encoding="utf-8")
        print(report)


if __name__ == "__main__":
    main()
