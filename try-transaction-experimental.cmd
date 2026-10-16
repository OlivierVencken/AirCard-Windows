@echo off
setlocal
if not exist "%~dp0target\release\aircard.exe" (
    echo Build with cargo build --locked --release first.
    pause
    exit /b 1
)
echo Experimental transaction trial: missing transaction asset originals are allowed.
echo Restore may leave three transaction emitter PNGs and the shape SVG behind.
set "AIRCARD_EXPERIMENTAL_PARALLAX_NO_DYNAMIC_BACKUP="
set "AIRCARD_EXPERIMENTAL_CROSS_DISSOLVE_NO_ALTERNATE_BACKUP="
set "AIRCARD_EXPERIMENTAL_TRANSACTION_NO_ASSET_BACKUP=1"
start "" "%~dp0target\release\aircard.exe"
endlocal
