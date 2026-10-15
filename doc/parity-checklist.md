# C++ → Rust parity checklist

Every C++ test file in `tests/` and every documented behavior area of `doc/implementation.md`,
mapped to the Rust test or manual check that now covers it. `[x]` = covered by an automated Rust
test that passes; `[~]` = covered only by a live-machine test or the GTK smoke test, or not
verified visually (the reason is given). Nothing is marked `[x]` on the strength of code reading.

## C++ test files

| C++ file (assertions) | Behavior | Rust coverage |
|---|---|---|
| `device-rules.cpp` (67) | visibility rules, categories, PCI class decoding | [x] `model.rs` tests (`keyboard_and_audio_functions_stay_visible`, `input_endpoint_is_hidden`, `virtual_device_keeps_category_but_hides`, `partition_is_hidden`, `pci_class_decoding`, `category_table_matches_cpp_order`) |
| | generations, selection identity, reconcile | [x] `model.rs::reconcile_keeps_generation_only_for_the_same_instance`, `tree.rs::live_updates_keep_instances_and_replace_removed_ones` |
| | device naming (CPU, input, USB, PNP, codec, ACPI, faux, platform, root hub, hwdb) | [x] `naming.rs` (10 tests) |
| | labels (battery, mains, UCSI port, I2C/touchpad/mouse, ALSA jack/mic, EFI) | [x] `label.rs` (7 tests) |
| | NVMe/bridge pairing, ALSA grouping, USB network naming | [x] `presentation.rs` (7 tests) |
| `device-search.cpp` (82) | raw/status/errno search text, numeric aliases, EFI byte views, storage and mounts | [x] `search.rs` (6 tests), `propertytext.rs::storage_values_explain_themselves` |
| | name vs deep search, hidden exclusion, stale generations, clearing | [x] `tree.rs::search_inputs_exclude_hidden_devices_and_name_search_ignores_metadata`, `search.rs::deep_search_batches_caches_by_generation_and_cancels` |
| | connection/driver/resource projections, bus-qualified drivers, root and counts | [x] `tree.rs` (`connection_view_*`, `driver_projections_*`, `every_projection_*`, `resource_views_*`) |
| | storage presentation, health, ATA copy text, Details exclusion | [x] `storage_presentation.rs` (6 tests), `propertytext.rs::block_devices_keep_storage_identity_out_of_details` |
| `device-events.cpp` (14) | event identities, stale removal, bounds, canonical paths | [x] `events.rs` (4 tests), `monitorstate.rs::hints_are_bounded_and_canonical` |
| `device-resources.cpp` (20) | PCI/PnP resource parsing, IRQ rules, malformed input | [x] `resources.rs` (6 tests), `resourceformat.rs` (3 tests) |
| `directory-scan.cpp` (13) | bounded, cancellable, name-filtered directory listing | [x] `sysfs.rs::directory_names_follow_cpp_fixture`, `dirfd.rs::reads_are_anchored_and_classified` |
| `monitor-state.cpp` (8) | coalescing, loss, untrusted snapshot, exclusion | [x] `monitorstate.rs` (3 tests), `monitor.rs::session_publishes_one_snapshot_at_a_time_and_stops_promptly` |
| `system-properties.cpp` (60) | os-release quoting, cpu lists, instruction sets, ReBAR, memory slots, caches, SecureBoot, MOK | [x] `system.rs` (7 tests), `secureboot.rs` (7 tests), `cpuinfo.rs` (3 tests) |

## Documented behavior (doc/implementation.md sections)

| Section | Status |
|---|---|
| Application behavior: tree views by type / connection / driver / resources, filter bar, deep search, hidden devices toggle, refresh, status line, settings | [x] model logic in `tree.rs`; [~] window wiring driven by `qldm-gtk/tests/ui_smoke.rs` on a real session (rows, expand, select, show-all, refresh, close ≈ 45 ms, saved size); keyboard accelerators, context menu, Enter-vs-double-click and theme/layout **not verified visually** |
| Hardware and driver views (names, grouping, connectors) | [x] `naming.rs`, `label.rs`, `presentation.rs`; [~] live names checked against this machine (GPU, NVMe, webcam) |
| Device properties: General, Driver, Details, Resources | [x] `propertytext.rs` (9 tests), `properties.rs` (8), `resourceformat.rs`; [~] all tabs open in smoke test for Wi-Fi, webcam, NVMe, eDP, AC adapter |
| Network / Ethernet | [x] `network.rs`, `ethernet.rs` (netlink framing and parsers), `report.rs`; [~] live ethtool answered on this machine |
| Display / EDID / Wayland / X11 | [x] `display*.rs`, `monitor_edid.rs`, `report.rs`; [~] live X11 and Wayland probes run here |
| Camera | [x] `camera.rs`, `tests/camera_helper.rs` (hang killed, bad output, per-node); [~] live webcam answered |
| USB, PCIe, Battery tabs | [x] wording and decoding in `report.rs`, `properties.rs`; USB power tab has no live device test |
| Storage, volumes, health | [x] `storage.rs`, `storage_presentation.rs`; [~] live UDisks2 NVMe health read; volume/health selection behaviour in the GTK page **not driven by a test** |
| System information (all tabs) | [x] `system.rs`, `system_report.rs`; [~] smoke test reads real values; Copy all text is unit tested, the button wiring is not |
| Live device events tab | [x] `events.rs`, `events_view.rs`; the live update path into an open dialog is **not exercised by a test** |
| UEFI variables | [x] `efivars.rs`, `properties.rs::efi_variable_bytes_*`, `label.rs::efi_variables_use_readable_labels` |
| Permissions and settings | [x] `settings.rs` round trip; settings now live in `settings.conf` under the same config directory (the Qt `QLinuxDeviceManager.conf` is not read or migrated — behavior change) |
| Translations | [x] gettext hook and catalogue test; English only |
| Packaging | [~] `make install` staged and inspected; the CI workflow, Flatpak and AppImage recipes were **never run** |

## Known gaps accepted at cutover

1. No human has looked at the GTK windows: layout, spacing, theming, tooltips, clipboard buttons.
2. Qt-era settings are not migrated.
3. Details drop-down has no readable/unavailable separator; tables are GTK column views.
4. `tr(variable)` strings are not extracted into the `.pot`.
