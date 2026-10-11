---
name: review
description: Review a diff, one line per problem
---
Read the diff (`git diff`, then the calling files if needed). One problem per line:
`path:line: severity: problem. fix.`
Look in this order: logic bugs, data loss/security, edge cases (empty, unicode, I/O errors), `unwrap`/panics on user input, dead or duplicated code, missing tests.
Ignore style that `cargo fmt`/`clippy` handle. No compliments. Nothing found: say so in one line.
