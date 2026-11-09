@echo off
rem ytui installer for cmd.exe: runs install.ps1 without touching the
rem machine-wide execution policy.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
