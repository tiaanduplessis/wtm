# Security

This project is in early development. Once releases exist, security fixes are intended
for the latest release; older versions do not have a separate maintenance commitment.

## Reporting a vulnerability

Use **Report a vulnerability** in the repository's GitHub Security tab to contact the
maintainer privately. This route requires private vulnerability reporting to be enabled;
it is a first-publication check in [docs/releasing.md](docs/releasing.md).

If that option is unavailable, open an issue asking for a private contact route. Do not
put exploit details, credentials, private paths, or proprietary content in a public issue.
No response-time guarantee is offered.

Include the affected version, OS, Git version, impact, and a minimal reproduction using
disposable repositories. Never run a cleanup reproduction against valuable worktrees.

## Scope

Relevant reports include unintended deletion, repository identity confusion, command
injection, terminal escape injection, credential disclosure, unsafe installers, and
release artifact substitution. Normal operations follow Git's configuration, hooks,
filters, and transports; use the tool only on repositories whose Git configuration you
trust. This is not a sandbox for hostile repositories.

Checksums detect corrupted or mismatched release downloads. They are distributed with
the archives through GitHub and are not independent signatures or attestations.
