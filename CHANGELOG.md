# Changelog

## 0.3.1

### Nouveautés
- **Harnais plus léger.** Le jeu d'outils par défaut d'un agent sans liste `tools` passe de 17 à 9 outils (read, write, edit, bash, grep, job, todo, web_search, web_fetch, + task) : `ls`, `find`, `write_plan`, `ask`, `handoff` et `list_agents` ne sont envoyés qu'aux agents qui les listent dans `tools`. Plan et Auto sont inchangés.
- **Outil `job` unique.** `jobs`, `wait`, `job_output` et `job_kill` fusionnent en `job` avec `action` = `list`, `output`, `kill`, `wait`. Les anciens noms restent acceptés dans la liste `tools:` d'un agent.
- **Compaction plafonnée.** Nouveau réglage `compaction.max_tokens` (150000 par défaut, 0 = aucun plafond) : la compaction se déclenche au plus bas de `threshold` × fenêtre et de ce plafond, plutôt qu'à 800k tokens sur une fenêtre de 1M.
- **Coût MCP visible.** `/mcp` affiche le coût approximatif en tokens par requête de chaque serveur. Les outils MCP ne sont plus envoyés aux subagents dont l'agent n'a pas de liste `tools:` (listez `mcp__<serveur>` pour les leur donner).

### Corrections
- Sorties d'outils moins coûteuses : plafond en octets à 60 par ligne autorisée (≈ 18 Ko par défaut, au lieu de 80 Ko), `max_lines` à 300, `read` limité à ~450 lignes et 24 Ko avec l'indication `offset=` pour continuer, `web_fetch` à 12 000 caractères par défaut.
- Descriptions d'outils raccourcies (plus de détails internes rtk/ANSI) ; paramètre `effort` retiré de `task` (réglé par les agents et `[models]`).
- Prompts Plan et Auto dédoublonnés et raccourcis ; la règle `Build` « ne pose pas de question » est désormais vraie (`ask` n'est plus envoyé à Build).

## 0.3.0

### Nouveautés
- **Jobs de fond.** `bash` et `task` acceptent `background: true` et rendent la main avec un id. Nouveaux outils `job_output` et `job_kill`. `/jobs` (ou `alt+j`) liste les jobs de la session, affiche la sortie en direct (`entrée`) et arrête un job (`k`) pendant que l'agent continue. La fin d'un job est lue par l'agent à son prochain step.
- **Linter de fond.** Linters embarqués par langage (ruff, eslint, clippy, go vet, shellcheck…), complétés ou remplacés par `[linters]`. `/linters` indique ceux qui sont installés ; un linter absent est signalé dans `/jobs`. Le linter du fichier tourne en fond quand l'agent passe à un autre fichier ou termine son tour, et n'envoie un retour que s'il a quelque chose à dire.
- **Agent Auto.** Orchestrateur autonome : il planifie avec toi (questions via `ask`, validation du plan), puis exécute de bout en bout sans s'arrêter entre les étapes. Il n'écrit pas de code lui-même : des sous-agents `Build` écrivent, des sous-agents `Plan` relisent, il corrige jusqu'à ce que ce soit propre. S'il s'arrête avec des todos non terminés, il est relancé (3 fois max).
- **Délégation encadrée.** Un agent à outils read-only (Plan) ne peut spawner que des agents read-only ; seul un agent `delegate: all` (Auto) peut déléguer l'écriture. Un sous-agent ne peut jamais spawner de sous-agent.
- **Agents livrés mis à jour.** `~/.theta/agents/.shipped` suit la version livrée de Build, Plan et Auto. Un agent non modifié est remplacé par la nouvelle version ; un agent modifié est conservé, la nouvelle version est écrite en `<Agent>.md.new` et son diff est affiché une fois sur un terminal.
- **Hooks de message.** Section `[hooks]` : `pre_message` (peut rejeter le message ou ajouter du contexte) et `post_message` (à la fin du run).

## 0.2.0

### Nouveautés
- **Canaux stable et nightly.** `install.sh` demande le canal (`--stable` ou `--dev`). Stable suit la branche `main`, nightly suit `dev` et publie des prereleases `dev-v…` à chaque push. `theta update` et `install.sh update` suivent le canal installé. Le binaire est identique sur les deux canaux : le mode dev est un drapeau d'exécution (`~/.theta/dev`, `THETA_DEV=1` ou `theta channel dev`) qui affiche `(dev)` à côté du logo et active les détails de debug. En mode dev, `theta update` suit les releases stable et nightly (le plus grand numéro de build gagne).
- **Steering.** Pendant qu'un agent travaille, `Entrée` met le message en file d'attente (envoyé à la fin du run). `Ctrl+Entrée` injecte le message dans la conversation : l'agent le voit à son prochain step.
- **Arrêt en double Échap.** Deux `Échap` dans les 2 s arrêtent l'agent. Un run arrêté affiche `■ interrupted` au lieu de `✓ done`.
- **Plan par session.** L'outil `write_plan` écrit le plan dans `~/.theta/plan/<session>.md`. Chaque session a son fichier, les agents ne se marchent pas dessus.
- **Tabs restaurés.** Au redémarrage dans un projet, les tabs ouverts à la dernière fermeture sont rouverts. Les statuts non lus (`done`, `error`, `asked`) restent tant que le tab n'a pas été ouvert (`tabs.json`).
- **`!commande`.** Une ligne commençant par `!` lance la commande dans le shell courant, dans le dossier du projet. La sortie s'affiche comme une réponse de l'assistant.
- **Mise à jour du daemon.** `theta update` relance le daemon s'il est inactif, sinon il avertit qu'il faut le relancer à la main.
- **Muse Spark 1.3 (OpenCode Go).** Les modèles `muse-*` passent par l'API Responses, plus par chat completions.

### Corrections
- `/new` et le bouton de nouvel onglet ne créent plus de session vide en double.
- Un agent arrêté ne s'affiche plus comme terminé.
