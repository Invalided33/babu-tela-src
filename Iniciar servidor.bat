@echo off
title Servidor Babu Tela
cd /d "%~dp0"
rem The desktop app already has a built-in server; use this only to run the
rem server on its own. Data goes to the server-data folder next to this file.
if not exist bin\babutela-server.exe (
  echo Gerando o servidor...
  go build -o bin\babutela-server.exe .\cmd\babutela-server || goto :fim
)
set PROSA_DATA=%~dp0server-data
echo Servidor do Babu Tela rodando. Deixe esta janela aberta enquanto usa o app.
echo.
bin\babutela-server.exe
:fim
echo.
echo O servidor parou.
pause
