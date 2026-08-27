use crate::bridge;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Win32 process flag that suppresses a console window for helper processes.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `MoveFileExW` flags used for replace-existing, write-through behavior.
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

// Win32 supplies short-name expansion and replace-existing rename semantics.
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn GetLongPathNameW(short: *const u16, long: *mut u16, capacity: u32) -> u32;
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
}

/// Minimal Cygwin-side launcher; output and artifact processing remain in Rust.
const TOOL_SCRIPT: &str = r#"
cygshim_tex_args=("${cygshim_args[@]}")

# Non-option absolute paths may name source files. Options are left alone except
# for the directory-valued forms recognized by both supported tools.
for ((cygshim_i = 0; cygshim_i < ${#cygshim_tex_args[@]}; cygshim_i++)); do
  cygshim_arg=${cygshim_tex_args[cygshim_i]}
  case $cygshim_arg in
    -outdir=*|-output-directory=*|--output-directory=*|-auxdir=*|-aux-directory=*|--aux-directory=*)
      cygshim_prefix=${cygshim_arg%%=*}=
      cygshim_value=${cygshim_arg#*=}
      cygshim_tex_args[cygshim_i]="$cygshim_prefix$(cygshim_to_posix_if_windows_absolute "$cygshim_value")"
      ;;
    -*)
      ;;
    *)
      cygshim_tex_args[cygshim_i]=$(cygshim_to_posix_if_windows_absolute "$cygshim_arg")
      ;;
  esac
done

exec "$cygshim_tex_program" "${cygshim_tex_args[@]}"
"#;

/// Runs one of the supported Cygwin TeX programs through the native shim.
pub fn run(program: &str) -> ExitCode {
    match run_inner(program) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("cygshim: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Supervises redirected streams and post-processes artifacts around one run.
fn run_inner(program: &str) -> io::Result<u8> {
    // TeX treats a tilde as syntax, so expand resolvable 8.3 aliases such as
    // RUNNER~1 before path conversion while preserving nonexistent paths.
    let args = env::args_os()
        .skip(1)
        .map(expand_tex_path_argument)
        .collect::<Vec<_>>();
    let invocation = TexInvocation::parse(&args)?;
    // Snapshot before spawning so only log and SyncTeX files changed by this
    // invocation are rewritten.
    let artifacts = Artifacts::snapshot(&invocation);
    let script = format!("cygshim_tex_program={program}\n{TOOL_SCRIPT}");
    let mut command = bridge::make_command(&script, args)?;
    // Use cygpath from the same installation as the Bash selected by the bridge.
    let cygpath = Path::new(command.get_program()).with_file_name("cygpath.exe");
    if !cygpath.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "could not find cygpath.exe next to {}",
                command.get_program().to_string_lossy()
            ),
        ));
    }
    let mapper = Arc::new(Mutex::new(PathMapper::new(
        cygpath,
        invocation.absolute_windows_paths(),
    )?));
    let filter_stdout = !io::stdout().is_terminal();
    let filter_stderr = !io::stderr().is_terminal();

    // Native editors redirect these streams and need native diagnostic paths;
    // interactive terminals retain TeX's direct stream behavior.
    if filter_stdout {
        command.stdout(Stdio::piped());
    }
    if filter_stderr {
        command.stderr(Stdio::piped());
    }

    let mut child = command.spawn()?;
    // Drain redirected streams concurrently so neither child pipe can block the
    // TeX process while the other is being read.
    let stdout_thread = child.stdout.take().map(|stdout| {
        let mapper = Arc::clone(&mapper);
        thread::spawn(move || {
            let output = io::stdout();
            forward_diagnostics(stdout, output.lock(), mapper)
        })
    });
    let stderr_thread = child.stderr.take().map(|stderr| {
        let mapper = Arc::clone(&mapper);
        thread::spawn(move || {
            let output = io::stderr();
            forward_diagnostics(stderr, output.lock(), mapper)
        })
    });

    let status = child.wait()?;
    report_forwarding_result("stdout", stdout_thread);
    report_forwarding_result("stderr", stderr_thread);

    // Post-processing is advisory and must not replace TeX's completed status.
    if let Ok(mut mapper) = mapper.lock()
        && let Err(error) = postprocess(&artifacts, &mut mapper)
    {
        eprintln!("cygshim: TeX post-processing failed: {error}");
    }

    Ok(status.code().unwrap_or(1) as u8)
}

fn report_forwarding_result(stream: &str, handle: Option<thread::JoinHandle<io::Result<()>>>) {
    let Some(handle) = handle else {
        return;
    };
    match handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Ok(Err(error)) => eprintln!("cygshim: could not filter TeX {stream}: {error}"),
        Err(_) => eprintln!("cygshim: TeX {stream} filter panicked"),
    }
}

/// Rewrites known paths across read boundaries, then parses complete diagnostics.
fn forward_diagnostics(
    mut reader: impl Read,
    mut writer: impl Write,
    mapper: Arc<Mutex<PathMapper>>,
) -> io::Result<()> {
    let mut input = [0; 8192];
    let mut path_buffer = Vec::new();
    let mut line_buffer = Vec::new();
    loop {
        let count = reader.read(&mut input)?;
        path_buffer.extend_from_slice(&input[..count]);
        let known_paths = match mapper.lock() {
            Ok(mapper) => drain_known_paths(&mut path_buffer, &mapper.ordered_paths, count == 0),
            Err(_) => std::mem::take(&mut path_buffer),
        };
        line_buffer.extend_from_slice(&known_paths);
        forward_complete_lines(&mut line_buffer, &mut writer, &mapper)?;
        if count == 0 {
            if !line_buffer.is_empty() {
                let line = std::mem::take(&mut line_buffer);
                let rewritten = match mapper.lock() {
                    Ok(mut mapper) => mapper.rewrite_diagnostic(&line),
                    Err(_) => line,
                };
                writer.write_all(&rewritten)?;
            }
            writer.flush()?;
            return Ok(());
        }
        writer.flush()?;
    }
}

fn forward_complete_lines(
    buffer: &mut Vec<u8>,
    writer: &mut impl Write,
    mapper: &Arc<Mutex<PathMapper>>,
) -> io::Result<()> {
    while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
        let line = buffer.drain(..=end).collect::<Vec<_>>();
        let rewritten = match mapper.lock() {
            Ok(mut mapper) => mapper.rewrite_diagnostic(&line),
            Err(_) => line,
        };
        writer.write_all(&rewritten)?;
    }
    Ok(())
}

/// Maps discovered Cygwin paths to native paths for structured and textual use.
struct PathMapper {
    cygpath: PathBuf,
    // Exact Cygwin-to-Windows mappings used for structured formats.
    paths: HashMap<Vec<u8>, Vec<u8>>,
    // The same mappings, longest first, for deterministic textual replacement.
    ordered_paths: Vec<(Vec<u8>, Vec<u8>)>,
}

impl PathMapper {
    fn new(cygpath: PathBuf, windows_paths: Vec<String>) -> io::Result<Self> {
        let mut mapper = Self {
            cygpath,
            paths: HashMap::new(),
            ordered_paths: Vec::new(),
        };
        if !windows_paths.is_empty() {
            // TeX echoes the converted POSIX forms of native invocation paths,
            // so establish both sides of those mappings before it starts.
            let inputs = windows_paths
                .iter()
                .map(|path| path.as_bytes().to_vec())
                .collect::<Vec<_>>();
            let posix_paths = bulk_cygpath(&mapper.cygpath, "-u", &inputs)?;
            for (posix, windows) in posix_paths.into_iter().zip(windows_paths) {
                mapper.paths.insert(posix, windows.into_bytes());
            }
            mapper.rebuild_ordered_paths();
        }
        Ok(mapper)
    }

    fn add_posix_paths(&mut self, paths: impl IntoIterator<Item = Vec<u8>>) -> io::Result<()> {
        // Deduplicate before paying for a single bulk cygpath invocation.
        let mut unseen = Vec::new();
        let mut seen = HashSet::new();
        for path in paths {
            if is_posix_absolute(&path)
                && !self.paths.contains_key(&path)
                && seen.insert(path.clone())
            {
                unseen.push(path);
            }
        }
        if unseen.is_empty() {
            return Ok(());
        }

        let windows_paths = bulk_cygpath(&self.cygpath, "-m", &unseen)?;
        for (posix, windows) in unseen.into_iter().zip(windows_paths) {
            self.paths.insert(posix, windows);
        }
        self.rebuild_ordered_paths();
        Ok(())
    }

    fn rebuild_ordered_paths(&mut self) {
        self.ordered_paths = self
            .paths
            .iter()
            .map(|(from, to)| (from.clone(), to.clone()))
            .collect();
        // Prevent a directory prefix from taking precedence over a full path.
        self.ordered_paths
            .sort_unstable_by_key(|mapping| std::cmp::Reverse(mapping.0.len()));
    }

    fn rewrite_diagnostic(&mut self, line: &[u8]) -> Vec<u8> {
        // Known paths need no delimiter inference. Unknown paths are learned only
        // from the unambiguous file:line form, never TeX's parenthesized syntax.
        let rewritten = replace_known_paths(line, &self.ordered_paths);
        let Some((start, end)) = file_line_path_span(&rewritten) else {
            return rewritten;
        };
        let path = rewritten[start..end].to_vec();
        if !self.paths.contains_key(&path) && self.add_posix_paths([path.clone()]).is_err() {
            return rewritten;
        }
        let Some(windows) = self.paths.get(&path) else {
            return rewritten;
        };
        let mut result = Vec::with_capacity(rewritten.len() - path.len() + windows.len());
        result.extend_from_slice(&rewritten[..start]);
        result.extend_from_slice(windows);
        result.extend_from_slice(&rewritten[end..]);
        result
    }
}

/// Converts a batch of newline-free paths in one `cygpath -f -` invocation.
fn bulk_cygpath(cygpath: &Path, mode: &str, paths: &[Vec<u8>]) -> io::Result<Vec<Vec<u8>>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = Command::new(cygpath);
    command
        .args([mode, "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("piped cygpath stdin");
    let input = paths
        .iter()
        .flat_map(|path| path.iter().copied().chain(*b"\n"))
        .collect::<Vec<_>>();
    // Feed stdin while wait_with_output drains both output pipes, avoiding a
    // deadlock even if either side grows beyond the OS pipe buffer.
    let writer = thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    match writer.join() {
        Ok(result) => result?,
        Err(_) => return Err(io::Error::other("cygpath input writer panicked")),
    }
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cygpath failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let converted = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).to_vec())
        .collect::<Vec<_>>();
    if converted.len() != paths.len() {
        return Err(io::Error::other(format!(
            "cygpath returned {} paths for {} inputs",
            converted.len(),
            paths.len()
        )));
    }
    Ok(converted)
}

/// Finds the path in a leading `/path:line:` diagnostic, optionally after `(`.
fn file_line_path_span(line: &[u8]) -> Option<(usize, usize)> {
    let content_end = line
        .iter()
        .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
        .map_or(0, |index| index + 1);
    for colon in 1..content_end {
        if line[colon] != b':' {
            continue;
        }
        let mut cursor = colon + 1;
        let digits_start = cursor;
        while cursor < content_end && line[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start || cursor >= content_end || line[cursor] != b':' {
            continue;
        }
        if line[0] == b'/' {
            return Some((0, colon));
        }
        if line.starts_with(b"(/") {
            return Some((1, colon));
        }
    }
    None
}

fn replace_known_paths(data: &[u8], mappings: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut buffer = data.to_vec();
    drain_known_paths(&mut buffer, mappings, true)
}

/// Emits the safe prefix and retains a possible partial path until more input.
fn drain_known_paths(
    buffer: &mut Vec<u8>,
    mappings: &[(Vec<u8>, Vec<u8>)],
    end_of_input: bool,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(buffer.len());
    let mut cursor = 0;
    while cursor < buffer.len() {
        let mut replacement = None;
        let mut needs_more_input = false;
        for (from, to) in mappings {
            match match_known_path(&buffer[cursor..], from) {
                PathMatch::Full(consumed) => {
                    replacement = Some((consumed, to));
                    break;
                }
                PathMatch::Partial if !end_of_input => {
                    needs_more_input = true;
                    break;
                }
                PathMatch::Partial | PathMatch::None => {}
            }
        }
        if let Some((consumed, to)) = replacement {
            output.extend_from_slice(to);
            cursor += consumed;
        } else if needs_more_input {
            break;
        } else {
            output.push(buffer[cursor]);
            cursor += 1;
        }
    }
    buffer.drain(..cursor);
    output
}

enum PathMatch {
    Full(usize),
    Partial,
    None,
}

/// Matches a known path while ignoring hard line breaks inserted by TeX.
fn match_known_path(data: &[u8], path: &[u8]) -> PathMatch {
    if data.first() != path.first() || path.is_empty() {
        return PathMatch::None;
    }
    let mut data_index = 0;
    for &path_byte in path {
        while matches!(data.get(data_index), Some(b'\r' | b'\n')) {
            data_index += 1;
        }
        let Some(&data_byte) = data.get(data_index) else {
            return PathMatch::Partial;
        };
        if data_byte != path_byte {
            return PathMatch::None;
        }
        data_index += 1;
    }
    PathMatch::Full(data_index)
}

/// Discovers structured paths and rewrites only artifacts changed by this run.
fn postprocess(artifacts: &Artifacts, mapper: &mut PathMapper) -> io::Result<()> {
    let logs = artifacts.changed(&artifacts.logs);
    let synctex_files = artifacts.changed(&artifacts.synctex);
    if logs.is_empty() && synctex_files.is_empty() {
        return Ok(());
    }

    let mut paths = Vec::new();
    for fls in artifacts.existing(&artifacts.fls) {
        match fs::read(&fls.path) {
            Ok(contents) => paths.extend(paths_from_fls(&contents)),
            Err(error) => eprintln!("cygshim: could not read {}: {error}", fls.path.display()),
        }
    }

    let mut synctex_contents = Vec::new();
    for synctex in &synctex_files {
        match read_synctex(&synctex.path) {
            Ok((contents, compressed)) => {
                paths.extend(paths_from_synctex(&contents));
                synctex_contents.push((synctex.path.clone(), contents, compressed));
            }
            Err(error) => eprintln!(
                "cygshim: could not read SyncTeX file {}: {error}",
                synctex.path.display()
            ),
        }
    }

    mapper.add_posix_paths(paths)?;

    for log in logs {
        let contents = fs::read(&log.path)?;
        let rewritten = replace_known_paths(&contents, &mapper.ordered_paths);
        if rewritten != contents {
            replace_file(&log.path, &rewritten)?;
        }
    }

    for (path, contents, compressed) in synctex_contents {
        let rewritten = rewrite_synctex(&contents, &mapper.paths);
        if rewritten == contents {
            continue;
        }
        let output = if compressed {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&rewritten)?;
            encoder.finish()?
        } else {
            rewritten
        };
        replace_file(&path, &output)?;
    }
    Ok(())
}

/// Extracts absolute paths from recorder `INPUT` and `OUTPUT` records.
fn paths_from_fls(contents: &[u8]) -> Vec<Vec<u8>> {
    contents
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            [b"INPUT ".as_slice(), b"OUTPUT ".as_slice()]
                .iter()
                .find_map(|prefix| line.strip_prefix(*prefix))
                .filter(|path| is_posix_absolute(path))
                .map(<[u8]>::to_vec)
        })
        .collect()
}

/// Extracts paths only from SyncTeX's structured `Input:<tag>:<path>` records.
fn paths_from_synctex(contents: &[u8]) -> Vec<Vec<u8>> {
    contents
        .split(|byte| *byte == b'\n')
        .filter_map(synctex_path)
        .filter(|path| is_posix_absolute(path))
        .map(<[u8]>::to_vec)
        .collect()
}

/// Returns the path field while allowing colons inside the path itself.
fn synctex_path(line: &[u8]) -> Option<&[u8]> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let rest = line.strip_prefix(b"Input:")?;
    let separator = rest.iter().position(|byte| *byte == b':')?;
    Some(&rest[separator + 1..])
}

/// Rewrites structured path fields while preserving all other bytes and endings.
fn rewrite_synctex(contents: &[u8], mappings: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut output = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
        let line_without_cr = line_without_newline
            .strip_suffix(b"\r")
            .unwrap_or(line_without_newline);
        if let Some(path) = synctex_path(line_without_cr)
            && let Some(replacement) = mappings.get(path)
        {
            let path_start = line_without_cr.len() - path.len();
            output.extend_from_slice(&line_without_cr[..path_start]);
            output.extend_from_slice(replacement);
            if line_without_newline.ends_with(b"\r") {
                output.push(b'\r');
            }
            if line.ends_with(b"\n") {
                output.push(b'\n');
            }
            continue;
        }
        output.extend_from_slice(line);
    }
    output
}

/// Reads plain or gzip SyncTeX and reports which form must be written back.
fn read_synctex(path: &Path) -> io::Result<(Vec<u8>, bool)> {
    let compressed = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".gz"));
    let mut contents = Vec::new();
    if compressed {
        GzDecoder::new(File::open(path)?).read_to_end(&mut contents)?;
    } else {
        File::open(path)?.read_to_end(&mut contents)?;
    }
    Ok((contents, compressed))
}

/// Replaces a file through a same-directory temporary while retaining permissions.
fn replace_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    let mut attempt = 0u32;
    let temporary = loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".cygshim.{}.{}", std::process::id(), attempt));
        let candidate = PathBuf::from(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                let write_result = file.write_all(contents).and_then(|()| file.flush());
                // Windows cannot remove an open file created without delete sharing.
                drop(file);
                if let Err(error) = write_result {
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                if let Err(error) = fs::set_permissions(&candidate, metadata.permissions()) {
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                break candidate;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => attempt += 1,
            Err(error) => return Err(error),
        }
    };

    if let Err(error) = move_replace(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn move_replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = wide_string(source.as_os_str());
    let destination = wide_string(destination.as_os_str());
    // SAFETY: Both paths are NUL-terminated and remain alive for the call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide_string(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn is_posix_absolute(path: &[u8]) -> bool {
    path.starts_with(b"/")
}

/// Cheap identity used to determine whether a candidate artifact changed.
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    length: u64,
    modified: Option<std::time::SystemTime>,
}

impl Fingerprint {
    fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        Some(Self::from_metadata(&metadata))
    }

    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

struct WatchedFile {
    path: PathBuf,
    before: Option<Fingerprint>,
}

impl WatchedFile {
    fn new(path: PathBuf) -> Self {
        let before = Fingerprint::read(&path);
        Self { path, before }
    }

    fn changed(&self) -> bool {
        Fingerprint::read(&self.path).is_some_and(|after| self.before.as_ref() != Some(&after))
    }

    fn exists(&self) -> bool {
        self.path.is_file()
    }
}

/// Candidate artifacts fingerprinted before execution to detect fresh output.
struct Artifacts {
    logs: Vec<WatchedFile>,
    fls: Vec<WatchedFile>,
    synctex: Vec<WatchedFile>,
}

impl Artifacts {
    fn snapshot(invocation: &TexInvocation) -> Self {
        let mut logs = Vec::new();
        let mut fls = Vec::new();
        let mut synctex = Vec::new();
        let Some(base) = &invocation.base else {
            return Self { logs, fls, synctex };
        };
        // Different TeX and Latexmk options place artifacts in different roots;
        // watching all plausible directories is cheaper than guessing one.
        for directory in &invocation.directories {
            logs.push(WatchedFile::new(directory.join(base).with_extension("log")));
            fls.push(WatchedFile::new(directory.join(base).with_extension("fls")));
            synctex.push(WatchedFile::new(
                directory.join(base).with_extension("synctex.gz"),
            ));
            synctex.push(WatchedFile::new(
                directory.join(base).with_extension("synctex"),
            ));
        }
        Self { logs, fls, synctex }
    }

    fn changed<'a>(&self, files: &'a [WatchedFile]) -> Vec<&'a WatchedFile> {
        files.iter().filter(|file| file.changed()).collect()
    }

    fn existing<'a>(&self, files: &'a [WatchedFile]) -> Vec<&'a WatchedFile> {
        files.iter().filter(|file| file.exists()).collect()
    }
}

/// The command-line subset needed to seed mappings and locate output artifacts.
struct TexInvocation {
    args: Vec<String>,
    base: Option<OsString>,
    directories: Vec<PathBuf>,
}

impl TexInvocation {
    fn parse(args: &[OsString]) -> io::Result<Self> {
        let args = args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let cwd = env::current_dir()?;
        // The supported one-document invocation uses the last positional .tex
        // argument as its source and derives the default job name from it.
        let source = args
            .iter()
            .rev()
            .find(|argument| {
                !argument.starts_with('-') && argument.to_ascii_lowercase().ends_with(".tex")
            })
            .map(PathBuf::from);
        let jobname = option_value(&args, &["-jobname="]).map(OsString::from);
        let base = jobname.or_else(|| {
            source
                .as_ref()
                .and_then(|path| path.file_stem())
                .map(OsStr::to_os_string)
        });
        let outdir = option_value(
            &args,
            &["-outdir=", "-output-directory=", "--output-directory="],
        )
        .map(|directory| resolve_directory(&cwd, directory));
        let auxdir = option_value(&args, &["-auxdir=", "-aux-directory=", "--aux-directory="])
            .map(|directory| resolve_directory(&cwd, directory));
        let change_directory = args.iter().any(|argument| argument == "-cd")
            && !args.iter().any(|argument| argument == "-cd-");
        let mut directories = Vec::new();
        if let Some(directory) = auxdir {
            push_unique(&mut directories, directory);
        }
        if let Some(directory) = outdir {
            push_unique(&mut directories, directory);
        }
        if change_directory && let Some(parent) = source.as_ref().and_then(|path| path.parent()) {
            push_unique(&mut directories, resolve_path(&cwd, parent));
        }
        push_unique(&mut directories, cwd.clone());
        if let Some(parent) = source.as_ref().and_then(|path| path.parent())
            && !parent.as_os_str().is_empty()
        {
            push_unique(&mut directories, resolve_path(&cwd, parent));
        }

        Ok(Self {
            args,
            base,
            directories,
        })
    }

    /// Returns native invocation paths that TeX may later echo in POSIX form.
    fn absolute_windows_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for argument in &self.args {
            let value = option_path_value(argument).unwrap_or(argument);
            if Path::new(value).is_absolute() {
                let normalized = normalize_windows_path(value);
                if !paths.contains(&normalized) {
                    paths.push(normalized);
                }
            }
        }
        paths
    }
}

fn option_value<'a>(args: &'a [String], prefixes: &[&str]) -> Option<&'a str> {
    args.iter().rev().find_map(|argument| {
        prefixes
            .iter()
            .find_map(|prefix| argument.strip_prefix(prefix))
    })
}

fn option_path_value(argument: &str) -> Option<&str> {
    [
        "-outdir=",
        "-output-directory=",
        "--output-directory=",
        "-auxdir=",
        "-aux-directory=",
        "--aux-directory=",
    ]
    .iter()
    .find_map(|prefix| argument.strip_prefix(prefix))
}

/// Expands existing absolute path arguments from Windows short to long form.
fn expand_tex_path_argument(argument: OsString) -> OsString {
    let text = argument.to_string_lossy();
    if let Some(value) = option_path_value(&text) {
        let prefix_length = text.len() - value.len();
        if let Some(path) = long_path_name(Path::new(value)) {
            let mut expanded = OsString::from(&text[..prefix_length]);
            expanded.push(path);
            return expanded;
        }
        return argument;
    }
    if text.starts_with('-') {
        return argument;
    }
    long_path_name(Path::new(argument.as_os_str()))
        .map(PathBuf::into_os_string)
        .unwrap_or(argument)
}

/// Uses Windows filesystem metadata to expand every resolvable short component.
fn long_path_name(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let input = wide_string(path.as_os_str());
    let mut output = vec![0; input.len()];
    loop {
        let capacity = u32::try_from(output.len()).ok()?;
        // SAFETY: The input is NUL-terminated, and output describes a writable
        // buffer of exactly `capacity` UTF-16 code units.
        let length = unsafe { GetLongPathNameW(input.as_ptr(), output.as_mut_ptr(), capacity) };
        if length == 0 {
            return None;
        }
        let length = length as usize;
        if length < output.len() {
            output.truncate(length);
            return Some(PathBuf::from(OsString::from_wide(&output)));
        }
        // A too-small result includes space for the terminating NUL.
        output.resize(length, 0);
    }
}

/// Removes extended-length prefixes unsupported by cygpath and normalizes slashes.
fn normalize_windows_path(path: &str) -> String {
    let path = if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        path.to_owned()
    };
    path.replace('\\', "/")
}

fn resolve_directory(cwd: &Path, directory: &str) -> PathBuf {
    resolve_path(cwd, Path::new(directory))
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Uses a linear scan, but artifact discovery adds at most five paths.
fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_unambiguous_file_line_paths() {
        assert_eq!(
            file_line_path_span(b"/cygdrive/c/a file (draft).tex:42: error\n"),
            Some((0, 30))
        );
        assert_eq!(
            file_line_path_span(b"(/usr/share/texmf/a.cls:7: error\n"),
            Some((1, 23))
        );
        assert_eq!(file_line_path_span(b"(ambiguous file.tex)\n"), None);
    }

    #[test]
    fn reads_paths_with_spaces_and_parentheses_from_fls() {
        let paths = paths_from_fls(
            b"PWD /cygdrive/c/work\nINPUT /cygdrive/c/a file (draft).tex\nOUTPUT relative.pdf\n",
        );
        assert_eq!(paths, [b"/cygdrive/c/a file (draft).tex".to_vec()]);
    }

    #[test]
    fn rewrites_only_synctex_input_path_fields() {
        let input = b"SyncTeX Version:1\nInput:1:/cygdrive/c/a file.tex\nOutput:pdf\n";
        let mappings = HashMap::from([(
            b"/cygdrive/c/a file.tex".to_vec(),
            b"C:/a file.tex".to_vec(),
        )]);
        assert_eq!(
            rewrite_synctex(input, &mappings),
            b"SyncTeX Version:1\nInput:1:C:/a file.tex\nOutput:pdf\n"
        );
    }

    #[test]
    fn longest_known_path_wins() {
        let mappings = [
            (b"/root/a.tex".to_vec(), b"C:/root/a.tex".to_vec()),
            (b"/root".to_vec(), b"C:/root".to_vec()),
        ];
        assert_eq!(
            replace_known_paths(b"(/root/a.tex)", &mappings),
            b"(C:/root/a.tex)"
        );
    }

    #[test]
    fn rewrites_known_paths_across_tex_line_wrapping() {
        let mappings = [(
            b"/cygdrive/c/a file (draft).tex".to_vec(),
            b"C:/a file (draft).tex".to_vec(),
        )];
        assert_eq!(
            replace_known_paths(
                b"(/cygdrive/c/a file\r\n (draft).tex:42: error\n",
                &mappings,
            ),
            b"(C:/a file (draft).tex:42: error\n"
        );
    }

    #[test]
    fn buffers_a_known_path_split_across_reads() {
        let mappings = [(b"/cygdrive/c/a.tex".to_vec(), b"C:/a.tex".to_vec())];
        let mut buffer = b"before (/cygdrive/c/a".to_vec();
        assert_eq!(
            drain_known_paths(&mut buffer, &mappings, false),
            b"before ("
        );
        assert_eq!(buffer, b"/cygdrive/c/a");
        buffer.extend_from_slice(b".tex) after\n");
        assert_eq!(
            drain_known_paths(&mut buffer, &mappings, false),
            b"C:/a.tex) after\n"
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn expands_existing_short_path_options() {
        let short_path = Path::new(r"C:\PROGRA~1");
        if !short_path.exists() {
            return;
        }
        let expanded = expand_tex_path_argument(OsString::from(r"-outdir=C:\PROGRA~1"));
        assert!(!expanded.to_string_lossy().contains('~'));
    }
}
