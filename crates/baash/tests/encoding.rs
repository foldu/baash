//! What a script written on Windows looks like by the time baash parses it.
//!
//! A Windows editor is not obliged to write what a POSIX shell expects: it may
//! prefix a byte order mark, and it writes `\r\n` line endings. Both used to
//! surface as a syntax error that pointed at the shell rather than at the file
//! — a `for` loop whose `for` is the word `\u{feff}for` is "unexpected `do`".
//!
//! These cases run the real binary on a real file, because the decoding is a
//! property of the reader (`read_source`, `run_stdin`), not of the parser: the
//! lexer still sees exactly the bytes it is handed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn baash() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baash"))
}

/// A temporary directory that removes itself.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("baash-encoding-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, bytes).expect("write the script");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Run the script file at `path`, requiring success, and return stdout.
fn run_file(path: &Path) -> String {
    let output = baash().arg(path).output().expect("spawn baash");
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected success for {}; stderr: {}",
        path.display(),
        stderr(&output)
    );
    stdout(&output)
}

/// A `for` loop, the shape whose keywords a mangled encoding breaks first.
const LOOP_CRLF: &[u8] = b"for i in 1 2; do\r\n\techo $i\r\ndone\r\n";

#[test]
fn a_crlf_script_runs() {
    let dir = TestDir::new("crlf");
    let path = dir.write("loop.sh", LOOP_CRLF);
    assert_eq!(run_file(&path), "1\n2\n");
}

#[test]
fn a_utf8_bom_script_runs() {
    let dir = TestDir::new("bom8");
    let mut script = vec![0xEF, 0xBB, 0xBF];
    script.extend_from_slice(b"for i in 1 2; do echo $i; done\n");
    let path = dir.write("loop.sh", &script);
    assert_eq!(run_file(&path), "1\n2\n");
}

#[test]
fn a_utf16le_script_runs() {
    // PowerShell 5's `>` writes UTF-16LE with a mark, so this is what a script
    // captured from it looks like.
    let dir = TestDir::new("bom16");
    let mut script = vec![0xFF, 0xFE];
    for unit in "for i in 1 2; do echo $i; done\n".encode_utf16() {
        script.extend_from_slice(&unit.to_le_bytes());
    }
    let path = dir.write("loop.sh", &script);
    assert_eq!(run_file(&path), "1\n2\n");
}

#[test]
fn a_crlf_script_on_stdin_runs() {
    let mut child = baash()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn baash");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(LOOP_CRLF)
        .expect("write the script");
    let output = child.wait_with_output().expect("wait for baash");
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "1\n2\n");
}

#[test]
fn a_crlf_command_string_runs() {
    // `baash -c "$(cat file)"` on a Windows file: the bytes arrive from the
    // command line, not from a file, and are settled the same way.
    let output = baash()
        .arg("-c")
        .arg("for i in 1 2; do\r\necho $i\r\ndone\r\n")
        .output()
        .expect("spawn baash");
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "1\n2\n");
}

#[test]
fn a_lone_carriage_return_is_still_a_character() {
    // Only the `\r` of a `\r\n` pair is a line ending; a bare one is a byte of
    // the line, as it is for bash. Counting what `printf` writes is the
    // shortest way to see it survive: three bytes, not two.
    let dir = TestDir::new("lone");
    let path = dir.write("script.sh", b"printf 'a\\rb' | wc -c\n");
    assert_eq!(run_file(&path).trim(), "3");
}
