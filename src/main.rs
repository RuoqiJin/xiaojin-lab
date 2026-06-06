use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};

mod web;

const DEFAULT_BOARD_PATH: &str = "boards/r5c.toml";
const DEFAULT_LOG_DIR: &str = "logs";
const DEFAULT_OS_DIR: &str = "../xiaojin-os";
const DEFAULT_IMAGE_PATH: &str = "../xiaojin-os/target/aarch64-unknown-none/release/xiaojin-os.bin";
const DEFAULT_XLOAD_ADDR: &str = "auto";
const EMBEDDED_R5C_BOARD: &str = include_str!("../boards/r5c.toml");
const SOH: u8 = 0x01;
const STX: u8 = 0x02;
const EOT: u8 = 0x04;
const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const CAN: u8 = 0x18;
const CRC_REQ: u8 = b'C';

#[derive(Parser, Debug)]
#[command(name = "xjctl")]
#[command(about = "Host-side controller for Xiaojin OS boards")]
struct Cli {
    #[arg(long, default_value = DEFAULT_BOARD_PATH)]
    board: PathBuf,

    #[arg(long, default_value = DEFAULT_LOG_DIR)]
    log_dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build the Xiaojin OS kernel.
    BuildOs {
        #[arg(long, default_value = DEFAULT_OS_DIR)]
        os_dir: PathBuf,
    },

    /// Copy a kernel image to a mounted FAT32 SD card.
    SdCopy {
        #[arg(long)]
        volume: PathBuf,

        #[arg(long, default_value = DEFAULT_IMAGE_PATH)]
        image: PathBuf,

        #[arg(long)]
        name: Option<String>,
    },

    /// Listen to the board serial console.
    Serial {
        #[arg(long)]
        interactive: bool,
    },

    /// Break into U-Boot and optionally stay interactive.
    Break {
        #[arg(long)]
        arm_after_enter: bool,

        #[arg(long)]
        interactive: bool,

        #[arg(long)]
        seconds: Option<f32>,
    },

    /// Break into U-Boot, load the SD image, jump into Xiaojin OS, and verify output.
    RunSd {
        #[arg(long)]
        arm_after_enter: bool,

        #[arg(long)]
        no_break: bool,

        #[arg(long)]
        interactive: bool,

        #[arg(long)]
        run_seconds: Option<f32>,
    },

    /// Probe useful U-Boot commands for faster development loops.
    ProbeUboot {
        #[arg(long)]
        arm_after_enter: bool,

        #[arg(long)]
        no_break: bool,
    },

    /// Upload the kernel over the serial console using U-Boot loady/YMODEM.
    RunSerial {
        #[arg(long)]
        arm_after_enter: bool,

        #[arg(long)]
        no_break: bool,

        #[arg(long, default_value = DEFAULT_IMAGE_PATH)]
        image: PathBuf,

        #[arg(long)]
        run_seconds: Option<f32>,
    },

    /// Upload an image through the running Xiaojin OS resident xload receiver.
    Xload {
        #[arg(long, default_value = DEFAULT_IMAGE_PATH)]
        image: PathBuf,

        #[arg(long)]
        addr: Option<String>,

        #[arg(long)]
        no_go: bool,

        #[arg(long)]
        run_seconds: Option<f32>,
    },

    /// Inspect saved serial logs.
    Logs {
        #[command(subcommand)]
        command: LogCommands,
    },

    /// Serve the local Xiaojin Lab web console.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        #[arg(long, default_value_t = 8795)]
        port: u16,

        #[arg(long, default_value = DEFAULT_OS_DIR)]
        os_dir: PathBuf,

        #[arg(long, default_value = DEFAULT_IMAGE_PATH)]
        image: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum LogCommands {
    Latest {
        #[arg(long, default_value_t = 120)]
        lines: usize,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(crate) struct BoardProfile {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) port: String,
    pub(crate) baud: u32,
    pub(crate) load_addr: String,
    pub(crate) mmc_dev: u8,
    pub(crate) mmc_part: u8,
    pub(crate) image_name: String,
    #[serde(default)]
    pub(crate) xload_addr: Option<String>,
    pub(crate) break_keys: Vec<String>,
    pub(crate) break_seconds: f32,
    pub(crate) expect: String,
}

pub(crate) struct SerialSession {
    port: Box<dyn SerialPort>,
    log: File,
    pub(crate) log_path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let board = read_board(&cli.board)?;
    let log_dir = resolve_log_dir(cli.log_dir);

    match cli.command {
        Commands::BuildOs { os_dir } => build_os(&resolve_os_dir(os_dir)),
        Commands::SdCopy {
            volume,
            image,
            name,
        } => sd_copy(&board, &volume, &resolve_image_path(image), name.as_deref()),
        Commands::Serial { interactive } => {
            let mut session = open_session(&board, &log_dir, "serial")?;
            println!("listening on {} at {} 8N1", board.port, board.baud);
            if interactive {
                spawn_stdin_writer(session.port.try_clone()?);
            }
            pump_serial_forever(&mut session)
        }
        Commands::Break {
            arm_after_enter,
            interactive,
            seconds,
        } => {
            let mut session = open_session(&board, &log_dir, "uboot-break")?;
            maybe_arm(arm_after_enter)?;
            let seconds = seconds.unwrap_or(board.break_seconds);
            break_to_prompt(&mut session, &board, Duration::from_secs_f32(seconds))?;
            if interactive {
                spawn_stdin_writer(session.port.try_clone()?);
                pump_serial_forever(&mut session)?;
            }
            Ok(())
        }
        Commands::RunSd {
            arm_after_enter,
            no_break,
            interactive,
            run_seconds,
        } => {
            let mut session = open_session(&board, &log_dir, "run-sd")?;
            maybe_arm(arm_after_enter)?;
            if no_break {
                write_serial(&mut session, b"\r")?;
                read_until(&mut session, b"=>", Duration::from_secs(3))?;
            } else {
                break_to_prompt(
                    &mut session,
                    &board,
                    Duration::from_secs_f32(board.break_seconds),
                )?;
            }
            run_sd_image(
                &mut session,
                &board,
                Duration::from_secs_f32(run_seconds.unwrap_or(8.0)),
                interactive,
            )
        }
        Commands::ProbeUboot {
            arm_after_enter,
            no_break,
        } => {
            let mut session = open_session(&board, &log_dir, "probe-uboot")?;
            enter_uboot(&mut session, &board, arm_after_enter, no_break)?;
            probe_uboot(&mut session)
        }
        Commands::RunSerial {
            arm_after_enter,
            no_break,
            image,
            run_seconds,
        } => {
            let image = resolve_image_path(image);
            let mut session = open_session(&board, &log_dir, "run-serial")?;
            enter_uboot(&mut session, &board, arm_after_enter, no_break)?;
            run_serial_image(
                &mut session,
                &board,
                &image,
                Duration::from_secs_f32(run_seconds.unwrap_or(8.0)),
            )
        }
        Commands::Xload {
            image,
            addr,
            no_go,
            run_seconds,
        } => {
            let image = resolve_image_path(image);
            let target = addr
                .or_else(|| board.xload_addr.clone())
                .unwrap_or_else(|| DEFAULT_XLOAD_ADDR.to_string());
            let mut session = open_session(&board, &log_dir, "xload")?;
            run_xload_image(
                &mut session,
                &board,
                &image,
                &target,
                !no_go,
                Duration::from_secs_f32(run_seconds.unwrap_or(8.0)),
            )
        }
        Commands::Logs { command } => match command {
            LogCommands::Latest { lines } => show_latest_log(&log_dir, lines),
        },
        Commands::Serve {
            host,
            port,
            os_dir,
            image,
        } => web::serve(web::ServeOptions {
            host,
            port,
            board,
            board_path: cli.board,
            log_dir,
            os_dir: resolve_os_dir(os_dir),
            image: resolve_image_path(image),
        }),
    }
}

pub(crate) fn read_board(path: &Path) -> Result<BoardProfile> {
    if !path.exists() && path == Path::new(DEFAULT_BOARD_PATH) {
        return toml::from_str(EMBEDDED_R5C_BOARD)
            .context("failed to parse embedded R5C board profile");
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read board profile {}", path.display()))?;
    let board: BoardProfile = toml::from_str(&text)
        .with_context(|| format!("failed to parse board profile {}", path.display()))?;
    Ok(board)
}

fn resolve_os_dir(path: PathBuf) -> PathBuf {
    if path.exists() {
        return path;
    }
    if path == Path::new(DEFAULT_OS_DIR) {
        let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../xiaojin-os");
        if fallback.exists() {
            return fallback;
        }
    }
    path
}

fn resolve_log_dir(path: PathBuf) -> PathBuf {
    if path == Path::new(DEFAULT_LOG_DIR) {
        return PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_LOG_DIR);
    }
    path
}

fn resolve_image_path(path: PathBuf) -> PathBuf {
    if path.is_file() {
        return path;
    }
    if path == Path::new(DEFAULT_IMAGE_PATH)
        || path
            .file_name()
            .is_some_and(|name| name == "xiaojin-os.bin")
    {
        let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../xiaojin-os/target/aarch64-unknown-none/release/xiaojin-os.bin");
        if fallback.is_file() {
            return fallback;
        }
    }
    path
}

pub(crate) fn build_os(os_dir: &Path) -> Result<()> {
    println!("building Xiaojin OS in {}", os_dir.display());
    let status = Command::new("./build.sh")
        .current_dir(os_dir)
        .status()
        .with_context(|| format!("failed to run {}/build.sh", os_dir.display()))?;
    if !status.success() {
        bail!("build failed with status {status}");
    }
    Ok(())
}

pub(crate) fn sd_copy(
    board: &BoardProfile,
    volume: &Path,
    image: &Path,
    name: Option<&str>,
) -> Result<()> {
    if !volume.is_dir() {
        bail!(
            "SD volume is not mounted or is not a directory: {}",
            volume.display()
        );
    }
    if !image.is_file() {
        bail!("image does not exist: {}", image.display());
    }

    let dest_name = name.unwrap_or(&board.image_name);
    let dest = volume.join(dest_name);
    fs::copy(image, &dest)
        .with_context(|| format!("failed to copy {} to {}", image.display(), dest.display()))?;
    let _ = Command::new("sync").status();
    println!("copied {} -> {}", image.display(), dest.display());
    Ok(())
}

pub(crate) fn open_session(
    board: &BoardProfile,
    log_dir: &Path,
    kind: &str,
) -> Result<SerialSession> {
    fs::create_dir_all(log_dir)
        .with_context(|| format!("failed to create log dir {}", log_dir.display()))?;
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    let log_path = log_dir.join(format!("{}-{}-{}.log", stamp, board.id, kind));
    let mut log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log_path)
        .with_context(|| format!("failed to create log {}", log_path.display()))?;

    writeln!(
        log,
        "--- xjctl serial start {} board={} name={} port={} baud={} kind={} ---",
        Local::now().to_rfc3339(),
        board.id,
        board.name,
        board.port,
        board.baud,
        kind
    )?;
    log.flush()?;

    let port = serialport::new(&board.port, board.baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(50))
        .open()
        .with_context(|| format!("failed to open serial port {}", board.port))?;

    println!("logging serial bytes to {}", log_path.display());
    Ok(SerialSession {
        port,
        log,
        log_path,
    })
}

fn maybe_arm(arm_after_enter: bool) -> Result<()> {
    if arm_after_enter {
        println!("Power off/reset the board. Press Enter here, then immediately power it on.");
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
    }
    Ok(())
}

fn enter_uboot(
    session: &mut SerialSession,
    board: &BoardProfile,
    arm_after_enter: bool,
    no_break: bool,
) -> Result<()> {
    maybe_arm(arm_after_enter)?;
    if no_break {
        write_serial(session, b"\r")?;
        read_until(session, b"=>", Duration::from_secs(3))?;
    } else {
        break_to_prompt(session, board, Duration::from_secs_f32(board.break_seconds))?;
    }
    Ok(())
}

fn key_sequence(keys: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for key in keys {
        match key.trim().to_ascii_lowercase().as_str() {
            "ctrl-c" | "^c" => out.push(0x03),
            "ctrl-m" | "^m" | "enter" | "return" => out.push(b'\r'),
            "space" => out.push(b' '),
            "esc" => out.push(0x1b),
            other if other.starts_with("raw:") => out.extend_from_slice(other[4..].as_bytes()),
            other => out.extend_from_slice(other.as_bytes()),
        }
    }
    if out.is_empty() {
        out.push(0x03);
    }
    out
}

pub(crate) fn break_to_prompt(
    session: &mut SerialSession,
    board: &BoardProfile,
    spam_for: Duration,
) -> Result<()> {
    let keys = key_sequence(&board.break_keys);
    let start = Instant::now();
    let mut next_send = start;
    let total_timeout = spam_for + Duration::from_secs(10);
    let mut seen = Vec::new();
    let mut warned_running_os = false;

    println!(
        "breaking into U-Boot on {} for {:.1}s...",
        board.port,
        spam_for.as_secs_f32()
    );

    while start.elapsed() <= total_timeout {
        let now = Instant::now();
        if start.elapsed() <= spam_for && now >= next_send {
            write_serial(session, &keys)?;
            next_send = now + Duration::from_millis(50);
        }

        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            seen.extend_from_slice(&chunk);
            if has_uboot_prompt(&seen) {
                println!("\nU-Boot prompt detected.");
                return Ok(());
            }
            if !warned_running_os
                && (contains_bytes(&seen, b"xiaojin-os heartbeat")
                    || contains_bytes(&seen, b"r5c-os heartbeat")
                    || contains_bytes(&seen, b"xj> "))
            {
                println!(
                    "\nThe board is still running Xiaojin OS. Power-cycle or press RESET now; serial input cannot jump back to U-Boot from this kernel."
                );
                warned_running_os = true;
            }
            if seen.len() > 8192 {
                let keep_from = seen.len() - 8192;
                seen.drain(..keep_from);
            }
        }
    }

    bail!("timed out waiting for U-Boot prompt");
}

pub(crate) fn run_sd_image(
    session: &mut SerialSession,
    board: &BoardProfile,
    run_for: Duration,
    interactive: bool,
) -> Result<()> {
    let addr = parse_hex_or_dec(&board.load_addr)?;
    let commands = [
        format!("mmc dev {}", board.mmc_dev),
        format!("fatls mmc {}:{}", board.mmc_dev, board.mmc_part),
        format!(
            "fatload mmc {}:{} 0x{addr:08x} {}",
            board.mmc_dev, board.mmc_part, board.image_name
        ),
    ];

    for cmd in commands {
        send_uboot_command(session, &cmd)?;
    }

    let go_cmd = format!("go 0x{addr:08x}");
    println!(">> {go_cmd}");
    write_serial(session, go_cmd.as_bytes())?;
    write_serial(session, b"\r")?;

    let output = read_for(session, run_for)?;
    if output.contains(&board.expect) {
        println!("\nverified expected output: {}", board.expect);
        if interactive {
            println!("interactive shell attached; press Ctrl-C in this terminal to exit xjctl.");
            spawn_stdin_writer(session.port.try_clone()?);
            pump_serial_forever(session)
        } else {
            Ok(())
        }
    } else {
        bail!("did not see expected output: {}", board.expect);
    }
}

fn probe_uboot(session: &mut SerialSession) -> Result<()> {
    let commands = [
        "version",
        "bdinfo",
        "help loady",
        "help loadx",
        "help loads",
        "help tftpboot",
        "help dhcp",
        "help usb",
        "printenv ipaddr serverip ethaddr eth1addr",
    ];

    let mut summary = Vec::new();
    for command in commands {
        match run_uboot_command(session, command, Duration::from_secs(10)) {
            Ok(output) => summary.push((command, classify_probe_output(&output).to_string())),
            Err(err) => {
                println!("\nprobe command timed out: {command}: {err:#}");
                summary.push((command, "timeout".to_string()));
                write_serial(session, b"\r")?;
                let _ = read_uboot_prompt(session, Duration::from_secs(2));
            }
        }
    }

    println!("\nU-Boot probe summary:");
    log_host_line(session, "U-Boot probe summary:")?;
    for (command, status) in summary {
        println!("  {command:<40} {status}");
        log_host_line(session, &format!("  {command:<40} {status}"))?;
    }
    println!("probe complete; log: {}", session.log_path.display());
    Ok(())
}

fn classify_probe_output(output: &str) -> &'static str {
    if output.contains("Unknown command") {
        "unsupported"
    } else if output.contains("## Error") || output.contains("** Invalid") {
        "error"
    } else {
        "available"
    }
}

fn run_serial_image(
    session: &mut SerialSession,
    board: &BoardProfile,
    image: &Path,
    run_for: Duration,
) -> Result<()> {
    if !image.is_file() {
        bail!("image does not exist: {}", image.display());
    }

    let data = fs::read(image).with_context(|| format!("failed to read {}", image.display()))?;
    let addr = parse_hex_or_dec(&board.load_addr)?;
    let file_name = image
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&board.image_name);

    let load_cmd = format!("loady 0x{addr:08x}");
    drain_serial(
        session,
        Duration::from_millis(100),
        Duration::from_millis(500),
    )?;
    println!(">> {load_cmd}");
    write_serial(session, load_cmd.as_bytes())?;
    write_serial(session, b"\r")?;

    wait_for_ymodem_receiver(session, Duration::from_secs(20))?;
    ymodem_send(session, file_name, &data)?;

    let post_load = read_until(session, b"=>", Duration::from_secs(12))?;
    if post_load.contains("Unknown command") {
        bail!("U-Boot does not support loady; run `xjctl probe-uboot` and check serial-loading support");
    }

    let go_cmd = format!("go 0x{addr:08x}");
    println!(">> {go_cmd}");
    write_serial(session, go_cmd.as_bytes())?;
    write_serial(session, b"\r")?;

    let output = read_for(session, run_for)?;
    if output.contains(&board.expect) {
        println!("\nverified expected output: {}", board.expect);
        Ok(())
    } else {
        bail!("did not see expected output: {}", board.expect);
    }
}

fn run_xload_image(
    session: &mut SerialSession,
    board: &BoardProfile,
    image: &Path,
    target: &str,
    go: bool,
    run_for: Duration,
) -> Result<()> {
    if !image.is_file() {
        bail!("image does not exist: {}", image.display());
    }

    let data = fs::read(image).with_context(|| format!("failed to read {}", image.display()))?;
    let crc = crc32(&data);

    println!("waiting for Xiaojin OS shell prompt on {}...", board.port);
    drain_serial(
        session,
        Duration::from_millis(100),
        Duration::from_millis(500),
    )?;
    write_serial(session, b"\r")?;
    read_until(session, board.expect.as_bytes(), Duration::from_secs(5)).with_context(|| {
        format!(
            "did not see Xiaojin OS prompt {}; boot the SD loader first with `xjctl run-sd --arm-after-enter --interactive`",
            board.expect
        )
    })?;

    let command = format!(
        "xload {target} {} 0x{crc:08x}{}",
        data.len(),
        if go { " go" } else { "" }
    );
    log_host_line(
        session,
        &format!(
            "xload image={} bytes={} crc32=0x{crc:08x} target={target} go={go}",
            image.display(),
            data.len()
        ),
    )?;
    println!(">> {command}");
    write_serial(session, command.as_bytes())?;
    write_serial(session, b"\r")?;

    let (ready_index, ready_output) = read_until_any(
        session,
        &[b"XLOAD READY", b"xload refused", b"usage: xload"],
        Duration::from_secs(8),
    )?;
    if ready_index != 0 {
        bail!("xload receiver did not arm; output: {ready_output:?}");
    }

    println!(
        "sending {} bytes to xload receiver (crc32=0x{crc:08x})...",
        data.len()
    );
    send_raw_image(session, &data)?;

    let (status_index, status_output) = read_until_any(
        session,
        &[b"XLOAD OK", b"XLOAD ERR"],
        Duration::from_secs(20),
    )?;
    if status_index != 0 {
        bail!("xload transfer failed; output: {status_output:?}");
    }

    if status_output.contains(&board.expect) {
        println!("\nverified expected output: {}", board.expect);
        return Ok(());
    }

    let output = read_until(session, board.expect.as_bytes(), run_for)?;
    if output.contains(&board.expect) {
        println!("\nverified expected output: {}", board.expect);
        Ok(())
    } else {
        bail!("did not see expected output: {}", board.expect);
    }
}

fn send_raw_image(session: &mut SerialSession, data: &[u8]) -> Result<()> {
    let mut sent = 0usize;
    let mut next_progress = Instant::now() + Duration::from_millis(500);
    for chunk in data.chunks(4096) {
        session.port.write_all(chunk)?;
        sent += chunk.len();
        let now = Instant::now();
        if now >= next_progress || sent == data.len() {
            print!("\rsent {sent}/{} bytes", data.len());
            io::stdout().flush()?;
            next_progress = now + Duration::from_millis(500);
        }
    }
    session.port.flush()?;
    println!();
    Ok(())
}

fn send_uboot_command(session: &mut SerialSession, command: &str) -> Result<String> {
    let output = run_uboot_command(session, command, Duration::from_secs(8))?;
    if output.contains("Unknown command") || output.contains("** Invalid") {
        bail!("U-Boot command failed: {command}");
    }
    Ok(output)
}

fn run_uboot_command(
    session: &mut SerialSession,
    command: &str,
    timeout: Duration,
) -> Result<String> {
    drain_serial(
        session,
        Duration::from_millis(100),
        Duration::from_millis(500),
    )?;
    println!(">> {command}");
    write_serial(session, command.as_bytes())?;
    write_serial(session, b"\r")?;
    read_uboot_command_response(session, command, timeout)
}

fn parse_hex_or_dec(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).with_context(|| format!("invalid hex value {value}"))
    } else {
        trimmed
            .parse::<u64>()
            .with_context(|| format!("invalid integer value {value}"))
    }
}

pub(crate) fn write_serial(session: &mut SerialSession, bytes: &[u8]) -> Result<()> {
    session.port.write_all(bytes)?;
    session.port.flush()?;
    Ok(())
}

fn log_host_line(session: &mut SerialSession, line: &str) -> Result<()> {
    writeln!(session.log, "\n--- xjctl: {line} ---")?;
    session.log.flush()?;
    Ok(())
}

fn read_once(session: &mut SerialSession) -> Result<Vec<u8>> {
    let mut buf = [0u8; 4096];
    match session.port.read(&mut buf) {
        Ok(n) if n > 0 => {
            let data = buf[..n].to_vec();
            session.log.write_all(&data)?;
            session.log.flush()?;
            print!("{}", String::from_utf8_lossy(&data));
            io::stdout().flush()?;
            Ok(data)
        }
        Ok(_) => Ok(Vec::new()),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => Ok(Vec::new()),
        Err(err) => Err(err).context("serial read failed"),
    }
}

fn drain_serial(session: &mut SerialSession, idle_for: Duration, max_for: Duration) -> Result<()> {
    let start = Instant::now();
    let mut last_data = Instant::now();

    while start.elapsed() <= max_for && last_data.elapsed() <= idle_for {
        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            last_data = Instant::now();
        }
    }

    Ok(())
}

pub(crate) fn read_until(
    session: &mut SerialSession,
    needle: &[u8],
    timeout: Duration,
) -> Result<String> {
    let start = Instant::now();
    let mut collected = Vec::new();
    while start.elapsed() <= timeout {
        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
            if needle == b"=>" && has_uboot_prompt(&collected) {
                return Ok(String::from_utf8_lossy(&collected).into_owned());
            }
            if needle != b"=>" && contains_bytes(&collected, needle) {
                return Ok(String::from_utf8_lossy(&collected).into_owned());
            }
        }
    }
    Err(anyhow!(
        "timed out waiting for serial text {:?}",
        String::from_utf8_lossy(needle)
    ))
}

fn read_until_any(
    session: &mut SerialSession,
    needles: &[&[u8]],
    timeout: Duration,
) -> Result<(usize, String)> {
    let start = Instant::now();
    let mut collected = Vec::new();
    while start.elapsed() <= timeout {
        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
            for (index, needle) in needles.iter().enumerate() {
                if contains_bytes(&collected, needle) {
                    return Ok((index, String::from_utf8_lossy(&collected).into_owned()));
                }
            }
        }
    }

    let labels = needles
        .iter()
        .map(|needle| String::from_utf8_lossy(needle).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!("timed out waiting for one of: {labels}"))
}

fn read_uboot_prompt(session: &mut SerialSession, timeout: Duration) -> Result<String> {
    let start = Instant::now();
    let mut collected = Vec::new();
    while start.elapsed() <= timeout {
        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
            if has_uboot_prompt(&collected) {
                return Ok(String::from_utf8_lossy(&collected).into_owned());
            }
        }
    }

    Err(anyhow!("timed out waiting for U-Boot prompt"))
}

fn read_uboot_command_response(
    session: &mut SerialSession,
    command: &str,
    timeout: Duration,
) -> Result<String> {
    let start = Instant::now();
    let mut collected = Vec::new();
    let mut command_end = None;
    let command_bytes = command.as_bytes();

    while start.elapsed() <= timeout {
        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
            if command_end.is_none() {
                command_end =
                    find_bytes(&collected, command_bytes).map(|index| index + command_bytes.len());
            }
            if let Some(min_prompt_index) = command_end {
                if has_uboot_prompt_after(&collected, min_prompt_index) {
                    return Ok(String::from_utf8_lossy(&collected).into_owned());
                }
            }
        }
    }

    Err(anyhow!("timed out waiting for U-Boot command `{command}`"))
}

fn read_for(session: &mut SerialSession, duration: Duration) -> Result<String> {
    let start = Instant::now();
    let mut collected = Vec::new();
    while start.elapsed() <= duration {
        let chunk = read_once(session)?;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
        }
    }
    Ok(String::from_utf8_lossy(&collected).into_owned())
}

fn wait_for_ymodem_receiver(session: &mut SerialSession, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut text_window = Vec::new();

    while start.elapsed() <= timeout {
        for byte in read_raw_once(session, true)? {
            if byte == CRC_REQ {
                println!("\nYMODEM receiver is ready.");
                return Ok(());
            }
            text_window.push(byte);
            if contains_bytes(&text_window, b"Unknown command") {
                bail!("U-Boot does not support `loady` on this build");
            }
            if has_uboot_prompt(&text_window) {
                bail!("U-Boot returned to the prompt without starting YMODEM; this build likely lacks serial upload support");
            }
            if text_window.len() > 4096 {
                let keep_from = text_window.len() - 4096;
                text_window.drain(..keep_from);
            }
        }
    }

    let text = String::from_utf8_lossy(&text_window);
    if text.contains("Unknown command") {
        bail!("U-Boot does not support `loady` on this build");
    }
    bail!("timed out waiting for U-Boot YMODEM receiver after `loady`");
}

fn ymodem_send(session: &mut SerialSession, file_name: &str, data: &[u8]) -> Result<()> {
    println!(
        "sending {} ({} bytes) with YMODEM...",
        file_name,
        data.len()
    );

    let mut metadata = [0u8; 128];
    let header = format!("{file_name}\0{}\0", data.len());
    let header_bytes = header.as_bytes();
    if header_bytes.len() > metadata.len() {
        bail!("YMODEM metadata is too long for file name {file_name:?}");
    }
    metadata[..header_bytes.len()].copy_from_slice(header_bytes);
    send_ymodem_packet(session, SOH, 0, &metadata)?;
    expect_protocol_byte(session, ACK, Duration::from_secs(10), "metadata ACK")?;
    expect_protocol_byte(
        session,
        CRC_REQ,
        Duration::from_secs(10),
        "data start request",
    )?;

    let mut block_number = 1u8;
    for chunk in data.chunks(1024) {
        let mut payload = [0x1au8; 1024];
        payload[..chunk.len()].copy_from_slice(chunk);
        send_ymodem_packet(session, STX, block_number, &payload)?;
        expect_protocol_byte(session, ACK, Duration::from_secs(10), "data packet ACK")?;
        block_number = block_number.wrapping_add(1);
    }

    send_eot(session)?;
    expect_protocol_byte(
        session,
        CRC_REQ,
        Duration::from_secs(10),
        "final empty packet request",
    )?;

    let empty = [0u8; 128];
    send_ymodem_packet(session, SOH, 0, &empty)?;
    expect_protocol_byte(session, ACK, Duration::from_secs(10), "final ACK")?;

    println!("YMODEM transfer complete.");
    Ok(())
}

fn send_eot(session: &mut SerialSession) -> Result<()> {
    session.port.write_all(&[EOT])?;
    session.port.flush()?;
    match expect_any_protocol_byte(
        session,
        &[ACK, NAK],
        Duration::from_secs(10),
        "EOT response",
    )? {
        ACK => Ok(()),
        NAK => {
            session.port.write_all(&[EOT])?;
            session.port.flush()?;
            expect_protocol_byte(session, ACK, Duration::from_secs(10), "second EOT ACK")
        }
        other => bail!("unexpected EOT response byte 0x{other:02x}"),
    }
}

fn send_ymodem_packet(
    session: &mut SerialSession,
    start_byte: u8,
    block_number: u8,
    payload: &[u8],
) -> Result<()> {
    let expected_len = match start_byte {
        SOH => 128,
        STX => 1024,
        _ => bail!("invalid YMODEM packet start byte 0x{start_byte:02x}"),
    };
    if payload.len() != expected_len {
        bail!(
            "invalid YMODEM payload size {}; expected {expected_len}",
            payload.len()
        );
    }

    let crc = crc16_xmodem(payload);
    let mut packet = Vec::with_capacity(3 + payload.len() + 2);
    packet.push(start_byte);
    packet.push(block_number);
    packet.push(0xffu8.wrapping_sub(block_number));
    packet.extend_from_slice(payload);
    packet.push((crc >> 8) as u8);
    packet.push((crc & 0xff) as u8);

    session.port.write_all(&packet)?;
    session.port.flush()?;
    Ok(())
}

fn expect_protocol_byte(
    session: &mut SerialSession,
    expected: u8,
    timeout: Duration,
    label: &str,
) -> Result<()> {
    let actual = expect_any_protocol_byte(session, &[expected], timeout, label)?;
    if actual == expected {
        Ok(())
    } else {
        bail!(
            "unexpected protocol byte while waiting for {label}: got 0x{actual:02x}, expected 0x{expected:02x}"
        );
    }
}

fn expect_any_protocol_byte(
    session: &mut SerialSession,
    expected: &[u8],
    timeout: Duration,
    label: &str,
) -> Result<u8> {
    let start = Instant::now();
    let mut text_window = Vec::new();

    while start.elapsed() <= timeout {
        for byte in read_raw_once(session, true)? {
            if expected.contains(&byte) {
                return Ok(byte);
            }
            if byte == CAN {
                bail!("receiver cancelled YMODEM transfer while waiting for {label}");
            }
            text_window.push(byte);
            if text_window.len() > 4096 {
                let keep_from = text_window.len() - 4096;
                text_window.drain(..keep_from);
            }
        }
    }

    let text = String::from_utf8_lossy(&text_window);
    bail!("timed out waiting for {label}; recent receiver text: {text:?}");
}

fn read_raw_once(session: &mut SerialSession, print_text: bool) -> Result<Vec<u8>> {
    let mut buf = [0u8; 4096];
    match session.port.read(&mut buf) {
        Ok(n) if n > 0 => {
            let data = buf[..n].to_vec();
            session.log.write_all(&data)?;
            session.log.flush()?;
            if print_text {
                print!("{}", String::from_utf8_lossy(&data));
                io::stdout().flush()?;
            }
            Ok(data)
        }
        Ok(_) => Ok(Vec::new()),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => Ok(Vec::new()),
        Err(err) => Err(err).context("serial read failed"),
    }
}

fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc = crc32_update(crc, *byte);
    }
    !crc
}

fn crc32_update(mut crc: u32, byte: u8) -> u32 {
    crc ^= byte as u32;
    for _ in 0..8 {
        if crc & 1 != 0 {
            crc = (crc >> 1) ^ 0xedb8_8320;
        } else {
            crc >>= 1;
        }
    }
    crc
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn has_uboot_prompt(bytes: &[u8]) -> bool {
    has_uboot_prompt_after(bytes, 0)
}

fn has_uboot_prompt_after(bytes: &[u8], min_index: usize) -> bool {
    bytes.windows(2).enumerate().any(|(index, window)| {
        if index < min_index {
            return false;
        }
        if window != b"=>" {
            return false;
        }

        let before_ok = index == 0 || matches!(bytes[index - 1], b'\r' | b'\n');
        if !before_ok {
            return false;
        }

        let mut after_index = index + 2;
        while after_index < bytes.len() && bytes[after_index] == b' ' {
            after_index += 1;
        }

        after_index == bytes.len() || matches!(bytes[after_index], b'\r' | b'\n')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_xmodem_known_vector() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31c3);
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn detects_uboot_prompt_at_line_boundary() {
        assert!(has_uboot_prompt(b"\r\n=> "));
        assert!(has_uboot_prompt(b"=> "));
        assert!(has_uboot_prompt(b"\r\n=> \r\n"));
        assert!(!has_uboot_prompt(b"foo=> "));
        assert!(!has_uboot_prompt(b"=> help loady"));
        assert!(!has_uboot_prompt(b"cost=>value"));
    }

    #[test]
    fn detects_prompt_only_after_command_echo() {
        let text = b"=> help loady\r\nUnknown command 'loady'\r\n=> ";
        let command_end = find_bytes(text, b"help loady").unwrap() + b"help loady".len();

        assert!(has_uboot_prompt_after(text, command_end));
        assert!(!has_uboot_prompt_after(b"=> ", command_end));
    }
}

fn spawn_stdin_writer(mut port: Box<dyn SerialPort>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if port.write_all(line.as_bytes()).is_err() {
                break;
            }
            if port.write_all(b"\r").is_err() {
                break;
            }
            let _ = port.flush();
        }
    });
}

fn pump_serial_forever(session: &mut SerialSession) -> Result<()> {
    loop {
        read_once(session)?;
    }
}

fn show_latest_log(log_dir: &Path, lines: usize) -> Result<()> {
    let mut entries = fs::read_dir(log_dir)
        .with_context(|| format!("failed to read log dir {}", log_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
        .collect::<Vec<_>>();

    entries.sort_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
    let latest = entries
        .pop()
        .ok_or_else(|| anyhow!("no log files found in {}", log_dir.display()))?;
    let path = latest.path();
    println!("{}", path.display());

    let bytes =
        fs::read(&path).with_context(|| format!("failed to read log {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let all_lines = text.lines().collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }
    Ok(())
}
