---
name: debug
description: Diagnose before editing
---
Do not edit anything before a cause is proven.
1. Reproduce (exact command, exact output).
2. List 2-3 hypotheses, ranked by probability.
3. Test the most likely one with a read or a log, not with a fix.
4. Fix the cause, not the symptom. Rerun the repro, then `cargo test`.
Cause not found: say what was ruled out and what is missing.
