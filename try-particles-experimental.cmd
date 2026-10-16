@echo off
setlocal
if not exist "%~dp0target\release\aircard.exe" (
    echo Build with cargo build --locked --release first.
    pause
    exit /b 1
)
echo Experimental particle trial: unknown originals allowed only for three neutral emitter PNGs.
echo Restore may leave neutralEmitter.png and its @2x/@3x variants behind.
echo Metadata, base, alternate and transaction originals remain required.
set "AIRCARD_EXPERIMENTAL_PARALLAX_NO_DYNAMIC_BACKUP="
set "AIRCARD_EXPERIMENTAL_CROSS_DISSOLVE_NO_ALTERNATE_BACKUP="
set "AIRCARD_EXPERIMENTAL_TRANSACTION_NO_ASSET_BACKUP="
set "AIRCARD_EXPERIMENTAL_PARTICLES_NO_EMITTER_BACKUP=1"
start "" "%~dp0target\release\aircard.exe"
endlocal
