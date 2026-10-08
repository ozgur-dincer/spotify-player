@echo off
REM Rebuilds this repo's spotify_player binary (release mode, with our volume-guard
REM fix) using the same feature set the project's CI/AGENTS.md convention builds
REM with, then installs it over the `cargo install`-managed binary on PATH.
setlocal
cd /d "%~dp0"

set FEATURES=rodio-backend,media-control,image,notify,fzf
set TARGET_EXE=%~dp0target\release\spotify_player.exe
set INSTALL_EXE=%USERPROFILE%\.cargo\bin\spotify_player.exe

cargo build --release --no-default-features --features %FEATURES% %*
if errorlevel 1 (
    echo Build failed, not installing.
    endlocal
    exit /b 1
)

echo Copying "%TARGET_EXE%" -^> "%INSTALL_EXE%"
copy /y "%TARGET_EXE%" "%INSTALL_EXE%"
if errorlevel 1 (
    echo Copy failed - is spotify_player.exe still running? Close it and re-run.
    endlocal
    exit /b 1
)

echo Done.
endlocal
