# Waveshare ESP32-P4 factory flash layout

This layout records the reference backup reported by the developer for the
ESP32-P4-WIFI6-POE-ETH with 32 MiB flash. It is not autodetection and does not
prove that another board or a later flash state matches it.

| Region | Offset | Size |
| --- | --- | --- |
| Factory bootloader | 0x2000 | Preserve existing contents |
| Partition table | 0x8000 | Preserve existing contents |
| nvsfactory | 0x9000 | 200 KiB |
| nvs | 0x3B000 | 840 KiB |
| otadata | 0x10D000 | 8 KiB |
| phy_init | 0x10F000 | 4 KiB |
| factory application | 0x110000 | 9 MiB |
| storage | 0xA10000 | 5 MiB |

The table source is `examples/qa/partitions/waveshare-p4-factory.csv`. Its
partition entries are contiguous and non-overlapping. Unlisted flash regions
have no implied write authorization.

Build an application-only image for the factory partition. Preserve the
bootloader and table. Do not program a merged image at 0x0 or an application at
0x10000: these addresses do not match this board's reported factory layout.

For offline image preparation, the intended espflash form is:

```sh
espflash save-image --chip esp32p4 --flash-mode dio --flash-size 32mb --flash-freq 80mhz \
  --partition-table examples/qa/partitions/waveshare-p4-factory.csv \
  --target-app-partition factory -S <firmware.elf> <application.bin>
```

Image preparation is not programming permission. Before a write, verify the
actual table, approved manifest, backup and application hash through Device Lab.
The approved envelope must cover complete erase sectors and any loader-specific
erase expansion. Historical scripts may use narrower trial-specific envelopes;
these are not a general permission to write the whole factory partition.

The preserved factory bootloader is an external boot prerequisite. The Rust
`esp-bootloader-esp-idf` crate's application descriptor does not imply linking
an ESP-IDF SDIO driver or C FFI into the application.
