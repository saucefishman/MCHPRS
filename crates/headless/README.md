# mchprs_headless

Runs a schematic on MCHPRS's own redstone and redpiler code without starting a server.

```
cargo build --release -p mchprs_headless
mchprs_headless test <schem> <pins.txt> <script.txt> [--modes redstone,redpiler] [--add-lever NAME]... [--redpiler-flags FLAGS] [--probes NAME.server.json] [--run-clock]
mchprs_headless settle <schem> <cuts.txt> <states.txt> [--max-ticks 20000] [--allow-pending]
```

Schematics may be `.schem` or `.litematic`. All coordinates are relative to the schematic's
minimum corner. In every input file, blank lines and lines starting with `#` are ignored.

## test

Exits 0 if every `expect` holds, 1 if any fails, 2 on bad input.

Modes: `redstone` and `redpiler` update every block after pasting, as `/watch` does. `noupdate`
runs the redstone code on the blocks exactly as pasted. `--redpiler-flags` takes the same flags as
`/redpiler compile`, such as `-oi`.

`pins.txt` has one `<name> <x> <y> <z> <nx> <nz>` per line: the pin's cell and its outward normal.
A pin is an input if there is a lever at `cell + (nx, 1, nz)`, otherwise an output.
`--add-lever NAME` first places a stone and lever at that spot, for inputs a build leaves undriven.

Outputs are read from repeaters, comparators, dust, torches and lamps.

`script.txt` commands:

| Command | Effect |
| --- | --- |
| `settle N` | run N redstone ticks |
| `set NAME 0\|1` | move the input's lever to that position |
| `pulse NAME N` | toggle the lever, run N ticks, toggle it back |
| `expect NAME 0\|1` | count a failure if the output differs |
| `print LABEL` | print the label and all outputs |

### Probes

`--probes` prints signals when a trigger block changes, like Verilog's `$write`:

```json
{
  "version": 2,
  "schematic_size": [64, 16, 64],
  "probes": [{
    "name": "cpu.v:87",
    "format": [{"text": "r="}, {"bits": [0, 1, 2], "radix": "d", "signed": false}, {"text": "\n"}],
    "trigger": {"pos": [1, 2, 3], "edge": "rising"},
    "enable": {"pos": [4, 2, 3]},
    "data": [{"pos": [5, 2, 3]}, {"pos": [6, 2, 3]}, {"const": 0}]
  }],
  "clock": {"pin": "clk", "pos": [0, 2, 0], "half_period_ticks": 4}
}
```

- `schematic_size` must match the schematic, and every `pos` must be a repeater, comparator, dust,
  torch, lamp or lever, or the run exits 2 before ticking.
- `edge` is `rising` or `falling`. `enable` and `data` entries are a `pos` or a `const` of 0 or 1.
  The output is skipped if `enable` is low.
- `bits` are indices into `data`, LSB first, at most 128 per field. `radix` is `d`, `x`, `o`, `b` or
  `c`, which writes the low 8 bits as one raw byte. `signed` only affects `d`.
- Fields may set `width` (minimum characters, default 0 for no padding), `pad` (one character,
  default space) and `justify`: `>` right (default), `<` left, or `=` to pad between a number's sign
  and its digits.

Output goes to stdout with no prefix or added newline, in file order for probes that fire together.
It is mixed in with the script's own output, and with each mode's output if you run several.

Triggers are checked after every tick and straight after every lever toggle. A probe sees the state
at the end of the tick its trigger changed in, which is before any block with a delay reacts to the
trigger, but after dust does. A trigger already active when the run starts does not fire.

In redpiler mode, probes read compiled node state and every probed block is kept through
optimization, so they work with any flags.

`clock` is optional and makes the server toggle a lever instead of the schematic generating a
clock. `pos` must be a lever in the schematic, or the file is rejected. `half_period_ticks` is at
least 1, in plot ticks: a 1-delay repeater takes one tick, and a full period is twice this. The
lever keeps its pasted state until the first toggle, `half_period_ticks` after the file loads, so
paste it off to make that a rising edge. The clock follows `/rtps`, and doesn't run while the plot
has no players. `pin` is only used in messages. A lever under redpiler is kept through optimization
even if it drives nothing.

The test command ignores the clock unless you pass `--run-clock`, so scripts that use `pulse` keep
their tick counts.

In game, `/watch` loads the server file next to the schematic, so `claude.litematic` uses
`claude.server.json`, and reloads both when either changes. It runs the clock if there is one. `/watch claude.litematic -c -oi` works
too. Each probe that fires prints straight away, as its own chat message and console line, because
chat can't add to an existing message. A `%c` probe therefore prints one message per byte. A final
newline is removed and other newlines split the message. At most 200 messages print per second, with
the rest counted as dropped.

## settle

Removes the blocks in `cuts.txt` (`x y z` per line), updates every block and ticks until nothing is
pending, then writes each signal block's state to `states.txt`:

```
x y z powered <0|1> <0|1>    repeater: powered, locked
x y z power <n>              dust
x y z lit <0|1>              torch, lamp
```

Exits 1 if ticks are still pending after `--max-ticks`, which usually means a clock or oscillator
is in the schematic. `--allow-pending` writes the states anyway and exits 0.
