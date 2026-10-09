# Changelog

## 0.2.0

### Nouveautés
- **Canaux stable et nightly.** `install.sh` demande le canal (`--stable` ou `--dev`). Stable suit la branche `main`, nightly suit `dev` et publie des prereleases `dev-v…` à chaque push. `theta update` et `install.sh update` suivent le canal installé. Un binaire nightly affiche `(dev)` à côté du logo.
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
