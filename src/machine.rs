//! This module contains systemd machined related APIs

use crate::common::{is_legacy_workspace, CIEL_INST_DIR};
use crate::dbus_machine1::ManagerProxyBlocking;
use crate::dbus_machine1_machine::MachineProxyBlocking;
use crate::overlayfs::is_mounted;
use crate::{info, overlayfs::LayerManager, warn};
use adler32::adler32;
use anyhow::{anyhow, Result};
use console::style;
use libc::{c_char, ftok, waitpid, WNOHANG};
use libsystemd_sys::bus::{sd_bus_flush_close_unref, sd_bus_open_system_machine};
use std::{
    ffi::{CString, OsStr},
    mem::MaybeUninit,
    process::Command,
};
use std::{fs, time::Duration};
use std::{os::unix::ffi::OsStrExt, process::Child};
use std::{path::Path, process::Stdio, thread::sleep};
use zbus::blocking::Connection;

const DEFAULT_NSPAWN_OPTIONS: &[&str] = &[
    "-qb",
    "--capability=CAP_IPC_LOCK",
    "--system-call-filter=swapcontext",
];

/// Instance status information
#[derive(Debug)]
pub struct CielInstance {
    name: String,
    // namespace name (in the form of `$name-$id`)
    pub ns_name: String,
    pub mounted: bool,
    running: bool,
    pub started: bool,
    booted: Option<bool>,
}

/// Used for getting the instance name from Ciel 1/2
fn legacy_container_name(path: &Path) -> Result<String> {
    let key_id;
    let current_dir = std::env::current_dir()?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid container path: {:?}", path))?;
    let mut path = current_dir.as_os_str().as_bytes().to_owned();
    path.push(0); // add trailing null terminator
    unsafe {
        // unsafe because of the `ftok` invocation
        key_id = ftok(path.as_ptr() as *const c_char, 0);
    }
    if key_id < 0 {
        return Err(anyhow!("ftok() failed."));
    }

    Ok(format!(
        "{}-{:x}",
        name.to_str()
            .ok_or_else(|| anyhow!("Container name is not valid unicode."))?,
        key_id
    ))
}

/// Used for getting the instance name from Ciel 3+
fn new_container_name(path: &Path) -> Result<String> {
    // New container name is calculated using the following formula:
    // $name-adler32($PWD)
    let hash = adler32(path.as_os_str().as_bytes())?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid container path: {:?}", path))?;

    Ok(format!(
        "{}-{:x}",
        name.to_str()
            .ok_or_else(|| anyhow!("Container name is not valid unicode."))?,
        hash
    ))
}

fn try_open_container_bus(ns_name: &str) -> Result<()> {
    // There are bunch of trickeries happening here
    // First we initialize an empty pointer
    let mut buf = MaybeUninit::uninit();
    // Convert the ns_name to C-style `const char*` (NUL-terminated)
    let ns_name = CString::new(ns_name)?;
    // unsafe: these functions are from libsystemd, which involving FFI calls
    unsafe {
        // Try opening a connection to the container
        if sd_bus_open_system_machine(buf.as_mut_ptr(), ns_name.as_ptr()) >= 0 {
            // If successful, just close the connection and drop the pointer
            sd_bus_flush_close_unref(buf.assume_init());
            return Ok(());
        }
    }

    Err(anyhow!("Could not open container bus"))
}

fn wait_for_container(child: &mut Child, ns_name: &str, retry: usize) -> Result<()> {
    for i in 0..retry {
        let exited = child.try_wait()?;
        if let Some(status) = exited {
            return Err(anyhow!("nspawn exited too early! (Status: {})", status));
        }
        // why this is used: because PTY spawning can happen before the systemd in the container
        // is fully initialized. To spawn a new process in the container, we need the systemd
        // in the container to be fully initialized and listening for connections.
        // One way to resolve this issue is to test the connection to the container's systemd.
        if try_open_container_bus(ns_name).is_ok() {
            return Ok(());
        }
        // wait for a while, sleep time follows a natural-logarithm distribution
        sleep(Duration::from_secs_f32(((i + 1) as f32).ln().ceil()));
    }

    Err(anyhow!("Timeout waiting for container {}", ns_name))
}

/// Setting up cross-namespace bind-mounts for the container using systemd
fn setup_bind_mounts(ns_name: &str, mounts: &[(String, &str)]) -> Result<()> {
    let conn = Connection::system()?;
    let proxy = ManagerProxyBlocking::new(&conn)?;
    for mount in mounts {
        fs::create_dir_all(&mount.0)?;
        let source_path = fs::canonicalize(&mount.0)?;
        proxy.bind_mount_machine(
            ns_name,
            &source_path.to_string_lossy(),
            mount.1,
            false,
            true,
        )?;
    }

    Ok(())
}

/// Get the container name (ns_name) of the instance
pub fn get_container_ns_name<P: AsRef<Path>>(path: P, legacy: bool) -> Result<String> {
    let current_dir = std::env::current_dir()?;
    let path = current_dir.join(path);
    if legacy {
        warn!("You are working in a legacy workspace. Use `ciel init --upgrade` to upgrade.");
        warn!("Please make sure to save your work before upgrading.");
        return legacy_container_name(&path);
    }

    new_container_name(&path)
}

/// Spawn a new container using nspawn
pub fn spawn_container<P: AsRef<Path>>(
    ns_name: &str,
    path: P,
    extra_options: &[String],
    mounts: &[(String, &str)],
) -> Result<()> {
    let path = path
        .as_ref()
        .to_str()
        .ok_or_else(|| anyhow!("Path contains invalid Unicode characters."))?;
    let mut child = Command::new("systemd-nspawn")
        .args(DEFAULT_NSPAWN_OPTIONS)
        .args(extra_options)
        .args(["-D", path, "-M", ns_name, "--"])
        .env("SYSTEMD_NSPAWN_TMPFS_TMP", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    info!("{}: waiting for container to start...", ns_name);
    wait_for_container(&mut child, ns_name, 10)?;
    info!("{}: setting up mounts...", ns_name);
    if let Err(e) = setup_bind_mounts(ns_name, mounts) {
        warn!("Failed to setup bind mounts: {:?}", e);
    }

    Ok(())
}

/// Execute a command in the container
pub fn execute_container_command<S: AsRef<OsStr>>(ns_name: &str, args: &[S]) -> Result<i32> {
    let mut extra_options = vec!["--setenv=HOME=/root".to_string()];
    if std::env::var("CIEL_STAGE2").is_ok() {
        extra_options.push("--setenv=ABSTAGE2=1".to_string());
    }
    // TODO: maybe replace with systemd API cross-namespace call?
    let exit_code = Command::new("systemd-run")
        .args(extra_options)
        .args(["-M", ns_name, "-qt", "--"])
        .args(args)
        .spawn()?
        .wait()?
        .code()
        .unwrap_or(127);

    Ok(exit_code)
}

/// Reap all the exited child processes
pub(crate) fn clean_child_process() {
    let mut status = 0;
    unsafe { waitpid(-1, &mut status, WNOHANG) };
}

fn kill_container(proxy: &MachineProxyBlocking) -> Result<()> {
    proxy.kill("all", libc::SIGKILL)?;
    proxy.terminate()?;

    Ok(())
}

fn execute_poweroff(ns_name: &str) -> Result<()> {
    // TODO: maybe replace with systemd API cross-namespace call?
    let exit_code = Command::new("systemd-run")
        .args(["-M", ns_name, "-q", "--no-block", "--", "poweroff"])
        .spawn()?
        .wait()?
        .code()
        .unwrap_or(127);

    if exit_code != 0 {
        Err(anyhow!("Could not execute shutdown command: {}", exit_code))
    } else {
        Ok(())
    }
}

fn wait_for_poweroff(proxy: &MachineProxyBlocking) -> Result<()> {
    let ns_name = proxy.name()?;
    let conn = proxy.connection();
    let proxy = ManagerProxyBlocking::new(conn)?;
    for _ in 0..10 {
        if proxy.get_machine(&ns_name).is_err() {
            // machine object no longer exists
            return Ok(());
        }
        sleep(Duration::from_secs(1));
    }

    Err(anyhow!("shutdown failed"))
}

fn is_booted(proxy: &MachineProxyBlocking) -> Result<bool> {
    let leader_pid = proxy.leader()?;
    // let's inspect the cmdline of the PID 1 in the container
    let f = std::fs::read(format!("/proc/{}/cmdline", leader_pid))?;
    // take until the first null byte
    let pos: usize = f
        .iter()
        .position(|c| *c == 0u8)
        .ok_or_else(|| anyhow!("Unable to parse the process cmdline of PID 1 in the container"))?;
    // ... well, of course it's a path
    let path = Path::new(OsStr::from_bytes(&f[..pos]));
    let exe_name = path.file_name();
    // if PID 1 is systemd or init (System V init) then it should be a "booted" container
    if let Some(exe_name) = exe_name {
        return Ok(exe_name == "systemd" || exe_name == "init");
    }

    Ok(false)
}

fn terminate_container(proxy: &MachineProxyBlocking) -> Result<()> {
    let ns_name = proxy.name()?;
    let _ = proxy.receive_state_changed();
    if execute_poweroff(&ns_name).is_ok() {
        // Successfully passed poweroff command to the container, wait for it
        if wait_for_poweroff(proxy).is_ok() {
            return Ok(());
        }
        // still did not poweroff?
        warn!("Container did not respond to the poweroff command correctly...");
        warn!("Killing the container by sending SIGKILL...");
        // fall back to nuke
    }

    // violently kill everything inside the container
    kill_container(proxy)?;
    proxy.terminate().ok();
    // status re-check, in the event of I/O problems, the container may still be running (stuck)
    if wait_for_poweroff(proxy).is_ok() {
        return Ok(());
    }

    Err(anyhow!("Failed to kill the container! This may indicate a problem with your I/O, see dmesg or journalctl for more details."))
}

/// Terminate the container (Use graceful method if possible)
pub fn terminate_container_by_name(ns_name: &str) -> Result<()> {
    let conn = Connection::system()?;
    let proxy = ManagerProxyBlocking::new(&conn)?;
    let path = proxy.get_machine(ns_name)?;
    let proxy = MachineProxyBlocking::builder(&conn).path(&path)?.build()?;

    terminate_container(&proxy)
}

/// Mount the filesystem layers using the specified layer manager and the instance name
pub fn mount_layers(manager: &mut dyn LayerManager, name: &str) -> Result<()> {
    let target = std::env::current_dir()?.join(name);
    if !manager.is_mounted(&target)? {
        fs::create_dir_all(&target)?;
        manager.mount(&target)?;
    }

    Ok(())
}

/// Get the information of the container specified
pub fn inspect_instance(name: &str, ns_name: &str) -> Result<CielInstance> {
    let full_path = std::env::current_dir()?.join(name);
    let mounted = is_mounted(&full_path, OsStr::new("overlay"))?;
    let conn = Connection::system()?;
    let proxy = ManagerProxyBlocking::new(&conn)?;
    let path = proxy.get_machine(ns_name);
    if let Err(e) = path {
        if let zbus::Error::MethodError(ref err_name, _, _) = e {
            if err_name.as_ref() == "org.freedesktop.machine1.NoSuchMachine" {
                return Ok(CielInstance {
                    name: name.to_owned(),
                    ns_name: ns_name.to_owned(),
                    started: false,
                    running: false,
                    mounted,
                    booted: None,
                });
            }
        }
        // For all other errors, just return the original error object
        return Err(anyhow!("{}", e));
    }
    let path = path?;
    let proxy = MachineProxyBlocking::builder(&conn).path(&path)?.build()?;
    let state = proxy.state()?;
    // Sometimes the system in the container is misconfigured, so we also accept "degraded" status as "running"
    let running = state == "running" || state == "degraded";
    let booted = is_booted(&proxy)?;

    Ok(CielInstance {
        name: name.to_owned(),
        ns_name: ns_name.to_owned(),
        started: true,
        running,
        mounted,
        booted: Some(booted),
    })
}

/// List all the instances under the current directory
pub fn list_instances() -> Result<Vec<CielInstance>> {
    let legacy = is_legacy_workspace()?;
    let mut instances: Vec<CielInstance> = Vec::new();
    for entry in (fs::read_dir(CIEL_INST_DIR)?).flatten() {
        if entry.file_type().map(|e| e.is_dir())? {
            instances.push(inspect_instance(
                &entry.file_name().to_string_lossy(),
                &get_container_ns_name(&entry.file_name(), legacy)?,
            )?);
        }
    }

    Ok(instances)
}

/// List all the instances under the current directory, returns only instance names
pub fn list_instances_simple() -> Result<Vec<String>> {
    let mut instances: Vec<String> = Vec::new();
    for entry in (fs::read_dir(CIEL_INST_DIR)?).flatten() {
        if entry.file_type().map(|e| e.is_dir())? {
            instances.push(entry.file_name().to_string_lossy().to_string());
        }
    }

    Ok(instances)
}

/// Print all the instances under the current directory
pub fn print_instances() -> Result<()> {
    use crate::logging::color_bool;
    use std::io::Write;
    use tabwriter::TabWriter;

    let instances = list_instances()?;
    let mut formatter = TabWriter::new(std::io::stderr());
    writeln!(&mut formatter, "NAME\tMOUNTED\tRUNNING\tBOOTED")?;
    for instance in instances {
        let mounted = color_bool(instance.mounted);
        let running = color_bool(instance.running);
        let booted = {
            if let Some(booted) = instance.booted {
                color_bool(booted)
            } else {
                // dim
                "\x1b[2m-\x1b[0m"
            }
        };
        writeln!(
            &mut formatter,
            "{}\t{}\t{}\t{}",
            instance.name, mounted, running, booted
        )?;
    }
    formatter.flush()?;

    Ok(())
}

#[test]
fn test_inspect_instance() {
    println!("{:#?}", inspect_instance("alpine", "alpine"));
}

#[test]
fn test_container_name() {
    assert_eq!(
        get_container_ns_name(Path::new("/tmp/"), false).unwrap(),
        "tmp-51601b0".to_string()
    );
    println!(
        "{:#?}",
        get_container_ns_name(Path::new("/tmp/"), true).unwrap()
    );
}
