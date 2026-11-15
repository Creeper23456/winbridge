// SPDX-License-Identifier: GPL-3.0-or-later

//! Host acceptance of milestone M1 (docs/GFX-DESIGN.md section 10), without a VM.
//!
//! The real Vulkan loader of this machine loads the WinBridge ICD (`libwinbridge_gfx_icd.so`,
//! the same code that is built as `winbridge_gfx_icd.dll` for the guest) through
//! `VK_DRIVER_FILES`. The ICD talks over a Unix socket (instead of the virtio-serial port) to
//! the host dispatcher running in this test process, which executes the calls on every real
//! Vulkan device of the machine (llvmpipe and, if present, the GPU).

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::thread;
use winbridge_gfx::{serve, DeviceSelect, ServeError, Session};
use winbridge_gfx_proto::frame::{FrameError, FrameReader, FrameWriter, StreamTransport};
use winbridge_gfx_proto::{Hello, OP_ERROR, OP_HELLO, OP_VK_CALL, PROTOCOL_VERSION};

/// The ICD cdylib, built once through cargo; the path is the one cargo itself reports.
static ICD: LazyLock<PathBuf> = LazyLock::new(|| {
    let out = Command::new(env!("CARGO"))
        .args(["build", "-p", "winbridge-gfx-icd", "--message-format=json"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stderr(Stdio::inherit())
        .output()
        .expect("run cargo build for the ICD");
    assert!(out.status.success(), "building winbridge-gfx-icd failed");
    let text = String::from_utf8_lossy(&out.stdout);
    let end = text
        .find("libwinbridge_gfx_icd.so\"")
        .expect("cargo did not report the ICD cdylib")
        + "libwinbridge_gfx_icd.so".len();
    let start = text[..end].rfind('"').unwrap() + 1;
    let p = PathBuf::from(&text[start..end]);
    assert!(p.exists(), "{} does not exist", p.display());
    p
});

fn icd_library() -> &'static Path {
    &ICD
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("wbgfx-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn host_device_names() -> Vec<String> {
    // SAFETY: loads the machine's Vulkan loader.
    let entry =
        unsafe { ash::Entry::load() }.expect("the host needs a Vulkan loader for this test");
    let app = ash::vk::ApplicationInfo::default().api_version(ash::vk::API_VERSION_1_3);
    // SAFETY: plain instance creation.
    let inst = unsafe {
        entry.create_instance(
            &ash::vk::InstanceCreateInfo::default().application_info(&app),
            None,
        )
    }
    .unwrap();
    let names = unsafe { inst.enumerate_physical_devices() }
        .unwrap()
        .into_iter()
        .map(|p| {
            let props = unsafe { inst.get_physical_device_properties(p) };
            unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    unsafe { inst.destroy_instance(None) };
    names
}

/// Runs the probe as a child process whose loader sees only the WinBridge ICD, against a
/// dispatcher thread that serves host device `index`. Returns the probe's stdout.
fn run_probe_against(index: usize, extra: &[&str]) -> (String, Result<(), ServeError>) {
    run_probe_with_env(index, extra, &[]).0
}

/// As `run_probe_against`, with extra environment variables; also returns the ICD log.
fn run_probe_with_env(index: usize, extra: &[&str], env: &[(&str, &str)]) -> ((String, Result<(), ServeError>), String) {
    let scratch = Scratch::new(&format!("dev{index}"));
    let sock = scratch.0.join("gfx.sock");
    let manifest = scratch.0.join("icd.json");
    std::fs::write(
        &manifest,
        format!(
            "{{\"file_format_version\":\"1.0.0\",\"ICD\":{{\"library_path\":\"{}\",\"api_version\":\"1.3.0\"}}}}",
            icd_library().display()
        ),
    )
    .unwrap();
    let listener = UnixListener::bind(&sock).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // SAFETY: loads the machine's Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.unwrap();
        let mut session = Session::new(entry, DeviceSelect::Index(index));
        serve(&mut StreamTransport::new(stream.try_clone().unwrap(), stream), &mut session)
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_winbridge-gfx-probe"));
    cmd.args(["--require-winbridge"])
        .args(extra)
        .env("VK_DRIVER_FILES", &manifest)
        .env("VK_ICD_FILENAMES", &manifest)
        .env("WINBRIDGE_GFX_SOCKET", &sock)
        .env("WINBRIDGE_GFX_LOG", scratch.0.join("icd.log"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run the probe");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let log = std::fs::read_to_string(scratch.0.join("icd.log")).unwrap_or_default();
    println!(
        "--- probe stdout ---\n{stdout}--- probe stderr ---\n{}--- icd log ---\n{log}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "probe failed:\n{stdout}");
    // The probe exited, so its end of the socket is closed: the dispatcher must see a clean close.
    ((stdout, server.join().expect("dispatcher thread panicked")), log)
}

fn field<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("probe printed no `{key}`:\n{out}"))
}

fn fields<'a>(out: &'a str, key: &str) -> Vec<&'a str> {
    out.lines().filter_map(|l| l.strip_prefix(&format!("{key}: "))).collect()
}

#[test]
fn loader_lists_winbridge_device_and_buffer_round_trips_on_every_host_gpu() {
    let names = host_device_names();
    assert!(!names.is_empty(), "no Vulkan device on this host");
    println!("host devices: {names:?}");
    for (i, name) in names.iter().enumerate() {
        let (out, served) = run_probe_against(i, &["--mib", "8"]);
        assert!(served.is_ok(), "dispatcher ended with {served:?}");
        assert_eq!(
            field(&out, "physical_device_count"),
            "1",
            "the guest sees exactly the selected host device"
        );
        assert_eq!(field(&out, "selected_device"), format!("WinBridge: {name}"));
        assert_eq!(field(&out, "dxvk_checks"), "71/71", "{name}");
        assert_eq!(field(&out, "device_created"), "yes");
        let (w, r) = (fields(&out, "buffer_checksum_written"), fields(&out, "buffer_checksum_read_back"));
        assert!(!w.is_empty() && w == r, "{name}: {w:?} vs {r:?}");
        assert_eq!(fields(&out, "partial_flush_checksum_expected"), fields(&out, "partial_flush_checksum_read_back"));
        assert!(out.contains("RESULT: PASS"), "{name}:\n{out}");
        // The specification requires a host-visible coherent type, and a pass must have used it.
        assert!(!field(&out, "memory_type_coherent").starts_with("none"), "{name}:\n{out}");
        assert!(fields(&out, "buffer_checksum_written").len() >= 1);
    }
}

#[test]
fn buffer_larger_than_one_upload_chunk_round_trips() {
    // 20 MiB: five 4 MiB upload requests, each split into 64 KiB frames.
    let (out, served) = run_probe_against(0, &["--mib", "20"]);
    assert!(served.is_ok());
    assert_eq!(fields(&out, "buffer_checksum_written"), fields(&out, "buffer_checksum_read_back"));
    assert!(out.contains("RESULT: PASS"));
}

fn new_session() -> Session {
    // SAFETY: loads the machine's Vulkan loader.
    Session::new(
        unsafe { ash::Entry::load() }.expect("Vulkan loader"),
        DeviceSelect::Auto,
    )
}

fn hello() -> Vec<u8> {
    Hello {
        version: PROTOCOL_VERSION,
        features: 0,
        window: 0,
        name: "test".into(),
    }
    .encode()
}

#[test]
fn dispatcher_rejects_protocol_violations_and_closes() {
    // Garbage instead of a frame.
    let (mut a, b) = UnixStream::pair().unwrap();
    a.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut s = new_session();
    let r = serve(&mut StreamTransport::new(b.try_clone().unwrap(), b), &mut s);
    assert!(
        matches!(r, Err(ServeError::Frame(FrameError::BadMagic(_)))),
        "{r:?}"
    );

    // A call before HELLO.
    let (a, b) = UnixStream::pair().unwrap();
    let mut w = FrameWriter::new(a.try_clone().unwrap());
    w.send(OP_VK_CALL, 0, 1, &[0, 0]).unwrap();
    let r = serve(&mut StreamTransport::new(b.try_clone().unwrap(), b), &mut s);
    assert!(
        matches!(&r, Err(ServeError::Protocol(m)) if m.contains("HELLO")),
        "{r:?}"
    );
    let m = FrameReader::new(a).recv().unwrap();
    assert_eq!(m.opcode, OP_ERROR);

    // A command id outside the generated set, after a valid HELLO.
    let (a, b) = UnixStream::pair().unwrap();
    let mut w = FrameWriter::new(a.try_clone().unwrap());
    w.send(OP_HELLO, 0, 1, &hello()).unwrap();
    w.send(OP_VK_CALL, 0, 2, &9999u16.to_le_bytes()).unwrap();
    let r = serve(&mut StreamTransport::new(b.try_clone().unwrap(), b), &mut s);
    assert!(
        matches!(&r, Err(ServeError::Protocol(m)) if m.contains("vk call")),
        "{r:?}"
    );
    let mut rd = FrameReader::new(a);
    assert_eq!(rd.recv().unwrap().opcode, OP_HELLO);
    assert_eq!(rd.recv().unwrap().opcode, OP_ERROR);

    // A handle id that was never created.
    let (a, b) = UnixStream::pair().unwrap();
    let mut w = FrameWriter::new(a.try_clone().unwrap());
    w.send(OP_HELLO, 0, 1, &hello()).unwrap();
    let mut call = Vec::new();
    let cmd = winbridge_gfx_vk::COMMAND_NAMES
        .iter()
        .position(|n| *n == "vkDeviceWaitIdle")
        .unwrap() as u16;
    call.extend_from_slice(&cmd.to_le_bytes());
    call.extend_from_slice(&12345u64.to_le_bytes());
    w.send(OP_VK_CALL, 0, 2, &call).unwrap();
    let r = serve(&mut StreamTransport::new(b.try_clone().unwrap(), b), &mut s);
    assert!(
        matches!(&r, Err(ServeError::Protocol(m)) if m.contains("handle id 12345")),
        "{r:?}"
    );

    // Wrong protocol version.
    let (a, b) = UnixStream::pair().unwrap();
    let mut w = FrameWriter::new(a.try_clone().unwrap());
    let bad = Hello {
        version: PROTOCOL_VERSION + 1,
        features: 0,
        window: 0,
        name: "x".into(),
    }
    .encode();
    w.send(OP_HELLO, 0, 1, &bad).unwrap();
    let r = serve(&mut StreamTransport::new(b.try_clone().unwrap(), b), &mut s);
    assert!(
        matches!(&r, Err(ServeError::Protocol(m)) if m.contains("version")),
        "{r:?}"
    );
}

/// Reads a binary PPM (P6) written by the probe or the ICD.
fn read_ppm(path: &Path) -> (usize, usize, Vec<u8>) {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut fields = Vec::new();
    let mut pos = 0;
    while fields.len() < 4 {
        while data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..pos]).into_owned());
    }
    pos += 1;
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    (w, h, data[pos..].to_vec())
}

fn host_device_index(fragment: &str) -> Option<usize> {
    host_device_names().iter().position(|n| n.contains(fragment))
}

/// M2 host acceptance: a frame rendered through the ICD, the host dispatcher and the host
/// swapchain equals a reference rendered directly on llvmpipe; on the discrete GPU the frame is
/// compared with the same tolerance statement printed below.
#[test]
fn presented_triangle_matches_the_llvmpipe_reference() {
    const W: usize = 320;
    const H: usize = 240;
    let scratch = Scratch::new("ref");
    let ref_ppm = scratch.0.join("reference.ppm");
    let out = Command::new(env!("CARGO_BIN_EXE_winbridge-gfx-probe"))
        .args(["--reference", "320", "240", "--device-name", "llvmpipe", "--reference-out"])
        .arg(&ref_ppm)
        .output()
        .unwrap();
    let ref_stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "reference render failed: {ref_stdout}");
    let ref_sum = field(&ref_stdout, "reference_fnv1a").to_string();
    let (rw, rh, reference) = read_ppm(&ref_ppm);
    assert_eq!((rw, rh), (W, H));
    // The scene must actually have drawn something: clear colour and triangle colour both present.
    let px = |buf: &[u8], x: usize, y: usize| [buf[(y * W + x) * 3], buf[(y * W + x) * 3 + 1], buf[(y * W + x) * 3 + 2]];
    let clear = [(0.05f32 * 255.0).round() as u8, (0.10f32 * 255.0).round() as u8, (0.20f32 * 255.0).round() as u8];
    assert_eq!(px(&reference, 2, 2), clear, "corner must be the clear colour");
    assert_ne!(px(&reference, W / 2, H / 2), clear, "the triangle must cover the centre");

    for (fragment, exact) in [("llvmpipe", true), ("NVIDIA", false)] {
        let Some(index) = host_device_index(fragment) else {
            println!("no {fragment} device on this host: skipped");
            continue;
        };
        let frame_ppm = scratch.0.join(format!("frame-{fragment}.ppm"));
        let (r, log) = run_probe_with_env(
            index,
            &["--render", "--frames", "3"],
            &[("WINBRIDGE_GFX_WINDOW", "320x240"), ("WINBRIDGE_GFX_FRAME_OUT", frame_ppm.to_str().unwrap())],
        );
        let (out, served) = r;
        assert!(served.is_ok(), "{fragment}: {served:?}");
        assert!(out.contains("present_ok: 3"), "{fragment}:\n{out}");
        let sums: Vec<&str> = log.lines().filter_map(|l| l.split("fnv1a ").nth(1)).collect();
        assert_eq!(sums.len(), 3, "{fragment}: one logged frame per present:\n{log}");
        assert!(sums.iter().all(|s| *s == sums[0]), "{fragment}: a static scene must give identical frames: {sums:?}");
        let (fw, fh, frame) = read_ppm(&frame_ppm);
        assert_eq!((fw, fh), (W, H));
        let channel_delta = |a: &[u8], b: &[u8]| a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
        let differing = frame.chunks_exact(3).zip(reference.chunks_exact(3)).filter(|(a, b)| a != b).count();
        let big = frame.chunks_exact(3).zip(reference.chunks_exact(3)).filter(|(a, b)| channel_delta(a, b) > 2).count();
        let max_delta = channel_delta(&frame, &reference);
        println!(
            "{fragment}: frame fnv1a {} reference fnv1a {ref_sum}; {differing} of {} pixels differ, {big} by more than 2 per channel, max channel delta {max_delta}; corner {:?} vs {:?}, centre {:?} vs {:?}",
            sums[0], W * H, px(&frame, 2, 2), px(&reference, 2, 2), px(&frame, W / 2, H / 2), px(&reference, W / 2, H / 2)
        );
        if exact {
            assert_eq!(sums[0], ref_sum, "{fragment}: the frame must equal the reference bit for bit");
            assert_eq!(differing, 0);
        } else {
            // A different GPU rounds colours and covers triangle edges differently: allow 2 per
            // channel everywhere and at most 1% of the pixels beyond that (edge pixels).
            assert!(big * 100 <= W * H, "{fragment}: {big} pixels differ by more than 2 per channel");
        }
    }
}

/// vkcube and many applications create their instance with apiVersion 1.0. The device must
/// still be created: the host's DXVK check needs the Vulkan 1.1+ feature structs, which drivers
/// hide from a 1.0 instance (found in the VM with vkcube: `ERROR_FEATURE_NOT_PRESENT`).
#[test]
fn device_is_created_for_an_api_1_0_instance() {
    for (i, name) in host_device_names().iter().enumerate() {
        let ((out, served), _) = run_probe_with_env(i, &[], &[("WINBRIDGE_PROBE_API", "1.0")]);
        assert!(served.is_ok(), "{name}: {served:?}");
        assert_eq!(field(&out, "device_created"), "yes", "{name}:\n{out}");
    }
}
