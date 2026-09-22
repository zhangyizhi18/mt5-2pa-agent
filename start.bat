@echo off
chcp 65001 >nul
title MT5 2PA Agent (Rust Edition)
echo ====================================================
echo   MT5 2PA Agent (Rust High-Performance Edition)
echo   Starting server at http://127.0.0.1:8066 ...
echo ====================================================
echo.
echo   前置条件：MT5 终端已启动并挂载 PABridge EA，
echo   且已在 EA 设置中允许 WebRequest 访问 127.0.0.1:8066。
echo.

REM Automatically add Cargo bin directory to PATH if not already present
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

if exist "target\release\mt5-2pa-agent.exe" (
    "target\release\mt5-2pa-agent.exe" --host 127.0.0.1 --port 8066
) else (
    cargo run --release -- --host 127.0.0.1 --port 8066
)
pause
