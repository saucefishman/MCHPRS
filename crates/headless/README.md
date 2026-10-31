# mchprs_headless

Runs a schematic on MCHPRS's own redstone and redpiler code without starting a server.

```
cargo build --release -p mchprs_headless
mchprs_headless test <schem> <pins.txt> <script.txt> [--modes redstone,redpiler] [--add-lever NAME]...
mchprs_headless settle <schem> <cuts.txt> <states.txt> [--max-ticks 20000] [--allow-pending]
```

Schematics may be `.schem` or `.litematic`. All coordinates are relative to the schematic's
minimum corner. In every input file, blank lines and lines starting with `#` are ignored.

## test

Exits 0 if every `expect` holds, 1 if any fails, 2 on bad input.

Modes: `redstone` and `redpiler` update every block after pasting, as `/watch` does. `noupdate`
runs the redstone code on the blocks exactly as pasted.

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
