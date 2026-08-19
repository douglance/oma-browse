//! Resolve every installed Omarchy theme, to prove the palette path holds across
//! the whole stock set — including light themes and sparse `colors.toml` files.
//!
//!     cargo run -p oma-theme --example survey

use std::path::PathBuf;

fn main() {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in ["/usr/share/omarchy/themes", &format!("{}/.config/omarchy/themes", env!("HOME"))] {
        if let Ok(rd) = std::fs::read_dir(root) {
            dirs.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    dirs.sort();

    let (mut ok, mut skipped, mut failed) = (0, 0, 0);
    println!("{:<28} {:>6} {:>6}  {:<9} {:<9} {}", "THEME", "KEYS", "MODE", "BG", "FG", "ACCENT");

    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let colors = dir.join("colors.toml");
        if !colors.exists() {
            // Not a failure: overlay themes ship only the files they override,
            // and Omarchy synthesises colors.toml from alacritty.toml at
            // theme-set time. The *live* theme directory always has one.
            println!("{name:<28} {:>6} {:>6}  (overlay: no colors.toml of its own)", "-", "-");
            skipped += 1;
            continue;
        }
        match oma_theme::Palette::resolve_file(&colors) {
            Ok(p) => {
                println!(
                    "{name:<28} {:>6} {:>6}  {:<9} {:<9} {}",
                    p.len(),
                    p.mode().as_str(),
                    p.background().to_hex(),
                    p.foreground().to_hex(),
                    p.accent().to_hex()
                );
                ok += 1;
            }
            Err(e) => {
                println!("{name:<28} FAILED: {e}");
                failed += 1;
            }
        }
    }
    println!("\n{ok} resolved, {skipped} overlay themes, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
}
