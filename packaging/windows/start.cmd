@echo off
rem Start the server with persistent data beside this launcher.
setlocal DisableDelayedExpansion
cd /d "%~dp0"
set "FMP_DATA_DIR=%~dp0data"
if not defined FMP_LISTEN set "FMP_LISTEN=127.0.0.1:8317"
echo fakemodelproxy
echo Console: http://%FMP_LISTEN%/
echo Keep this window open. Press Ctrl+C to stop.
"%~dp0fakemodelproxy.exe"
set "FMP_EXIT_CODE=%ERRORLEVEL%"
if not "%FMP_EXIT_CODE%"=="0" (
    echo Server exited with code %FMP_EXIT_CODE%.
    pause
)
exit /b %FMP_EXIT_CODE%
