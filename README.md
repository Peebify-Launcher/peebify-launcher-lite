<div align="center">

<img width="960" height="534" alt="image" src="https://github.com/user-attachments/assets/1fa95629-86ee-4ff2-866c-b024b2980115" />

# Peebify Launcher Lite

**One launcher for all your favorite gacha games**
Install, update, repair, and mod

![Platform](https://img.shields.io/badge/platform-Windows-0078D4?style=for-the-badge&logo=windows&logoColor=white)
![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri-FFC131?style=for-the-badge&logo=tauri&logoColor=white)
![Games](https://img.shields.io/badge/games-15-ff69b4?style=for-the-badge)

[Website](https://peebify.net/) · [Discord](https://discord.gg/5kfpJTv2Xc) · [Lite vs. full](#lite-vs-full-peebify) · [Features](#features) · [Supported Games](#supported-games) · [Building](#building-from-source) · [Credits](#credits)

</div>

---

## Lite vs. full Peebify

Peebify Launcher Lite is the open-source, standalone edition of [Peebify](https://peebify.net/). It is the same launcher core, minus everything that depends on a Peebify account or server.

| | Lite | Full Peebify |
|---|:---:|:---:|
| Install, update, repair and mod games | ✅ | ✅ |
| Optional account with syncing (playtime, settings, mods) | ❌ | ✅ |
| Launcher updates itself | ❌ | ✅ |
| Wallpapers | ✅ | ✅ |

The only things it fetches from Peebify's servers are the launcher's endpoint config (`launcher/api.json`) and the wallpapers. It does not update itself, to get newer versions of Peebify Launcher Lite you must grab from the latest releases of this repository or rebuild it yourself.

## Supported Games

<table>
  <tr>
    <td>Wuthering Waves</td>
    <td>Punishing: Gray Raven</td>
    <td>Genshin Impact</td>
  </tr>
  <tr>
    <td>Honkai: Star Rail</td>
    <td>Honkai Impact 3rd</td>
    <td>Zenless Zone Zero</td>
  </tr>
  <tr>
    <td>Arknights: Endfield</td>
    <td>Neverness to Everness</td>
    <td>Girls' Frontline 2: Exilium</td>
  </tr>
  <tr>
    <td>Girls' Frontline <i>(Steam only)</i></td>
    <td>Reverse: 1999</td>
    <td>Brown Dust II</td>
  </tr>
  <tr>
    <td>Arknights</td>
    <td>Blue Archive <i>(Steam only)</i></td>
    <td>Duet Night Abyss</td>
  </tr>
</table>

## Features

### Game management
- **Install, Update & Repair** with no official launcher required, including scheduled game auto-updates.
- **Downloads page** with a queue and in-depth progress for every game being installed, repaired, or updated.
- **Mods** for visuals and skins, powered by [XXMI Launcher's](https://github.com/SpectrumQT/XXMI-Launcher) framework.

### Stay informed
- **News & Notices** from every game you play, in one feed.
- **Community Tools** linking to official sites, news, wikis, pity calculators, and character builds.
- **Playtime tracking** with a dedicated page to visualize it, stored locally.

### Make it yours
- **Static & animated wallpapers**, customizable in settings.
- **Quick Settings** for common actions like Check for Updates, Game Folder, and Screenshots.
- **Plenty of preferences**: launch with Windows, toggle animated wallpapers, hide specific launcher UI elements, and more.

## Building from source

Lite is distributed as source. You will need:

- Windows 10/11 with the [WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
- [Node.js](https://nodejs.org/) and npm
- [Rust](https://rustup.rs/) (stable) with the MSVC build tools

```bash
npm install
npm --prefix webui install

npm run dev     # run the launcher in development
npm run build   # production build, packaged as a setup installer
```

`npm run build` writes the setup installer to `dist/installer/Peebify-Launcher-Lite-Setup-<version>.exe`. The bare launcher executable is at `target/release/peebify-launcher.exe`.

### Project layout

| Path | What it is |
|---|---|
| `src-tauri/` | The launcher backend (Rust, Tauri 2) |
| `webui/` | The launcher interface (React, TypeScript, Vite, Tailwind) |
| `helpers/` | Helper processes: FPS unlocker, mod loader, overlay hotkeys |
| `installer/` | The Windows setup program |
| `scripts/` | The build script that packages the launcher into the setup installer |

## Get the full version

Want accounts, cloud syncing and a launcher that keeps itself up to date? Download the full version of Peebify from the [official website](https://peebify.net/).

## Join our Discord

Get help, report a bug, or just say hello in the [Peebify Discord](https://discord.gg/5kfpJTv2Xc).

## Credits

| Role | Who |
|---|---|
| **Developer** | [Acheuy](https://github.com/Cheu3172) |
| **Contributors** | [MrFlashStudio](https://github.com/mrflashstudio) · [Blackwolf](https://github.com/blackwolf660) |
| **Repo credits** | [XXMI Launcher](https://github.com/SpectrumQT/XXMI-Launcher) |