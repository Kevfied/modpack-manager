# Modpack Manager — User Manual

For friends & players. No technical knowledge needed — just follow the steps for what you want to do.

- [Installing](#1-installing)
- [The window](#2-the-window)
- [Loader — install a pack someone gave you](#3-loader--install-a-pack-someone-gave-you)
- [Mods — pick your mods](#4-mods--pick-your-mods)
- [Build — make a pack](#5-build--make-a-pack)
- [Connect — sync mods with friends](#6-connect--sync-mods-with-friends)
- [App updates](#7-app-updates)
- [Where your stuff lives](#8-where-your-stuff-lives)
- [Uninstalling](#9-uninstalling)
- [Troubleshooting](#10-troubleshooting)

---

## 1. Installing

1. Get `Modpack Manager_X.X.X_x64-setup.exe` — either from the [Releases page](https://github.com/Kevfied/modpack-manager/releases/latest) or straight from a friend.
2. Run it. Windows will likely show a blue **"Windows protected your PC"** box — the app isn't signed with a paid certificate. Click **"More info" → "Run anyway"**. It's safe.
3. Follow the installer. Done — "Modpack Manager" is in your Start menu.

**If your antivirus complains later** when the app tries to save files (some AVs have "ransomware protection" that blocks apps writing to Documents/Desktop): allow the app in your AV's settings. It only writes to its own folder and the places you point it at.

---

## 2. The window

Small dark window, four tabs at the top:

| Tab | What it's for |
|---|---|
| **Loader** | Install a ready-made pack (`.mrpack` / `.zip`) into your launcher |
| **Mods** | Pick mods from folders/files, see what they are |
| **Build** | Turn your picked mods into a pack |
| **Connect** | Sync mods live with friends over LAN / Radmin |

Most people only need **Loader** and **Connect**. Mods+Build is for whoever assembles the pack.

---

## 3. Loader — install a pack someone gave you

Use this when a friend sends you a pack file.

1. Open the **Loader** tab.
2. Click **"Choose .mrpack / .zip…"** and pick the file.
3. Pick your launcher from the detected list (official launcher, TLauncher, and Prism Launcher are found automatically).
4. Confirm. The pack is installed into your launcher — **the app never starts the game or the launcher for you.** Open your launcher yourself and play.

- If a pack with the same name already exists, it'll ask before overwriting.
- Packs you receive through **Connect** also show up here under *"Synced packs (from Connect)"* so you can load them the same way.
- If it says **"No launchers detected"**, your launcher isn't on the auto-detect list — tell whoever made the pack.

---

## 4. Mods — pick your mods

This is the workbench for assembling a pack.

- **"Add folder…"** — point it at a mods folder; it scans every `.jar` inside, including subfolders.
- **"Add mods…"** — pick individual `.jar` files. Both sources merge into one list; duplicates are removed automatically.
- Check/uncheck mods to include them. **All** / **None** select everything or nothing at once. **✕** removes a mod from the list.
- The **Modrinth** column shows whether each mod was identified on Modrinth — `matched`, `not found`, or `parse err`. "Not found" just means it's not on Modrinth or metadata couldn't be read; the mod still works fine in a pack.
- Scanning runs in the background — you can keep clicking while a big folder loads.

---

## 5. Build — make a pack

Turn your selected mods into something shareable.

1. Fill in **Pack name**, **MC version**, **Loader** (e.g. Fabric/Forge), and **Loader version** — use the pickers; the app checks Modrinth for real version combos.
   - *"Pick an MC version to enable export"* — export stays disabled until the basics are set.
   - *"Possibly missing dependencies"* — mods that other mods might need; worth a look.
   - *"No matching builds found on Modrinth for this target"* — that MC version + loader combo doesn't exist; try another.
2. Click **Export** and choose an output:

| Format | Use when |
|---|---|
| `.mrpack` | Sharing properly — friends can load it via the Loader tab or any launcher that supports Modrinth packs |
| `.zip` | A plain archive of the jars — quick and universal |
| Instance directory | A ready-made game folder |
| `modlist.txt` | Just a text list of the contents |

---

## 6. Connect — sync mods with friends

This is the killer feature. One person **hosts** a mods folder; everyone else **joins** and gets exactly those mods — always current, no re-sending zip files.

Requirements: you must be on the **same network** — same house LAN, or connected to the same **Radmin VPN** network.

### Joining a friend's network (the usual case)

1. Make sure your friend is hosting (see below) and you're on the same LAN/Radmin.
2. The app opens on **Connect**. It actively asks the LAN for networks — their network appears under **"Found networks"** almost instantly (a dimmed *offline* entry means the app saw them before but they're not answering right now — it lights up the moment they host again).
   - Nothing showing up? Type their IP into the *"…or host IP"* box manually (e.g. `26.12.34.56` for Radmin).
3. Click **Join** on their network.
4. Enter **your display name** and the **join code** if they set one.
5. You're in. When the host publishes, you'll see a pending update — click **Apply update** and the new/changed mods land in your synced folder. Only the differences download.
6. A launcher picker pops up when the download finishes — pick the launcher you play Minecraft with. Done: open that launcher and run the pack.

**Lost? Press the `?` button** next to the network name while you're in a network — it shows the exact next step for wherever you are (no pack yet → "ask the host to publish", downloaded → "pick your launcher", installed → "open it and play").

While joined you can:
- **Check for updates** — refresh manually if you're impatient
- See all **Members** and their app versions
- **Leave network** anytime

> If the host kicks you, you're removed immediately — but you can rejoin with the join code whenever. It's not a ban.
>
> Closing the window just hides the app to the tray — it keeps syncing in the background. Real quit is in the tray menu.

### Hosting a network (one person does this)

1. Put all the mods for the pack in **one folder**.
2. Connect tab → **Host a network**: set a **network name**, an optional **join code** (*"friends need this to join"*), and **Pick folder…** for that mods folder.
3. Hit the host button. Your friends will now find it.

While hosting:
- **Change the folder anytime** — add/remove jar files, then press **"Publish to everyone"** to push it.
- ⚠️ **Nothing publishes until you press publish.** Adding files to the folder does *not* automatically share them — deliberate, so you can't leak half-finished changes by accident.
- **Publish to one member** — you can also send an update to a specific person only (they get a badge showing it was targeted at them).
- **Edit name/code live** — click the edit control by your network name; changing the code only affects *new* joins, existing members stay.
- **Kick** — the **✕** next to a member (only the host sees it). They can rejoin with the code.
- Badges in the member list: `v3 ✓` = they're current · `v2 behind` = they haven't applied yet · `app v0.1.0` = their app version.

### How to think about it

```
host:   edits folder ──presses PUBLISH──▶ members see "update available"
member: sees update ──presses APPLY──▶ downloads only what changed
```

The host's app does the serving. If the host is offline, nobody can fetch new changes — but everything already applied stays put.

---

## 7. App updates

The app can update *itself* over LAN — whoever publishes a new version (probably the same friend who gave you the app) can push it to everyone on the network. This is separate from Connect — no join codes needed.

What you'll see when one exists:

1. **A banner appears** on any tab: *"app update vX.Y.Z available"*
   - **Update now** → downloads it in the background with a progress bar
   - **Later** → dismisses the banner but keeps downloading — it comes back when ready
   - **Skip this version** → hides this version only; you'll be asked again for the *next* one
2. When downloaded and verified, it asks **"Restart & install?"**
   - **Restart & install** → app closes, installer runs a few seconds, new version opens. Your networks and data carry over.
   - **Cancel** → keeps the downloaded update for later; it'll still be ready next time.
3. SmartScreen may appear during install — same **"More info" → "Run anyway"** as before.

Extra bits that just work:
- **Downloads are verified** — every update is cryptographically signed. A fake or corrupted one is rejected and deleted automatically; it can never install.
- **Friends seed to friends** — once you've downloaded an update, your copy can serve it to others. So even if the original publisher goes offline, anyone who already has it keeps it flowing.
- **Interrupted download?** It resumes where it left off — no starting over.
- *"waiting for a seeder"* means the app knows an update exists but nobody's serving it right now — it'll pick up automatically when someone's app comes online.

---

## 8. Where your stuff lives

Everything the app remembers is in:

```
%LOCALAPPDATA%\modpack-manager\
```

Networks you joined, your member identity, synced packs, update state. Installing an update **never wipes this** — you stay in your networks and don't re-download anything.

Synced modpack files themselves land in a subfolder there; the Loader tab knows where they are, so you never have to browse to them.

---

## 9. Uninstalling

1. **Settings → Apps → Installed apps → "Modpack Manager" → ⋯ → Uninstall** (also works from Control Panel → Programs and Features, or right-clicking the Start menu entry).
2. That removes the program and its shortcuts.

**Your data stays behind** on purpose — `%LOCALAPPDATA%\modpack-manager\` keeps your joined networks, synced packs, and update state, so reinstalling later puts everything back. To wipe it completely, delete that folder after uninstalling (or tick "delete application data" if the uninstaller offers it).

---

## 10. Troubleshooting

| Problem | Fix |
|---|---|
| "Windows protected your PC" | Normal — **More info → Run anyway** |
| Antivirus blocks the app saving files | Add the app to your AV's allowed list (ransomware/controlled-folder protection) |
| Firewall prompt about port 47676 | Allow it — that's how friends' apps talk to yours. It only listens while hosting or seeding |
| **Found networks** is empty | Check you're on the same LAN / connected to the same Radmin network; make sure the friend actually pressed host; try their IP in the manual box; check firewall didn't block the app |
| "incorrect join code" | Ask the host — they may have rotated it. Existing members aren't affected |
| You were kicked | Rejoin with the join code — kicks aren't bans |
| Update download failed | Press **Update now** again — it resumes, and automatically tries other seeders |
| Update says "waiting for a seeder" | Normal — nobody online has the file right now. It continues automatically when someone comes online |
| "No launchers detected" in Loader | Your launcher isn't auto-detected (official launcher, TLauncher, and Prism are). Tell the pack author |
| Something looks stuck | Close and reopen the app — state is persisted, nothing is lost |

**Still stuck?** Tell the friend who runs the network / builds the app — they can check `AGENTS.md` and the test suites in the repo.
