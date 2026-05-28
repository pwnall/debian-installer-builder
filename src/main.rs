use anyhow::Result;
use clap::Parser;
use std::fs;
use std::path::PathBuf;

use debian_installer_builder::{
    check_xorriso, download_iso, flash_to_usb, get_latest_debian_iso_url, inject_preseed,
};

/// Tool to download, preseed, and flash a Debian server image to a USB drive
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Target USB block device (e.g., /dev/sdX on Linux)
    #[arg(short, long)]
    device: PathBuf,

    /// Path to the preseed file
    #[arg(short, long)]
    preseed: PathBuf,

    /// URL to the Debian DVD ISO. If omitted, downloads the latest stable release.
    #[arg(short, long)]
    url: Option<String>,

    /// Optional path to a pre-downloaded ISO (skips download if provided)
    #[arg(short, long)]
    iso: Option<PathBuf>,

    /// Override the username in the preseed file
    #[arg(long)]
    username: Option<String>,

    /// Override the password in the preseed file
    #[arg(long)]
    password: Option<String>,

    /// Override the hostname in the preseed file
    #[arg(long)]
    hostname: Option<String>,

    /// Enable serial console output for the installer (useful for headless VMs)
    #[arg(long)]
    serial_console: bool,
}

/// Main entry point for the application.
///
/// This function parses command-line arguments, checks prerequisites, downloads
/// or uses a local Debian ISO, injects a preseed configuration into it,
/// and finally writes the customized ISO to the specified target USB device.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 1. Check permissions / prerequisites
    if !args.preseed.exists() {
        anyhow::bail!("Preseed file not found at: {:?}", args.preseed);
    }
    check_xorriso()?;

    // 2. Download or use existing ISO
    let iso_path = if let Some(path) = args.iso {
        if !path.exists() {
            anyhow::bail!("Provided ISO path does not exist: {:?}", path);
        }
        path
    } else {
        let temp_dir = std::env::temp_dir();
        let download_path = temp_dir.join("debian-dvd.iso");

        let url = match args.url {
            Some(u) => u,
            None => get_latest_debian_iso_url().await?,
        };

        download_iso(&url, &download_path).await?;
        download_path
    };

    // 3. Inject Preseed
    let custom_iso_path = std::env::temp_dir().join("debian-custom.iso");
    if custom_iso_path.exists() {
        let _ = fs::remove_file(&custom_iso_path);
    }
    inject_preseed(
        &iso_path,
        &args.preseed,
        &custom_iso_path,
        args.username.as_deref(),
        args.password.as_deref(),
        args.hostname.as_deref(),
        args.serial_console,
    )?;

    // 4. Flash to USB
    flash_to_usb(&custom_iso_path, &args.device)?;

    println!(
        "Success! The customized Debian installer has been written to {:?}",
        args.device
    );
    Ok(())
}
