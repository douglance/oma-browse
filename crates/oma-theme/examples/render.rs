fn main() {
    for name in ["tokyo-night", "catppuccin-latte"] {
        let tsv = std::fs::read_to_string(format!("{}/{name}.tsv", std::env::args().nth(1).unwrap())).unwrap();
        let p = oma_theme::Palette::from_resolver_output(&tsv).unwrap();
        println!("{name:>18}  mode={:<5}  bg={}  fg={}  accent={}",
            p.mode().as_str(), p.background(), p.foreground(), p.accent());
    }
}
