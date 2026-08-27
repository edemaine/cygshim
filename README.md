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

The transport lives in a shared source module within this package. Each shim is
a separate Rust binary target with its own tool-specific logic; there is no
separately published shared crate and no runtime shell-script installation.

A shared bridge transports process data, including arguments as described above.
Each shim then does limited further conversions appropriate to its program,
including path translation.

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

## Requirements

- 64-bit Windows
- 64-bit Cygwin, including `bash` and `cygpath`
- The Cygwin packages for the shim being built and used (`git`, `latexmk`,
  `texlive-collection-latex`, and related TeX packages as appropriate)
- A native Windows Rust toolchain

The last distinction is important. Cygwin's Rust package reports a host such as
`x86_64-pc-cygwin` and produces executables linked to `cygwin1.dll`; those are
not suitable as native shims. `rustc -vV` should instead report
`x86_64-pc-windows-msvc` or `x86_64-pc-windows-gnu`. The standard rustup Windows
installation provides an appropriate toolchain.

## Building

If you just have a native Windows Rust toolchain, you can build via

```powershell
cargo build --release
```

If you also have a Cygwin Rust toolchain, use the included build helper:

```bash
./build.sh
```

With no arguments, `build.sh` runs `cargo build --release`. Arguments replace
that default Cargo command, so the same helper can run tests or clean artifacts.
It puts the native rustup directory first on `PATH` and removes
`CARGO_MAKEFLAGS`. Invoking native `cargo.exe` by its full path is not sufficient
when Cygwin's `rustc` still occurs first on `PATH`: Cargo would then compile for
`x86_64-pc-cygwin`, where `std::os::windows` is unavailable. `CARGO_MAKEFLAGS`
can likewise contain a jobserver representation that is meaningful only to the
toolchain that created it.

The standalone executables are written to:

```text
target/release/git.exe
target/release/latexmk.exe
target/release/pdflatex.exe
```

## Installing

Copy the desired executables to a dedicated directory.
(Do not copy the shims over Cygwin's real executables.)
Suitable locations are:

- `%LOCALAPPDATA%\Programs\Cygshim` for one user, without administrator access.
- `C:\Program Files\Cygshim` for a machine-wide installation, with administrator
  access.

For example:

```text
C:\Program Files\Cygshim\git.exe
C:\Program Files\Cygshim\latexmk.exe
C:\Program Files\Cygshim\pdflatex.exe
```

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

Applications that support an explicit executable path do not need a PATH
change. For VS Code's built-in Git support:

```json
{
  "git.path": "C:\\Program Files\\Cygshim\\git.exe"
}
```

For LaTeX Workshop, define tools whose `command` is the corresponding native
shim. The ordinary LaTeX arguments can then be listed directly.

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
- Cygwin POSIX-only paths outside mounted Windows filesystems remain POSIX paths;
  there may be no meaningful native Windows path to report.

## Testing

Run tests with the native Windows Rust environment configured under
[**Building**](#building):

```powershell
./build.sh test --all-targets
```

If you just have a native Windows Rust toolchain, this is roughly equivalent to
`cargo test --all-targets`, but note LaTeX tests discussed below.

The transport tests cover empty arguments, whitespace, braces, brackets, glob
characters, literal quotes, backslash-quote sequences, embedded newlines, and
extended-length Windows paths. An end-to-end Git test initializes a repository
whose path contains brackets and braces, verifies native path output, and
repeats the argument checks with an inherited `CYGWIN=noglob`.

When `build.sh` runs a Cargo `test` command, it detects `/usr/bin/pdflatex` and
`/usr/bin/latexmk`. If both are installed, it automatically enables the
end-to-end TeX tests; otherwise, they remain ignored. They cover direct
`pdflatex` without a recorder and Latexmk with its default recorder, using
source and output paths containing spaces and parentheses. From PowerShell,
enable them explicitly when the Cygwin TeX packages are installed:

```powershell
cargo test --features cygwin-tex-tests --test tex
```

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
