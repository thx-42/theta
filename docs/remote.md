# Remote control

`/remote` lets you drive theta from a browser. It goes through a relay server (theta-server), so the browser needs no direct access to the machine.

## Commands

- `/remote`: start the link for the current project and show the code to enter in the browser.
- `/remote status`: whether the link is up.
- `/remote stop`: close the link.

## Server

- Default URL: `https://theta.sputnk.net`, set in `[remote] url`.
- Env `THETA_REMOTE_URL` overrides the setting.
- The link runs in the theta daemon, so it stays up when the terminal closes.

## Limits

- Control requests (shutdown, reload, start or stop remote) are not allowed through the relay. Only local clients can run them.

## Troubleshooting

- Link fails: check the server URL and network, then `/remote status`.
- No code shown: run `/remote` again in the TUI.
