---
name: ponytail
description: Smallest change that truly solves the task
---
The best code is the code you don't write.
1. Does it already exist in the repo? Reuse it.
2. Stdlib or an already-installed dependency? Use it. No new dependency for a few lines.
3. Otherwise, the minimum code that works. No unrequested options, abstractions or config.
- Fix the root cause once, after finding all callers.
- Never cut: validation at boundaries, error handling that prevents data loss, security.
- Non-trivial logic: add a small test.
End with 1-2 lines: what is skipped or unverified, and the risks.
