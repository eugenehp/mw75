# Changelog

## 0.0.8 — 2026-10-15

### BLE moves from `btleplug` to `webbluetooth`

Scanning, connecting and the activation handshake now go through
[`webbluetooth`](https://github.com/eugenehp/webbluetooth) 0.0.4 instead of
`btleplug`. RFCOMM streaming is unchanged: `bluer` on Linux, IOBluetooth on
macOS, WinRT on Windows.

The public API is the same apart from one type:

* **`Mw75Device` holds only `name` and `id`.** It used to carry `btleplug`'s
  `Peripheral` and `Adapter` in crate-private fields; `connect_to` now re-finds
  the device by `id`. Code that only read `name` and `id`, or passed the value
  back to `connect_to`, is unaffected.

What changed underneath, and why it matters:

* **Disconnects are reported reliably.** The notification task now ends when
  the link drops, rather than waiting on a stream that `btleplug` kept open, so
  `Mw75Event::Disconnected` arrives once and promptly. An expected disconnect
  (before RFCOMM) is still suppressed.
* **`disconnect_ble()` returns once BLE is actually down**, not once it has
  been asked to go down. On macOS the RFCOMM channel will not open while the
  LE link is up, so this removes a race at the hand-over.
* **`peripheral_id()` returns `AA:BB:CC:DD:EE:FF`** on Linux and Windows — the
  address the RFCOMM code parses. On macOS it is still CoreBluetooth's UUID, and
  RFCOMM still finds the headset by name.
* **Scanning matches by name, service UUID or manufacturer data** as before,
  folding together the separate packets a device advertises in, so a name sent
  only in a scan response is still matched.

The service UUID filter used by `scan_all` is unchanged, and every
characteristic is still subscribed to, as `btleplug` allowed; that needs
`webbluetooth`'s `unrestricted` feature, which this crate enables.

### Not yet run against a headset

The port builds and passes its tests on macOS, and `webbluetooth` itself was
exercised against emulated and mocked peers and, for LE and RFCOMM, between a
Mac and a Linux machine. A full activation-and-stream run
with this version against an MW75 has not been done yet. The headset has to be
connected as an audio output to advertise over BLE — see the README.
