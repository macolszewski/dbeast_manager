# DBeast branding

The source is the user-provided `dbeast-icon.svg`. PNG exports were made in VectorCraft with a transparent background at a scale of `size / 1024`, without changing proportions or colors.

- `icon-32.png` to `icon-1024.png`: the set of resolutions for the interface and application materials.
- `icon-64.png`: 24-point thumbnail in the application bar.
- `icon-128.png`: window icon on platforms that support `ViewportBuilder::with_icon`.
- `icon-512.png`: splash screen and empty editor area, at most 160 points.
- `dbeast-macos.svg`: source of the Dock icon with a 100/1024 margin and an 824/1024 mark, so the icon does not look larger than neighboring applications.
- `DBeast.icns`: macOS icon with sizes 16, 32, 128, 256 and 512 plus their Retina counterparts. Export the macOS source to PNG at sizes 16, 32, 64, 128, 256, 512 and 1024, save them as `icon_NxN.png` and `icon_NxN@2x.png` in a `DBeast.iconset` directory, then run `iconutil -c icns DBeast.iconset -o DBeast.icns`.

The splash screen is drawn by `src/ui/branding.rs` with the embedded Inter font and text that is independent of the image. It does not need a separate large background file. Tests write previews to `docs/screenshots/branding-*.png`.

Local measurement on 2026-10-15, release profile: decoding the three embedded PNGs (512, 64 and 128), averaged over 50 runs, takes 1.51 ms. The raw RGBA of these images takes 1.08 MiB; the two UI textures take about 1.02 MiB before driver overhead. This measures decoding only, not the startup time of the whole application or the texture upload to the GPU. Later frames reuse the same texture handles.
