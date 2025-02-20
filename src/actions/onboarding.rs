use anyhow::{anyhow, Result};
use console::{style, user_attended};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use std::{fs, path::Path};

use crate::{
    cli::GIT_TREE_URL,
    common::*,
    config, error, info,
    network::{download_git, pick_latest_tarball},
    overlayfs::create_new_instance_fs,
    repo::{init_repo, refresh_repo},
    warn,
};

use super::{load_os, mount_fs};

/// Show interactive onboarding guide, triggered by issuing `ciel new`
pub fn onboarding(custom_tarball: Option<&String>, arch: Option<&str>) -> Result<()> {
    let theme = ColorfulTheme::default();
    info!("Welcome to ciel!");
    if Path::new(".ciel").exists() {
        error!("Seems like you've already created a ciel workspace here.");
        info!("Please run `ciel farewell` to nuke it before running this command.");
        return Err(anyhow!("Unable to create a ciel workspace."));
    }
    info!("Before continuing, I need to ask you a few questions:");
    let real_arch = if let Some(arch) = arch {
        arch
    } else if custom_tarball.is_some() {
        "custom"
    } else {
        ask_for_target_arch()?
    };
    let config = config::ask_for_config(None)?;
    let mut init_instance: Option<String> = None;
    if user_attended()
        && Confirm::with_theme(&theme)
            .with_prompt("Do you want to add a new instance now?")
            .interact()?
    {
        let name: String = Input::with_theme(&theme)
            .with_prompt("Name of the instance")
            .interact_text()?;
        init_instance = Some(name.clone());
        info!(
            "Understood. `{}` will be created after initialization is finished.",
            name
        );
    } else {
        info!("Okay. You can always add a new instance later.");
    }

    info!("Initializing workspace...");
    ciel_init()?;
    info!("Initializing container OS...");
    let (tarball_url, tarball_sha256) = match custom_tarball {
        Some(tarball) => {
            info!("Using custom tarball from {}", tarball);
            (tarball.clone(), None)
        }
        None => {
            info!("Searching for latest AOSC OS buildkit release...");
            auto_pick_tarball(&theme, real_arch)?
        }
    };
    load_os(&tarball_url, tarball_sha256)?;
    info!("Initializing ABBS tree...");
    if Path::new("TREE").is_dir() {
        warn!("TREE already exists, skipping this step...");
    } else {
        // if TREE is a file, then remove it
        fs::remove_file("TREE").ok();
        download_git(GIT_TREE_URL, Path::new("TREE"))?;
    }
    config::apply_config(CIEL_DIST_DIR, &config)?;
    info!("Applying configurations...");
    fs::write(
        Path::new(CIEL_DATA_DIR).join("config.toml"),
        config.save_config()?,
    )?;
    info!("Configurations applied.");
    let cwd = std::env::current_dir()?;
    if config.local_repo {
        info!("Setting up local repository ...");
        refresh_repo(&cwd.join("OUTPUT"))?;
        info!("Local repository ready.");
    }
    if let Some(init_instance) = init_instance {
        create_new_instance_fs(CIEL_INST_DIR, &init_instance)?;
        info!("{}: instance initialized.", init_instance);
        if config.local_repo {
            mount_fs(&init_instance)?;
            init_repo(&cwd.join("OUTPUT"), &cwd.join(&init_instance))?;
            info!("{}: local repository initialized.", init_instance);
        }
    }

    Ok(())
}

#[inline]
fn auto_pick_tarball(
    theme: &dyn dialoguer::theme::Theme,
    arch: &str,
) -> Result<(String, Option<String>)> {
    if let Ok(tarball) = pick_latest_tarball(arch) {
        info!(
            "Ciel has picked buildkit for {}, released on {}",
            tarball.arch, tarball.date
        );
        Ok((
            format!("https://releases.aosc.io/{}", tarball.path),
            Some(tarball.sha256sum),
        ))
    } else {
        warn!(
            "Ciel was unable to find a suitable buildkit release. Please specify the URL manually."
        );
        let tarball_url = Input::<String>::with_theme(theme)
            .with_prompt("Tarball URL")
            .interact_text()?;
        Ok((tarball_url, None))
    }
}
