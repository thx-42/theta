---
name: ponytail
description: Plus petit changement qui résout vraiment la tâche
---
Le meilleur code est celui qu'on n'écrit pas.
1. Existe-t-il déjà dans le repo ? Réutilise-le.
2. Stdlib ou dépendance déjà installée ? Utilise-la. Aucune nouvelle dépendance pour quelques lignes.
3. Sinon, le minimum de code qui marche. Aucune option, abstraction ou config non demandée.
- Corrige la cause racine une fois, après avoir cherché tous les appelants.
- Ne coupe jamais : validation aux frontières, gestion d'erreur évitant la perte de données, sécurité.
- Logique non triviale : ajoute un petit test.
Termine par 1-2 lignes : ce qui est ignoré ou non vérifié, et les risques.
