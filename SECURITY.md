# Security Policy

Gunda is still under active development, but security reports are welcome.

## Reporting a vulnerability

Please don't report security vulnerabilities in a public issue.

Use GitHub's [Security Advisory](https://github.com/AghastyGD/gunda/security/advisories/new) to report the problem privately.

Include whatever is useful to understand and reproduce the issue, such as:

- the affected component
- reproduction steps
- expected and observed behavior
- possible impact
- a minimal proof of concept, if appropriate

Please don't include credentials, cookies, tokens, private URLs, or other unrelated sensitive information.

If the problem cannot be reported through GitHub Security Advisories, open a public issue asking for a private contact method without including details of the vulnerability.

## Scope

Security issues in Gunda may include things such as:

- unsafe handling of remote filenames or paths
- unintended file overwrite
- sensitive request data being exposed or logged
- unsafe handling of network input
- browser or local IPC boundaries
- unsafe external process execution

Gunda does not aim to bypass DRM systems such as Widevine, PlayReady, or FairPlay.

More details about the project's security boundaries are documented in
[the architecture overview](docs/architecture/overview.md#security-boundaries).