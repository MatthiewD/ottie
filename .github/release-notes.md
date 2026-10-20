**An experimental build, for testing.** Things will break, and what Ottie saves may change before 1.0. Keep a copy of anything you can't lose, and please report what goes wrong in [Issues](https://github.com/MatthiewD/ottie/issues): your system, what you did, what happened.

## Which file

| System | File |
|---|---|
| Windows 10 and 11 | `…_x64-setup.exe` (or the `.msi`) |
| Ubuntu, Debian, Mint | `…_amd64.deb` |
| Fedora, openSUSE | `….x86_64.rpm` |
| Any other Linux | `…_amd64.AppImage`: make it executable, then run it |
| macOS on Apple silicon (M1 and later) | `…_aarch64.dmg` |

None of these are signed yet, so your system will warn you:

- **Windows:** SmartScreen says "Windows protected your PC": click **More info**, then **Run anyway**.
- **macOS:** after copying Ottie to Applications, open it once, then go to **System Settings → Privacy & Security** and click **Open Anyway**. If macOS says the app is damaged, run `xattr -dr com.apple.quarantine /Applications/Ottie.app` in Terminal. macOS is the least tested system: expect rough edges.

The app installs `ottie`, the program that does the downloading. It starts with the app, keeps running in the tray when you close the window, and the browser extension uses it.

## Browser extension

The extensions aren't in the browsers' stores yet. Install the desktop app first: the extension then signs in to it by itself.

- **Chrome, Edge, Brave, Vivaldi:** unzip `…_extension-chrome.zip` into a folder you keep. Open the extensions page (`chrome://extensions`, `edge://extensions` or `brave://extensions`), turn on **Developer mode**, click **Load unpacked** and pick that folder.
- **Firefox:** open `about:debugging#/runtime/this-firefox`, click **Load Temporary Add-on…** and pick `…_extension-firefox.zip`. Firefox removes it when it closes, so this needs doing again after each restart. In `about:addons` → Ottie → **Permissions**, allow access to all websites.
