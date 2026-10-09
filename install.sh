#!/usr/bin/env bash
# θ theta — installeur / mise à jour / désinstallation
# usage: ./install.sh [install|update|uninstall|status|help]

set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="theta"

if [[ -t 1 ]]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
  C_CYAN=$'\033[36m'; C_MAGENTA=$'\033[35m'; C_GREEN=$'\033[32m'
  C_YELLOW=$'\033[33m'; C_RED=$'\033[31m'
else
  C_RESET=""; C_BOLD=""; C_DIM=""; C_CYAN=""; C_MAGENTA=""
  C_GREEN=""; C_YELLOW=""; C_RED=""
fi

banner() {
  printf '%s' "$C_MAGENTA$C_BOLD"
  cat <<'EOF'
        _   _          _
       | |_| |__   ___| |_ __ _
       | __| '_ \ / _ \ __/ _` |
       | |_| | | |  __/ || (_| |
        \__|_| |_|\___|\__\__,_|
EOF
  printf '%s' "$C_RESET"
  printf '%s  ~ harness de coding agent léger en Rust ~%s\n\n' "$C_DIM" "$C_RESET"
}

step()  { printf '%s▸%s %s\n' "$C_CYAN$C_BOLD" "$C_RESET" "$*"; }
ok()    { printf '%s✔%s %s\n' "$C_GREEN$C_BOLD" "$C_RESET" "$*"; }
warn()  { printf '%s!%s %s\n' "$C_YELLOW$C_BOLD" "$C_RESET" "$*"; }
die()   { printf '%s✘%s %s\n' "$C_RED$C_BOLD" "$C_RESET" "$*" >&2; exit 1; }

need_cargo() {
  if ! command -v cargo >/dev/null 2>&1; then
    die "cargo introuvable. Installe Rust : curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  fi
}

installed_version() {
  if command -v "$BIN_NAME" >/dev/null 2>&1; then
    "$BIN_NAME" --version 2>/dev/null | head -n1 || echo "inconnue"
  else
    echo "absent"
  fi
}

source_version() {
  sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -n1
}

build_and_install() {
  need_cargo
  step "compilation et installation (cargo install --path $SRC)"
  cargo install --path "$SRC" --locked --force
}

cmd_install() {
  banner
  step "installation de θ $(source_version)"
  build_and_install
  ok "θ installé → $(command -v "$BIN_NAME" || echo "~/.cargo/bin/$BIN_NAME")"
  printf '\n  %sessaie :%s theta --help\n' "$C_DIM" "$C_RESET"
}

cmd_update() {
  banner
  if [[ -d "$SRC/.git" ]]; then
    step "récupération des changements (git pull)"
    if ! git -C "$SRC" pull --ff-only; then
      die "git pull a échoué (modifs locales ou branche divergente)"
    fi
  else
    warn "pas de dépôt git ici : mise à jour depuis les sources locales uniquement"
  fi
  local before after
  before="$(installed_version)"
  build_and_install
  after="$(installed_version)"
  if [[ "$before" == "$after" ]]; then
    ok "θ déjà à jour ($after)"
  else
    ok "θ mis à jour : $before → $after"
  fi
}

cmd_uninstall() {
  banner
  if ! command -v cargo >/dev/null 2>&1; then
    die "cargo introuvable, impossible de désinstaller via cargo"
  fi
  step "désinstallation de $BIN_NAME"
  cargo uninstall "$BIN_NAME" || warn "rien à désinstaller"
  ok "θ retiré"
  warn "données conservées : ~/.theta (réglages, sessions, auth.json). Supprime-les à la main si besoin."
}

cmd_status() {
  banner
  printf '  %ssource%s    %s (v%s)\n' "$C_DIM" "$C_RESET" "$SRC" "$(source_version)"
  printf '  %sinstallé%s  %s\n' "$C_DIM" "$C_RESET" "$(installed_version)"
  if command -v "$BIN_NAME" >/dev/null 2>&1; then
    printf '  %schemin%s    %s\n' "$C_DIM" "$C_RESET" "$(command -v "$BIN_NAME")"
  fi
  if command -v rtk >/dev/null 2>&1; then
    printf '  %srtk%s       présent\n' "$C_DIM" "$C_RESET"
  else
    printf '  %srtk%s       absent (optionnel)\n' "$C_DIM" "$C_RESET"
  fi
}

cmd_help() {
  banner
  cat <<EOF
  ${C_BOLD}commandes${C_RESET}
    ${C_CYAN}install${C_RESET}     compile et installe θ (défaut)
    ${C_CYAN}update${C_RESET}      git pull (si dépôt git) puis réinstalle
    ${C_CYAN}uninstall${C_RESET}   retire le binaire (garde ~/.theta)
    ${C_CYAN}status${C_RESET}      versions et chemins
    ${C_CYAN}help${C_RESET}        cette aide
EOF
}

case "${1:-install}" in
  install)   cmd_install ;;
  update)    cmd_update ;;
  uninstall) cmd_uninstall ;;
  status)    cmd_status ;;
  help|-h|--help) cmd_help ;;
  *) cmd_help; die "commande inconnue : $1" ;;
esac
