#!/usr/bin/env bash
# θ theta — installeur / mise à jour / désinstallation
# usage: install.sh [install [--release|--source]|update|uninstall|status|help]
# one-liner: curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh | bash

set -euo pipefail

SRC=""
if [[ -n "${BASH_SOURCE[0]:-}" && -f "$(dirname "${BASH_SOURCE[0]}")/Cargo.toml" ]]; then
  SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
BIN_NAME="theta"
REPO="${THETA_REPO:-thx-42/theta}"
PREFIX="${THETA_PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
STATE="$HOME/.theta/install"

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
  command -v cargo >/dev/null 2>&1 ||
    die "cargo introuvable. Installe Rust : curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
}

# état d'installation : method=release|source, ref=<tag release | sha git>
state_get() { [[ -f "$STATE" ]] && sed -n "s/^$1=//p" "$STATE" | head -n1 || true; }
state_set() {
  mkdir -p "$(dirname "$STATE")"
  printf 'method=%s\nref=%s\n' "$1" "$2" > "$STATE"
}

installed_version() {
  if [[ -x "$BIN_DIR/$BIN_NAME" ]]; then
    "$BIN_DIR/$BIN_NAME" --version 2>/dev/null | head -n1 || echo "inconnue"
  else
    echo "absent"
  fi
}

detect_target() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os-$arch" in
    Linux-x86_64)               echo x86_64-unknown-linux-gnu ;;
    Linux-aarch64|Linux-arm64)  echo aarch64-unknown-linux-gnu ;;
    Darwin-arm64)               echo aarch64-apple-darwin ;;
    Darwin-x86_64)              echo x86_64-apple-darwin ;;
    *) die "plateforme non supportée : $os $arch (utilise la compilation depuis les sources)" ;;
  esac
}

latest_release() {
  curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1
}

remote_head() { git ls-remote "https://github.com/$REPO" HEAD | cut -f1; }

install_release() {
  command -v curl >/dev/null || die "curl requis"
  local target tag tmp url want got
  target="$(detect_target)"
  tag="$(latest_release)"
  [[ -n "$tag" ]] || die "aucune release trouvée sur github.com/$REPO"
  step "téléchargement de $tag ($target)"
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' RETURN
  url="https://github.com/$REPO/releases/download/$tag"
  curl -fsSL "$url/theta-$target.tar.gz" -o "$tmp/theta.tar.gz"
  curl -fsSL "$url/checksums.txt" -o "$tmp/checksums.txt"
  want="$(grep " theta-$target.tar.gz\$" "$tmp/checksums.txt" | cut -d' ' -f1)"
  if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/theta.tar.gz" | cut -d' ' -f1)"
  else got="$(shasum -a 256 "$tmp/theta.tar.gz" | cut -d' ' -f1)"; fi
  [[ -n "$want" && "$want" == "$got" ]] || die "checksum invalide"
  tar -xzf "$tmp/theta.tar.gz" -C "$tmp"
  mkdir -p "$BIN_DIR"
  install -m 755 "$tmp/theta" "$BIN_DIR/$BIN_NAME"
  state_set release "$tag"
}

install_source() {
  need_cargo
  local ref
  if [[ -n "$SRC" ]]; then
    step "compilation depuis $SRC"
    cargo install --path "$SRC" --locked --force --root "$PREFIX"
    ref="$(git -C "$SRC" rev-parse HEAD 2>/dev/null || echo local)"
  else
    command -v git >/dev/null || die "git requis"
    step "compilation depuis github.com/$REPO"
    cargo install --git "https://github.com/$REPO" --locked --force --root "$PREFIX"
    ref="$(remote_head)"
  fi
  state_set source "$ref"
}

ask_method() {
  local ans
  if [[ ! -r /dev/tty ]]; then echo release; return; fi
  {
    printf '%sComment installer θ ?%s\n' "$C_BOLD" "$C_RESET"
    printf '  %s1)%s binaire pré-compilé (release GitHub, rapide)\n' "$C_CYAN" "$C_RESET"
    printf '  %s2)%s compiler depuis les sources (cargo)\n' "$C_CYAN" "$C_RESET"
    printf 'choix [1] : '
  } >/dev/tty
  read -r ans </dev/tty || ans=""
  case "${ans:-1}" in 2|s|source) echo source ;; *) echo release ;; esac
}

path_hint() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR n'est pas dans ton PATH. Ajoute : export PATH=\"$BIN_DIR:\$PATH\"" ;;
  esac
}

cmd_install() {
  banner
  local method="${1:-}"
  [[ -n "$method" ]] || method="$(ask_method)"
  case "$method" in
    release) install_release ;;
    source)  install_source ;;
    *) die "méthode inconnue : $method" ;;
  esac
  ok "θ installé ($method) → $BIN_DIR/$BIN_NAME"
  path_hint
  printf '\n  %sessaie :%s theta --help\n' "$C_DIM" "$C_RESET"
}

cmd_update() {
  banner
  local method cur new
  method="$(state_get method)"; cur="$(state_get ref)"
  [[ -n "$method" ]] || die "θ n'a pas été installé via ce script : lance « install »"
  case "$method" in
    release) new="$(latest_release)" ;;
    source)
      if [[ -n "$SRC" && -d "$SRC/.git" ]]; then
        git -C "$SRC" pull --ff-only || die "git pull a échoué (modifs locales ou branche divergente)"
        new="$(git -C "$SRC" rev-parse HEAD)"
      else
        new="$(remote_head)"
      fi ;;
    *) die "état d'installation corrompu ($STATE)" ;;
  esac
  [[ -n "$new" ]] || die "impossible de déterminer la dernière version"
  if [[ "$cur" == "$new" ]]; then
    ok "θ déjà à jour ($method, ${cur:0:12})"
    return
  fi
  step "mise à jour ($method) : ${cur:0:12} → ${new:0:12}"
  if [[ "$method" == release ]]; then install_release; else install_source; fi
  ok "θ mis à jour"
}

cmd_uninstall() {
  banner
  step "désinstallation de $BIN_NAME"
  rm -f "$BIN_DIR/$BIN_NAME" "$STATE"
  ok "θ retiré"
  warn "données conservées : ~/.theta (réglages, sessions, auth.json). Supprime-les à la main si besoin."
}

cmd_status() {
  banner
  local method cur latest=""
  method="$(state_get method)"; cur="$(state_get ref)"
  printf '  %sinstallé%s  %s\n' "$C_DIM" "$C_RESET" "$(installed_version)"
  printf '  %schemin%s    %s\n' "$C_DIM" "$C_RESET" "$BIN_DIR/$BIN_NAME"
  printf '  %sméthode%s   %s (%s)\n' "$C_DIM" "$C_RESET" "${method:-inconnue}" "${cur:0:12}"
  case "$method" in
    release) latest="$(latest_release 2>/dev/null || true)" ;;
    source)  latest="$(remote_head 2>/dev/null || true)" ;;
  esac
  if [[ -z "$latest" ]]; then return; fi
  if [[ "$latest" != "$cur" ]]; then
    printf '  %sdispo%s     %s %s(lance « update »)%s\n' "$C_DIM" "$C_RESET" "${latest:0:12}" "$C_YELLOW" "$C_RESET"
  else
    printf '  %sdispo%s     à jour\n' "$C_DIM" "$C_RESET"
  fi
}

cmd_help() {
  banner
  cat <<EOF
  ${C_BOLD}commandes${C_RESET}
    ${C_CYAN}install${C_RESET} [--release|--source]   installe θ (menu interactif sans option)
    ${C_CYAN}update${C_RESET}      installe la dernière release / les derniers commits
    ${C_CYAN}uninstall${C_RESET}   retire le binaire (garde ~/.theta)
    ${C_CYAN}status${C_RESET}      version installée et mise à jour disponible
    ${C_CYAN}help${C_RESET}        cette aide

  ${C_DIM}env : THETA_REPO (défaut $REPO), THETA_PREFIX (défaut ~/.local)${C_RESET}
EOF
}

case "${1:-install}" in
  install)
    case "${2:-}" in
      --release) cmd_install release ;;
      --source)  cmd_install source ;;
      "")        cmd_install ;;
      *) die "option inconnue : $2" ;;
    esac ;;
  update)    cmd_update ;;
  uninstall) cmd_uninstall ;;
  status)    cmd_status ;;
  help|-h|--help) cmd_help ;;
  *) cmd_help; die "commande inconnue : $1" ;;
esac
