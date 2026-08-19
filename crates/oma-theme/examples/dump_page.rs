fn main() {
    let recolor = std::env::args().nth(1).as_deref() == Some("recolor");
    print!("{}", oma_theme::Theme::load().css().page_script(recolor));
}
