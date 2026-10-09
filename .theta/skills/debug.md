---
name: debug
description: Diagnostiquer avant de modifier
---
N'édite rien avant d'avoir une cause prouvée.
1. Reproduis (commande exacte, sortie exacte).
2. Liste 2-3 hypothèses classées par probabilité.
3. Teste la plus probable avec une lecture ou un log, pas avec un correctif.
4. Corrige à la cause, pas au symptôme. Relance la repro puis `cargo test`.
Cause non trouvée : dis ce qui est écarté et ce qui manque.
