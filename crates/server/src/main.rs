fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match kingdom_server::Config::parse(std::env::args().skip(1)) {
        Ok(config) => kingdom_server::run(config),
        Err(text) => {
            eprintln!("{text}");
            std::process::exit(if text.starts_with("Usage") { 0 } else { 2 });
        }
    }
}
