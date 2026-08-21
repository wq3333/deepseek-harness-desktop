@echo off
setlocal
chcp 65001 >nul
cd /d "%~dp0"

echo.
echo [1/4] Checking build environment...
where node >nul 2>&1 || (echo [ERROR] Node.js not found, please install from https://nodejs.org/ & exit /b 1)
where npm  >nul 2>&1 || (echo [ERROR] npm not found, please install Node.js & exit /b 1)
where cargo >nul 2>&1 || (echo [ERROR] cargo not found, please install Rust: https://rustup.rs & exit /b 1)
echo       Environment OK (node/npm/cargo)

echo.
echo [2/4] npm install...
call npm install
if errorlevel 1 (echo [ERROR] npm install failed & exit /b 1)

echo.
echo [3/4] tauri build (release)...
call npm run tauri build -- --no-bundle
if errorlevel 1 (echo [ERROR] tauri build failed & exit /b 1)

echo.
echo [4/4] Collecting artifacts...
if not exist "..\publish" mkdir ..\publish
copy /y "src-tauri\target\release\deepseek-harness.exe" "..\publish\DeepSeekHarness.exe" >nul
if errorlevel 1 (echo [ERROR] Failed to copy exe & exit /b 1)

echo.
echo ============================================================
echo   Done!
echo ============================================================
pause
endlocal
exit /b 0