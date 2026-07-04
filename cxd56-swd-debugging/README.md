# Debugging the Spresense with the Serial Wire Debug (SWD) interface

This directory holds relevant information for debugging the CXD5602 on the Sony Spresense with the
Serial Wire Debug (SWD) interface. With this, you can attach the host computer to a program running
on the Sony Spresense and use commands with a GDB server for a richer debugging experience compared
to printf-debugging.

Unfortunately, the necessary pins for accessing SWD on the Spresense are not available on the main board.
Instead, you need to use the extension board and some soldering to make the port available.

You can use any 0.05" / 1.27 mm pitch SWD connector. In my case I used the
[Adafruit Mini SWD 0.05" Pitch Connector - 10 Pin SMT Box Header](https://www.adafruit.com/product/4048).

Also for those curious, if you lose the mini-spacers that come with the Extension board, you can buy any M2 screw atleast 8 mm long
will be long enough to span up through the extension board and to the main board. Spresense documents recommend getting nylon screws
as metal screws can intefere with some components like the GNSS. An 8mm is close to the bare minimum, and I couldn't get get a washer
along with the nut to fit with the 8 mm. I used a 12mm screw which was long enough it was easy to get the washer and nut, but won't
be long enough to reach an addon board like the CommonSense SensiEdge.

_IMPORTANT_: Don't be silly like me and solder the debug port the wrong way. The "tooth" of the port should face inside
or to the left, assuming you orient so that the Sony logo is oriented in up in a readable way.

This set up was tested using the SWD port on the Sony Spresense Extension Board, and attaching
a JTAG cable to a breakout board for connecting to a Raspberry Pi Debug Probe.

## OpenOCD

```bash
# Flash a simple binary to the board
$ cd examples
$ cargo run --bin rust_hello_uart

$ openocd -f interface/cmsis-dap.cfg -c "transport select swd" -f cxd5602.cfg
# ...
Info : [cxd5602.cpu3] Cortex-M4 r0p1 processor detected
Info : [cxd5602.cpu3] target has 6 breakpoints, 4 watchpoints
Info : starting gdb server for cxd5602.cpu3 on 3333
Info : Listening on port 3333 for gdb connections

$ arm-none-eabi-gdb target/thumbv7em-none-eabihf/debug/rust_hello_uart
Reading symbols from target/thumbv7em-none-eabihf/debug/rust_hello_uart...
(gdb) target remote :3333
Remote debugging using :3333
core::ptr::read_volatile<u32> (src=0xe000e010)
    at /path/to/home/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library/core/src/ptr/mod.rs:2097
2097    }
# output from cargo run should be stopped
```

## `probe-rs`

```bash
# Flash a simple binary to the board
$ cd examples
$ cargo run --bin rust_hello_uart

# Switch to another terminal tab to keep watch of the output

$ probe-rs gdb --chip CXD5602 --chip-description-path cxd5602.yml
Firing up GDB stub for Armv7em cores at [127.0.0.1:1337, [::1]:1337]
$ arm-none-eabi-gdb target/thumbv7em-none-eabihf/debug/rust_hello_uart
Reading symbols from target/thumbv7em-none-eabihf/debug/rust_hello_uart...
(gdb) target remote :1337
Remote debugging using :1337
0x0d0080c0 in core::num::{impl#11}::count_ones (self=4)
    at /path/to/home/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library/core/src/num/uint_macros.rs:85
85                  return intrinsics::ctpop(self);
# output from cargo run should be stopped
```

## VS Code (probe-rs extension)

The repo ships a ready-made F5 workflow in the tracked `.vscode/launch.json` and `.vscode/tasks.json`
at the repository root, currently wired for the `rust_blink` example. One-time setup:

1. Install the `probe-rs` CLI tools (<https://probe.rs/docs/getting-started/installation/>).
2. Install the `probe-rs.probe-rs-debugger` extension from the VS Code marketplace. The extension
   and the `probe-rs` binary versions must match — the extension will offer to install matching
   tools if they're missing or out of sync.
3. Open the **repository root** in VS Code (the launch config paths are relative to it).

Then pick "rust_blink (debug build) — flash + SWD attach" in the Run and Debug panel and press F5:

- The `flash-rust_blink-debug` task builds the example and flashes it over the UART bootloader
  (`cargo spresense-flash --bin rust_blink`). Note this deliberately skips `--monitor` so the task
  terminates — a pre-launch task that holds the serial monitor open would block the debug session
  from ever starting.
- The firmware starts running immediately after flashing; probe-rs then **attaches** over SWD to
  `cpu3` (probe-rs cannot flash or reset-boot this chip — the bootloader is proprietary and the
  chip description has no flash algorithm, so attach-to-running is the only mode).
- From there you can pause, set breakpoints, step, and inspect peripheral registers (the SVD view
  is fed by `svd/cxd5602.svd.patched`).

The release-build variant exists too; the release profile carries full debug info (`debug = 2`),
but LTO and `opt-level = "s"` make stepping jumpy, so prefer the debug build for interactive work.

To debug a different example, duplicate one task + one launch configuration and change the
`--bin` argument and `programBinary` path. Examples outside the workspace `default-members`
additionally need `-p examples-<member>` in the task's args.

VS Code-specific caveats, on top of the known issues below:

- Attach-to-running means code that executes before the attach completes can't be trapped.
- Only `cpu3` is described in `cxd5602.yml`, so subcores of multicore examples aren't debuggable
  this way.
- RTT is disabled in the launch configs — the examples log via `defmt-serial` over UART, so open a
  serial monitor in a separate terminal if you want log output while debugging.

## Troubleshooting / Known Issues

- Sometimes `monitor reset` will cause the connection to be lost, other times it works correctly
- Getting breakpoints to fire seems to be difficult, it might be because `main` is wrapped in the macro attribute, so
the line numbers are different than what they should be?

## License

The `cxd5602.cfg` is [copied directly](https://github.com/sonydevworld/spresense/blob/master/sdk/tools/cxd5602.cfg) from the Sony Spresense SDK
and is included here for convenience, so thats likely Apache-2.0 License.
