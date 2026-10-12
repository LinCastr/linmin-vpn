#!/usr/bin/env bash
# tools/capture_cipher.sh
# Inicia o pipespy e o RvRvpnGuiSpy para capturar blobs de senha 030d via known-plaintext.

set -e

WINE_PREFIX="$HOME/.local/share/radmin-vpn-linux/wineprefix"
WINE_BIN="/tmp/.mount_RadminjOLJoK/wine/bin/wine"
RADMIN_DIR="$WINE_PREFIX/drive_c/Program Files (x86)/Radmin VPN"
PIPESPY_EXE="$(cd "$(dirname "$0")/.." && pwd)/bridge/pipespy.exe"
LOG_FILE="/tmp/spy_known_plaintext.log"

# Se o wine da pasta montada nao estiver disponivel, usa o wine do sistema
if [ ! -f "$WINE_BIN" ]; then
    WINE_BIN="wine"
fi

echo "==> Fechando instancias anteriores da GUI e pipespy..."
killall -q RvRvpnGui.exe RvRvpnGuiSpy.exe pipespy.exe || true
sleep 0.5

rm -f "$LOG_FILE"

echo "==> Iniciando pipespy (gravando em $LOG_FILE)..."
WINEPREFIX="$WINE_PREFIX" "$WINE_BIN" "$PIPESPY_EXE" "$LOG_FILE" &
SPY_PID=$!
sleep 1

cleanup() {
    echo "==> Encerrando pipespy..."
    kill "$SPY_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> Abrindo RvRvpnGuiSpy.exe..."
echo "Instruções:"
echo "1. Na janela do Radmin, crie uma rede (menu Rede -> Criar rede ou botão direito no vazio)"
echo "2. Amostra 1: Nome 'testenet1' e Senha 'AAAAAA'"
echo "3. Amostra 2: Nome 'testenet1' e Senha 'BBBBBB'"
echo "4. Amostra 3: Nome 'testenet2' e Senha 'AAAAAA'"
echo "5. Feche a janela do Radmin quando terminar."
echo "------------------------------------------------"

WINEPREFIX="$WINE_PREFIX" WINEDEBUG="-all" "$WINE_BIN" "$RADMIN_DIR/RvRvpnGuiSpy.exe"

python3 "$(dirname "$0")/rvdecode.py" "$LOG_FILE" | grep -E "(0306 str|030d)" -B 1 -A 1 || echo "Nenhum blob 030d encontrado ainda."
