
## Downloads

| System | File |
|---|---|
| macOS 13 or later, Apple Silicon | `DBeast-<version>-macos-arm64.zip` |
| Linux x86-64 (glibc 2.35 or later: Ubuntu 22.04, Debian 12, Fedora 36 and newer) | `DBeast-<version>-linux-x86_64.tar.gz` |
| Windows 10 and 11, x86-64 | `DBeast-<version>-windows-x86_64.zip` |

`SHA256SUMS.txt` lists the checksums of all files.

**macOS.** Unzip and move `DBeast.app` to Applications. The application is not notarized
yet, so the first start needs a right-click on the app and **Open**, or:
`xattr -dr com.apple.quarantine /Applications/DBeast.app`.

**Linux.** Unpack and run `./dbeast`. It needs GTK 3, ALSA and OpenGL, present on common
desktops (Debian and Ubuntu: `libgtk-3-0 libasound2 libgl1`).

**Windows.** Unzip and run `DBeast.exe`. The executable is not code-signed yet, so
SmartScreen may show **More info**, then **Run anyway**.
