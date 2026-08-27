# Cygshim

Cygshim provides native Windows shims for a few Cygwin command-line programs,
so that Windows-native applications such as
[Visual Studio Code](https://code.visualstudio.com/) and
[T3 Code](https://github.com/pingdotgg/t3code) can invoke them *without
knowledge of Cygwin*.

- Git: `git.exe`
- TeX: `latexmk.exe` and `pdflatex.exe`

Each `.exe` is a native Windows program (independent of `cygwin1.dll`).
Native applications can therefore discover and launch them normally.
The shim safely crosses into Cygwin, then runs the corresponding
Cygwin program from `/usr/bin`.

## Installation

Cygshim requires:

- 64-bit x86 Windows 10 or newer
- 64-bit Cygwin, including `bash` and `cygpath`
- The Cygwin packages for each shim being used (`git`, `latexmk`,
  `texlive-collection-latex`, and related TeX packages as appropriate)

Prebuilt executables are available from the
[GitHub releases](https://github.com/edemaine/cygshim/releases).
Download and extract the Windows x86-64 ZIP archive, then move all or
selected executables to the desired installation directory. For example:

- `%LOCALAPPDATA%\Programs\Cygshim` for one user, without administrator access.
- `C:\Program Files\Cygshim` for a machine-wide installation,
  with administrator access.

Do not install the shims over Cygwin's real executables.

For native applications to discover the shims by name, open **System
Properties → Advanced → Environment Variables** and edit `Path`. Use the user
variable for a per-user installation or the system variable for a machine-wide
installation. Add the Cygshim directory before Cygwin's `bin` directory in the
effective Windows PATH, then restart applications so they inherit the change.
If Cygwin is on the system PATH and Cygshim is only on the user PATH, their
relative order may not be controllable from the two separate lists; use the
same PATH scope or configure the application explicitly.

A Cygwin login shell [normally places `/usr/bin` before inherited Windows PATH
entries](https://cygwin.com/doc/preview/faq/faq.html#faq.using.path), thereby
bypassing Cygshim. This is normally what you want inside Cygwin: `git` etc.
behave normally, without a shim. But if you run a native application from a
Cygwin shell, that application may consequently find Cygwin's executables
instead of the shims. Point those applications directly at Cygshim, or launch
them from the Windows desktop or another native environment.

If you'd rather not change your PATH, you can still use applications that
support an explicit executable path. For VS Code's built-in Git support:

```json
{
  "git.path": "C:\\Program Files\\Cygshim\\git.exe"
}
```

For LaTeX Workshop, configure tools so that the `command` is the corresponding
native shim.

## Problem

Windows starts a process with one command-line string rather than a POSIX
`argv` array. The child runtime must parse that string back into arguments.
When a native Windows process starts a Cygwin executable, Cygwin performs its
own command-line processing and, by default, glob expansion. This can change
literal arguments containing characters such as `*`, `?`, `[`, or `{` before
the program receives them.

This is especially damaging to Git clients. Git revisions, pathspecs, hidden
checkpoint refs, and generated ref names are data, not shell expressions. A
native application expects an argument passed as `[name]` or `{name}` to reach
Git unchanged.

Cygwin documents a `noglob` option for the
[`CYGWIN` environment variable](https://cygwin.com/cygwin-ug-net/using-cygwinenv.html).
Unfortunately, `noglob` selects another command-line parser with a longstanding
quote incompatibility. In a native `CreateProcess` reproduction, a literal
double quote (represented by `\"`)
[loses the quote instead of reaching the Cygwin program literally](https://cygwin.com/pipermail/cygwin/2016-May/227720.html).
Choosing between normal glob parsing and `noglob` therefore only chooses which
arguments may be damaged. Escaping arguments into a Bash command string adds a
second parser and another opportunity for quoting bugs.

There is also a path-namespace mismatch. Native callers generally use paths
such as `C:\Users\name\project`, while Cygwin tools may report the same path as
`/cygdrive/c/Users/name/project` (depending on configuration). Programs that
consume Git output, compiler diagnostics, or SyncTeX data need paths in the
native namespace.

## Approach

Cygshim provides a native shim (written in Rust) that receives the caller's
arguments (using Rust's native Windows argument parsing) before Cygwin can
process them. It transports the arguments through the Windows environment,
using one contiguous numbered variable per argument:

```text
CYGSHIM_ARG_0=first
CYGSHIM_ARG_1=
CYGSHIM_ARG_2=[literal]
```

These environment variables guarantee safe passage of the literal arguments
into Cygwin. The shim starts Cygwin Bash with one fixed bootstrap command. A
small, trusted launch script is embedded in the `.exe` and supplied through
another environment variable. User arguments are never interpolated into or
evaluated as shell code. Bash reconstructs them into an array, converts the
absolute Windows input paths selected by the shim, and invokes the wrapped
program with `"${args[@]}"` semantics. Output parsing and artifact rewriting
remain in native Rust rather than crossing another shell-quoting boundary.
Before adding its transport variables, the shim removes every inherited
environment variable whose name begins with `CYGSHIM_`. Stale values from a
parent shim therefore cannot be mistaken for arguments or internal state.

If the caller's `CYGWIN` setting contains `noglob`, the shim temporarily removes
that option while Cygwin parses the fixed bootstrap,
`eval "$CYGSHIM_SCRIPT"`. Under `noglob`, Cygwin's quote incompatibility would
drop those literal quotes, so Bash would receive a different command. Normal
glob parsing preserves this fixed command, which contains no glob characters.
The embedded script restores the exact original `CYGWIN` value before starting
the requested tool. User arguments never cross this native-to-Cygwin parsing
boundary.

The process inherits the caller's working directory and remaining environment.
Standard streams and exit status are forwarded, with the narrow TeX filtering
described below when a stream is redirected. When none of the standard streams
is attached to a terminal, the shim asks Windows not to create a console window,
avoiding flashes when GUI applications run frequent commands.

The transport lives in a shared bridge source module within this package. Each
shim is a separate Rust binary target that uses the bridge and adds only its
tool-specific path conversion and output handling.

### Git

The Git shim:

- Converts absolute Windows path arguments, including values of `--git-dir=`
  and `--work-tree=`, through Cygwin's `cygpath`.
- Recognizes ordinary drive paths, UNC paths, and extended-length `\\?\` drive
  and UNC paths. Extended-length prefixes are removed before calling `cygpath`,
  which does not understand them directly.
- Runs `/usr/bin/git` explicitly, so placing the shim before Cygwin on `PATH`
  cannot recurse back into the shim.
- Converts absolute path lines emitted by path-producing `git rev-parse`
  operations such as `--show-toplevel`, `--absolute-git-dir`, and absolute
  `--git-dir`/`--git-common-dir` queries into forward-slash Windows paths.
- Passes all other Git output through byte-for-byte. In particular, diffs,
  objects, file contents, and NUL-delimited output are never passed through a
  global text replacement.

Converting every absolute Windows-looking argument is a narrow heuristic. It is
useful because a native caller naturally supplies absolute filesystem paths,
and Git ref names cannot contain a colon. A command such as `git grep` could in
principle use a regular expression beginning with a drive-like `C:\` sequence;
that unusual value would currently be treated as a path.

### TeX: latexmk and pdflatex

The TeX shims:

- Convert absolute Windows source-file and output-directory arguments with
  `cygpath`.
- Expand resolvable Windows 8.3 short-name aliases before conversion, preventing
  alias tildes such as `RUNNER~1` from being interpreted as TeX syntax.
- Run `/usr/bin/latexmk` or `/usr/bin/pdflatex` explicitly.
- When stdout or stderr is redirected, supervise it in Rust and rewrite known
  invocation paths plus unambiguous `/path:line:` diagnostics to mixed-form
  Windows paths such as `C:/Users/name/file.tex`. Known paths are recognized
  even when TeX inserts a hard line break inside them. Terminal output is left
  attached directly to TeX.
- After the tool exits, read actual path fields from generated SyncTeX files and
  from the optional recorder `.fls` file. Paths are deduplicated and converted
  together with one `cygpath -m -f -` process instead of probing possible drive
  mounts individually.
- Rewrite only `Input:<number>:<path>` fields in `.synctex.gz` or `.synctex`.
  Gzip decoding and encoding happen in Rust.
- Rewrite `.log` files by substituting only paths learned from the invocation,
  SyncTeX, or `.fls`, longest first. Parenthesized TeX file-open messages do not
  unambiguously delimit filenames containing spaces or parentheses, so the shim
  does not guess unknown paths from that syntax.
- Replace changed `.log` and SyncTeX files atomically while preserving their
  permissions. The recorder `.fls` file is read but not modified.
- Preserve the TeX program's exit status even if post-processing emits a
  warning.

Latexmk enables TeX's `-recorder` option by default, unless its configuration or
command line disables it, so a normal run produces the `.fls` data used for
broader `.log` coverage. Direct `pdflatex` does not; Cygshim does not silently
enable it. SyncTeX rewriting still works without a recorder because SyncTeX
supplies its own structured input paths. A caller that wants the additional
`.log` coverage can pass `-recorder` explicitly.

The current SyncTeX discovery covers the common one-document invocation used by
LaTeX Workshop, with `-outdir=`, `-output-directory=`, and optional `-jobname=`.
Supporting multiple source documents or additional TeX engines should extend
the TeX-specific logic rather than complicate the process bridge. `latexmk`
launches its configured TeX engine from inside Cygwin, so that nested invocation
does not need or use another native shim.

## Development

Building from source requires a native Windows Rust toolchain,
as provided by the
[standard rustup installation for Windows](https://rust-lang.org/tools/install/).
The command `rustc -vV` should report `x86_64-pc-windows-msvc` or
`x86_64-pc-windows-gnu`.
If you see a host such as `x86_64-pc-cygwin`, that Cygwin Rust toolchain will
produce executables linked to `cygwin1.dll`, which won't work for native shims.

Run development commands from Cygwin through the included `build.sh` helper.
It selects the native Windows Rust toolchain even when Cygwin's toolchain occurs
first on `PATH`, and removes an incompatible inherited Cargo jobserver setting.

### Building

Build optimized standalone executables with:

```bash
./build.sh
```

With no arguments, this is `cargo build --release` in Rust terminology.
Arguments replace that default Cargo command, so `build.sh` can run any Cargo
development command. Invoking native `cargo.exe` by its full path is not
sufficient when Cygwin's `rustc` still occurs first on `PATH`: Cargo would then
compile for `x86_64-pc-cygwin`, where `std::os::windows` is unavailable.
`CARGO_MAKEFLAGS` can likewise contain a jobserver representation that is
meaningful only to the toolchain that created it.

The standalone executables are written to:

```text
target/release/git.exe
target/release/latexmk.exe
target/release/pdflatex.exe
```

MSVC builds statically link the C runtime, so these executables do not require
the Visual C++ Redistributable to be installed separately.

### Checking everything

Run the same complete check as continuous integration with:

```bash
./check.sh
```

This checks Rust formatting and shell-script syntax, runs Clippy with warnings
denied, and runs all tests, including both Cygwin TeX integrations.

### Testing

Run all unit and integration tests with:

```bash
./build.sh test --all-targets
```

The Cargo equivalent is `cargo test --all-targets`, except for the automatic
TeX test detection described below.

The transport tests cover empty arguments, whitespace, braces, brackets, glob
characters, literal quotes, backslash-quote sequences, embedded newlines, and
extended-length Windows paths. An end-to-end Git test initializes a repository
whose path contains brackets and braces, verifies native path output, and
repeats the argument checks with an inherited `CYGWIN=noglob`.

When `build.sh` runs a Cargo `test` command, it detects `/usr/bin/pdflatex` and
`/usr/bin/latexmk` independently and enables each corresponding end-to-end test
that can run. The unavailable tests remain ignored. They cover direct
`pdflatex` without a recorder and Latexmk with its default recorder, using
source and output paths containing spaces and parentheses. With both tools
installed, the direct Cargo equivalent is:

```text
cargo test --features cygwin-tex-tests --test tex
```

### Formatting and linting

Format the Rust sources with:

```bash
./build.sh fmt
```

Use `./build.sh fmt -- --check` to check formatting without changing files. The
Cargo equivalents are `cargo fmt` and `cargo fmt -- --check`.

Run the Rust linter with:

```bash
./build.sh clippy --all-targets --all-features -- -D warnings
```

Clippy is Rust's standard collection of additional correctness, performance,
and style checks. `-D warnings` makes any finding fail the command.

### Installing from source

To build and install all three shims from source for the current user, run:

```bash
./install.sh -u
```

For a machine-wide installation, run an elevated Cygwin shell and use:

```bash
./install.sh -g
```

You can instead give a Windows or Cygwin directory explicitly:

```bash
./install.sh 'D:\Tools\Cygshim'
./install.sh /cygdrive/d/Tools/Cygshim
```

With no arguments, `install.sh` displays its usage. It runs the release build,
creates the destination if necessary, and copies `git.exe`, `latexmk.exe`, and
`pdflatex.exe`. It does not modify `PATH`. The predefined destinations are:

- `%LOCALAPPDATA%\Programs\Cygshim` for one user, without administrator access.
- `C:\Program Files\Cygshim` for a machine-wide installation,
  with administrator access.

Do not install the shims over Cygwin's real executables. To install only a
subset, build the project and copy the desired executables manually.

Then [update your PATH or configure the application explicitly](#installation).

### Releasing

Update the package version in `Cargo.toml` and `Cargo.lock`, commit it, and push
the `main` branch. Then build, check, package, tag, and publish the release with:

```bash
./release.sh --publish
```

The script derives the tag from the package version. Publishing requires an
authenticated [GitHub CLI](https://cli.github.com/), a clean `main` branch
matching `origin/main`, and a tag and release that do not already exist. It
repeats those remote checks after building and testing, then uploads a ZIP
archive and its SHA-256 checksum. GitHub generates the release notes.

By default, the script exercises the checks, build, and packaging without
accessing GitHub:

```bash
./release.sh
```

Rehearse the complete release, including its read-only GitHub and repository
checks, without publishing anything with:

```bash
./release.sh --dry-run
```

`--dry-run` takes precedence if combined with `--publish`.

Assets are left in `target/release-assets` for inspection.

## Finding Cygwin

The shim searches for Bash in this order:

1. `$CYGSHIM_BASH`, interpreted as the complete path to Cygwin `bash.exe`.
2. The Cygwin root reported by `cygpath.exe` candidates on `PATH`, in order,
   using `cygpath -w /` and then appending `bin\bash.exe`.
3. A `bash.exe` in a Windows `PATH` directory that also contains
   `cygwin1.dll`.
4. `C:\cygwin64\bin\bash.exe` and `C:\cygwin\bin\bash.exe`.

Every Bash candidate must have `cygwin1.dll` beside it. This prevents a
`cygpath.exe` or `bash.exe` from Git for Windows, MSYS2, or WSL from selecting
the wrong environment. `CYGSHIM_BASH` is consumed by the shim and is not
forwarded to the wrapped program.

## Limitations

- Windows limits the complete environment block to approximately 32 KiB. The
  numbered-variable transport therefore has a similar upper bound to the
  Windows command line, reduced by the size of the inherited environment and
  embedded tool script.
- Only Windows is a build target. Running the resulting shims requires Cygwin.
- Non-interactive Bash honors an inherited `BASH_ENV` before running the fixed
  bootstrap. Cygshim does not suppress this standard startup hook, so such a
  script can affect shim behavior.
- Output conversion is intentionally limited. New path-producing Git commands
  or TeX invocation forms may require shim-specific changes.
- TeX's unstructured file-open chatter may retain an unknown POSIX path in live
  output. Generated SyncTeX data is handled structurally, and `.log` coverage is
  broader when recorder data is available.
- TeX and Latexmk impose filename restrictions. For example,
  [Latexmk rejects filenames](https://latex.us/support/latexmk/latexmk.txt)
  containing `$`, `%`, `\`, `~`, a leading `&`, or unbalanced double quotes.
  Cygshim expands resolvable 8.3 aliases containing `~`, but cannot make a
  literal unsupported filename valid.
- Cygwin POSIX-only paths outside mounted Windows filesystems remain POSIX paths;
  there may be no meaningful native Windows path to report.

## Prior work

Cygshim was informed by these projects but does not incorporate their code:

- [nukata/cyg-git](https://github.com/nukata/cyg-git), an early native proxy
  and shell wrapper for using Cygwin Git from Go and VS Code.
- [andy-5/wslgit](https://github.com/andy-5/wslgit), a mature Rust bridge for
  using WSL Git from native Windows applications.

The projects solve related interoperability problems, but Cygwin's native
command-line parsing and its configurable mount table require a different
transport and path-conversion design.

## License

MIT
