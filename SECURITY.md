# Security

## Reporting a problem

Please report security issues privately using GitHub's **Report a vulnerability** button on this
repository's **Security** tab, not a public issue. Include what you found, how to reproduce it, and
the version or commit. You'll get a reply within a week.

## Supported versions

Only the latest release gets security fixes.

## Scope

CapraLink is designed for a trusted home or office network. The design, threat model and what is
stored or broadcast are described in [ARCHITECTURE.md](ARCHITECTURE.md). In short:

- **Pairing:** needs a 6-digit PIN that is only valid for 2 minutes after you press Show. Wrong
  guesses lock pairing out for longer and longer.
- **Sessions and audio:** encrypted end to end between paired computers, with fresh keys every session.
- **Remote configuration:** off unless you turn it on.
- **Local control:** the window and the background engine authenticate each other through a token
  that only your user account can read.

## Known advisories in dependencies

`cargo audit` reports no vulnerabilities. It does flag these warnings, all from the Linux
GTK/WebKit stack that Tauri uses:

- [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429): `glib` iterator
  unsoundness. CapraLink doesn't call the affected API itself, and it goes away when Tauri moves
  to a newer GTK binding.
- `proc-macro-error` is unmaintained. It's only used at build time.
- `yoke-derive 0.8.3` is yanked. It's a build-time macro, pinned by the lockfile until a dependency update.

These get re-checked whenever dependencies are updated.
