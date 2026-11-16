// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-side end-to-end driver for a folder drop, without the GTK viewer.
//!
//! It plays the host role against a real guest agent over the serial channel (the viewer must not
//! be connected: the chardev takes one client). It builds a test tree, packs it the way the viewer
//! does (`archive::pack` for Windows), streams it as `DropBegin` + `DropArchiveStart` +
//! `DropFileChunk`s + `DropFileFinish` and prints the guest's `DropResult` and the
//! `ArchiveProgress` it saw. It also writes `expected.txt` into the work directory: one
//! `relative/path<TAB>size<TAB>sha256` line per file the guest must end up with (a directory has
//! `-` for size and hash), so the guest folder can be compared.
//!
//! The tree holds nested and empty folders, a non-ASCII name, a multi-MiB file, and names Windows
//! cannot create (`a:b.txt`, `trailing.`, `CON.txt`) that must be left out and reported.
//!
//! ```text
//! cargo run --release -p winbridge-host --example folderdnd -- \
//!     --socket /path/to/winbridge.sock --work-dir /path/to/scratch/folderdnd
//! ```

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use winbridge_proto::archive::{self, Dest};
use winbridge_proto::{read_frame, write_frame, Message};

const CHUNK: usize = 256 * 1024;
const TRANSFER_ID: u32 = 0x0f01_d000;

fn die(msg: &str) -> ! {
    eprintln!("folderdnd: {msg}");
    std::process::exit(2);
}

fn main() {
    let mut socket = None;
    let mut work = None;
    let (mut x, mut y) = (0u32, 0u32);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| die(&format!("{a} needs a value")));
        match a.as_str() {
            "--socket" => socket = Some(PathBuf::from(value())),
            "--work-dir" => work = Some(PathBuf::from(value())),
            "--x" => x = value().parse().unwrap_or_else(|_| die("--x is a number")),
            "--y" => y = value().parse().unwrap_or_else(|_| die("--y is a number")),
            _ => die(&format!("unknown argument {a}")),
        }
    }
    let socket = socket.unwrap_or_else(|| die("--socket is required"));
    let work = work.unwrap_or_else(|| die("--work-dir is required"));

    let _ = std::fs::remove_dir_all(&work);
    let tree = work.join("src").join("wbdnd-tree");
    build_tree(&tree);
    let lone = work.join("src").join("wbdnd-lone.txt");
    std::fs::write(&lone, b"a single file dropped together with the folder\n").unwrap();

    let zip = work.join("drop.zip");
    let report = archive::pack(&[tree.clone(), lone.clone()], Dest::Windows, &zip, 8 << 30, &mut |_, _| true)
        .unwrap_or_else(|e| die(&format!("pack: {e}")));
    println!("PACKED {} file(s), {} folder(s), {} bytes, {} left out", report.files, report.dirs, report.bytes, report.skipped);
    println!("NOTICE {}", report.notice().unwrap_or_else(|| "(none)".into()).replace('\n', " | "));

    // What the guest must have: unpack the same zip here, as a Linux machine would.
    let expected = work.join("expected");
    let tops = archive::unpack(&zip, &expected, Dest::Linux, 8 << 30, &mut |_, _| true).unwrap_or_else(|e| die(&format!("unpack: {e}")));
    write_expected(&expected, &tops, &work.join("expected.txt"));

    let mut stream = UnixStream::connect(&socket).unwrap_or_else(|e| die(&format!("cannot connect to {}: {e}", socket.display())));
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        write_frame(&mut stream, &Message::QueryStatus).unwrap();
        match read_frame(&mut stream) {
            Ok(Message::StatusReply { os_description, .. }) => {
                println!("AGENT {os_description}");
                break;
            }
            _ if Instant::now() > deadline => die("the agent did not answer QueryStatus"),
            _ => {}
        }
    }

    let total = std::fs::metadata(&zip).unwrap().len();
    let t0 = Instant::now();
    send(&mut stream, Message::DropBegin { transfer_id: TRANSFER_ID, x, y, file_count: 1 });
    send(&mut stream, Message::DropArchiveStart { transfer_id: TRANSFER_ID, operation_id: TRANSFER_ID, total_bytes: total });
    let mut file = std::fs::File::open(&zip).unwrap();
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0u64;
    loop {
        let n = std::io::Read::read(&mut file, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        send(&mut stream, Message::DropFileChunk { transfer_id: TRANSFER_ID, offset, data: buf[..n].to_vec() });
        offset += n as u64;
    }
    send(&mut stream, Message::DropFileFinish { transfer_id: TRANSFER_ID, success: true, error: None });
    println!("SENT {total} bytes of zip in {:.2} s", t0.elapsed().as_secs_f64());

    let until = Instant::now() + Duration::from_secs(300);
    let mut last_phase = None;
    loop {
        if Instant::now() > until {
            die("no DropResult from the guest");
        }
        match read_frame(&mut stream) {
            Ok(Message::ArchiveProgress { phase, done, total, .. }) => {
                if last_phase != Some(phase) {
                    println!("PROGRESS phase {phase} ({}) {done}/{total} at {:.2} s", archive::phase_name(phase), t0.elapsed().as_secs_f64());
                    last_phase = Some(phase);
                }
            }
            Ok(Message::DropResult { success, message }) => {
                println!("DROP_RESULT ok={success} at {:.2} s: {}", t0.elapsed().as_secs_f64(), message.replace('\n', " | "));
                std::process::exit(if success { 0 } else { 1 });
            }
            Ok(_) => {}
            Err(_) => {} // read timeout: keep waiting
        }
    }
}

fn send(stream: &mut UnixStream, m: Message) {
    write_frame(stream, &m).unwrap_or_else(|e| die(&format!("write: {e}")));
}

fn put(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, data).unwrap();
}

/// Deterministic content: xorshift64.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut v = Vec::with_capacity(len + 8);
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("empty")).unwrap();
    put(root, "a/b/c/deep.txt", b"deep\n");
    put(root, "a/side.txt", b"side\n");
    put(root, "uni-\u{e9}-\u{65e5}\u{672c}.txt", "unicode name\n".as_bytes());
    put(root, "big.bin", &noise(5 * 1024 * 1024 + 123));
    put(root, "zero.bin", b"");
    // None of these can be created on Windows: they must be left out and reported.
    put(root, "bad:name.txt", b"colon\n");
    put(root, "trailing.", b"dot\n");
    put(root, "CON.txt", b"reserved\n");
    put(root, "a/b/also?bad.txt", b"question mark\n");
}

fn sha256(path: &Path) -> String {
    let out = Command::new("sha256sum").arg(path).output().unwrap_or_else(|e| die(&format!("sha256sum: {e}")));
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_string()
}

fn write_expected(root: &Path, tops: &[PathBuf], out: &Path) {
    let mut lines = Vec::new();
    for top in tops {
        walk(root, top, &mut lines);
    }
    lines.sort();
    let mut f = std::fs::File::create(out).unwrap();
    for l in &lines {
        writeln!(f, "{l}").unwrap();
    }
    println!("EXPECTED {} entries in {}", lines.len(), out.display());
}

fn walk(root: &Path, path: &Path, lines: &mut Vec<String>) {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    let meta = std::fs::symlink_metadata(path).unwrap();
    if meta.is_dir() {
        lines.push(format!("{rel}\t-\t-"));
        for e in std::fs::read_dir(path).unwrap() {
            walk(root, &e.unwrap().path(), lines);
        }
    } else {
        lines.push(format!("{rel}\t{}\t{}", meta.len(), sha256(path)));
    }
}
