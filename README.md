<div align="center">

```
        _   _          _
       | |_| |__   ___| |_ __ _
       | __| '_ \ / _ \ __/ _` |
       | |_| | | |  __/ || (_| |
        \__|_| |_|\___|\__\__,_|
```

**Harness de coding agent léger, écrit en Rust.**
petit · configurable · économe en tokens

[![release](https://img.shields.io/github/v/release/thx-42/theta?style=flat-square&color=8b5cf6&labelColor=07060b)](https://github.com/thx-42/theta/releases)
[![rust](https://img.shields.io/badge/rust-2024-c4b5fd?style=flat-square&labelColor=07060b&logo=rust&logoColor=c4b5fd)](https://www.rust-lang.org)
[![platforms](https://img.shields.io/badge/linux%20·%20macOS-x86__64%20·%20arm64-8b5cf6?style=flat-square&labelColor=07060b)](#installation)
[![providers](https://img.shields.io/badge/providers-20%2B-c4b5fd?style=flat-square&labelColor=07060b)](#providers)

[Installation](#installation) · [Providers](#providers) · [Agents](#agents) · [Tools](#tools) · [TUI](#tui) · [Configuration](#configuration)

```bash
curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh | bash
```

`theta login` ──▶ `theta` ──▶ `theta -p "…"`

</div>

---

## ✦ Pourquoi θ

|  |  |
|---|---|
| **TUI + daemon** | Client plein écran et daemon en arrière-plan. Ferme ton shell : rien ne s'arrête. Plusieurs terminaux s'attachent à la même session, en direct. |
| **Sobre en tokens** | Relecture intelligente, chemins relatifs, prompt caching Anthropic, compaction auto à 80 % de la fenêtre. |
| **Agents** | `Build` code, `Plan` planifie sans coder, `Auto` orchestre de bout en bout avec des sous-agents. Les tiens sont des fichiers `.md`. |
| **SOUL & skills** | `SOUL.md` s'applique à tous les agents. Les skills s'injectent avec `$nom`, ou tout seuls en `auto`. |
| **MCP natif** | Serveurs stdio + HTTP, OAuth 2.1 PKCE, tools `mcp__serveur__tool`. |
| **Onglets & arbre** | Sessions en onglets, conversation en arbre append-only : bifurque, réédite, ne perds rien. |
| **En fond** | Jobs shell et subagents en arrière-plan, linter automatique, hooks de message. |

```
┌─────┐      ┌────────┐         tokens ▓▓▓░░░░░ 30%
│ tui │ <──> │ daemon │         cache on · compact 80%
└─────┘      └────────┘
  ~/.theta/theta.sock           tabs [1] [2●] [3]  ·  alt+n
```

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh | bash
```

Le script demande : binaire pré-compilé (release GitHub, sha256 vérifié) ou compilation depuis les sources. Installe dans `~/.local/bin` (`THETA_PREFIX` pour changer). Chaque push sur `main` publie une release (Linux/macOS, x86_64/arm64).

| Méthode | Commande |
|---|---|
| Binaire | `curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh \| bash` |
| Sources | `curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh \| bash -s -- install --source` (ou `./install.sh install --source` depuis un clone ; nécessite `cargo` + `git`) |

Si `~/.local/bin` n'est pas dans ton `PATH` : `export PATH="$HOME/.local/bin:$PATH"`.

### Premiers pas

```bash
theta login                        # choisir un provider (abonnement OAuth ou clé API)
theta                              # TUI plein écran
theta -p "explique src/main.rs"    # mode print (non interactif)
```

Options : `-c` reprend la dernière session du dossier, `-r <id>` une session précise, `-m provider/model`, `-a <agent>`.
Sous-commandes : `login [provider]`, `logout <provider>`, `models [filtre]`, `agents`, `refresh` (catalogue models.dev), `update [--check] [--force]`.

<details>
<summary><b>Canaux, mises à jour, désinstallation</b></summary>

<br>

Canaux : `stable` (branche `main`, releases) ou `nightly` (branche `dev`, prereleases `dev-v…` publiées à chaque push sur `dev`). Le script demande le canal ; `--stable` / `--dev` le fixent sans menu. Le canal est gardé dans `~/.theta/install` et `theta update` suit ce canal. Un binaire nightly affiche `(dev)` à côté du logo.

Mise à jour depuis θ lui-même : `theta update` cherche la dernière release, vérifie le sha256 et remplace le binaire (`--check` pour seulement regarder, `--force` pour réinstaller ou remplacer un build local). Si le daemon tourne, `theta daemon stop` le relance sur la nouvelle version.

Les agents livrés (`Build`, `Plan`, `Auto`) suivent la mise à jour : un agent non modifié est remplacé ; un agent que tu as édité est conservé, la nouvelle version est écrite en `<Agent>.md.new` et son diff est affiché une fois sur un terminal.

Suivi des mises à jour (depuis un clone, ou `curl … | bash -s -- <cmd>`) :

```bash
./install.sh status      # version installée vs dernière disponible
./install.sh update      # met à jour selon la méthode d'origine
./install.sh uninstall
```

</details>

## Providers

| Accès | Providers |
|---|---|
| 🔑 **Abonnement (OAuth)** | Anthropic Claude Pro/Max, ChatGPT (Codex), GitHub Copilot |
| 🗝️ **Clé API** | Anthropic, OpenAI, Google Gemini, OpenRouter, Groq, xAI, Mistral, DeepSeek, Cerebras, Together, Fireworks, Z.ai, Moonshot, Hugging Face, OpenCode Zen, OpenCode Go, OpenCode Go (Anthropic), Ollama Cloud |
| 🏠 **Local** | Ollama, LM Studio, ou tout endpoint OpenAI-compatible |

Connexion : `/login` dans la TUI (ou `theta login`) → provider → méthode (abonnement ou clé API). OpenAI regroupe l'abonnement ChatGPT (Codex) et la clé API. Pour l'OAuth, le navigateur s'ouvre ; sinon colle le code dans la fenêtre (`ctrl+y` copie l'URL). Aussi : variables d'env (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, …). Les identifiants sont stockés dans `~/.theta/auth.json` (mode 0600).
Catalogue des modèles (contexte, prix) : [models.dev](https://models.dev), mis en cache 24 h.

<details>
<summary><b>Provider personnalisé</b></summary>

```toml
[providers.local]
api = "openai-chat"          # openai-chat | openai-responses | anthropic | google
base_url = "http://localhost:8080/v1"
api_key_env = "LOCAL_KEY"    # ou api_key = "..."
models = ["qwen3-coder"]
context = 128000
```

</details>

## Agents

| Agent | Rôle |
|---|---|
| **Build** | Agent par défaut : lit, édite, lance et vérifie le code. |
| **Plan** | Clarifie, trouve les angles morts, planifie. Lecture seule : ne code pas, passe la main (`handoff`). |
| **Auto** | Orchestrateur autonome. Planifie avec toi et fait valider le plan, puis exécute de bout en bout : des sous-agents `Build` écrivent, des sous-agents `Plan` relisent, il corrige jusqu'à ce que ce soit propre. N'écrit pas de code lui-même. |

```
        validation du plan
 user ◀──────────────────────▶ Auto
                                │ task
                  ┌─────────────┴─────────────┐
                  ▼                           ▼
             Build (écrit)             Plan (relit)
```

**Règles de délégation**

- Un agent à outils read-only (comme `Plan`) ne peut spawner que des agents read-only. Seul un agent `delegate: all` (comme `Auto`) peut déléguer l'écriture.
- Seul le thread principal spawne des sous-agents : un sous-agent ne peut jamais en lancer d'autres, pas de récursion.
- `Auto` est relancé s'il s'arrête avec des todos non terminés (3 fois max).

### Définir un agent

```markdown
---
name: Review
description: Revue de code stricte   # sa tâche
can: lire le code, lister les bugs   # optionnel, montré au LLM qui choisit un handoff
cannot: modifier des fichiers        # optionnel
model: anthropic/claude-opus-5-5     # optionnel
effort: high                         # optionnel
subagent_model: anthropic/claude-haiku-5-5   # optionnel, modèle des subagents lancés par cet agent
tools: read, grep, find, ls, task    # optionnel, défaut = tous
delegate: all                        # optionnel, peut spawner des agents qui écrivent
---
Tu relis le code et listes les bugs, du plus grave au moins grave.
```

Modèle et effort par agent, dans `settings.toml` (global ou projet) :

```toml
[agents.Build]
model = "anthropic/claude-haiku-5-5"
effort = "medium"

[agents.Plan]
model = "anthropic/claude-sonnet-5-5"
effort = "high"
```

Ces valeurs se règlent aussi dans `/settings`, onglet **Models** (modèle et effort par agent, modèles compaction / titre / subagent). `tab` change d'onglet dans ce panneau.

Priorité : `-m` / `--agent` en ligne de commande > `[agents.<nom>]` > frontmatter de l'agent > `model` / `effort` globaux. Valable au démarrage, au changement d'agent (`tab`) et pour les subagents lancés par `task` avec `agent`.

## Tools

| Tool | Rôle |
|---|---|
| `read` `write` `edit` `ls` `find` `grep` | fichiers ; `grep`/`find` respectent `.gitignore`, résultats groupés |
| `bash` | shell ; commandes réécrites via **rtk** si installé, sortie compactée (ANSI, répétitions, troncature au milieu + log complet sauvegardé). `background: true` lance la commande en job de fond |
| `todo` | liste de tâches affichée au-dessus de la saisie |
| `web_search` | Exa MCP par défaut (sans clé) ; `firecrawl` / `brave` / `tavily` / `duckduckgo` ; repli DuckDuckGo |
| `web_fetch` | page → texte compact |
| `task` | subagent à contexte neuf, renvoie seulement son rapport final. `background: true` : il tourne en job de fond |
| `job_output` `job_kill` | sortie / arrêt d'un job de fond (shell, lint ou subagent) par id |
| `list_agents` | liste les agents (tâche, `can`, `cannot`) pour choisir une cible de handoff. Réservé aux agents qui le listent dans `tools` |
| `handoff` | propose de passer à un autre agent (Plan → Build) ; si oui, Build enchaîne dans la même conversation. Réservé aux agents qui le listent dans `tools` |
| `ask` | pose une question : choix multiple, oui/non ou texte libre (réponse libre toujours possible) |

Économies de tokens : relecture d'un fichier inchangé → simple notice ; chemins relatifs ; diff visibles dans l'UI mais pas renvoyés au modèle ; prompt caching Anthropic (tools, system, 2 derniers tours).

<details>
<summary><b>Jobs de fond, linter et hooks</b></summary>

<br>

**Jobs.** Les shells (`bash` avec `background: true`), les subagents (`task` avec `background: true`) et les linters tournent à côté du thread principal. `/jobs` (ou `alt+j`) liste les jobs de la session : `entrée` affiche la sortie en direct, `k` arrête le job. La barre d'état affiche `⚙ n` tant que des jobs tournent. Quand un job se termine, son résultat est mis en file et l'agent le lit à son prochain step (ou au prochain message) ; aucun tour n'est lancé automatiquement. Les jobs ne survivent pas à l'arrêt du daemon.

**Linter.** Quand l'agent quitte un fichier modifié pour en modifier un autre (ou termine son tour), le linter du type du premier fichier tourne en fond. Sans retour (code 0, sortie vide), rien n'est envoyé à l'agent. Theta embarque une liste de linters par langage (ruff, eslint, clippy, go vet, shellcheck, rubocop, yamllint…) ; `[linters]` la complète ou la remplace, `""` désactive une extension. `{file}` est remplacé par le chemin. `/linters` montre pour chaque linter s'il est installé (`✗ … absent` sinon) ; un linter absent est signalé une fois dans `/jobs` et le lint est ignoré.

```toml
[linters]
py = "ruff check {file}"
rs = "cargo clippy --quiet"
```

**Hooks.** Commandes shell autour de chaque message de l'utilisateur, dans le dossier du projet, timeout 30 s. Variables : `THETA_SESSION`, `THETA_CWD`, `THETA_HOOK`, et `THETA_STATUS` (`done` ou `error`) pour `post_message`. Un run interrompu ne déclenche pas `post_message`.

```toml
[hooks]
pre_message = ["./scripts/check-message.sh"]   # message sur stdin ; exit != 0 le rejette ; stdout est ajouté comme contexte
post_message = ["notify-send theta"]           # dernière réponse sur stdin ; sortie ignorée
```

> [!WARNING]
> Ces commandes viennent de `settings.toml`, y compris celui du projet (`<projet>/.theta/settings.toml`) : elles s'exécutent avec tes droits, comme les serveurs MCP. Ne lance pas un agent dans un dépôt dont tu ne fais pas confiance aux réglages.

</details>

## Configuration

### Dossiers `.theta`

```
~/.theta/                     global
  settings.toml               réglages
  SOUL.md                     consignes appliquées à TOUS les agents
  agents/Build.md             agent par défaut
  agents/Plan.md              clarifie et planifie, sans coder, puis passe la main à Build
  agents/Auto.md              orchestre Build et Plan de bout en bout
  agents/*.md                 agents globaux  → affichés « (global) »
  sessions/<projet>/*.jsonl   sessions
<projet>/.theta/              local au projet (racine = dossier contenant .theta ou .git)
  settings.toml               surcharge les réglages globaux
  SOUL.md                     ajouté après le SOUL global
  agents/*.md                 agents locaux   → affichés « (local) », prioritaires sur un global du même nom
```

Le prompt système = SOUL global + SOUL projet + prompt de l'agent + environnement + `AGENTS.md`/`CLAUDE.md` du projet.

### MCP

Serveurs MCP (tools uniquement) dans `settings.toml`, global ou projet :

```toml
[mcp.fs]                                   # stdio
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
[mcp.notion]                               # HTTP (streamable)
url = "https://mcp.notion.com/mcp"
headers = { Authorization = "Bearer ${NOTION_TOKEN}" }   # optionnel ; `${VAR}` lu dans l'environnement
```

- Les tools s'appellent `mcp__<serveur>__<tool>`. Un agent sans `tools:` les reçoit tous ; sinon listez `mcp__*` ou `mcp__<serveur>` dans `tools`.
- `/mcp` liste les serveurs (● connecté, ○ login requis, ✗ erreur) ; `/mcp login|logout|reconnect <serveur>` (aussi `theta mcp list|login|logout`). Le login est OAuth 2.1 (PKCE, enregistrement dynamique) ; tokens dans `~/.theta/mcp-auth.json` (0600), rafraîchis automatiquement.

### Skills

Fichiers de consignes, globaux (`~/.theta/skills/`) ou locaux (`<projet>/.theta/skills/`, prioritaires). Un skill est `nom.md` ou `nom/SKILL.md`, frontmatter optionnel :

```markdown
---
name: caveman
description: réponses terses
auto: true        # injecté dans le prompt système de tous les agents
---
Réponds court, sans formules de politesse.
```

- `$nom` dans un message injecte le skill dans ce message (`/skills` liste les skills, `*` = auto).
- Rendre un skill automatique sans toucher au fichier : `[skills] auto = ["caveman", "ponytail"]` dans `settings.toml` (global ou projet).

Skills fournis dans `.theta/skills/` (actifs quand theta tourne dans ce repo) : `$caveman`, `$ponytail`, `$review`, `$debug`, `$commit`. Copie-les dans `~/.theta/skills/` pour les avoir partout.

### Contexte

Compaction automatique au-delà de `compaction.threshold` (80 % par défaut) de la fenêtre : les anciens tours sont résumés par le **modèle de compaction**, les ~20k derniers tokens sont gardés tels quels. `/compact` force. Chaque tâche a son modèle :

```toml
[models]
compaction = "anthropic/claude-haiku-5-5"
title = "anthropic/claude-haiku-5-5"   # nommage auto des sessions
subagent = ""                          # "" = modèle principal
```

Un modèle de tâche sans identifiants retombe sur le modèle principal.

## Serveur / client

```
 terminal A ─┐
 terminal B ─┼──▶ daemon ──▶ sessions · runs agent · MCP
 theta -r id ┘     ~/.theta/theta.sock
```

`theta` est un client TUI. Au premier lancement il démarre un daemon en arrière-plan (`theta daemon`, socket `~/.theta/theta.sock`, log dans `~/.theta/cache/logs/daemon.log`). Le daemon possède les sessions, les runs de l'agent et les connexions MCP : fermer le TUI ne coupe rien.

- Plusieurs terminaux peuvent s'attacher à la même session (`theta -c`, `theta -r <id>`, `/resume`) : messages, streaming, questions `ask`, todos, modèle et agent sont synchronisés en direct.
- Un client qui se reconnecte (ou une coupure réseau/terminal) récupère l'historique et le step en cours.
- `theta daemon stop` arrête le daemon (les runs en cours sont perdues). `-p/--print` reste en process, sans daemon.

## TUI

| Touche | Action |
|---|---|
| `enter` / `alt+enter`, `ctrl+j` | envoyer / nouvelle ligne |
| `esc` | interrompre l'agent |
| `tab` / `shift+tab` | agent suivant / effort suivant |
| `ctrl+p` `ctrl+t` `ctrl+r` | modèle · arbre de conversation · sessions |
| `ctrl+o` | sortie **full** (tools, thinking) ↔ **compact** (réponse finale + ligne `n read · n write · n cmd · n tools`) |
| `pgup` `pgdn` `shift+↑↓` molette | défiler |
| `alt+n` / `alt+w` | nouvel onglet de session / fermer l'onglet |
| `alt+1` … `alt+9` | aller à l'onglet n |
| `alt+pgdn` / `alt+pgup` | onglet suivant / précédent |
| clic sur un onglet · sur `×` · sur `+` | aller à / fermer / ouvrir un onglet |

`/` affiche les commandes et `$` les skills : `↑↓` pour choisir, `tab` pour compléter, `enter` pour lancer (sur un `$skill` incomplet, `enter` complète d'abord). Le sélecteur de modèles (`ctrl+p`) ne liste que les providers connectés, groupés par provider.

Commandes : `/new /resume /session /tree /jobs /linters /model /agent /effort /settings /verbose /compact /btw /title /login /logout /copy /help /quit`.

<details>
<summary><b>Onglets</b></summary>

<br>

`/session` ouvre une nouvelle session dans un onglet (même agent et modèle), `/session close` ferme l'onglet courant (un run en cours est interrompu), `/session <n>` y va. Chaque onglet a sa propre session ; les onglets en arrière-plan continuent de tourner. La barre est toujours visible, avec un spinner pendant qu'un agent travaille, `●` quand un onglet en arrière-plan a fini (ou attend une réponse), `!` en cas d'erreur. Les raccourcis sont en `alt` : `ctrl+tab` et `ctrl+shift+t` sont pris par les émulateurs (Warp…). Sur macOS Terminal / iTerm, `alt` doit envoyer Meta (Option comme Alt/Meta dans les réglages du terminal).

`tab_orientation = "horizontal"` (défaut), `"vertical"` (barre latérale) ou `"hidden"` (barre cachée) dans `settings.toml` ou `/settings`. `/session hide` cache la barre, `/session show` la remet en horizontal (ou `alt+b` pour basculer). En vertical, glisse le bord droit de la barre pour changer sa largeur ; chaque onglet est une carte de 3 lignes : titre, agent · modèle, étape en cours ou statut. Largeur de la barre : `tab_width` (14 à 60, défaut 24), réglable dans `/settings` ou avec `/session width <n>`.

</details>

<details>
<summary><b>Arbre de conversation</b></summary>

<br>

`ctrl+t` : une ligne par message utilisateur, les branches n'apparaissent qu'aux bifurcations. `enter` reprend après ce tour (le prochain message crée une branche), `e` réédite le message pour créer une branche sœur. Tout est conservé dans le fichier de session (JSONL append-only, chaque entrée pointe vers son parent).

</details>

## Notes

- L'OAuth Claude Pro/Max reprend le flux Claude Code (identité Claude Code). Vérifie que cet usage respecte les conditions d'Anthropic pour ton compte.
- Les modèles Claude 4.6+ utilisent le thinking `adaptive` et `output_config.effort` ; les blocs de thinking ne sont renvoyés qu'au modèle qui les a produits.

---

<div align="center">

<sub>θ — écrit en Rust · <a href="CHANGELOG.md">changelog</a> · <a href="https://github.com/thx-42/theta/releases">releases</a></sub>

</div>
