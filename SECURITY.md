# Security Policy

## Project status

Gunda has no released or supported version yet. The current repository is a
bootstrap binary and does not implement downloads, browser integration, local
IPC, or credential storage.

Security reports about the repository and its dependencies are still welcome.

## Reporting a vulnerability

Do not include exploit details, credentials, private URLs, cookies, tokens, or
other sensitive data in a public issue.

Use GitHub's
[private vulnerability reporting form](https://github.com/AghastyGD/gunda/security/advisories/new)
if it is available. If GitHub does not offer the form, open a public issue that
asks the maintainer to establish private contact and contains no sensitive
details.

Include enough non-secret information to reproduce and assess the problem:

- the affected commit or version;
- the affected component;
- the expected and observed behavior;
- reproduction steps or a minimal proof of concept;
- the likely impact;
- any known mitigation.

The project does not currently promise a response or disclosure timeline. A
timeline will be added when the maintainer can support one consistently.

## Scope

Relevant reports include unsafe handling of network input, filenames or paths,
credential disclosure, unintended file overwrite, manifest resource exhaustion,
native messaging or local IPC exposure, and unsafe external process invocation.

Gunda will not implement DRM circumvention. Reports that require defeating
Widevine, PlayReady, FairPlay, or another DRM system are outside the intended
product boundary.

The accepted trust boundaries and non-negotiable security constraints are
documented in the [architecture overview](docs/architecture/overview.md#security-boundaries).
