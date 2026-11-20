#!/usr/bin/env bash
# Atualiza o disk-util-rs DESTA máquina a partir do fonte ao lado deste script.
# Funciona de qualquer diretório e via symlink em ~/.local/bin — resolve o
# symlink p/ achar o repo (caminhos absolutos).
#
# Uso: disk-util-update   (ou: bash ~/src/disk-util-rs/scripts/update-local.sh)
set -euo pipefail

SOURCE="${BASH_SOURCE[0]}"
while [ -L "$SOURCE" ]; do
    DIR="$(cd "$(dirname "$SOURCE")" && pwd)"
    SOURCE="$(readlink "$SOURCE")"
    [[ $SOURCE != /* ]] && SOURCE="$DIR/$SOURCE"
done
REPO="$(cd "$(dirname "$SOURCE")/.." && pwd)"
BIN="$HOME/.local/bin/disk-util-rs"

cargo build --release --manifest-path "$REPO/Cargo.toml" --quiet
install -m 0755 "$REPO/target/release/disk-util-rs" "$HOME/.local/bin/disk-util-rs.new"
mv "$HOME/.local/bin/disk-util-rs.new" "$BIN"
echo "ok: $BIN ($("$BIN" --version)) — fonte: $REPO"
