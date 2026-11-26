# Installing Taskboard on Windows

Taskboard for Windows is an unzip-and-run app. There's no installer program: the first time you run
`taskboard-app.exe` from the unzipped folder, it copies itself into your user profile, adds itself to
the Start menu and Settings ▸ Apps, and starts. It needs no administrator rights.

**Needs:** 64-bit Windows 10 or Windows 11, and no extra runtime (the Visual C++ redistributable isn't
required). Taskboard shows the Claude Code sessions running in the Midna terminal app, so install Midna
and Claude Code too ([Midna's Windows guide](https://github.com/slider9/midna/blob/main/docs/INSTALL-windows.md)).

## 1. Download

Open <https://github.com/slider9/taskboard/releases> and pick the newest release whose title ends in
**for Windows** (`Taskboard 0.1.0-beta.97.w.1 for Windows`; its tag is `windows-v<version>`). While
the macOS beta it's built from is a beta, Windows releases are marked **Pre-release**. Under Assets,
download `Taskboard-<version>-windows-x64.zip`.

Releases whose tag is `v<version>`, with no `windows-`, are macOS builds: they have no Windows zip.

## 2. Unzip

Unzip it into a folder of its own, such as Downloads: right-click the zip ▸ **Extract All…**. You get
a `Taskboard` folder with `taskboard-app.exe`, `taskboardd.exe`, `tb.exe`, `plugin`, `Fonts`,
`install.json` and `taskboard-bundle.txt`.

Run it from a folder that holds only Taskboard. If `taskboard-bundle.txt` ends up among other files,
Taskboard refuses to install from there rather than copy them. The Taskboard that gets installed is a
copy; this folder is only where it starts from.

## 3. The SmartScreen warning

These builds aren't signed with a code-signing certificate yet, so the first time you run
`taskboard-app.exe`, Windows may show a blue **Windows protected your PC** box ("Microsoft Defender
SmartScreen prevented an unrecognized app from starting"). It shows because the file came from the
internet and isn't signed, not because a check found anything.

To go on: click **More info**, then **Run anyway**.

You answer it once. The install drops the "downloaded from the internet" mark from the copies it
makes, so the installed `taskboard-app.exe`, which the Start menu and the login entry start, doesn't
show the box again. The zip and the unzipped folder keep the mark, so running `taskboard-app.exe`
from them again shows it again; start Taskboard from the Start menu instead. An update that Taskboard
downloads itself carries no mark either.

If you'd rather unblock the zip first: **Properties** on the zip, tick **Unblock** under Security,
then extract.

## 4. First run

Double-click `taskboard-app.exe` in the unzipped folder. Nothing is installed yet, so it:

1. copies the folder into `%LOCALAPPDATA%\Programs\Taskboard` (that is,
   `C:\Users\<you>\AppData\Local\Programs\Taskboard`);
2. starts the copy there and exits.

The installed copy then:

- starts the board's daemon, and adds a startup entry so it runs whenever you sign in (Settings ▸
  Apps ▸ Startup, or Task Manager's Startup tab, switches it off);
- registers the `taskboard://` link scheme;
- adds **Taskboard** to the Start menu;
- adds **Taskboard** to Settings ▸ Apps (Apps & features), with its version, folder and an Uninstall
  button;
- copies `tb.exe` into `%USERPROFILE%\.local\bin` and adds that folder to your user Path.

From now on, start Taskboard from the Start menu. The unzipped folder can be deleted.

Run `taskboard-app.exe` from the unzipped folder again and it starts the installed copy (which comes
to the front if it's already running) without copying anything. When the zip is a newer version than
the installed one, running it updates the installed copy; when it's older, Taskboard refuses and tells
you to uninstall the newer one first.

The board's data lives in `%USERPROFILE%\.config\taskboard`. Coming from the old Python board? See
[CUTOVER.md](CUTOVER.md).

## 5. The `tb` command and the Claude Code plugin

`tb.exe` is the command-line tool that agents, and you, use to talk to the board (`tb --help`,
`tb task new`, …). The first run copies it to `%USERPROFILE%\.local\bin\tb.exe` and adds that folder
to the end of your user Path, unless it's on your Path already. There's nothing to set up: open a
**new** PowerShell window and run `tb --help`.

- **Only new terminals have it.** A terminal that was open when Taskboard installed keeps the Path it
  started with: close it and open another.
- **Only your user Path changes**, never the system one, and nothing needs administrator rights.

The Claude Code plugin reports turns, commits, checkpoints and questions back to the board. Click
**Install hooks** at the foot of the app's sidebar (it installs the `task-board` plugin from the copy
inside the app). The sidebar then shows **● Hooks**; if it shows an amber **Reinstall hooks**, click
that. After an update, reinstall the hooks when it asks: Claude Code keeps running its cached copy
until you do. `tb install-hooks` from a terminal does the same.

## 6. Updates

An installed Taskboard checks for updates by itself and downloads a new version in the background.
Settings ▸ System ▸ **Update** shows the state: *Up to date*, *Downloading…*, or *<version> ready*
with **Restart to update**. **Check now** checks right away.

Windows builds only look at the releases of `slider9/taskboard`, never at the macOS app's releases,
and only at `windows-v<version>` tags. A build of a beta version (`0.1.0-beta.97.w.1`) gets every
Windows release, betas included; a build of a stable version only gets releases that aren't
pre-releases. Each update's zip is checked against GitHub's digest and must name the version the
release does before anything is replaced.

Updating swaps the files in `%LOCALAPPDATA%\Programs\Taskboard`, restarts the daemon on the new build
and reopens the app. If something fails, the old files are put back and Settings shows why. You can
always update by hand: download the newer zip and run `taskboard-app.exe` from it.

## 7. Uninstall

On Windows 10: Settings ▸ Apps ▸ **Apps & features** ▸ **Taskboard** ▸ **Uninstall**. On Windows 11:
Settings ▸ Apps ▸ **Installed apps** ▸ the **…** next to Taskboard ▸ **Uninstall**. It quits the app,
stops the daemon, and removes the startup entry, the link scheme, the `tb.exe` copy in `.local\bin`,
the `.local\bin` entry it added to your Path (one you had already stays), the Start menu entry, the
Settings ▸ Apps entry and the files in `%LOCALAPPDATA%\Programs\Taskboard`.

It leaves your data in `%USERPROFILE%\.config\taskboard`: delete that folder for a clean slate.
Installing again without deleting it picks up where you left off. To remove the plugin too, run
`claude plugin uninstall task-board@taskboard` and `claude plugin marketplace remove taskboard` first.

## When something goes wrong

- **Nothing happens when you run `taskboard-app.exe`.** Run it from a folder that holds only the
  unzipped build, and from a normal PowerShell or Explorer window.
- **"… is already installed … which is newer."** Uninstall the installed Taskboard (above), then run
  this one.
- **`tb` isn't found.** Open a new terminal. If it still isn't, check that
  `%USERPROFILE%\.local\bin` is on your user Path and holds `tb.exe`.
