@echo off
setlocal
: this is some script for dev

: check choco var
if not defined ChocolateyInstall (echo "Chocolatey not installed" && goto end)

set "REPO_PATH=%~dp0"
set "INSTALL_PATH=%ChocolateyInstall%\bin\"

: its rust
set "RELEASE_PATH=%REPO_PATH%\target\release\"
set "BINARY_NAME=clipdate.exe"
set "BINARY_PATH=%RELEASE_PATH%%BINARY_NAME%"

if exist "%BINARY_PATH%" (
    copy /y "%BINARY_PATH%" "%INSTALL_PATH%"
) else (
    echo "release binary not found"
    goto end
)


:end
if /i "%comspec% /c %~0 " equ "%cmdcmdline:"=%" timeout /t 3 /nobreak
endlocal
exit /b
