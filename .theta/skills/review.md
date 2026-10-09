---
name: review
description: Revue de code d'un diff, une ligne par problème
---
Relis le diff (`git diff`, puis les fichiers appelants si besoin). Un problème par ligne :
`chemin:ligne: gravité: problème. correctif.`
Cherche dans l'ordre : bugs de logique, perte de données/sécurité, cas limites (vide, unicode, I/O en erreur), `unwrap`/panics sur entrée utilisateur, code mort ou dupliqué, tests manquants.
Ignore le style que `cargo fmt`/`clippy` règlent. Pas de compliments. Rien trouvé : dis-le en une ligne.
