# Local build environment for this machine.
#
# The MSVC linker is absent, so builds use the GNU toolchain plus the WinLibs
# mingw64 copy at C:\mingw64. It is a real copy, not a junction: gcc resolves
# its sysroot through its own path, and this user's home path contains a space
# ("Sandeep Khadka"), which makes ld split the path and fail.
#
# Usage:  . .\.perch\cargo-env.ps1
#         cargo +stable-x86_64-pc-windows-gnu test
#
# The acceptance-fixture tests also need the `zstd` CLI on PATH (a harness
# prerequisite, not a runtime dependency).
$env:Path = "C:\mingw64\bin;" + [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")
