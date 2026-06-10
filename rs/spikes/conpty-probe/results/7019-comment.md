Fresh data on this issue from Windows 11 24H2 (OS build 26100, in-box conhost 10.0.26100.1), measured while investigating throughput of a ConPTY-based terminal. Three results that seem worth putting on the record — including one suggesting this is **already fixed in this repo, just not serviced to the in-box host**.

### 1. The in-box conhost still has the pathological repaint — inflation = rows repainted

A child streaming plain lines inside an `ESC[1;20r` scroll region: conhost re-renders the region on every scrolled line. Measured master-side VT inflation equals the number of rows repainted, and conhost generates repaint VT at a roughly constant ~25–29 MB/s, so the child is backpressured to `ceiling ÷ inflation`:

| host | case | grid | child-side MB/s | master bytes (2 MB payload) | inflation |
|---|---|---|---:|---:|---:|
| in-box 10.0.26100.1 | `ESC[1;20r` region | 120×30 | 1.20 | 40.72 MB | **20.4×** |
| in-box | `ESC[1;20r` region | 120×40 | 0.72 | 80.27 MB | **40.1×** |
| in-box | same lines, no region | 120×30 | 6.19 | 2.00 MB | 1.0× |
| in-box | region taller than grid (invalid → ignored) | 120×10 | 7.60 | 2.00 MB | 1.0× |

The only delta between the slow and fast cases is the DECSTBM prologue; the line content is identical.

### 2. OpenConsole 1.24 does NOT have the repaint — i.e. this appears fixed, just not in-box

Same repro against the sideloaded redistributable pair (`Microsoft.Windows.Console.ConPTY` 1.24.260512001; host identity verified mid-run — the pty host child process is `OpenConsole.exe 1.24.2605.12001`): **1.0× inflation** on the scroll-region case (8.00 MB in → 8.00 MB out, byte-for-byte). So the VtEngine-removal era work (#17510) appears to have fixed exactly what this issue describes — but every ConPTY consumer that doesn't sideload the NuGet pair (i.e., nearly all of them) still gets the in-box behavior above.

**Ask: could this be reopened to track servicing the fix into the in-box conhost** (or could the team confirm the servicing plan)? Sideloading (discussion #17608) is a workaround each app must discover and ship on its own.

(Also tested explicitly, for the record: passing `PSEUDOCONSOLE_PASSTHROUGH_MODE` (0x8) to `CreatePseudoConsole` — measured no-op on both hosts, consistent with #1173 / #1985 being closed.)

### 3. A possibly separate 1.24 observation: deferred flush under sustained output

While verifying, the 1.24 host (headless ConPTY use) buffered the child's raw VT without backpressure and delivered it only when the child *paused*: a phased workload (2 MB of region-scroll → 3 s idle → marker → exit) shows 0 bytes on the master while the child streams, with the full 2 MB arriving during the idle; under continuous output it flushed only at child exit (8 MB in one burst). If that's unexpected rather than by-design pacing, happy to file it separately with the repro.

### Repro

Self-contained Rust probe (portable-pty 0.9): spawns `node <script>` in a ConPTY, measures child-side input rate vs master-side bytes (the inflation factor), and reports which conpty host it loaded. It runs fully headless — one note for anyone reproducing: with `PSEUDOCONSOLE_INHERIT_CURSOR` the host emits nothing until the terminal answers the initial `ESC[6n` query, so the probe replies `ESC[1;1R`.

- Probe: https://github.com/Eyalm321/hyperpanes/tree/main/rs/spikes/conpty-probe
- Raw runs: https://github.com/Eyalm321/hyperpanes/tree/main/rs/spikes/conpty-probe/results
- Full write-up: https://github.com/Eyalm321/hyperpanes/blob/main/docs/conpty-passthrough-investigation.md

Related: #17510, #1173, #1985, discussion #17608.
