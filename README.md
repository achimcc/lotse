# lotse

Queues, admits and retries the heavy runs of parallel sessions on one
workstation — and tells each of them what the others are doing.

A dozen terminal sessions work on the same repository, each in its own
worktree. None of them knows about the others. Three of them evaluate the
same NixOS system at once, each evaluation grows to twelve gigabytes, and the
memory watchdog kills whichever shell it finds first. One session measures a
service while another one's deploy is restarting it, and reports an outage.
One waits for a build to end with `while pgrep -f "nix eval …"`, a pattern
that matches its own command line — the first time lotse looked at the
machine it came from, such a loop had been waiting for itself for 26 hours.
And a build that died of `Could not resolve host`, because a deploy had just
restarted the DNS, reads exactly like a failed check.

```console
$ lotse status
STATE     CLASS   TARGET  WORKTREE     PID      AGE    RSS/ESTIMATE  COMMAND
running   eval    -       audit-b12    3672958  3m04s  12.0G/12.0G   nix eval --raw .#nixosConfigurations.server.config…
observed  eval    server  dispatcharr  3683961  1m31s   5.7G/12.0G   colmena apply --on server build --keep-result
wait #1   eval    -       rust-ideen   3688102    12s     0K/12.0G   nix build .#nixosConfigurations.server.config.sys…
memory: 7.2G available, 4.0G reserve, 6.3G still to be claimed by running jobs

$ lotse run --class eval -- nix build .#nixosConfigurations.server.config.system.build.toplevel
lotse: waiting: memory: need 16.0G including the reserve, 0.9G left after what running jobs will still grow
…
lotse: exit=0 verdict=command attempts=1 waited=252s ran=431s log=/home/me/.local/state/lotse/logs/20260919-123816-00000007-3688102.log
```

There is no daemon. The state is a directory; whether a run is alive is
answered by the kernel.

## Commands

```
lotse run --class CLASS [--target T] [--max-wait D] [--no-retry] -- COMMAND…
lotse status [--json]
lotse wait CLASS [--target T] [--max-wait D]
```

**`run`** waits for admission, then runs the command. Its output goes to the
terminal unchanged and into a log under `$XDG_STATE_HOME/lotse/logs/`
(mode 0600, directory 0700; logs older than 30 days are removed when the next
run opens its own). lotse exits with the command's exit code (128 + signal if
a signal ended it), and the last line of the output and the log states it —
`lotse: exit=<code> verdict=<command|network|interrupted> attempts=…`. The
code there is always the command's own, also when lotse itself exits with
201; `verdict` says whose word it is. A background run whose notification
shows the exit code of some other line can be looked up there.

**`status`** lists what runs and what waits: registered runs, and runs that
nobody registered but that match a class (`observed`).

**`wait`** blocks until no run of the class is under way, registered or
observed. It is what belongs in front of a measurement
(`lotse wait deploy --target server && just smoke`) and in place of every
`pgrep` loop.

## Configuration

One file, from the first of:

1. `--config FILE`,
2. `$LOTSE_CONFIG`,
3. `$XDG_CONFIG_HOME/lotse/config.toml` (`~/.config/lotse/config.toml`).

**Never a `lotse.toml` found in or above the current directory** (up to
0.2.x it was). The hook runs in every session on the machine, in whatever
directory the session works — a checkout of someone else's repository, a
scratch directory under `/tmp` — and the class names of the file it finds end
up in that session's command lines. A repository that keeps its own
`lotse.toml` points at it: `export LOTSE_CONFIG="$PWD/lotse.toml"` in its dev
shell, or a symlink from `~/.config/lotse/config.toml` to it.

The file is read only if it is trustworthy, after all symlinks are followed:
the file and the directory it is in belong to you or root, and neither is
writable by group or others. Otherwise lotse refuses it (`run`, `status`,
`wait` exit 2; the hook stays silent and says why on stderr). A class name is
letters, digits, `_` and `-`; anything else is a configuration error.

See [`lotse.example.toml`](lotse.example.toml). An unknown key is an error: a
typo in a limit must not silently select the default.

| key | meaning |
|---|---|
| `reserve` | memory that stays free whatever is admitted |
| `max_wait` | how long `run` queues and `wait` blocks (default `30m`); also per class |
| `class.X.memory` | what one run of the class grows to; without it the class is not budgeted |
| `class.X.grows_for` | after this long a run has reached its size and claims nothing more; without it, it always counts with its whole estimate |
| `class.X.slots` | how many at once; without it only the memory budget limits |
| `class.X.per_target` | slots and queue count per `--target` |
| `class.X.exclusive_with` | classes that must not run at the same time, in both directions |
| `class.X.observe` | command lines that are a run of this class even unregistered |
| `class.X.ignore` | command lines that match `observe` and still are not |
| `class.X.wrap` | `lotse hook claude` puts `lotse run` in front of commands of this class |
| `class.X.retry` | `{ patterns, times, pause }`, see below |

Sizes take `K`, `M`, `G` (powers of two), durations `s`, `m`, `h`.

## Admission

Decided under a short global lock, in this order:

1. **First come, first served** within a class (per target if `per_target`),
   and behind an older waiter of a class that excludes this one — otherwise
   an exclusive run starves while the class it excludes keeps arriving.
2. **Slots** of the class. Observed runs hold slots like registered ones.
3. **Exclusion.**
4. **Memory:**

   ```
   MemAvailable − Σ max(0, estimate − resident)  ≥  estimate of the new run + reserve
                  over everything under way
   ```

   A run that has just started is small but will grow to its estimate; one
   that has grown is already missing from `MemAvailable`. With `grows_for`,
   a run older than that is taken at its present size: a `nix build` whose
   evaluation is over and that only waits for the builders is small and
   will stay so. Counting both as
   "one slot" is what fills a machine.

   If the budget says no while nothing at all is under way, the run starts
   anyway, with a warning: it would be waiting for memory nobody is going to
   free.

A run without `--target` in a `per_target` class meets every target: a deploy
that does not say where it goes may go anywhere.

A `lotse run` below a `lotse run` — a recipe that queues its build, called
from a command that was queued already — does not queue again: it is part of
the outer run and would otherwise wait for the memory the outer one has
claimed for exactly this work.

## Liveness

Each `lotse run` holds an `flock` on its entry for as long as it lives. An
entry whose lock can be taken belongs to a dead holder and is removed. That
is not a heuristic: the kernel releases the lock when the process dies,
`SIGKILL` included; a recycled PID cannot fake it, and no timeout has to be
guessed.

If lotse itself is killed and the command lives on — which is what a memory
watchdog does when it hits the wrapper — the entry disappears and the command
shows up again as `observed`. It keeps its slot and its share of the budget.

## Observation

At every admission, `status` and `wait`, lotse reads `/proc` for the
processes of the calling user and matches their command lines against the
classes' `observe` patterns. Not counted:

- lotse itself, and everything below a registered lotse (counted already);
- a wrapper whose subtree holds a registered lotse;
- all but the root of a chain of matches (`timeout 600 nix build …` and the
  nix below it are one run, with the resident set of the whole tree);
- **a shell that was handed its script as text** (`bash -c '…'`). That text
  may merely mention a build: a loop that waits for one, a `pgrep` for one.
  The program that does the work matches on its own. A shell running a
  script file is a program like any other.

Anchor the patterns at the program (`^(\S*/)?nix build\b…`). Unanchored,
`pgrep -f "nix eval …"` is an evaluation as well.

The target of an observed run is what follows `--on`.

Observation is what makes the queue honest while not every session uses it.
It is not enforcement: lotse cannot make an unregistered run wait.

## Retry

A run is repeated only if it **failed** and **the last error of its output
was the network** — never on the exit code alone, never on a pattern
somewhere in the output. Line by line, stdout and stderr in the order they
arrive:

- a line that matches one of the class's `retry.patterns` makes the verdict
  *network*;
- a later **error line of another kind** makes it *command* again. An error
  line starts with `error` or `fatal` (any case, colour codes ignored);
  indented lines — a nix builder's output quoted under the error that names
  it — are not error lines;
- except nix's **consequences** of a failed download, which name what could
  not be built for it and not why: `error: Cannot build '…`, `error: builder
  for '…`, `error: N dependencies of derivation '…`, `error: some substitutes
  for the outputs of derivation '…`, `error: some references of path '…`,
  `error: some outputs of '…`, `error: path '…' is required, but there is no
  substituter…`.

So `warning: unable to download …` followed by `error: assertion failed` is
the assertion's verdict, not the network's: nix prints download warnings long
before the error that decides. Up to 0.2.x any pattern anywhere counted, and
a red check came back as "network, no verdict".

After `times` repetitions lotse exits with **201**, which says: this was the
network every time, not a verdict of the command. The last line still names
the command's code: `lotse: exit=1 verdict=network …`. The slot is held
across the pause.

A class without `retry` is never repeated. Give none to anything that acts on
the outside world.

## A hook for Claude Code

A rule in a `CLAUDE.md` is a thing to remember, and a dozen sessions forget
it a dozen times. `lotse hook claude` is a `PreToolUse` hook that makes it a
property of the tool call: it puts `lotse run --class=… --` in front of every
command of a class with `wrap = true`.

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash",
    "hooks": [ { "type": "command", "command": "lotse hook claude", "timeout": 10 } ] } ] } }
```

```
cd /x && nix flake check 2>&1 | tail -3
cd /x && lotse run --class=eval -- nix flake check 2>&1 | tail -3
```

It rewrites a command only where the shell would run it: at the start, after
`;`, `&&`, `||`, `|`, `&`, in `( … )` and `$( … )`, after variable assignments
and keywords like `if` and `then`. Text that merely mentions a build —
`echo "nix flake check"`, `pgrep -f 'nix eval …'`, `ssh host 'nix build …'`, a
comment, a commit message — is not a command and stays.

**What it does not follow, it does not touch:** a here-document (its body is
data), backticks, `$(( … ))`, `$'…'` (it knows `\'`), `${…}` (it may hold
`;`, blanks and quotes of its own), unbalanced quotes. The whole call then runs as
it was written — unqueued, but observed like any other. It returns no
`permissionDecision`: the rewritten command goes through the same permission
flow as every other. It queues, it does not approve. It reads the configuration
as described above — never from the session's directory — and without one it
does nothing. Whatever goes wrong inside it ends in silence and exit code 0.

The wrapper is written as `--class=eval`, one word: a sandbox in front of the
shell may refuse a bare `eval`.

## Signals

`SIGINT`, `SIGTERM` and `SIGHUP` are passed on. Without a terminal on stdin
the command runs in its own process group and the whole group is signalled —
a `bash` in between does not pass `SIGTERM` on by itself. With a terminal it
stays in lotse's group (a background group reading the terminal would be
stopped), and `SIGINT` is left to the terminal. A second signal ten seconds
after the first becomes `SIGKILL`. A signal while queued removes the entry.

## Exit codes of lotse itself

| code | meaning |
|---|---|
| 2 | usage, configuration, or no state directory — lotse never starts a command uncoordinated because it could not coordinate |
| 127 | the command could not be started |
| 200 | `--max-wait` passed |
| 201 | every attempt died of the network (see "Retry"); the last line reads `exit=<the command's code> verdict=network` |

A command that exits with 200 or 201 itself is indistinguishable by the code;
the last line is not: `lotse: exit=201 verdict=command`.

## State

`$XDG_RUNTIME_DIR/lotse`, else `/tmp/lotse-<uid>`. The directory and its
`entries` must be real directories (no symlink), yours, and mode 0700 — under
`/tmp` anyone could have created it first — or lotse exits 2. Files in it are
created 0600 and never through a symlink.

## What it does not do

- **Limit memory.** It lets runs wait; it kills nothing. The estimate is a
  number you measured, not a cgroup.
- **Replace a lock on the target machine.** Two workstations deploying to one
  server do not see each other here.
- **Prioritise.** First come, first served.

## Install

```nix
inputs.lotse.url = "github:achimcc/lotse/v0.3.0";
# devShell or systemPackages:
inputs.lotse.packages.${system}.default
```

or `cargo install --git https://github.com/achimcc/lotse`. Linux only: `/proc`
and `flock(2)`.

## License

AGPL-3.0-only.
