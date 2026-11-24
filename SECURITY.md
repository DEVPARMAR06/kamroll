# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue: use GitHub's **Report a vulnerability** button on the repository's Security tab. Include what you found, how to reproduce it, and which version you used. You will get a reply within a few days.

## What Kamroll protects

- The engine listens on `127.0.0.1` only, checks the `Host` header, and requires a random per-launch token on every `/api` call.
- It reads the iPhone through Apple's USB service using the pairing record Apple already stored on your computer. It never writes to the phone.
- It makes no network connections other than to an Ollama server on `127.0.0.1` if you turn smart tags on. There is no telemetry.
- Files are only ever renamed inside the backup folder, never deleted. Damaged files are moved to `.photovault/quarantine`.

## Out of scope

Anything that needs an attacker who already controls your computer or can read your pairing records and backup drive.
