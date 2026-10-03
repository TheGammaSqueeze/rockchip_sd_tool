### Downloads

| File | Platform |
| --- | --- |
| `rockchip_sd_tool-windows-x86_64-<version>.zip` | Windows 10/11, 64-bit |
| `rockchip_sd_tool-macos-arm64-<version>.zip` | macOS 11 or newer on Apple silicon (M1 and later) |
| `rockchip_sd_tool-macos-x86_64-<version>.zip` | macOS 11 or newer on Intel |
| `rockchip_sd_tool-linux-x86_64-<version>.zip` | Linux, 64-bit x86 |
| `rockchip_sd_tool-linux-aarch64-<version>.zip` | Linux, 64-bit ARM |

### How to use

1. Unzip the download for your platform.
2. **Windows**: run `rockchip_sd_tool.exe`; accept the administrator prompt (raw disk access needs it). SmartScreen may warn once because the build is unsigned; choose More info, Run anyway. If an antivirus reports a named threat instead, it is a false positive on a new unsigned program that writes to raw disks; the checksums below and `gh attestation verify <file> --repo TheGammaSqueeze/rockchip_sd_tool` let you confirm the file came from this repository's build.
   **macOS**: open `Rockchip_SD_Tool.app.zip`, then right-click `Rockchip SD Tool.app` and choose Open (the build is not notarized, so the first launch needs this). The bare `rockchip_sd_tool` file next to it is the command line version.
   **Linux**: `chmod +x rockchip_sd_tool` and run it from a desktop session; it asks for your password through the system dialog when it writes to a card.
3. Choose the firmware image (the RKFW `.img` you downloaded), choose the SD card, press Write. Every block is read back as it is written and the card is verified at the end.
4. The write mode list offers four choices, and the window explains each one as you select it:
   * **Boot card (erases all)**: a card the device runs from. Use it for a new card or a clean start.
   * **Upgrade, keep user data**: re-flashes a card you already play on and keeps the partition table, saves and settings. A card whose layout does not match the image is refused before anything is written.
   * **Firmware update card**: the device boots from it once and flashes its own internal storage. It wipes the device's user data afterwards, as the firmware asks.
   * **Update card, keep user data**: the same, with the firmware's request to wipe removed, so the device keeps its saves. Only use it when the new firmware can read the data already on the device.

### Command line

```
rockchip_sd_tool info  <firmware.img>
rockchip_sd_tool list
rockchip_sd_tool write <firmware.img> --to <device>            (Linux: sudo, macOS: asks for the password)
rockchip_sd_tool write <firmware.img> --to card.img.xz --size 128GB
rockchip_sd_tool write <firmware.img> --to <device> --upgrade     (keep the table and user data)
rockchip_sd_tool write <firmware.img> --to <device> --update-card (a card that flashes the device)
rockchip_sd_tool write <firmware.img> --to <device> --update-card --keep-data
rockchip_sd_tool verify <firmware.img> --from <device or image>
```

Full documentation is in the README inside the zip.
