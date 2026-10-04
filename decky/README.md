# CapraLink for Steam Deck (Decky Loader plugin)

A Quick Access Menu panel for Game Mode: see who you are connected to, connect or disconnect a
paired computer, and switch Music Mode. It drives the CapraLink AppImage through its
command-line options (`capralink --status`, `--connect`, `--disconnect`, `--music`; see the
main README), so it does nothing on its own.

## Use

1. In Desktop Mode, install the CapraLink AppImage in `~/Applications/CapraLink/`, pair your
   computers, and let the engine run in the background (Settings, "Run in background").
2. Install [Decky Loader](https://decky.xyz/), then install the plugin zip (below).
3. In Game Mode, open the Quick Access Menu and pick the CapraLink tab.

If CapraLink isn't found, or its engine isn't running, the panel says so.

## Build and install

Needs Node.js 18+ and npm.

```bash
cd decky
npm ci
npm run typecheck && npm run build
```

CI builds `CapraLink.zip` on every push (artifact `capralink-decky-plugin`). To make it by hand,
zip a folder named `CapraLink` holding `dist/index.js`, `main.py`, `plugin.json`, `package.json`
and `LICENSE`.

To install on the Deck: copy the zip over, then in Game Mode open Decky settings, Developer, and
use "Install plugin from ZIP" (turn on Developer mode first). Or unpack it into
`~/homebrew/plugins/` and restart Decky (`sudo systemctl restart plugin_loader`).

## How the backend runs the CLI

`main.py` looks for the newest `*.AppImage` in `$DECKY_USER_HOME/Applications/CapraLink/` and runs
it with `HOME` set to the Deck user's home, 10 s timeout. `plugin.json` has no `_root` flag, so
Decky runs the backend as the Deck user; if it is ever run as root, the call goes through
`runuser -u $DECKY_USER`. Each panel refresh (every 2 s while open) starts the AppImage once.

## Publishing to the Decky store (not done)

- A license file in the plugin (included: GPL-3.0; the store needs one).
- `plugin.json` with `name`, `author`, `flags`, `api_version`, and a `publish` block (`tags`,
  `description`, and an `image` URL, which is still to be added).
- `package.json`, `main.py`, and a `dist/index.js` build in the release zip.
- A pull request to [decky-plugin-database](https://github.com/SteamDeckHomebrew/decky-plugin-database)
  adding this repo as a submodule under `plugins/`; the Decky team reviews and tests it, and later
  updates come as new submodule commits.
- See the [Decky plugin template](https://github.com/SteamDeckHomebrew/decky-plugin-template) for the current rules.
