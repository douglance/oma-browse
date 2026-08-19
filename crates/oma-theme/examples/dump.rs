//! Print the live Omarchy theme as this crate resolves it.
//!
//!     cargo run -p oma-theme --example dump

fn main() {
    let theme = oma_theme::Theme::load();
    let css = theme.css();

    eprintln!("theme: {}", css.theme_name);
    eprintln!("mode:  {}", css.mode_str());
    eprintln!("colors resolved: {}", theme.palette.len());
    eprintln!("shell.toml sections present: controls={} launcher={} menu={} font={}",
        theme.shell.has_section("controls"),
        theme.shell.has_section("launcher"),
        theme.shell.has_section("menu"),
        theme.shell.has_section("font"));
    eprintln!("fingerprint: {:016x}", css.fingerprint);
    eprintln!("---");

    println!("{}", css.chrome);
}
