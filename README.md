# Easy SCS Mod Manager (Rust)

**English** · [한국어](README.ko.md) · [简体中文](README.zh-CN.md)

A mod and load-order manager for Euro Truck Simulator 2 and American Truck Simulator.

It is a rewrite of [Switch-Bros/easy-scsmodmanager](https://github.com/Switch-Bros/easy-scsmodmanager) (Python / PyQt6) in Rust and Tauri, under the same GPL-3.0-or-later license.

## Download and run

Get `Easy-SCS-Mod-Manager-<version>-portable.zip` from the [releases](https://github.com/doveis99/easy-scsmodmanager-rs/releases), unpack it anywhere and run `easy-scsmodmanager.exe`. Nothing is installed.

- Windows 10/11 (64-bit) only.
- Settings, category pins, favourites, profile backups and caches are kept in the `data` folder next to the executable.
- When started from a place it cannot write to (Program Files etc.), the app uses `%APPDATA%\easy-scsmodmanager-rs` instead.
- The Microsoft Edge WebView2 runtime is required. Windows 11 ships with it.
- The executable is not code-signed, so Windows SmartScreen may warn on the first start.

## Languages

The interface is available in English, Korean and Simplified Chinese. It follows the Windows language at first; choose another one under **Settings → Language**. The change applies at once.

## Features

- **Mod list**: scans the `mod` folder and the Steam Workshop. Reads ZIP, HashFS v1/v2 and AEM archives as well as unpacked folders.
- **Automatic categories**: on top of what a mod declares, its content corrects the category: map files and physics definitions, the folders its definitions sit in, and the units they define. The units are read from a sample of the files, including scrambled (3nK) and binary definitions and the unnamed files of locked archives. Where the manifest declares nothing, or something its files do not back at all, the content decides; a less certain finding is only shown as a suggestion on the card. `scsmm classify` prints the reasoning for every mod.
- **Manual categories**: pin a mod to a category yourself. A mod that covers several things can be given several categories, one of them as the main one. The main category decides the group in the load order; the others feed the category filter and the badges.
- **Load order**: shown in groups; drag rows or press `Alt+↑/↓` to reorder.
- **Auto sort**: sorts mods into their groups but never flips two mods that carry the same file, so sorting does not change which mod wins a file.
- **Conflicts**: marks mods whose `def/` files are overridden by a mod above them, with a full report.
- **Compatibility**: warns when the versions a mod declares do not match the game version.
- **Locked archives**: archives that hide their file listing are marked `Hidden listing`. Their file names are recovered by matching against the path list of the game's own archives, and used for conflict detection and categories. The path list is cached in `data/cache/gamepaths` and rebuilt when the game changes.
- **Extraction**: the `Extract…` button in a mod's details unpacks its `.scs` archive into a new folder. Packed HashFS v2 textures come out as `.dds`. For locked archives, names are recovered from the game's path list, from paths found inside the files and from naming conventions; whatever stays nameless is written to `_unnamed` as `<hash>.<extension>` (extension guessed from the content). The game's own archives can be unpacked with `scsmm extract` on the command line.
- **Saving and backups**: every save backs up the whole profile folder first; backups can be restored.
- **Workshop info**: titles and previews of Workshop mods without a name are fetched from Steam's public API. Can be switched off in the settings.
- **Sharing**: export and import load orders as files; also reads someone else's `profile.sii` and map combos. The file formats are those of the original app.
- **Mod management**: details, favourites, delete to the recycle bin, show in folder.
- **Remembered view**: the last game and profile, and the library's sort order and filters, come back on the next start.

Not ported from the original: online share codes, auto-update, the German interface, Linux and macOS packaging.

## Layout

| Path | Content |
|---|---|
| `crates/scsmm-core` | All logic that needs no window: archive reading, SII parsing, scanning, categories, load order, profiles, sharing |
| `crates/scsmm-cli` | Read-only command-line tool `scsmm` |
| `src-tauri` | The Tauri app; exposes the core to the interface as commands |
| `ui` | The interface (TypeScript, Vite) |
| `ui/src/locales` | Interface texts, one file per language |
| `scripts` | Portable build and clean-up scripts |

## Development

Rust, Node.js and pnpm are required.

```bash
pnpm install          # once
pnpm tauri dev        # run in development mode
cargo test --workspace
pnpm portable         # build the portable folder (dist-portable/)
pnpm clean            # trim the target folder (keeps the release build cache)
pnpm clean --all      # delete the whole target folder
```

The command-line tool shows scan results and the load order without touching a profile.

```bash
cargo run -p scsmm-cli -- installs
cargo run -p scsmm-cli -- scan --game ets2
cargo run -p scsmm-cli -- order --game ets2
cargo run -p scsmm-cli -- extract <archive.scs> <empty folder>
```

### Adding a language

Korean (`ui/src/locales/ko.ts`) is the source. Copy it, translate the values, then register the new file in `ui/src/i18n.ts` (`Lang`, `MESSAGES`, `LOCALE`, `LANGS`) and in `Settings::normalize_language` in `crates/scsmm-core/src/settings.rs`. A missing key fails the build.

## License

GPL-3.0-or-later. The full text is in [LICENSE](LICENSE).
