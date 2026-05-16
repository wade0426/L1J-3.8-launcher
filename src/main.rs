//! Lineage 3.8 launcher.
//!
//! 預設直接啟動 `TW13081901.bin 2130706433`，不使用 DLL、不 suspended。
//! 時間保護、IMG 上限與 FileHook 由 stage2 在遊戲啟動後處理。

#![windows_subsystem = "windows"]

#[path = "assist/mod.rs"]
mod aux;
mod config;
mod dpi_override;
mod equip_ui;
mod gui;
mod hook;
mod hp_mp_patch;
mod http;
mod i18n;
mod ime_inject;
mod img_hover;
mod inject;
mod legacy_text;
mod lineage_cfg;
mod logger;
mod login;
mod memory;
mod packet_proxy;
mod patch;
mod process;
mod smooth_run;
mod smooth_run_hook;

use crate::logger::log_line;
use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use std::net::Ipv4Addr;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, WAIT_OBJECT_0};
use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible,
};

pub const GAME_EXE: &str = "TW13081901.bin";

const DEFAULT_STAGE2_DELAY_MS: u64 = 0;
const DEFAULT_STAGE2_REMAINING_PATCH_DELAY_MS: u64 = 0;
const STAGE2_WINDOW_WAIT_TIMEOUT_MS: u64 = 60_000;
const STAGE2_WINDOW_WAIT_POLL_MS: u64 = 100;
const LHX_USE_ITEM_ADDR: u32 = 0x004B3EE0;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectTarget {
    ip: String,
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketEncryptConfig {
    pub rsa_d: u32,
    pub rsa_n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliLaunchOptions {
    ip: String,
    port: u16,
    game_dir: String,
    no_connect: bool,
    inject_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileHookStartTiming {
    Skipped,
    SpawnImmediatelyAfterAttach,
}

struct EarlyFileHookWorker {
    _join: std::thread::JoinHandle<()>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::panic::set_hook(Box::new(|info| {
        crate::logger::log(format!("[panic] {info}"));
    }));

    let stage2_mode = args.get(1).map(|s| s == "--stage2").unwrap_or(false);
    if should_attach_console(&args) {
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
                let _ = AllocConsole();
            }
        }
    }

    let result = if stage2_mode {
        run_stage2_cli(&args)
    } else if args.len() <= 1 {
        gui::run_gui()
    } else {
        run_cli(&args)
    };

    if let Err(e) = result {
        crate::logger::log(format!("[error] {e:#}"));
        eprintln!("[?航炊] {e:#}");
        std::process::exit(1);
    }
}

fn should_attach_console(args: &[String]) -> bool {
    let stage2_mode = args.get(1).map(|s| s == "--stage2").unwrap_or(false);
    cfg!(feature = "verbose-log") && args.len() > 1 && !stage2_mode
}

fn run_cli(args: &[String]) -> Result<()> {
    let (default_ip, default_port) =
        load_list_txt_default().unwrap_or_else(|| ("127.0.0.1".to_string(), 7001));
    let locked_game_dir = default_game_dir().context("cannot resolve launcher directory")?;
    let opts = parse_cli_args(args, default_ip, default_port, locked_game_dir)?;

    launch_game(
        &opts.ip,
        opts.port,
        &opts.game_dir,
        opts.no_connect,
        None,
        opts.inject_path,
        None,
        true,
        5,
        None,
    )
}

fn parse_cli_args(
    args: &[String],
    default_ip: String,
    default_port: u16,
    locked_game_dir: String,
) -> Result<CliLaunchOptions> {
    let mut ip = default_ip;
    let mut port = default_port;
    let mut no_connect = false;
    let mut inject_path: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: {} [IP PORT] [--inject FILE] [--no-connect]",
                    args[0]
                );
                println!("  --inject FILE  load .pak or .txt inject file");
                println!("  --no-connect   銝?鋆?connect hook");
                println!("  no args        open GUI");
                std::process::exit(0);
            }
            "--no-connect" => no_connect = true,
            "--no-smooth-run-hook" => {
                std::env::set_var("LOGIN38_DISABLE_SMOOTH_RUN_HOOK", "1");
            }
            "--inject" => {
                i += 1;
                if i >= args.len() {
                    bail!("--inject requires a file path");
                }
                inject_path = Some(args[i].clone());
            }
            value => {
                if i + 1 < args.len() && args[i + 1].parse::<u16>().is_ok() {
                    let _: Ipv4Addr = value.parse().context("CLI IP parse failed")?;
                    ip = value.to_string();
                    port = args[i + 1]
                        .parse::<u16>()
                        .context("CLI port parse failed")?;
                    i += 1;
                } else {
                    bail!("unknown argument: {value}");
                }
            }
        }
        i += 1;
    }

    Ok(CliLaunchOptions {
        ip,
        port,
        game_dir: locked_game_dir,
        no_connect,
        inject_path,
    })
}

/// ??launcher.exe ????list.txt嚗仃????批遣?身??
fn default_game_dir() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    default_game_dir_from_exe_path(&exe)
}

fn default_game_dir_from_exe_path(exe: &std::path::Path) -> Option<String> {
    exe.parent().map(|dir| dir.to_string_lossy().into_owned())
}

fn load_list_txt_default() -> Option<(String, u16)> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let list_path = exe_dir.join("list.txt");
    if !list_path.exists() {
        return None;
    }

    let raw = launcher::legacy_text::read_text_file(&list_path).ok()?;
    let plain = launcher::server_list::decrypt_config_text(&raw).unwrap_or(raw);
    let servers = launcher::server_list::parse_list_txt(&plain).ok()?;
    let active = servers.into_iter().find(|s| s.used)?;
    let port = u16::try_from(active.port).ok()?;
    log_line!(
        "[config] list.txt active server: {} {}:{}",
        active.name,
        active.ip,
        port
    );
    Some((active.ip, port))
}

/// 霈??config.ini/list.txt 銝剔? [aux] 閮剖?嚗?銝撠曹蝙?典??券?閮剖潦?
fn load_aux_config() -> launcher::server_list::AuxConfig {
    let result = (|| -> Option<launcher::server_list::AuxConfig> {
        let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        for name in ["config.ini", "list.txt"] {
            let path = exe_dir.join(name);
            if !path.exists() {
                continue;
            }
            let raw = launcher::legacy_text::read_text_file(&path).ok()?;
            let plain = launcher::server_list::decrypt_config_text(&raw).unwrap_or(raw);
            if let Ok(parsed) = launcher::server_list::parse_list_file(&plain) {
                return Some(parsed.aux);
            }
        }
        None
    })();
    let aux = result.unwrap_or_default();
    crate::legacy_text::set_text_encoding_mode(
        crate::legacy_text::TextEncodingMode::from_config_value(
            aux.text_encoding.as_config_value(),
        ),
    );
    aux
}

fn enabled_by_default_env_flag(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .and_then(|v| v.to_str())
        .map(|v| {
            let v = v.trim();
            !(v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off"))
        })
        .unwrap_or(true)
}

fn opt_in_env_flag_requested(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .and_then(|v| v.to_str())
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}

fn game_ip_arg_requested(value: Option<&std::ffi::OsStr>) -> bool {
    opt_in_env_flag_requested(value)
}

fn game_ip_arg_enabled() -> bool {
    let value = std::env::var_os("LOGIN38_GAME_IP_ARG");
    game_ip_arg_requested(value.as_deref())
}

fn connect_hook_requested(no_connect: bool, value: Option<&std::ffi::OsStr>) -> bool {
    !no_connect && enabled_by_default_env_flag(value)
}

fn connect_hook_enabled(no_connect: bool) -> bool {
    let value = std::env::var_os("LOGIN38_CONNECT_HOOK");
    connect_hook_requested(no_connect, value.as_deref())
}

fn keep_launcher_alive_after_stage2_requested(value: Option<&std::ffi::OsStr>) -> bool {
    opt_in_env_flag_requested(value)
}

fn keep_launcher_alive_after_stage2_enabled() -> bool {
    let value = std::env::var_os("LOGIN38_KEEP_LAUNCHER_ALIVE");
    keep_launcher_alive_after_stage2_requested(value.as_deref())
}

fn stage2_pre_visible_attach_requested(value: Option<&std::ffi::OsStr>) -> bool {
    opt_in_env_flag_requested(value)
}

fn stage2_pre_visible_attach_enabled() -> bool {
    let value = std::env::var_os("LOGIN38_STAGE2_ATTACH_BEFORE_WINDOW")
        .or_else(|| std::env::var_os("LOGIN38_STAGE2_PRE_VISIBLE_ATTACH"));
    stage2_pre_visible_attach_requested(value.as_deref())
}

fn stage2_delay_ms() -> u64 {
    std::env::var("LOGIN38_STAGE2_DELAY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms <= 120_000)
        .unwrap_or(DEFAULT_STAGE2_DELAY_MS)
}

fn stage2_remaining_patch_delay_ms() -> u64 {
    std::env::var("LOGIN38_STAGE2_REMAINING_DELAY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms <= 120_000)
        .unwrap_or(DEFAULT_STAGE2_REMAINING_PATCH_DELAY_MS)
}

fn marker_file_present(name: &str) -> bool {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join(name).exists();
        }
    }
    false
}

fn env_truthy(var: &str) -> bool {
    let Some(raw) = std::env::var_os(var) else {
        return false;
    };
    matches!(
        raw.to_string_lossy().trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn smooth_run_hook_disabled_by_env() -> bool {
    env_truthy("LOGIN38_DISABLE_SMOOTH_RUN_HOOK")
        || marker_file_present("disable_smooth_run_hook.flag")
}

fn img_hover_disabled_by_env() -> bool {
    env_truthy("LOGIN38_DISABLE_IMG_HOVER") || marker_file_present("disable_img_hover.flag")
}

fn img_limit_disabled_by_env() -> bool {
    env_truthy("LOGIN38_DISABLE_IMG_LIMIT") || marker_file_present("disable_img_limit.flag")
}

fn hp_mp_limit_disabled_by_env() -> bool {
    env_truthy("LOGIN38_DISABLE_HP_MP_LIMIT") || marker_file_present("disable_hp_mp_limit.flag")
}

fn equip_ui_disabled_by_env() -> bool {
    env_truthy("LOGIN38_DISABLE_EQUIP_UI") || marker_file_present("disable_equip_ui.flag")
}

fn create_game_suspended_requested(pre_resume_startup_hook: bool) -> bool {
    pre_resume_startup_hook
}

fn packet_encrypt_requires_startup_hook(_enabled: bool) -> bool {
    false
}

fn connect_target_for_launch(
    real_ip: &str,
    real_port: u16,
    proxy: Option<(&str, u16)>,
) -> ConnectTarget {
    match proxy {
        Some((ip, port)) => ConnectTarget {
            ip: ip.to_string(),
            port,
        },
        None => ConnectTarget {
            ip: real_ip.to_string(),
            port: real_port,
        },
    }
}

fn ipv4_decimal_arg(ip: &str) -> Option<String> {
    let addr: Ipv4Addr = ip.parse().ok()?;
    Some(u32::from(addr).to_string())
}

fn find_visible_window_for_pid(pid: u32) -> Option<(HWND, String)> {
    struct Search {
        pid: u32,
        found: Option<(HWND, String)>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }

        let mut window_pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
        if window_pid != search.pid {
            return true.into();
        }

        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..copied as usize]);
        if !title.trim().is_empty() {
            search.found = Some((hwnd, title));
            return false.into();
        }
        true.into()
    }

    let mut search = Search { pid, found: None };
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM((&mut search as *mut Search) as isize),
        );
    }
    search.found
}

fn wait_for_visible_window(pid: u32, label: &str) -> bool {
    let start = Instant::now();
    let timeout = Duration::from_millis(STAGE2_WINDOW_WAIT_TIMEOUT_MS);
    let poll = Duration::from_millis(STAGE2_WINDOW_WAIT_POLL_MS);
    let mut next_log = Duration::from_secs(5);

    log_line!("[stage2] {label}: waiting visible game window");
    while start.elapsed() < timeout {
        if let Some((hwnd, title)) = find_visible_window_for_pid(pid) {
            let hwnd_value = hwnd.0 as usize;
            log_line!(
                "[stage2] {label}: visible after {:.3}s hwnd=0x{hwnd_value:X} title={title}",
                start.elapsed().as_secs_f64()
            );
            return true;
        }

        if start.elapsed() >= next_log {
            log_line!(
                "[stage2] {label}: still waiting visible game window {:.3}s",
                start.elapsed().as_secs_f64()
            );
            next_log += Duration::from_secs(5);
        }
        std::thread::sleep(poll);
    }

    log_line!(
        "[stage2] {label}: visible wait timeout {:.3}s; fallback attach",
        start.elapsed().as_secs_f64()
    );
    false
}

fn spawn_delayed_stage2_self(
    pid: u32,
    ip: &str,
    port: u16,
    game_dir: &str,
    no_connect: bool,
    inject_path: Option<&str>,
    windowed: bool,
) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    let delay_ms = stage2_delay_ms();
    let mut child = Command::new(&exe);
    child
        .arg("--stage2")
        .arg(pid.to_string())
        .arg(ip)
        .arg(port.to_string())
        .arg(game_dir)
        .arg("--delay-ms")
        .arg(delay_ms.to_string());
    child.arg(if windowed {
        "--windowed"
    } else {
        "--fullscreen"
    });
    if no_connect {
        child.arg("--no-connect");
    }
    if let Some(path) = inject_path {
        child.arg("--inject").arg(path);
    }

    child
        .creation_flags(0x0800_0000)
        .spawn()
        .context("spawn stage2 failed")?;

    log_line!("[StartupHook] scheduled same EXE stage2 attach after {delay_ms}ms");
    Ok(())
}

fn run_stage2_cli(args: &[String]) -> Result<()> {
    if args.len() < 6 {
        bail!("--stage2 ?銝雲");
    }

    let pid: u32 = args[2].parse().context("--stage2 pid parse failed")?;
    let ip = args[3].clone();
    let port: u16 = args[4].parse().context("--stage2 port parse failed")?;
    let game_dir = args[5].clone();
    let mut no_connect = false;
    let mut inject_path: Option<String> = None;
    let mut delay_ms = 0_u64;
    let mut windowed = true;

    let mut i = 6;
    while i < args.len() {
        match args[i].as_str() {
            "--no-connect" => no_connect = true,
            "--windowed" => windowed = true,
            "--fullscreen" => windowed = false,
            "--delay-ms" => {
                i += 1;
                if i >= args.len() {
                    bail!("--stage2 --delay-ms ?閬?摰神蝘");
                }
                delay_ms = args[i]
                    .parse()
                    .context("--stage2 --delay-ms parse failed")?;
            }
            "--inject" => {
                i += 1;
                if i >= args.len() {
                    bail!("--stage2 --inject requires a file path");
                }
                inject_path = Some(args[i].clone());
            }
            other => log_line!("[stage2] ignore unknown arg: {other}"),
        }
        i += 1;
    }

    if delay_ms > 0 {
        log_line!("[stage2] sleep {delay_ms}ms before attach");
        std::thread::sleep(Duration::from_millis(delay_ms));
    }

    if stage2_pre_visible_attach_enabled() {
        log_line!("[stage2] pre-visible attach enabled by env");
    }

    log_line!("[stage2] attach pid={pid} target={ip}:{port} game_dir={game_dir}");
    let h_process = process::open_game_process(pid)?;

    let connect_hook_installed =
        install_stage2_connect_hook(h_process, pid, &ip, port, no_connect, "pre-patch")?;

    let file_hook_worker = spawn_early_file_hook_worker(h_process, pid, inject_path.as_deref())?;
    let file_hook_requested = file_hook_worker.is_some();

    log_line!("[stage2] time protection bypass: wait_and_patch");
    patch::wait_and_patch(h_process, pid)?;
    log_line!("[stage2] time protection bypass patched");

    if let Err(e) = inject_ime_overlay(h_process) {
        log_line!("[stage2] IME overlay preload failed: {e:#}");
    }

    run_stage2_feature_patches(
        h_process,
        pid,
        &ip,
        port,
        no_connect,
        connect_hook_installed,
        file_hook_requested,
        &game_dir,
    )?;

    let _ = wait_for_visible_window(pid, "post-start feature patch");
    let kept_alive_for_aux = run_lhx_aux_until_game_exit(h_process, pid)?;

    unsafe {
        let _ = CloseHandle(h_process);
    }
    if kept_alive_for_aux {
        log_line!("[stage2] done after LHX aux shutdown");
    } else {
        log_line!("[stage2] done");
    }
    Ok(())
}

fn inject_ime_overlay(h_process: HANDLE) -> Result<()> {
    let cache_dir = ime_inject::ensure_cached()?;
    ime_inject::inject_ime_dll(h_process, &cache_dir)?;
    log_line!("[stage2] IME overlay preload done");
    Ok(())
}

fn install_stage2_connect_hook(
    h_process: HANDLE,
    pid: u32,
    ip: &str,
    port: u16,
    no_connect: bool,
    phase: &str,
) -> Result<bool> {
    if no_connect {
        log_line!("[stage2] {phase} connect hook skipped by --no-connect");
        return Ok(false);
    }

    hook::hook_connect(h_process, pid, ip, port, 0)?;
    log_line!("[stage2] {phase} connect hook installed");
    Ok(true)
}

fn file_hook_start_timing(
    inject_path: Option<&str>,
    path_exists: impl FnOnce(&str) -> bool,
) -> FileHookStartTiming {
    match inject_path {
        Some(path) if path_exists(path) => FileHookStartTiming::SpawnImmediatelyAfterAttach,
        _ => FileHookStartTiming::Skipped,
    }
}

fn spawn_early_file_hook_worker(
    h_process: HANDLE,
    pid: u32,
    inject_path: Option<&str>,
) -> Result<Option<EarlyFileHookWorker>> {
    let Some(path) = inject_path else {
        log_line!("[stage2] no inject file path; early FileHook skipped");
        return Ok(None);
    };

    if file_hook_start_timing(Some(path), |p| std::path::Path::new(p).exists())
        == FileHookStartTiming::Skipped
    {
        log_line!("[stage2] inject file not found, early FileHook skipped: {path}");
        return Ok(None);
    }

    let buffer = inject::load_inject_file(path)?;
    let h_raw = h_process.0 as usize;
    log_line!("[stage2] FileHook worker spawned immediately after attach");
    let join = std::thread::spawn(move || {
        let h_process = HANDLE(h_raw as *mut _);
        match inject::start_file_hook_worker(h_process, pid, &buffer) {
            Ok(()) => log_line!("[stage2] FileHook installed by early worker"),
            Err(e) => log_line!("[stage2] FileHook worker failed: {e:#}"),
        }
    });
    Ok(Some(EarlyFileHookWorker { _join: join }))
}

fn run_stage2_feature_patches(
    h_process: HANDLE,
    pid: u32,
    ip: &str,
    port: u16,
    no_connect: bool,
    connect_hook_installed: bool,
    file_hook_installed: bool,
    game_dir: &str,
) -> Result<()> {
    let aux_cfg = load_aux_config();
    crate::aux::notification::set_enabled(
        aux_cfg.pickup_toast_enabled,
        aux_cfg.exp_drift_enabled,
    );

    if connect_hook_installed {
        log_line!("[stage2] early connect hook already installed before time patch");
    } else {
        install_stage2_connect_hook(h_process, pid, ip, port, no_connect, "early")?;
    }

    login::install_login_hooks(h_process, pid)?;
    log_line!("[stage2] early login hooks installed");

    patch::patch_ac_check(h_process)?;
    log_line!("[stage2] AC check bypass patched");

    patch::patch_crt_watson(h_process, pid)?;
    log_line!("[stage2] CRT Watson bypass patched");

    if let Err(e) = aux::chat_width::install_chat_width_patch(h_process) {
        log_line!("[stage2] chat width patch 失敗(略過,不影響啟動): {e:#}");
    }

    if aux_cfg.img_limit_enabled && !img_limit_disabled_by_env() {
        patch::patch_img_limit(h_process, aux_cfg.img_limit_value)?;
        log_line!(
            "[stage2] post-start IMG limit patch value={}",
            aux_cfg.img_limit_value
        );
    } else if img_limit_disabled_by_env() {
        log_line!("[stage2] IMG limit DISABLED by user (marker file / env)");
    } else {
        log_line!("[stage2] IMG limit disabled by config");
    }

    // PNG 圖檔上限突破:1564 → 100000 (跟 IMG patch 同窗口下;失敗只 log,不中斷啟動)
    match patch::patch_png_limit(h_process, 100_000) {
        Ok(()) => log_line!("[stage2] post-start PNG limit patch applied"),
        Err(e) => log_line!("[stage2] PNG limit patch 失敗(略過,不影響啟動): {e:#}"),
    }

    if aux_cfg.inventory_limit_enabled {
        patch::patch_inventory_limit(h_process, aux_cfg.inventory_limit_value)?;
        log_line!(
            "[stage2] post-start inventory limit patch value={}",
            aux_cfg.inventory_limit_value
        );
    } else {
        log_line!("[stage2] inventory limit disabled by config");
    }

    if aux_cfg.dynamic_dialog_enabled && !img_hover_disabled_by_env() {
        match img_hover::install_img_hover_hook(h_process, pid) {
            Ok(Some(result)) => {
                log_line!("[stage2] img hover hook installed");
                spawn_hover_poll(result);
            }
            Ok(None) => log_line!("[stage2] img hover hook skipped"),
            Err(e) => log_line!("[stage2] img hover hook failed: {e}"),
        }
    } else if img_hover_disabled_by_env() {
        log_line!("[stage2] img hover hook DISABLED by user (marker file / env)");
    } else {
        log_line!("[stage2] dynamic dialog disabled by config");
    }

    if aux_cfg.equip_ui_enabled && !equip_ui_disabled_by_env() {
        equip_ui::install_equip_ui_patches(h_process, pid)?;
        log_line!("[stage2] equip UI patches installed");
    } else if equip_ui_disabled_by_env() {
        log_line!("[stage2] equip UI patches DISABLED by user (marker file / env)");
    } else {
        log_line!("[stage2] equip UI disabled by config");
    }

    if aux_cfg.hp_mp_limit_enabled && !hp_mp_limit_disabled_by_env() {
        hp_mp_patch::install_hp_mp_patches(h_process, pid)?;
        log_line!("[stage2] HP/MP limit patches installed");
    } else if hp_mp_limit_disabled_by_env() {
        log_line!("[stage2] HP/MP limit patches DISABLED by user (marker file / env)");
    } else {
        log_line!("[stage2] HP/MP limit disabled by config");
    }

    let remaining_delay_ms = stage2_remaining_patch_delay_ms();
    if remaining_delay_ms > 0 {
        log_line!("[stage2] sleep {remaining_delay_ms}ms before remaining patches");
        std::thread::sleep(Duration::from_millis(remaining_delay_ms));
    }

    if file_hook_installed && inject::morph_preprocess_enabled() {
        if smooth_run_hook_disabled_by_env() {
            log_line!("[stage2] smooth run hook DISABLED by user (env/CLI/marker file)");
        } else {
            match smooth_run_hook::install_smooth_run_hook(h_process, pid) {
                Ok(()) => log_line!("[stage2] smooth run hook installed (per-entity @ 0x00449776)"),
                Err(e) => log_line!("[stage2] smooth run hook failed: {e}"),
            }
        }
    } else if file_hook_installed {
        log_line!("[stage2] smooth run hook skipped; morph preprocess disabled");
    }
    if file_hook_installed {
        if let Err(e) = aux::poison_hook::install_poison_hook(h_process, pid) {
            log_line!("[stage2] poison hook failed: {e}");
        }
    }
    let _ = crate::aux::notification::install(
        h_process,
        pid,
        std::path::Path::new(&game_dir),
    )
    .map_err(|e| crate::logger::log_line!("[notification] install skipped: {e:#}"));
    if aux_cfg.move_packet_no_encrypt {
        spawn_delayed_move_packet_no_encrypt_patch(h_process);
    }

    log_line!("[stage2] all patches done");
    Ok(())
}

fn should_start_lhx_aux(aux_cfg: &launcher::server_list::AuxConfig) -> bool {
    aux_cfg.lhx_aux_enabled
}

fn player_state_ready_for_lhx(state: &aux::player_state::PlayerState) -> bool {
    state.max_hp > 0 && state.max_mp > 0
}

fn read_lhx_profile_name(h_process: HANDLE) -> String {
    for _ in 0..50 {
        if let Some(name) = aux::profile::read_player_name(h_process) {
            log_line!("[stage2] LHX aux profile={name}");
            return name;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log_line!("[stage2] LHX aux profile fallback=default");
    "default".to_string()
}

fn initialize_lhx_runtime_handles(h_process: HANDLE, control: &aux::runtime::AuxControl) {
    match memory::read_bytes(h_process, LHX_USE_ITEM_ADDR, 6) {
        Ok(bytes) => log_line!(
            "[stage2] LHX USE_ITEM ready @ 0x{LHX_USE_ITEM_ADDR:08X}: {}",
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Err(e) => log_line!(
            "[stage2] LHX USE_ITEM prologue read failed @ 0x{LHX_USE_ITEM_ADDR:08X}: {e:#}"
        ),
    }

    *control.drink.write() = Some(Arc::new(aux::drink_hook::DrinkHandle::new(
        LHX_USE_ITEM_ADDR,
    )));
    log_line!("[stage2] LHX DrinkHandle installed");
}

struct LhxActiveSession {
    profile_name: String,
    control: Arc<aux::runtime::AuxControl>,
    window: aux::lhx_window::WindowControl,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl LhxActiveSession {
    fn shutdown(self) {
        // 先存 game_handle —— restore_all_misc_patches 在 thread join 後才能跑,
        // 避免跟 GUI handler / sync thread race。但 join 後 self 已被 destruct
        // 一部分,提前存下 raw handle。
        let h_raw = self.window.game_handle.load(Ordering::Relaxed);

        self.control.shutdown();
        self.window
            .visible
            .store(aux::lhx_window::VISIBLE_CLOSE, Ordering::Relaxed);
        for handle in self.handles {
            let _ = handle.join();
        }
        let _ = self.window.thread.join();

        // 所有 thread 已退出 → 沒有 race。把 misc 分頁的 toggle/hook 全部還原,
        // 避免「換角後仍然全白天 / 不開輔助仍開水底通行」等 patch 殘留。
        if h_raw != 0 {
            let h = HANDLE(h_raw as *mut _);
            aux::lhx_window::restore_all_misc_patches(h);
        }

        aux::profile::save(&self.profile_name, &self.control.settings.read().clone());
    }
}

fn start_lhx_session(h_process: HANDLE, pid: u32) -> Result<LhxActiveSession> {
    let profile_name = read_lhx_profile_name(h_process);
    let settings = Arc::new(RwLock::new(aux::profile::load(&profile_name)));
    let control = Arc::new(aux::runtime::AuxControl::from_shared(settings.clone()));
    initialize_lhx_runtime_handles(h_process, &control);
    let window = aux::lhx_window::spawn_window_thread(
        settings,
        h_process,
        Arc::downgrade(&control.timer_resets),
    );
    window
        .visible
        .store(aux::lhx_window::VISIBLE_SHOWN, Ordering::Relaxed);
    let scheduler = aux::runtime::AuxScheduler::new(h_process, pid, control.clone());
    let handles = scheduler.spawn_all();
    // 推 in-game chat 綠字啟動字 — 走 ChatDispatch(0x00437500, channel=-1),
    // 保留 0x00437D30 ChatSideEffect 副作用避免聊天框 auto-tail 失效
    if let Err(e) = aux::chat::push_lhx_started(h_process) {
        log_line!("[stage2] push_lhx_started 失敗: {e:#}");
    }
    log_line!("[stage2] LHX aux started by Home");
    Ok(LhxActiveSession {
        profile_name,
        control,
        window,
        handles,
    })
}

fn spawn_lhx_home_toggle_thread(h_process: HANDLE, pid: u32) -> std::thread::JoinHandle<()> {
    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(vkey: i32) -> i16;
    }

    const VK_HOME: i32 = 0x24;
    let h_raw = h_process.0 as usize;
    std::thread::spawn(move || {
        let h_process = HANDLE(h_raw as *mut _);
        let mut last_state = unsafe { GetAsyncKeyState(VK_HOME) } as u16 & 0x8000 != 0;
        let mut last_in_world = false;
        let mut session: Option<LhxActiveSession> = None;
        log_line!("[stage2] LHX Home listener started; aux is idle until Home");
        loop {
            if unsafe { WaitForSingleObject(h_process, 0) } == WAIT_OBJECT_0 {
                if let Some(active) = session.take() {
                    active.shutdown();
                }
                log_line!("[stage2] LHX Home listener stopped: game exited");
                return;
            }

            let cur_in_world = crate::memory::read_u32(h_process, aux::address::G_GAME_STATE)
                .map(|s| s == 3)
                .unwrap_or(false);

            let pressed = unsafe { GetAsyncKeyState(VK_HOME) } as u16 & 0x8000 != 0;
            let rising_edge = pressed && !last_state;
            // GetAsyncKeyState 是全域鍵盤狀態,多開時 A、B 兩個 launcher 都會看到
            // Home 被按下。加 foreground PID 檢查 → 只有當前 launcher 啟動的遊戲視窗
            // 是 foreground 時才觸發,避免 A 焦點時按 Home 把 B 的 LHX 也一起切換。
            let target_focused = unsafe {
                let fg = GetForegroundWindow();
                if fg.0.is_null() {
                    false
                } else {
                    let mut fg_pid: u32 = 0;
                    GetWindowThreadProcessId(fg, Some(&mut fg_pid));
                    fg_pid == pid
                }
            };
            if rising_edge && target_focused {
                if let Some(active) = &session {
                    let current = active.window.visible.load(Ordering::Relaxed);
                    let next = if current == aux::lhx_window::VISIBLE_SHOWN {
                        aux::lhx_window::VISIBLE_HIDDEN
                    } else {
                        aux::lhx_window::VISIBLE_SHOWN
                    };
                    active.window.visible.store(next, Ordering::Relaxed);
                    log_line!(
                        "[stage2] LHX Home toggle -> {}",
                        if next == aux::lhx_window::VISIBLE_SHOWN {
                            "show"
                        } else {
                            "hide"
                        }
                    );
                } else if !cur_in_world {
                    log_line!("[stage2] LHX Home ignored: player is not in game world");
                } else {
                    match aux::player_state::read_player_state(h_process) {
                        Ok(state) if player_state_ready_for_lhx(&state) => {
                            match start_lhx_session(h_process, pid) {
                                Ok(active) => session = Some(active),
                                Err(e) => log_line!("[stage2] LHX Home start failed: {e:#}"),
                            }
                        }
                        Ok(_) => log_line!("[stage2] LHX Home ignored: player state not ready"),
                        Err(e) => {
                            log_line!("[stage2] LHX Home ignored: player state read failed: {e:#}")
                        }
                    }
                }
            }

            if last_in_world && !cur_in_world {
                if let Some(active) = session.take() {
                    active.shutdown();
                    log_line!("[stage2] LHX aux stopped after leaving game world");
                }
            }
            last_in_world = cur_in_world;
            last_state = pressed;
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

fn run_lhx_aux_until_game_exit(h_process: HANDLE, pid: u32) -> Result<bool> {
    let aux_cfg = load_aux_config();
    if !should_start_lhx_aux(&aux_cfg) {
        log_line!("[stage2] LHX aux disabled by config");
        return Ok(false);
    }

    let home_toggle = spawn_lhx_home_toggle_thread(h_process, pid);
    log_line!("[stage2] LHX aux enabled; waiting for Home or game exit");
    unsafe {
        WaitForSingleObject(h_process, INFINITE);
    }
    let _ = home_toggle.join();
    log_line!("[stage2] LHX aux shutdown complete");
    Ok(true)
}

fn spawn_delayed_move_packet_no_encrypt_patch(h: HANDLE) {
    let h_raw = h.0 as usize;
    std::thread::spawn(move || {
        let h = HANDLE(h_raw as *mut _);
        log_line!("[MoveNoEncrypt] wait for in-game state before patch");
        for _ in 0..6000 {
            if crate::memory::read_u32(h, aux::address::G_GAME_STATE).ok() == Some(3) {
                match patch::patch_move_packet_no_encrypt(h) {
                    Ok(()) => log_line!("[MoveNoEncrypt] patch installed"),
                    Err(e) => log_line!("[MoveNoEncrypt] patch failed: {e:#}"),
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        log_line!("[MoveNoEncrypt] timeout before in-game state");
    });
}

/// img hover 30ms polling:GetCursorPos → poll_hover_tick 寫 g_hover_id;
/// F6 rising-edge 觸發 log_calibration;遊戲程序結束時自動退出。
fn spawn_hover_poll(result: img_hover::HoverHookResult) {
    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(vkey: i32) -> i16;
        fn GetCursorPos(p: *mut HoverPoint) -> i32;
        fn GetClientRect(hwnd: isize, rect: *mut ClientRect) -> i32;
    }

    #[repr(C)]
    struct HoverPoint {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct ClientRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    const VK_F6: i32 = 0x75;
    let h_raw = result.game_handle.0 as usize;
    let cave_draw = result.cave_draw;
    let cave_blit = result.cave_blit;
    let pid = result.pid;
    let draw_hook_bytes = result.draw_hook_bytes;
    let draw_orig_bytes = result.draw_orig_bytes;

    std::thread::spawn(move || {
        let h = HANDLE(h_raw as *mut _);
        let hwnd = match img_hover::find_hwnd_by_pid(pid) {
            Ok(hwnd) => {
                log_line!(
                    "[stage2] img hover polling started (HWND=0x{:X}, F6 校準座標)",
                    hwnd
                );
                Some(hwnd)
            }
            Err(_) => {
                log_line!("[stage2] img hover polling started (HWND 未取得, F6 校準座標)");
                None
            }
        };
        let mut last_f6 = false;
        loop {
            if unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0 {
                log_line!("[stage2] img hover polling stopped: game exited");
                return;
            }
            let mut pt = HoverPoint { x: 0, y: 0 };
            unsafe {
                GetCursorPos(&mut pt as *mut HoverPoint);
            }
            let pressed = unsafe { GetAsyncKeyState(VK_F6) } as u16 & 0x8000 != 0;
            if pressed && !last_f6 {
                img_hover::log_calibration(h, cave_draw, cave_blit, pt.x, pt.y);
            }
            last_f6 = pressed;
            let _ = img_hover::poll_hover_tick(
                h,
                cave_draw,
                cave_blit,
                pid,
                pt.x,
                pt.y,
                &draw_hook_bytes,
                &draw_orig_bytes,
            );

            // Notification tick — piggyback 在同個 30ms 節奏(catch_unwind 守在 mod 內,
            // 這裡再加一層保險)。
            let (sw, sh) = if let Some(hwnd) = hwnd {
                let mut r = ClientRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                if unsafe { GetClientRect(hwnd, &mut r) } != 0 {
                    (r.right - r.left, r.bottom - r.top)
                } else {
                    (1024, 768)
                }
            } else {
                (1024, 768)
            };
            let _ = std::panic::catch_unwind(|| {
                let _ = aux::notification::on_polling_tick(h, Instant::now(), sw, sh);
            });

            std::thread::sleep(Duration::from_millis(30));
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub fn launch_game(
    ip: &str,
    port: u16,
    game_dir: &str,
    no_connect: bool,
    inject_buffer: Option<Vec<u8>>,
    inject_source_path: Option<String>,
    packet_encrypt: Option<PacketEncryptConfig>,
    windowed: bool,
    window_mode: u8,
    on_started: Option<Box<dyn FnOnce() + Send>>,
) -> Result<()> {
    let launch_start = Instant::now();
    let exe_path = format!("{game_dir}\\{GAME_EXE}");
    if !std::path::Path::new(&exe_path).exists() {
        bail!("?曆??圈??脣銵?: {exe_path}");
    }

    log_line!("========================================");
    log_line!("Lineage 3.8 Rust launcher");
    log_line!("?瑼?: {exe_path}");
    log_line!("?格?隡箸??? {ip}:{port}");
    log_line!(
        "閬?璅∪?: {} WindowMode={window_mode}",
        if windowed { "windowed" } else { "fullscreen" }
    );
    log_line!("[launch-time] launch_game start");
    let startup_hook_required = packet_encrypt_requires_startup_hook(packet_encrypt.is_some());
    log_line!("[StartupHook] pre-resume hook disabled; no DLL launch path");

    if let Some(cfg) = packet_encrypt {
        log_line!(
            "[PacketEncrypt] enabled rsa_d={} rsa_n={}",
            cfg.rsa_d,
            cfg.rsa_n
        );
    } else {
        log_line!("[PacketEncrypt] disabled");
    }

    let packet_proxy_endpoint = if let Some(cfg) = packet_encrypt {
        Some(packet_proxy::start_packet_encrypt_proxy(
            packet_proxy::PacketProxyConfig {
                server_ip: ip.to_string(),
                server_port: port,
                packet_encrypt: cfg,
            },
        )?)
    } else {
        None
    };
    let connect_target = connect_target_for_launch(
        ip,
        port,
        packet_proxy_endpoint
            .as_ref()
            .map(|endpoint| (endpoint.ip.as_str(), endpoint.port)),
    );
    if packet_proxy_endpoint.is_some() {
        log_line!(
            "[NetProxy] PacketEncrypt proxy route: game -> {}:{} -> {ip}:{port}",
            connect_target.ip,
            connect_target.port
        );
    } else {
        log_line!("[NetProxy] disabled: game connects directly to {ip}:{port}");
    }

    let connect_hook_enabled = connect_hook_enabled(no_connect);
    let patch_no_connect = !connect_hook_enabled;
    if connect_hook_enabled {
        log_line!("[ConnectHook] post-start connect hook enabled by env");
    } else {
        log_line!("[ConnectHook] direct IP mode; connect hook disabled by default");
    }

    if inject_buffer.is_some() && inject_source_path.is_none() {
        log_line!("[stage2] inject buffer has no source path; FileHook requires source path");
    }

    let game_ip_arg = if game_ip_arg_enabled() {
        let arg = ipv4_decimal_arg(&connect_target.ip);
        if let Some(arg) = &arg {
            log_line!(
                "[launch-time] game IPv4 arg={arg} ({}:{})",
                connect_target.ip,
                connect_target.port
            );
        }
        arg
    } else {
        log_line!("[launch-time] game IPv4 arg disabled by LOGIN38_GAME_IP_ARG");
        None
    };

    let create_suspended = create_game_suspended_requested(startup_hook_required);
    if create_suspended {
        log_line!("[launch-time] CreateProcess suspended for pre-resume DLL install");
    } else {
        log_line!("[launch-time] direct-bin CreateProcess without CREATE_SUSPENDED");
    }
    // 註:`inject_buffer` 不再用於 pre-resume FileHook(實測對 crash 無幫助、
    // 多 16 秒等 packer 解密)。FileHook 改回 stage2 spawn_early_file_hook_worker 路徑,
    // 跟 img hover / equip UI 等其他 hook 在同一 stage 安裝。
    let _ = inject_buffer;

    apply_display_mode_config(game_dir, windowed, window_mode);

    if windowed {
        log_line!("[compat] windowed launch; fullscreen optimization flag skipped");
    } else if let Err(e) = dpi_override::ensure_disable_fullscreen_optimizations(&exe_path) {
        log_line!("[compat] disable fullscreen optimizations failed: {e:#}");
    }

    let (h_process, h_thread, pid) = process::create_game_with_args(
        &exe_path,
        game_dir,
        create_suspended,
        game_ip_arg.as_deref(),
    )?;
    log_line!(
        "[launch-time] CreateProcess reached {:.3}s",
        launch_start.elapsed().as_secs_f64()
    );
    log_line!("[OK] game process started PID={pid}");

    if create_suspended {
        log_line!("[StartupHook] no startup_hook DLL before ResumeThread");
        process::resume_main_thread(h_thread);
        log_line!(
            "[launch-time] ResumeThread done {:.3}s",
            launch_start.elapsed().as_secs_f64()
        );
    } else {
        unsafe {
            let _ = CloseHandle(h_thread);
        }
        log_line!(
            "[launch-time] CreateProcess returned running process {:.3}s",
            launch_start.elapsed().as_secs_f64()
        );
    }

    if let Some(cb) = on_started {
        cb();
    }

    spawn_delayed_stage2_self(
        pid,
        &connect_target.ip,
        connect_target.port,
        game_dir,
        patch_no_connect,
        inject_source_path.as_deref(),
        windowed,
    )?;

    if keep_launcher_alive_after_stage2_enabled() || packet_proxy_endpoint.is_some() {
        log_line!("[StartupHook] fast stage1 mode: keep launcher alive for game process");
        unsafe {
            WaitForSingleObject(h_process, INFINITE);
            let _ = CloseHandle(h_process);
        }
        log_line!("[StartupHook] fast stage1 mode: game exited, launcher exits");
    } else {
        log_line!("[StartupHook] fast stage1 mode: stage2 scheduled, launcher exits immediately");
        unsafe {
            let _ = CloseHandle(h_process);
        }
    }

    Ok(())
}

fn apply_display_mode_config(game_dir: &str, windowed: bool, window_mode: u8) {
    let fullscreen = if windowed { 0 } else { 1 };
    if let Err(e) = lineage_cfg::set_fullscreen(game_dir, fullscreen) {
        log_line!("[cfg] set FullScreen={fullscreen} failed: {e:#}");
        return;
    }

    if windowed {
        if let Err(e) = lineage_cfg::set_window_mode(game_dir, window_mode as u32) {
            log_line!("[cfg] set WindowMode={window_mode} failed: {e:#}");
            return;
        }
    }

    log_line!(
        "[cfg] display mode applied: {} WindowMode={window_mode}",
        if windowed { "windowed" } else { "fullscreen" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_arg_uses_decimal_form_expected_by_bin() {
        assert_eq!(ipv4_decimal_arg("127.0.0.1").as_deref(), Some("2130706433"));
    }

    #[test]
    fn game_ip_arg_is_opt_in() {
        assert!(!game_ip_arg_requested(None));
        assert!(!game_ip_arg_requested(Some("0".as_ref())));
        assert!(!game_ip_arg_requested(Some("false".as_ref())));
        assert!(game_ip_arg_requested(Some("1".as_ref())));
    }

    #[test]
    fn direct_bin_mode_does_not_create_suspended_process() {
        assert!(!create_game_suspended_requested(false));
        assert!(create_game_suspended_requested(true));
    }

    #[test]
    fn packet_encrypt_does_not_require_startup_hook_dll() {
        assert!(!packet_encrypt_requires_startup_hook(true));
        assert!(!create_game_suspended_requested(
            packet_encrypt_requires_startup_hook(true),
        ));
    }

    #[test]
    fn packet_encrypt_routes_connect_to_local_proxy_endpoint() {
        let target = connect_target_for_launch("203.0.113.10", 7000, Some(("127.0.0.1", 49152)));

        assert_eq!(target.ip, "127.0.0.1");
        assert_eq!(target.port, 49152);
    }

    #[test]
    fn plain_launch_routes_connect_to_real_server() {
        let target = connect_target_for_launch("203.0.113.10", 7000, None);

        assert_eq!(target.ip, "203.0.113.10");
        assert_eq!(target.port, 7000);
    }

    #[test]
    fn release_build_does_not_attach_console_for_cli_logs() {
        let args = vec![
            "launcher.exe".to_string(),
            "127.0.0.1".to_string(),
            "7001".to_string(),
        ];

        assert_eq!(should_attach_console(&args), cfg!(feature = "verbose-log"));
    }

    #[test]
    fn stage2_never_attaches_console() {
        let args = vec!["launcher.exe".to_string(), "--stage2".to_string()];

        assert!(!should_attach_console(&args));
    }

    #[test]
    fn default_game_dir_is_launcher_exe_parent_not_hardcoded_path() {
        let path = std::path::Path::new(r"D:\locked-client\launcher.exe");

        assert_eq!(
            default_game_dir_from_exe_path(path).as_deref(),
            Some(r"D:\locked-client")
        );
    }

    #[test]
    fn cli_ip_port_keeps_launcher_exe_parent_as_game_dir() {
        let args = vec![
            "launcher.exe".to_string(),
            "192.168.1.10".to_string(),
            "7000".to_string(),
        ];

        let parsed = parse_cli_args(
            &args,
            "127.0.0.1".to_string(),
            7001,
            r"D:\locked-client".to_string(),
        )
        .unwrap();

        assert_eq!(parsed.ip, "192.168.1.10");
        assert_eq!(parsed.port, 7000);
        assert_eq!(parsed.game_dir, r"D:\locked-client");
    }

    #[test]
    fn cli_rejects_positional_game_dir_override() {
        let args = vec!["launcher.exe".to_string(), r"D:\other-client".to_string()];

        let err = parse_cli_args(
            &args,
            "127.0.0.1".to_string(),
            7001,
            r"D:\locked-client".to_string(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown argument"));
    }

    #[test]
    fn cli_rejects_path_even_when_followed_by_port() {
        let args = vec![
            "launcher.exe".to_string(),
            r"D:\other-client".to_string(),
            "7001".to_string(),
        ];

        let err = parse_cli_args(
            &args,
            "127.0.0.1".to_string(),
            7001,
            r"D:\locked-client".to_string(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("CLI IP parse failed"));
    }

    #[test]
    fn connect_hook_is_enabled_by_default() {
        assert!(connect_hook_requested(false, None));
        assert!(!connect_hook_requested(false, Some("0".as_ref())));
        assert!(!connect_hook_requested(true, Some("1".as_ref())));
        assert!(connect_hook_requested(false, Some("1".as_ref())));
    }

    #[test]
    fn stage2_pre_visible_attach_is_opt_in() {
        assert!(!stage2_pre_visible_attach_requested(None));
        assert!(!stage2_pre_visible_attach_requested(Some("0".as_ref())));
        assert!(stage2_pre_visible_attach_requested(Some("1".as_ref())));
    }

    #[test]
    fn launcher_parent_exits_after_stage2_by_default() {
        assert!(!keep_launcher_alive_after_stage2_requested(None));
        assert!(!keep_launcher_alive_after_stage2_requested(Some(
            "0".as_ref()
        )));
        assert!(keep_launcher_alive_after_stage2_requested(Some(
            "1".as_ref()
        )));
    }

    #[test]
    fn lhx_aux_switch_controls_stage2_keepalive_aux() {
        let mut aux = launcher::server_list::AuxConfig::default();
        aux.lhx_aux_enabled = false;
        assert!(!should_start_lhx_aux(&aux));

        aux.lhx_aux_enabled = true;
        assert!(should_start_lhx_aux(&aux));
    }

    #[test]
    fn lhx_aux_waits_for_real_player_state() {
        assert!(!player_state_ready_for_lhx(
            &aux::player_state::PlayerState::default()
        ));

        let state = aux::player_state::PlayerState {
            max_hp: 100,
            max_mp: 30,
            ..Default::default()
        };
        assert!(player_state_ready_for_lhx(&state));
    }

    #[test]
    fn stage2_attaches_immediately_by_default() {
        assert_eq!(DEFAULT_STAGE2_DELAY_MS, 0);
    }

    #[test]
    fn file_hook_worker_starts_immediately_after_stage2_attach_when_inject_exists() {
        assert_eq!(
            file_hook_start_timing(Some(r"D:\lineage3.81C\TW13081901.pak"), |_| true),
            FileHookStartTiming::SpawnImmediatelyAfterAttach
        );
    }
}
