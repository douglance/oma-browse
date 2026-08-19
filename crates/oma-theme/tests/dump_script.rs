// Not an assertion so much as a tap: `OMA_DUMP_SCRIPT=<path> cargo test -p
// oma-theme --test dump_script` writes the injected page runtime out so it can
// be run through a real JavaScript parser. A syntax error in there is otherwise
// invisible from Rust -- the script simply never runs, and pages stay unthemed.
#[test]
fn dump() {
    let Ok(path) = std::env::var("OMA_DUMP_SCRIPT") else { return };
    let theme = oma_theme::Theme::load();
    let css = oma_theme::css::ThemeCss::build(&theme);
    std::fs::write(path, css.page_script(true)).expect("write");
}
